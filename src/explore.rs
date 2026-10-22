//! Explicit navigation over a checked source snapshot; no retrieval or model is required.
use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::{Evidence, IndexState, Notice, search, store, workspace};

#[derive(Debug, Serialize)]
pub struct NavigationLocation {
    pub handle: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<usize>,
    pub range: [usize; 2],
    pub source_role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct NavigationLink {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub evidence: Evidence,
    /// Repeated occurrences are grouped; evidence identifies the first source occurrence.
    pub occurrences: usize,
}

#[derive(Debug, Serialize)]
pub struct NavigationResponse {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<NavigationLocation>,
    pub locations: Vec<NavigationLocation>,
    pub links: Vec<NavigationLink>,
    pub index: IndexState,
    pub notices: Vec<Notice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Incoming,
    Outgoing,
    #[default]
    Both,
}

#[derive(Debug, Clone, Copy, Default, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    #[default]
    All,
    Contains,
    Calls,
    References,
    Embeds,
    Documents,
}

impl Relation {
    fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Contains => "contains",
            Self::Calls => "calls",
            Self::References => "references",
            Self::Embeds => "embeds",
            Self::Documents => "documents",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NeighborOptions {
    pub relation: Relation,
    pub direction: Direction,
    pub scope: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
}

impl Default for NeighborOptions {
    fn default() -> Self {
        Self {
            relation: Relation::All,
            direction: Direction::Both,
            scope: None,
            limit: 20,
            cursor: None,
        }
    }
}

/// Handles bind the root, indexed source and extraction policy, plus the exact location identity.
/// SQLite row IDs alone are unsafe because reparsing can reuse them for other declarations.
pub(crate) fn generation(conn: &Connection) -> Result<String> {
    let values =
        ["root", "snapshot", "extraction_version", "resources"].map(|k| store::metadata(conn, k));
    let values: Vec<_> = values.into_iter().collect::<Result<_>>()?;
    Ok(blake3::hash(&serde_json::to_vec(&values)?)
        .to_hex()
        .to_string())
}

pub(crate) fn handle(conn: &Connection, generation: &str, id: i64) -> Result<String> {
    let identity = conn
        .query_row(
            "SELECT f.path,f.hash,l.symbol,l.kind,l.start_byte,l.end_byte,
            (SELECT min(start_line) FROM regions WHERE location_id=l.id AND view='combined'),
            (SELECT max(end_line) FROM regions WHERE location_id=l.id AND view='combined')
            FROM locations l JOIN files f ON f.id=l.file_id WHERE l.id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, Option<i64>>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                ))
            },
        )
        .context("navigation location no longer exists; locate it again")?;
    let digest = blake3::hash(&serde_json::to_vec(&identity)?);
    Ok(format!("sn1:{generation}:{id}:{}", digest.to_hex()))
}

pub(crate) struct Snapshot<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) generation: String,
    pub(crate) sources: BTreeMap<String, workspace::SourceFile>,
    pub(crate) index: IndexState,
    pub(crate) navigation_current: bool,
}

