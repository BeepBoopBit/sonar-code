//! Bounded repository anchors in ordinary prose. No rewriting or model inference.
use crate::{candidate::Candidate, search, workspace::SourceFile};
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct AnchorDiagnostic {
    pub status: String,
    pub anchors: Vec<EmbeddedAnchor>,
    pub channel: Vec<AnchorContribution>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddedAnchor {
    pub kind: String,
    pub key: String,
    pub text: String,
    pub query_bytes: [usize; 2],
    pub owners: Vec<AnchorOwner>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnchorOwner {
    pub location_id: i64,
    pub path: String,
    /// BLAKE3, matching the indexed file and current source scan.
    pub source_blake3: String,
    pub evidence: Vec<AnchorEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnchorEvidence {
    Path {
        value: String,
    },
    Symbol {
        value: String,
    },
    ResourceStem {
        value: String,
    },
    Source {
        lines: [usize; 2],
        bytes: [usize; 2],
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct AnchorContribution {
    pub location_id: i64,
    pub rank: usize,
    pub base_score: f64,
    pub addition: f64,
    pub anchor_indices: Vec<usize>,
    pub admitted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Path,
    Identifier,
    Quote,
    Resource,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Identifier => "identifier",
            Self::Quote => "quote",
            Self::Resource => "resource",
        }
    }
}

struct Surface {
    kind: Kind,
    key: String,
    span: [usize; 2],
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn identifier(s: &str) -> bool {
    let chars: Vec<_> = s.chars().collect();
    let distinctive = s.contains("::")
        || chars
            .windows(3)
            .any(|w| w[0].is_alphabetic() && w[1] == '_' && w[2].is_alphabetic())
        || chars
            .windows(2)
            .any(|w| w[0].is_lowercase() && w[1].is_uppercase());
    let mut parts = 0;
    let mut alpha = false;
    for (i, c) in chars.iter().copied().enumerate() {
        if matches!(c, ':' | '_') || i > 0 && chars[i - 1].is_lowercase() && c.is_uppercase() {
            parts += usize::from(alpha);
            alpha = false;
        }
        alpha |= c.is_alphabetic();
    }
    distinctive && parts + usize::from(alpha) >= 2
}

fn spans(s: &str, accept: impl Fn(char) -> bool) -> Vec<[usize; 2]> {
    let mut result = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if accept(c) {
            start.get_or_insert(i);
        } else if let Some(a) = start.take() {
            result.push([a, i]);
        }
    }
    if let Some(a) = start {
        result.push([a, s.len()]);
    }
    result
}

fn surfaces(query: &str) -> Vec<Surface> {
    let mut result = Vec::new();
    for [a, b] in spans(query, |c| word(c) || matches!(c, '.' | '/' | ':' | '-')) {
        let raw = &query[a..b];
        let value = raw.trim_matches(['.', ':', '-', '/']);
        if value.is_empty() {
            continue;
        }
        let path = raw.trim_end_matches(['.', ':']);
        let basename = path.rsplit('/').next().unwrap_or(path);
        if basename.contains('.') || path.contains('/') {
            result.push(Surface {
                kind: Kind::Path,
                key: path.strip_prefix("./").unwrap_or(path).into(),
                span: [a, a + path.len()],
            });
        }
        if identifier(value) {
            let start = a + raw.find(value).unwrap();
            result.push(Surface {
                kind: Kind::Identifier,
                key: normalize(value),
                span: [start, start + value.len()],
            });
        }
    }
    let chars: Vec<_> = query.char_indices().collect();
    let mut opened = None;
    for (i, &(byte, c)) in chars.iter().enumerate() {
        if !matches!(c, '\'' | '"' | '`') {
            continue;
        }
        if c == '\''
            && i > 0
            && chars[i - 1].1.is_alphanumeric()
            && chars.get(i + 1).is_some_and(|(_, c)| c.is_alphanumeric())
        {
            continue;
        }
        if chars[..i]
            .iter()
            .rev()
            .take_while(|(_, c)| *c == '\\')
            .count()
            % 2
            == 1
        {
            continue;
        }
        match opened {
            None => opened = Some((c, byte + c.len_utf8())),
            Some((delimiter, start)) if delimiter == c => {
                let value = &query[start..byte];
                let words: Vec<_> = value.split_whitespace().collect();
                if (2..=12).contains(&words.len()) && (8..=160).contains(&value.chars().count()) {
                    result.push(Surface {
                        kind: Kind::Quote,
                        key: words.join(" "),
                        span: [start, byte],
                    });
                }
                opened = None;
            }
            _ => {}
        }
    }
    for span in spans(query, word) {
        let value = &query[span[0]..span[1]];
        if value.chars().count() >= 4 && value.chars().all(char::is_alphabetic) {
            result.push(Surface {
                kind: Kind::Resource,
                key: value.to_lowercase(),
                span,
            });
        }
    }
    result
}

struct File<'a> {
    source: &'a SourceFile,
    lines: Vec<usize>,
}

struct Location {
    path: String,
    symbol: Option<String>,
    kind: String,
    definition: Option<usize>,
    parent: Option<i64>,
    bytes: [usize; 2],
    bounds: [usize; 2],
    regions: Vec<[usize; 2]>,
}

struct Index<'a> {
    files: BTreeMap<&'a str, File<'a>>,
    locations: BTreeMap<i64, Location>,
    by_file: BTreeMap<String, Vec<i64>>,
    paths: BTreeMap<String, BTreeSet<i64>>,
    symbols: BTreeMap<String, BTreeSet<i64>>,
    resources: BTreeMap<String, BTreeSet<i64>>,
}

type Owners = BTreeMap<i64, Vec<AnchorEvidence>>;

impl<'a> Index<'a> {
    fn load(
        conn: &Connection,
        current: &'a [SourceFile],
        scope: &str,
        stale: &BTreeSet<String>,
        resources_only: bool,
    ) -> Result<Self> {
        let indexed = crate::store::file_hashes(conn)?;
        let files = current
            .iter()
            .filter(|f| {
                indexed.get(&f.path) == Some(&f.hash)
                    && !stale.contains(&f.path)
                    && (scope.is_empty()
                        || f.path == scope
                        || f.path.starts_with(&format!("{scope}/")))
            })
            .map(|f| {
                let mut lines = vec![0];
                lines.extend(
                    f.source
                        .bytes()
                        .enumerate()
                        .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
                );
                (f.path.as_str(), File { source: f, lines })
            })
            .collect::<BTreeMap<_, _>>();
        let mut locations = BTreeMap::new();
        let mut by_file: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        let mut stmt = conn.prepare("SELECT l.id,f.path,l.symbol,l.kind,l.definition,l.parent_id,l.start_byte,l.end_byte FROM locations l JOIN files f ON f.id=l.file_id WHERE ?1=0 OR (l.kind='file' AND NOT EXISTS(SELECT 1 FROM locations child WHERE child.parent_id=l.id)) ORDER BY f.path,l.id")?;
        let mut rows = stmt.query([resources_only])?;
        while let Some(r) = rows.next()? {
            let path: String = r.get(1)?;
            if !files.contains_key(path.as_str()) {
                continue;
            }
            let id: i64 = r.get(0)?;
            by_file.entry(path.clone()).or_default().push(id);
            locations.insert(
                id,
                Location {
                    path,
                    symbol: r.get(2)?,
                    kind: r.get(3)?,
                    definition: r.get::<_, Option<u32>>(4)?.map(|n| n as usize),
                    parent: r.get(5)?,
                    bytes: [
                        usize::try_from(r.get::<_, i64>(6)?)?,
                        usize::try_from(r.get::<_, i64>(7)?)?,
                    ],
                    bounds: [0, 0],
                    regions: Vec::new(),
                },
            );
        }
        let ids = serde_json::to_string(&locations.keys().collect::<Vec<_>>())?;
        let mut stmt = conn.prepare("SELECT location_id,start_line,end_line FROM regions WHERE view='combined' AND location_id IN(SELECT value FROM json_each(?1)) ORDER BY start_line,id")?;
        let mut rows = stmt.query([ids])?;
        while let Some(r) = rows.next()? {
            if let Some(loc) = locations.get_mut(&r.get::<_, i64>(0)?) {
                loc.regions
                    .push([r.get::<_, u32>(1)? as usize, r.get::<_, u32>(2)? as usize]);
            }
        }
        let children: BTreeSet<_> = locations.values().filter_map(|l| l.parent).collect();
        let mut paths: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        let mut symbols: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        let mut resources: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
        for (&id, loc) in &mut locations {
            let file = &files[loc.path.as_str()];
            let first = loc
                .regions
                .iter()
                .map(|r| r[0])
                .min()
                .ok_or_else(|| anyhow::anyhow!("anchor location lacks source"))?;
            let last = loc.regions.iter().map(|r| r[1]).max().unwrap();
            let start = file.lines[first - 1];
            loc.bounds = if matches!(loc.kind.as_str(), "file" | "documentation") {
                [
                    start,
                    *file.lines.get(last).unwrap_or(&file.source.source.len()),
                ]
            } else {
                [
                    if first < loc.definition.unwrap_or(first) {
                        start.min(loc.bytes[0])
                    } else {
                        loc.bytes[0]
                    },
                    loc.bytes[1],
                ]
            };
            if let Some(symbol) = &loc.symbol {
                for name in [symbol.as_str(), symbol.rsplit("::").next().unwrap()] {
                    symbols.entry(normalize(name)).or_default().insert(id);
                }
            }
            if matches!(loc.kind.as_str(), "file" | "documentation") {
                for name in [loc.path.as_str(), loc.path.rsplit('/').next().unwrap()] {
                    paths.entry(name.into()).or_default().insert(id);
                }
            }
            if loc.kind == "file" && !children.contains(&id) {
                let stem = Path::new(&loc.path).file_stem().unwrap().to_str().unwrap();
                resources.entry(stem.to_lowercase()).or_default().insert(id);
            }
        }
        Ok(Self {
            files,
            locations,
            by_file,
            paths,
            symbols,
            resources,
        })
    }

    fn occurrences(&self, surface: &Surface, text: &str) -> Owners {
        let mut owners = Owners::new();
        for (path, ids) in &self.by_file {
            let file = &self.files[path.as_str()];
            for bytes in occurrences(&file.source.source, text, surface.kind == Kind::Quote) {
                let a = file.lines.partition_point(|&i| i <= bytes[0]);
                let b = file.lines.partition_point(|&i| i < bytes[1]);
                let mut hits = BTreeMap::new();
                for &id in ids {
                    let loc = &self.locations[&id];
                    if loc.bounds[0] > bytes[0] || loc.bounds[1] < bytes[1] {
                        continue;
                    }
                    let mut covered = a;
                    for &[x, y] in &loc.regions {
                        if x > covered {
                            break;
                        }
                        if y >= covered {
                            covered = y + 1;
                        }
                        if covered > b {
                            break;
                        }
                    }
                    if covered > b {
                        hits.insert(id, loc.bounds[1] - loc.bounds[0]);
                    }
                }
                for id in hits.keys().copied().collect::<Vec<_>>() {
                    let mut parent = self.locations[&id].parent;
                    while let Some(loc) = parent.and_then(|p| self.locations.get(&p)) {
                        hits.remove(&parent.unwrap());
                        parent = loc.parent;
                    }
                }
                if let Some(minimum) = hits.values().copied().min() {
                    for (id, width) in hits {
                        if width == minimum {
                            let evidence = AnchorEvidence::Source {
                                lines: [a, b],
                                bytes,
                            };
                            let entries = owners.entry(id).or_default();
                            if !entries.contains(&evidence) {
                                entries.push(evidence);
                            }
                        }
                    }
                }
            }
        }
        owners
    }

    fn resolve(&self, query: &str, surfaces: Vec<Surface>) -> Vec<EmbeddedAnchor> {
        let mut proposals: BTreeMap<[usize; 2], (Surface, Owners)> = BTreeMap::new();
        for surface in surfaces {
            let mut owners = Owners::new();
            match surface.kind {
                Kind::Path => {
                    if let Some(ids) = self.paths.get(&surface.key) {
                        for &id in ids {
                            owners.entry(id).or_default().push(AnchorEvidence::Path {
                                value: self.locations[&id].path.clone(),
                            });
                        }
                    }
                }
                Kind::Resource => {
                    if let Some(ids) = self.resources.get(&surface.key) {
                        for &id in ids {
                            owners
                                .entry(id)
                                .or_default()
                                .push(AnchorEvidence::ResourceStem {
                                    value: Path::new(&self.locations[&id].path)
                                        .file_stem()
                                        .unwrap()
                                        .to_str()
                                        .unwrap()
                                        .into(),
                                });
                        }
                    }
                }
                Kind::Identifier => {
                    if let Some(ids) = self.symbols.get(&surface.key) {
                        for &id in ids {
                            owners.entry(id).or_default().push(AnchorEvidence::Symbol {
                                value: self.locations[&id].symbol.clone().unwrap(),
                            });
                        }
                    }
                    for (id, evidence) in
                        self.occurrences(&surface, &query[surface.span[0]..surface.span[1]])
                    {
                        owners.entry(id).or_default().extend(evidence);
                    }
                }
                Kind::Quote => {
                    owners = self.occurrences(&surface, &query[surface.span[0]..surface.span[1]])
                }
            }
            if let Some((_, existing)) = proposals.get_mut(&surface.span) {
                for (id, evidence) in owners {
                    existing.entry(id).or_default().extend(evidence);
                }
            } else {
                proposals.insert(surface.span, (surface, owners));
            }
        }
        let mut unique = BTreeMap::new();
        for (span, (s, owners)) in proposals {
            if owners.is_empty() || owners.len() > 4 {
                continue;
            }
            let anchor = EmbeddedAnchor {
                kind: s.kind.name().into(),
                key: s.key.clone(),
                text: query[span[0]..span[1]].into(),
                query_bytes: span,
                owners: owners
                    .into_iter()
                    .map(|(id, evidence)| {
                        let path = &self.locations[&id].path;
                        AnchorOwner {
                            location_id: id,
                            path: path.clone(),
                            source_blake3: self.files[path.as_str()].source.hash.clone(),
                            evidence,
                        }
                    })
                    .collect(),
            };
            unique.entry((s.kind, s.key)).or_insert(anchor);
        }
        let mut anchors: Vec<_> = unique.into_values().collect();
        anchors.sort_by(|a, b| {
            (a.owners.len(), a.query_bytes[0], &a.kind, &a.key).cmp(&(
                b.owners.len(),
                b.query_bytes[0],
                &b.kind,
                &b.key,
            ))
        });
        anchors.truncate(8);
        anchors
    }
}

fn occurrences(source: &str, value: &str, quote: bool) -> Vec<[usize; 2]> {
    let mut result = Vec::new();
    if !quote {
        for (start, _) in source.match_indices(value) {
            let end = start + value.len();
            if !source[..start].chars().next_back().is_some_and(word)
                && !source[end..].chars().next().is_some_and(word)
            {
                result.push([start, end]);
            }
        }
    } else {
        let words: Vec<_> = value.split_whitespace().collect();
        let mut seek = 0;
        while let Some(offset) = source[seek..].find(words[0]) {
            let start = seek + offset;
            let mut end = start + words[0].len();
            let mut found = true;
            for word in &words[1..] {
                let space = source[end..]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum::<usize>();
                if space == 0 || !source[end + space..].starts_with(word) {
                    found = false;
                    break;
                }
                end += space + word.len();
            }
            if found {
                result.push([start, end]);
                seek = end;
            } else {
                seek = start + source[start..].chars().next().unwrap().len_utf8();
            }
        }
    }
    result
}

pub(crate) fn apply(
    conn: &Connection,
    query: &str,
    scope: &str,
    stale: &BTreeSet<String>,
    current: &[SourceFile],
    candidates: &mut Vec<Candidate>,
) -> Result<AnchorDiagnostic> {
    let mut diagnostic = AnchorDiagnostic {
        status: "no_anchors".into(),
        anchors: Vec::new(),
        channel: Vec::new(),
    };
    if candidates.iter().any(Candidate::exact) {
        diagnostic.status = "exact_bypass".into();
        return Ok(diagnostic);
    }
    if crate::quantity::canonical_query(query).is_some() {
        diagnostic.status = "quantity_bypass".into();
        return Ok(diagnostic);
    }
    let surfaces = surfaces(query);
    if surfaces.is_empty() {
        return Ok(diagnostic);
    }
    let index = Index::load(
        conn,
        current,
        scope,
        stale,
        surfaces.iter().all(|s| s.kind == Kind::Resource),
    )?;
    let anchors = index.resolve(query, surfaces);
    let mut support: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (i, anchor) in anchors.iter().enumerate() {
        for owner in &anchor.owners {
            support.entry(owner.location_id).or_default().push(i);
        }
    }
    let mut ordered: Vec<_> = support.into_iter().collect();
    ordered.sort_by_key(|(id, indices)| {
        let loc = &index.locations[id];
        (
            std::cmp::Reverse(indices.len()),
            indices
                .iter()
                .map(|&i| anchors[i].owners.len())
                .min()
                .unwrap(),
            indices
                .iter()
                .map(|&i| anchors[i].query_bytes[0])
                .min()
                .unwrap(),
            loc.path.clone(),
            loc.definition.unwrap_or(loc.regions[0][0]),
            *id,
        )
    });
    for (i, (id, anchor_indices)) in ordered.into_iter().enumerate() {
        let position = candidates.iter().position(|c| c.id == id);
        let admitted = position.is_none();
        let position = if let Some(position) = position {
            position
        } else {
            let mut candidate = search::load_parent(conn, id)?;
            candidate.hit.channel = "anchor".into();
            candidate.hits = vec![candidate.hit.clone()];
            candidates.push(candidate);
            candidates.len() - 1
        };
        let candidate = &mut candidates[position];
        let addition = 1.0 / (61.0 + i as f64);
        diagnostic.channel.push(AnchorContribution {
            location_id: id,
            rank: i + 1,
            base_score: candidate.score,
            addition,
            anchor_indices,
            admitted,
        });
        candidate.score += addition;
    }
    if !diagnostic.channel.is_empty() {
        diagnostic.status = "active".into();
        crate::candidate::sort(candidates);
    }
    diagnostic.anchors = anchors;
    Ok(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_preserve_utf8_and_quoted_contractions() {
        let q = "é why handle_event then \"library can't load\"?";
        let s = surfaces(q);
        let id = s.iter().find(|s| s.kind == Kind::Identifier).unwrap();
        assert_eq!(&q[id.span[0]..id.span[1]], "handle_event");
        assert_eq!(id.span[0], 7);
        assert!(
            s.iter()
                .any(|s| s.kind == Kind::Quote && s.key == "library can't load")
        );
        assert!(
            !surfaces("don't touch it")
                .iter()
                .any(|s| s.kind == Kind::Quote)
        );
        assert!(
            !surfaces("why \"never closed")
                .iter()
                .any(|s| s.kind == Kind::Quote)
        );
        assert!(identifier("δοκιμήEvent"));
    }

    #[test]
    fn literal_matches_are_whole_case_sensitive_and_whitespace_tolerant() {
        assert_eq!(
            occurrences(
                "xread_failed read_failed read_failed2",
                "read_failed",
                false
            ),
            vec![[13, 24]]
        );
        assert_eq!(
            occurrences("a library\n cannot load b", "library cannot load", true),
            vec![[2, 22]]
        );
        assert!(occurrences("library cannot load", "Library cannot load", true).is_empty());
        assert!(occurrences("library cannot load", "library cannot load now", true).is_empty());
    }
}
