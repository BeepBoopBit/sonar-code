//! Optional, source-bound navigation facts and bounded discovery experiments.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::{assembly, candidate::Candidate, search, store, workspace};

const POLICY: &str = "navigation-evidence-v1";
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS navigation_edges (
 from_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
 to_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
 kind TEXT NOT NULL, path TEXT NOT NULL, line INTEGER NOT NULL,
 start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
 PRIMARY KEY(from_id,to_id,kind,path,start_byte,end_byte));
CREATE INDEX IF NOT EXISTS navigation_edges_to ON navigation_edges(to_id);
CREATE TABLE IF NOT EXISTS navigation_test_files (
 path TEXT PRIMARY KEY, proof TEXT NOT NULL);
";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum NavigationPolicy {
    #[default]
    Off,
    Provenance,
    Owners,
    Siblings,
    /// Support retrieved functions with references to highly ranked fields/variants.
    Uses,
}

#[derive(Debug, Serialize)]
pub struct NavigationReport {
    pub snapshot: String,
    pub policy: String,
    pub edges: usize,
    pub test_files: BTreeMap<String, Vec<ModuleProof>>,
    pub skipped_parse_error_files: usize,
    pub unresolved_links: usize,
    pub unsupported_modules: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModuleProof {
    pub path: String,
    pub target: String,
    pub line: usize,
    pub bytes: [usize; 2],
    pub test_gate: bool,
}

#[derive(Debug, Serialize)]
pub struct NavigationDiagnostic {
    pub policy: NavigationPolicy,
    pub status: String,
    pub seeds: Vec<i64>,
    pub truncated: bool,
    pub channel: Vec<NavigationContribution>,
    pub role_corrections: Vec<i64>,
    pub edges: Vec<NavigationEdge>,
}

#[derive(Debug, Serialize)]
pub struct NavigationContribution {
    pub location_id: i64,
    pub rank: usize,
    pub relevance: f64,
    pub base_score: f64,
    pub addition: f64,
    pub admitted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_support: Option<ReferenceSupport>,
}

#[derive(Debug, Serialize)]
pub struct ReferenceSupport {
    pub seed: i64,
    pub users: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigationEdge {
    pub from: i64,
    pub to: i64,
    pub kind: String,
    pub path: String,
    pub line: usize,
    pub bytes: [usize; 2],
}

impl NavigationEdge {
    pub(crate) fn graph(&self) -> search::GraphEdge {
        search::GraphEdge {
            from: self.from,
            to: self.to,
            kind: self.kind.clone(),
            path: self.path.clone(),
            line: self.line,
        }
    }
}

/// Resolve only literal repository-relative paths; never follow filesystem links.
pub(crate) fn relative(directory: &Path, value: &str) -> Option<String> {
    if value.contains(['\\', ':', '\0']) || Path::new(value).is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for part in directory.join(value).components() {
        match part {
            Component::Normal(p) => parts.push(p.to_str()?.to_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn owner(conn: &Connection, path: &str, bytes: [usize; 2], line: usize) -> Result<Option<i64>> {
    let rust: Option<i64> = conn
        .query_row(
            "SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id
         WHERE f.path=?1 AND l.kind!='documentation' AND l.start_byte<=?2 AND l.end_byte>=?3
         ORDER BY l.end_byte-l.start_byte,l.id LIMIT 1",
            params![path, bytes[0] as i64, bytes[1] as i64],
            |r| r.get(0),
        )
        .optional()?;
    if rust.is_some() {
        return Ok(rust);
    }
    Ok(conn
        .query_row(
            "SELECT l.id FROM files f CROSS JOIN locations l ON l.file_id=f.id
        CROSS JOIN regions r ON r.location_id=l.id WHERE f.path=?1 AND l.kind='documentation'
        AND r.view='combined' AND r.start_line<=?2 AND r.end_line>=?2
        ORDER BY r.end_line-r.start_line,l.id LIMIT 1",
            params![path, line as i64],
            |r| r.get(0),
        )
        .optional()?)
}

fn file_owner(conn: &Connection, path: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id
        WHERE f.path=?1 AND l.kind='file' ORDER BY l.id LIMIT 1",
            [path],
            |r| r.get(0),
        )
        .optional()?)
}

pub(crate) struct Builder<'a> {
    pub(crate) conn: &'a Connection,
    pub(crate) files: BTreeMap<&'a str, &'a workspace::SourceFile>,
    pub(crate) modules: Vec<ModuleProof>,
    pub(crate) report: NavigationReport,
}

impl Builder<'_> {
    pub(crate) fn edge(
        &mut self,
        path: &str,
        span: [usize; 2],
        target: &str,
        kind: &str,
        target_line: Option<usize>,
    ) -> Result<()> {
        let Some(file) = self.files.get(path) else {
            return Ok(());
        };
        let line = file.source[..span[0]]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1;
        let from = owner(self.conn, path, span, line)?;
        let to = if let Some(target_file) = self.files.get(target) {
            if let Some(line) = target_line {
                let mut offset = 0;
                let mut found = None;
                for (i, s) in target_file.source.split_inclusive('\n').enumerate() {
                    if i + 1 == line {
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            let begin = offset + s.len() - s.trim_start().len();
                            found = Some([begin, begin + trimmed.len()]);
                        }
                        break;
                    }
                    offset += s.len();
                }
                if let Some(bytes) = found {
                    owner(self.conn, target, bytes, line)?
                } else {
                    None
                }
            } else if target.ends_with(".md") {
                // A document-level link does not identify an arbitrary heading.
                None
            } else {
                file_owner(self.conn, target)?
            }
        } else {
            None
        };
        if let (Some(from), Some(to)) = (from, to) {
            self.conn.execute(
                "INSERT OR IGNORE INTO navigation_edges VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    from,
                    to,
                    kind,
                    path,
                    line as i64,
                    span[0] as i64,
                    span[1] as i64
                ],
            )?;
        } else {
            self.report.unresolved_links += 1;
        }
        Ok(())
    }