impl Snapshot<'_> {
    pub(crate) fn semantic_for(&self, id: i64) -> Result<bool> {
        let path: String = self.conn.query_row(
            "SELECT f.path FROM locations l JOIN files f ON f.id=l.file_id WHERE l.id=?1",
            [id],
            |r| r.get(0),
        )?;
        Ok(crate::languages::for_path(&path).is_some_and(|a| {
            self.index
                .languages
                .iter()
                .any(|l| l.language == a.id() && l.semantic_freshness == "current")
        }))
    }

    pub(crate) fn graph_notice(&self, id: i64) -> Result<Notice> {
        let path: String = self.conn.query_row(
            "SELECT f.path FROM locations l JOIN files f ON f.id=l.file_id WHERE l.id=?1",
            [id],
            |r| r.get(0),
        )?;
        if let Some(adapter) = crate::languages::for_path(&path)
            && !adapter.capabilities().resolved_references
        {
            return Ok(Notice::new(
                "language_analysis_unavailable",
                format!(
                    "{} supports structural navigation; resolved calls, references, and field-use analysis are not available for this language.",
                    adapter.id()
                ),
            ));
        }
        if crate::source_language(&path) == Some("typescript") {
            return Ok(Notice::new(
                "graph_unavailable",
                "Current TypeScript compiler relationships are unavailable; run `sonar resolve --typescript <compiler module>` after indexing.",
            ));
        }
        Ok(Notice::new(
            "graph_unavailable",
            "Current compiler-resolved relationships are unavailable; run `sonar resolve` after indexing.",
        ))
    }

    pub(crate) fn resolve(&self, value: &str) -> Result<i64> {
        ensure!(value.len() <= 256, "invalid navigation handle");
        let parts: Vec<_> = value.split(':').collect();
        ensure!(
            parts.len() == 4 && parts[0] == "sn1",
            "invalid navigation handle"
        );
        ensure!(
            parts[1] == self.generation,
            "navigation handle belongs to another repository or index generation; locate again"
        );
        let id: i64 = parts[2].parse().context("invalid navigation location ID")?;
        ensure!(
            id > 0 && value == handle(self.conn, &self.generation, id)?,
            "navigation handle identity changed; locate again"
        );
        Ok(id)
    }

    pub(crate) fn location(&self, id: i64) -> Result<NavigationLocation> {
        let (path, symbol, kind, definition, parent, start, end, is_test) = self.conn.query_row(
            "SELECT f.path,l.symbol,l.kind,l.definition,l.parent_id,l.start_byte,l.end_byte,l.is_test FROM locations l JOIN files f ON f.id=l.file_id WHERE l.id=?1",
            [id], |r| Ok((r.get::<_,String>(0)?, r.get::<_,Option<String>>(1)?, r.get::<_,String>(2)?,
                r.get::<_,Option<u32>>(3)?, r.get::<_,Option<i64>>(4)?, r.get::<_,u32>(5)? as usize,
                r.get::<_,u32>(6)? as usize, r.get::<_,bool>(7)?)),
        )?;
        let source = &self.sources[&path].source;
        ensure!(
            start <= end
                && end <= source.len()
                && source.is_char_boundary(start)
                && source.is_char_boundary(end),
            "invalid indexed source span"
        );
        let line = |byte| {
            source.as_bytes()[..byte]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1
        };
        let inherited_test = self.navigation_current
            && self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM navigation_test_files WHERE path=?1)",
                [&path],
                |r| r.get::<_, bool>(0),
            )?;
        let range = if kind == "documentation" {
            self.conn.query_row("SELECT min(start_line),max(end_line) FROM regions WHERE location_id=?1 AND view='combined'", [id],
                |r| Ok([r.get::<_,u32>(0)? as usize,r.get::<_,u32>(1)? as usize]))?
        } else {
            [line(start), line(end.saturating_sub(1).max(start))]
        };
        Ok(NavigationLocation {
            handle: handle(self.conn, &self.generation, id)?,
            language: crate::source_language(&path).map(str::to_owned),
            title: if kind == "documentation" {
                search::markdown_title(self.conn, id)?
            } else {
                None
            },
            source_role: if kind == "documentation" || path.ends_with(".md") {
                "documentation"
            } else if is_test || inherited_test {
                "test"
            } else {
                "implementation"
            }
            .into(),
            path,
            symbol,
            kind,
            definition: definition.map(|n| n as usize),
            range,
            parent: parent
                .map(|p| handle(self.conn, &self.generation, p))
                .transpose()?,
        })
    }

    pub(crate) fn response(&self) -> NavigationResponse {
        NavigationResponse {
            version: 1,
            anchor: None,
            locations: Vec::new(),
            links: Vec::new(),
            index: IndexState {
                snapshot: self.index.snapshot.clone(),
                semantic_profile: self.index.semantic_profile.clone(),
                syntax_freshness: self.index.syntax_freshness.clone(),
                semantic_freshness: self.index.semantic_freshness.clone(),
                languages: self.index.languages.clone(),
            },
            notices: crate::languages::coverage_notices(&self.index.languages),
            next_cursor: None,
        }
    }
}