    fn markdown(&mut self, file: &workspace::SourceFile) -> Result<()> {
        let mut offset = 0;
        let mut fence = None;
        for line in file.source.split_inclusive('\n') {
            let trimmed = line.trim_start();
            let marker = trimmed.chars().next().unwrap_or(' ');
            let width = trimmed.chars().take_while(|c| *c == marker).count();
            if matches!(marker, '`' | '~') && width >= 3 {
                if fence.is_none() {
                    fence = Some((marker, width));
                } else if fence.is_some_and(|(c, n)| c == marker && width >= n) {
                    fence = None;
                }
                offset += line.len();
                continue;
            }
            if fence.is_none()
                && !line.starts_with("    ")
                && !line.starts_with('\t')
                && !line.contains('`')
            {
                let mut seek = 0;
                while let Some(i) = line[seek..].find("](") {
                    let at = seek + i;
                    let Some(open) = line[..at].rfind('[') else {
                        break;
                    };
                    let Some(end) = line[at + 2..].find(')').map(|e| at + 2 + e) else {
                        break;
                    };
                    seek = end + 1;
                    if line[..open].ends_with(['!', '\\'])
                        || line[..open].matches('`').count() % 2 == 1
                    {
                        continue;
                    }
                    let value = line[at + 2..end].trim();
                    if value.is_empty() || value.contains([' ', '\t', '(', ')', '<', '>', '%', '?'])
                    {
                        continue;
                    }
                    let (value, fragment) = value
                        .split_once('#')
                        .map_or((value, None), |(v, f)| (v, Some(f)));
                    let target = if value.is_empty() {
                        Some(file.path.clone())
                    } else {
                        relative(
                            Path::new(&file.path).parent().unwrap_or(Path::new("")),
                            value,
                        )
                    };
                    let Some(target) = target else {
                        continue;
                    };
                    let target_line = if let Some(fragment) = fragment {
                        if let Some(line) = fragment
                            .strip_prefix('L')
                            .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                            .and_then(|v| v.parse::<usize>().ok())
                            .filter(|n| *n > 0)
                        {
                            Some(line)
                        } else if target.ends_with(".md") {
                            self.heading_line(&target, fragment)?
                        } else {
                            None
                        }
                    } else if target.ends_with(".md") {
                        Some(1)
                    } else {
                        None
                    };
                    if fragment.is_some() && target_line.is_none() {
                        self.report.unresolved_links += 1;
                        continue;
                    }
                    self.edge(
                        &file.path,
                        [offset + open, offset + end + 1],
                        &target,
                        "documents",
                        target_line,
                    )?;
                }
            }
            offset += line.len();
        }
        Ok(())
    }