pub(crate) fn read<T>(db: &Path, action: impl FnOnce(&Snapshot<'_>) -> Result<T>) -> Result<T> {
    let mut conn = store::open(db, false)?;
    let tx = conn.transaction()?;
    let root = store::metadata(&tx, "root")?.context("index sources first")?;
    let snapshot = store::metadata(&tx, "snapshot")?.context("index is missing its snapshot")?;
    let sources = workspace::scan_index(&tx, Path::new(&root))?;
    ensure!(
        workspace::snapshot(&sources) == snapshot,
        "indexed sources changed; run `sonar index` and locate again before navigating"
    );
    let profile = crate::semantic::profile(&tx)?;
    let languages = crate::languages::states(&tx, &sources, &snapshot, true)?;
    crate::semantic::prepare(&tx, &languages)?;
    let current = languages.iter().any(|l| l.semantic_freshness == "current");
    let context = Snapshot {
        conn: &tx,
        generation: generation(&tx)?,
        navigation_current: crate::navigation::current(&tx, true, &snapshot)?,
        sources: sources.into_iter().map(|f| (f.path.clone(), f)).collect(),
        index: IndexState {
            snapshot,
            semantic_freshness: crate::languages::aggregate_semantic(
                &languages,
                current,
                profile.is_some(),
            )
            .into(),
            languages,
            syntax_freshness: "current".into(),
            semantic_profile: profile,
        },
    };
    action(&context)
}

/// Find the smallest indexed declaration containing a UTF-8 byte position.
/// Without a column, consider declarations overlapping the one-based line.
pub fn locate(
    db: &Path,
    path: &str,
    line: usize,
    column: Option<usize>,
) -> Result<NavigationResponse> {
    let path = search::normalize_scope(Some(path))?;
    ensure!(
        !path.is_empty() && line > 0 && column != Some(0),
        "provide a repository-relative file and positive line/column"
    );
    read(db, |s| {
        let file = s
            .sources
            .get(&path)
            .context("file is outside the current navigation index")?;
        let mut offsets = vec![0];
        offsets.extend(
            file.source
                .bytes()
                .enumerate()
                .filter(|(i, b)| *b == b'\n' && i + 1 < file.source.len())
                .map(|(i, _)| i + 1),
        );
        ensure!(line <= offsets.len(), "line is outside the file");
        let lo = offsets[line - 1];
        let hi = offsets.get(line).copied().unwrap_or(file.source.len());
        let point = column
            .map(|c| lo.checked_add(c - 1).context("column overflow"))
            .transpose()?;
        if let Some(point) = point {
            ensure!(
                point < hi && file.source.is_char_boundary(point),
                "column is outside the line or splits a UTF-8 character"
            );
        }
        let (a, b) = point.map_or((lo, hi), |p| (p, p + 1));
        let id = s.conn.query_row(
            "SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path=?1
             AND (l.kind='file' OR (l.start_byte < ?3 AND l.end_byte > ?2)
                OR (l.kind='documentation' AND ?4 BETWEEN
                    (SELECT min(start_line) FROM regions WHERE location_id=l.id AND view='combined')
                    AND (SELECT max(end_line) FROM regions WHERE location_id=l.id AND view='combined')))
             ORDER BY l.kind='file', l.end_byte-l.start_byte, l.start_byte, l.id LIMIT 1",
            params![path, a as i64, b as i64, line as i64],
            |r| r.get(0),
        ).optional()?;
        let mut response = s.response();
        if let Some(id) = id {
            response.locations.push(s.location(id)?);
        } else {
            response.notices.push(Notice::new(
                "no_locations",
                "This file has no indexed location at the requested line.",
            ));
        }
        Ok(response)
    })
}

pub(crate) fn page(cursor: Option<&str>, request: &str, limit: usize) -> Result<usize> {
    ensure!(
        (1..=100).contains(&limit),
        "navigation limit must be between 1 and 100"
    );
    let Some(cursor) = cursor else { return Ok(0) };
    ensure!(cursor.len() <= 128, "invalid navigation cursor");
    let parts: Vec<_> = cursor.split(':').collect();
    ensure!(
        parts.len() == 3 && parts[0] == "sc1" && parts[1] == request,
        "navigation cursor belongs to another request or generation"
    );
    let offset: usize = parts[2]
        .parse()
        .context("invalid navigation cursor offset")?;
    ensure!(
        offset <= i64::MAX as usize - 100,
        "navigation cursor offset is too large"
    );
    Ok(offset)
}

pub(crate) fn request(value: impl Serialize) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(&value)?)
        .to_hex()
        .to_string())
}

fn continuation(response: &mut NavigationResponse, more: bool, request: &str, offset: usize) {
    if more {
        response.next_cursor = Some(format!("sc1:{request}:{offset}"));
        response.notices.push(Notice::new(
            "results_truncated",
            "More navigation results are available through next_cursor.",
        ));
    }
}

/// List declarations in source order, preserving parent handles and bounded continuation.
pub fn outline(
    db: &Path,
    path: &str,
    limit: usize,
    cursor: Option<&str>,
) -> Result<NavigationResponse> {
    let path = search::normalize_scope(Some(path))?;
    read(db, |s| {
        ensure!(
            s.sources.contains_key(&path),
            "file is outside the current navigation index"
        );
        let request = request((&s.generation, "outline", &path, s.navigation_current))?;
        let offset = page(cursor, &request, limit)?;
        let file = s.conn.query_row("SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path=?1 AND l.kind='file'", [&path], |r| r.get(0)).optional()?;
        let mut stmt = s.conn.prepare("SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path=?1 AND l.kind!='file'
            ORDER BY (SELECT min(start_line) FROM regions WHERE location_id=l.id AND view='combined'), l.start_byte,l.end_byte DESC,l.id LIMIT ?2 OFFSET ?3")?;
        let ids: Vec<i64> = stmt
            .query_map(params![path, (limit + 1) as i64, offset as i64], |r| {
                r.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut response = s.response();
        response.anchor = file.map(|id| s.location(id)).transpose()?;
        for id in ids.iter().take(limit) {
            response.locations.push(s.location(*id)?);
        }
        continuation(&mut response, ids.len() > limit, &request, offset + limit);
        Ok(response)
    })
}

/// Follow recorded connections without reranking or inferring runtime dispatch.
pub fn neighbors(db: &Path, anchor: &str, options: &NeighborOptions) -> Result<NavigationResponse> {
    let scope = search::normalize_scope(options.scope.as_deref())?;
    read(db, |s| {
        let id = s.resolve(anchor)?;
        let mut response = s.response();
        let location = s.location(id)?;
        ensure!(
            search::in_scope(&location.path, &scope),
            "anchor is outside the requested scope"
        );
        response.anchor = Some(location);
        let semantic = s.semantic_for(id)?;
        let request = request((
            &s.generation,
            "neighbors",
            anchor,
            options.relation,
            options.direction,
            &scope,
            &s.index.semantic_profile,
            semantic,
            s.navigation_current,
        ))?;
        let offset = page(options.cursor.as_deref(), &request, options.limit)?;
        let direction = match options.direction {
            Direction::Incoming => "to_id=?1",
            Direction::Outgoing => "from_id=?1",
            Direction::Both => "(from_id=?1 OR to_id=?1)",
        };
        let mut sources = vec![format!("SELECT parent_id from_id,id to_id,'contains' kind,
            (SELECT path FROM files WHERE id=locations.file_id) path,
            coalesce(definition,(SELECT min(start_line) FROM regions WHERE location_id=locations.id)) line,
            1 occurrences FROM locations WHERE parent_id IS NOT NULL AND {}", match options.direction {
                Direction::Incoming => "id=?1", Direction::Outgoing => "parent_id=?1", Direction::Both => "(id=?1 OR parent_id=?1)",
            })];
        if semantic {
            sources.push(format!("SELECT from_id,to_id,kind,path,min(line) line,count(*) occurrences FROM semantic_edges WHERE {direction} GROUP BY from_id,to_id,kind,path"));
        } else if matches!(
            options.relation,
            Relation::All | Relation::Calls | Relation::References
        ) {
            response.notices.push(s.graph_notice(id)?);
        }
        if s.navigation_current {
            sources.push(format!("SELECT from_id,to_id,kind,path,min(line) line,count(*) occurrences FROM navigation_edges WHERE {direction} GROUP BY from_id,to_id,kind,path"));
        } else if matches!(
            options.relation,
            Relation::All | Relation::Embeds | Relation::Documents
        ) {
            response.notices.push(Notice::new("navigation_unavailable", "Literal resource/document connections are unavailable; run `sonar index-navigation`."));
        }
        if semantic
            && matches!(
                options.relation,
                Relation::All | Relation::Calls | Relation::References
            )
        {
            response.notices.push(Notice::new("semantic_coverage", if response.anchor.as_ref().is_some_and(|l| l.language.as_deref() == Some("typescript")) {
                "Recorded compiler analysis omits unmapped/external and ambiguous targets. Static calls do not prove runtime dispatch."
            } else {
                "Recorded Rust analysis omits unmapped/external and ambiguous targets. Static calls do not prove runtime dispatch."
            }));
            if response
                .anchor
                .as_ref()
                .is_some_and(|l| l.language.as_deref() == Some("typescript"))
                && let Some(notice) =
                    crate::languages::typescript::semantic::coverage_notice(s.conn)?
            {
                response.notices.push(notice);
            }
        }
        let sql = format!("WITH edges AS ({}) SELECT e.from_id,e.to_id,e.kind,e.path,e.line,e.occurrences FROM edges e
            JOIN locations a ON a.id=e.from_id JOIN files af ON af.id=a.file_id
            JOIN locations b ON b.id=e.to_id JOIN files bf ON bf.id=b.file_id
            WHERE (?2='all' OR e.kind=?2)
            AND (?3='' OR af.path=?3 OR substr(af.path,1,length(?3)+1)=?3||'/')
            AND (?3='' OR bf.path=?3 OR substr(bf.path,1,length(?3)+1)=?3||'/')
            ORDER BY e.kind,af.path,e.from_id,bf.path,e.to_id,e.path,e.line LIMIT ?4 OFFSET ?5", sources.join(" UNION ALL "));
        let mut stmt = s.conn.prepare(&sql)?;
        let rows: Vec<_> = stmt
            .query_map(
                params![
                    id,
                    options.relation.name(),
                    scope,
                    (options.limit + 1) as i64,
                    offset as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, u32>(4)?,
                        r.get::<_, u32>(5)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        let mut locations = BTreeMap::new();
        for (from, to, kind, path, line, count) in rows.iter().take(options.limit) {
            for other in [from, to] {
                if *other != id && !locations.contains_key(other) {
                    locations.insert(*other, s.location(*other)?);
                }
            }
            response.links.push(NavigationLink {
                from: handle(s.conn, &s.generation, *from)?,
                to: handle(s.conn, &s.generation, *to)?,
                kind: kind.clone(),
                evidence: Evidence {
                    path: path.clone(),
                    line: *line as usize,
                },
                occurrences: *count as usize,
            });
        }
        response.locations = locations.into_values().collect();
        continuation(
            &mut response,
            rows.len() > options.limit,
            &request,
            offset + options.limit,
        );
        Ok(response)
    })
}