    fn heading_line(&self, path: &str, fragment: &str) -> Result<Option<usize>> {
        let Some(file) = self.files.get(path) else {
            return Ok(None);
        };
        let mut stmt=self.conn.prepare("SELECT DISTINCT r.start_line FROM files f CROSS JOIN locations l ON l.file_id=f.id CROSS JOIN regions r ON r.location_id=l.id WHERE f.path=?1 AND l.kind='documentation' AND r.view='description' ORDER BY r.start_line")?;
        let starts = stmt
            .query_map([path], |r| r.get::<_, u32>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let lines: Vec<_> = file.source.lines().collect();
        let matches: Vec<_> = starts
            .into_iter()
            .filter(|n| {
                lines.get(*n as usize - 1).is_some_and(|line| {
                    if !line.trim_start().starts_with('#') {
                        return false;
                    }
                    let title = line.trim().trim_start_matches('#').trim();
                    let slug: String = title
                        .chars()
                        .filter(|c| {
                            c.is_alphanumeric() || c.is_whitespace() || matches!(c, '-' | '_')
                        })
                        .flat_map(char::to_lowercase)
                        .map(|c| if c.is_whitespace() { '-' } else { c })
                        .collect();
                    slug == fragment
                })
            })
            .collect();
        Ok((matches.len() == 1).then(|| matches[0] as usize))
    }
}

pub fn index_navigation(db: &Path) -> Result<NavigationReport> {
    let mut conn = store::open(db, true)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let root = store::metadata(&tx, "root")?.context("index sources first")?;
    let snapshot = store::metadata(&tx, "snapshot")?.context("index sources first")?;
    let files = workspace::scan_index(&tx, Path::new(&root))?;
    ensure!(
        workspace::snapshot(&files) == snapshot,
        "navigation indexing requires current sources; run sonar index first"
    );
    tx.execute_batch(SCHEMA)?;
    tx.execute("DELETE FROM navigation_edges", [])?;
    tx.execute("DELETE FROM navigation_test_files", [])?;
    let mut builder = Builder {
        conn: &tx,
        files: files.iter().map(|f| (f.path.as_str(), f)).collect(),
        modules: Vec::new(),
        report: NavigationReport {
            snapshot: snapshot.clone(),
            policy: POLICY.into(),
            edges: 0,
            test_files: BTreeMap::new(),
            skipped_parse_error_files: 0,
            unresolved_links: 0,
            unsupported_modules: 0,
        },
    };
    for file in &files {
        if let Some(adapter) = crate::languages::for_path(&file.path) {
            adapter.navigation(&mut builder, file)?;
        } else if file.path.ends_with(".md") {
            builder.markdown(file)?;
        }
    }
    // Only propagate along known routes. Shared files with a non-test route and
    // cycles with no proven gated ancestor are never labeled test-only.
    let incoming: BTreeMap<String, Vec<ModuleProof>> =
        builder
            .modules
            .into_iter()
            .fold(BTreeMap::new(), |mut m, p| {
                m.entry(p.target.clone()).or_default().push(p);
                m
            });
    let mut tests: BTreeSet<String> = BTreeSet::new();
    loop {
        let before = tests.len();
        for (target, proofs) in &incoming {
            if proofs
                .iter()
                .all(|p| p.test_gate || tests.contains(&p.path))
            {
                tests.insert(target.clone());
            }
        }
        if before == tests.len() {
            break;
        }
    }
    for path in tests {
        let proofs = incoming[&path].clone();
        tx.execute(
            "INSERT INTO navigation_test_files VALUES (?1,?2)",
            params![path, serde_json::to_string(&proofs)?],
        )?;
        builder.report.test_files.insert(path, proofs);
    }
    builder.report.edges = tx.query_row("SELECT count(*) FROM navigation_edges", [], |r| {
        r.get::<_, u32>(0)
    })? as usize;
    tx.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('navigation_policy',?1)",
        [POLICY],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO metadata VALUES ('navigation_snapshot',?1)",
        [snapshot],
    )?;
    let report = builder.report;
    tx.commit()?;
    Ok(report)
}

pub(crate) fn current(conn: &Connection, fresh: bool, snapshot: &str) -> Result<bool> {
    Ok(fresh
        && store::metadata(conn, "navigation_policy")?.as_deref() == Some(POLICY)
        && store::metadata(conn, "navigation_snapshot")?.as_deref() == Some(snapshot))
}

pub(crate) fn roles(conn: &Connection, candidates: &mut [Candidate]) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT path FROM navigation_test_files")?;
    let paths = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut corrected = Vec::new();
    for c in candidates {
        if paths.contains(&c.path) && !c.is_test {
            c.is_test = true;
            corrected.push(c.id);
        }
    }
    Ok(corrected)
}

pub(crate) fn edges(
    conn: &Connection,
    seeds: &[i64],
    scope: &str,
    semantic: bool,
) -> Result<(Vec<NavigationEdge>, bool)> {
    let mut result = BTreeMap::new();
    let mut truncated = false;
    for seed in seeds {
        let tables = if semantic {
            vec!["navigation_edges", "semantic_edges"]
        } else {
            vec!["navigation_edges"]
        };
        let mut rows = Vec::new();
        for table in tables {
            let mut stmt=conn.prepare(&format!("SELECT e.from_id,e.to_id,e.kind,e.path,e.line,e.start_byte,e.end_byte FROM {table} e JOIN locations a ON a.id=e.from_id JOIN files af ON af.id=a.file_id JOIN locations b ON b.id=e.to_id JOIN files bf ON bf.id=b.file_id WHERE (e.from_id=?1 OR e.to_id=?1) AND e.kind!='declares_module' AND (?2='' OR af.path=?2 OR substr(af.path,1,length(?2)+1)=?2||'/') AND (?2='' OR bf.path=?2 OR substr(bf.path,1,length(?2)+1)=?2||'/') ORDER BY e.kind,e.path,e.line,e.from_id,e.to_id,e.start_byte"))?;
            rows.extend(
                stmt.query_map(params![seed, scope], |r| {
                    Ok(NavigationEdge {
                        from: r.get(0)?,
                        to: r.get(1)?,
                        kind: r.get(2)?,
                        path: r.get(3)?,
                        line: r.get::<_, u32>(4)? as usize,
                        bytes: [r.get::<_, i64>(5)? as usize, r.get::<_, i64>(6)? as usize],
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?,
            );
        }
        rows.sort_by_key(|e| {
            (
                e.kind.clone(),
                e.path.clone(),
                e.line,
                e.from,
                e.to,
                e.bytes,
            )
        });
        let mut neighbors = BTreeSet::new();
        for e in rows {
            let neighbor = if e.from == *seed { e.to } else { e.from };
            if neighbor == *seed || neighbors.contains(&neighbor) {
                continue;
            }
            if neighbors.len() == 32 {
                truncated = true;
                continue;
            }
            neighbors.insert(neighbor);
            result.entry((e.from, e.to, e.kind.clone())).or_insert(e);
        }
    }
    Ok((result.into_values().collect(), truncated))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    conn: &Connection,
    candidates: &mut Vec<Candidate>,
    query: &str,
    scope: &str,
    policy: NavigationPolicy,
    excluded: &BTreeSet<String>,
    available: bool,
    semantic: bool,
    relevance_query: assembly::Query<'_>,
) -> Result<NavigationDiagnostic> {
    let mut d = NavigationDiagnostic {
        policy,
        status: "unavailable".into(),
        seeds: Vec::new(),
        truncated: false,
        channel: Vec::new(),
        role_corrections: Vec::new(),
        edges: Vec::new(),
    };
    if !available {
        return Ok(d);
    }
    d.role_corrections = roles(conn, candidates)?;
    d.status = if candidates.iter().any(Candidate::exact) {
        "exact_bypass"
    } else if crate::quantity::canonical_query(query).is_some() {
        "quantity_bypass"
    } else if policy == NavigationPolicy::Provenance {
        "provenance"
    } else {
        "inactive"
    }
    .into();
    if d.status != "inactive" {
        return Ok(d);
    }
    d.seeds = candidates.iter().take(5).map(|c| c.id).collect();
    let mut eligible = BTreeSet::new();
    if policy == NavigationPolicy::Owners {
        (d.edges, d.truncated) = edges(conn, &d.seeds, scope, semantic)?;
        for e in &d.edges {
            eligible.extend([e.from, e.to]);
        }
    } else {
        let paths: BTreeSet<_> = candidates.iter().take(5).map(|c| &c.path).collect();
        let mut stmt=conn.prepare("SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path IN (SELECT value FROM json_each(?1)) AND l.kind NOT IN ('file','module','impl','trait','struct','enum') ORDER BY f.path,coalesce(l.definition,0),l.id LIMIT 2049")?;
        let ids = stmt
            .query_map([serde_json::to_string(&paths)?], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        d.truncated = ids.len() > 2048;
        eligible.extend(ids.into_iter().take(2048));
    }
    let mut locations = eligible
        .iter()
        .map(|id| Ok((*id, search::load_parent(conn, *id)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    locations.retain(|_, c| !excluded.contains(&c.path));
    eligible.retain(|id| locations.contains_key(id));
    d.edges.retain(|e| {
        !excluded.contains(&e.path)
            && locations.contains_key(&e.from)
            && locations.contains_key(&e.to)
    });
    let mut measured = assembly::relevance(conn, &locations, &eligible, &relevance_query)?;
    measured.retain(|m| m.relevance > 0.0);
    measured.sort_by(|a, b| {
        b.relevance
            .total_cmp(&a.relevance)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| {
                locations[&a.location_id]
                    .start
                    .cmp(&locations[&b.location_id].start)
            })
            .then_with(|| a.location_id.cmp(&b.location_id))
    });
    for (i, m) in measured.into_iter().take(32).enumerate() {
        let position = candidates.iter().position(|c| c.id == m.location_id);
        let admitted = position.is_none();
        let index = if let Some(i) = position {
            i
        } else {
            let mut c = locations[&m.location_id].clone();
            c.use_hit(m.winning_hit);
            c.hits = vec![c.hit.clone()];
            candidates.push(c);
            candidates.len() - 1
        };
        let c = &mut candidates[index];
        let addition = m.relevance / (61 + i) as f64;
        d.channel.push(NavigationContribution {
            location_id: c.id,
            rank: i + 1,
            relevance: m.relevance,
            base_score: c.score,
            addition,
            admitted,
            reference_support: None,
        });
        c.score += addition;
    }
    d.role_corrections.extend(roles(conn, candidates)?);
    if !d.channel.is_empty() {
        d.status = "active".into();
        crate::candidate::sort(candidates);
    }
    Ok(d)
}
