use super::{DependencyCoverage, DependencyRelationship, UnresolvedDependency};
use crate::{Direction, IndexState, Notice, explore, search};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
};

#[derive(Debug, Clone)]
pub struct DependencyOptions {
    pub direction: Direction,
    /// Maximum file-to-file hops, 1–4.
    pub depth: usize,
    /// File-pair groups per page, 1–1000. Also bounds each auxiliary stream.
    pub limit: usize,
    /// Continue the same normalized path, direction, depth, and limit.
    pub cursor: Option<String>,
}
impl Default for DependencyOptions {
    fn default() -> Self {
        Self {
            direction: Direction::Outgoing,
            depth: 1,
            limit: 100,
            cursor: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct DependencyResponse {
    pub version: u32,
    pub anchor: String,
    pub direction: Direction,
    pub depth: usize,
    pub files: Vec<String>,
    pub relationships: Vec<DependencyRelationship>,
    /// File-pair groups reachable within depth, before pagination.
    pub relationships_total: usize,
    pub reachable_files: usize,
    pub next_cursor: Option<String>,
    pub unresolved: Vec<UnresolvedDependency>,
    /// Unresolved records in the visited files before the output limit.
    pub unresolved_total: usize,
    pub coverage: Vec<DependencyCoverage>,
    pub index: IndexState,
    /// More groups or auxiliary records are available through next_cursor.
    pub truncated: bool,
    pub depth_limited: bool,
    pub notices: Vec<Notice>,
}

/// Explain file dependencies using source-layout facts and current compiler relationships.
/// Static connections do not establish runtime reachability or complete impact coverage.
pub fn dependencies(
    db: &Path,
    path: &str,
    options: &DependencyOptions,
) -> Result<DependencyResponse> {
    ensure!(
        (1..=4).contains(&options.depth),
        "depth must be between 1 and 4"
    );
    ensure!(
        (1..=1000).contains(&options.limit),
        "limit must be between 1 and 1000"
    );
    let path = search::normalize_scope(Some(path))?;
    explore::read(db, |snapshot| {
        ensure!(
            snapshot.sources.contains_key(&path),
            "file is not indexed: {path}"
        );
        let super::graph::GraphData {
            relationships,
            mut unresolved,
            mut coverage,
            notices,
        } = super::graph::load(snapshot)?;
        let graph = traverse(&path, relationships, options);
        unresolved.retain(|u| graph.files.contains(&u.from));
        unresolved.sort_by(|a, b| {
            (
                &a.from,
                a.evidence.bytes,
                a.kind,
                &a.target,
                &a.reason,
                &a.evidence.provider,
            )
                .cmp(&(
                    &b.from,
                    b.evidence.bytes,
                    b.kind,
                    &b.target,
                    &b.reason,
                    &b.evidence.provider,
                ))
        });
        let relationships_total = graph.relationships.len();
        let unresolved_total = unresolved.len();
        let total = relationships_total
            .max(unresolved_total)
            .max(coverage.iter().map(|c| c.skipped_total).max().unwrap_or(0));
        // Include the result identity as well as provider freshness: re-resolution can
        // change relationships without changing source files or extraction identity.
        let request = explore::request((
            "dependencies-v2",
            &snapshot.generation,
            &snapshot.index,
            &path,
            options.direction,
            options.depth,
            options.limit,
            &graph.relationships,
            &unresolved,
            &coverage,
        ))?;
        let offset =
            super::pagination::offset(options.cursor.as_deref(), &request, options.limit, total)?;
        let next_cursor = super::pagination::next(&request, offset, options.limit, total);
        let relationships: Vec<_> = graph
            .relationships
            .into_iter()
            .skip(offset)
            .take(options.limit)
            .collect();
        let unresolved: Vec<_> = unresolved
            .into_iter()
            .skip(offset)
            .take(options.limit)
            .collect();
        for provider in &mut coverage {
            provider.skipped_files = std::mem::take(&mut provider.skipped_files)
                .into_iter()
                .skip(offset)
                .take(options.limit)
                .collect();
        }
        let mut files = BTreeSet::from([path.clone()]);
        for relationship in &relationships {
            files.insert(relationship.from.clone());
            files.insert(relationship.to.clone());
        }
        files.extend(unresolved.iter().map(|u| u.from.clone()));
        Ok(DependencyResponse {
            version: 2,
            anchor: path.clone(),
            direction: options.direction,
            depth: options.depth,
            files: files.into_iter().collect(),
            relationships,
            relationships_total,
            reachable_files: graph.files.len(),
            truncated: next_cursor.is_some(),
            next_cursor,
            unresolved,
            unresolved_total,
            coverage,
            index: snapshot.index.clone(),
            depth_limited: graph.depth_limited,
            notices,
        })
    })
}

struct Traversal {
    files: BTreeSet<String>,
    relationships: Vec<DependencyRelationship>,
    depth_limited: bool,
}

// Traverse groups before applying output pagination. A high occurrence count must
// not hide other consumers or prevent a transitive path from being explored.
fn traverse(
    anchor: &str,
    relationships: Vec<DependencyRelationship>,
    options: &DependencyOptions,
) -> Traversal {
    let mut adjacent: BTreeMap<&str, Vec<(usize, &str)>> = BTreeMap::new();
    for (i, relationship) in relationships.iter().enumerate() {
        if !matches!(options.direction, Direction::Incoming) {
            adjacent
                .entry(&relationship.from)
                .or_default()
                .push((i, &relationship.to));
        }
        if !matches!(options.direction, Direction::Outgoing) {
            adjacent
                .entry(&relationship.to)
                .or_default()
                .push((i, &relationship.from));
        }
    }
    let mut files = BTreeSet::from([anchor.to_owned()]);
    let mut queue = VecDeque::from([(anchor, 0)]);
    let mut selected = BTreeSet::new();
    let mut depth_limited = false;
    while let Some((file, depth)) = queue.pop_front() {
        for &(i, other) in adjacent.get(file).into_iter().flatten() {
            if depth == options.depth {
                if !files.contains(other) {
                    depth_limited = true;
                }
                continue;
            }
            selected.insert(i);
            if files.insert(other.into()) {
                queue.push_back((other, depth + 1));
            }
        }
    }
    Traversal {
        files,
        relationships: selected
            .into_iter()
            .map(|i| relationships[i].clone())
            .collect(),
        depth_limited,
    }
}

#[cfg(test)]
mod tests {
    use super::super::graph::group;
    use super::*;
    use crate::analysis::{DependencyEvidence, DependencyFact, DependencyKind};
    fn edge(a: &str, b: &str) -> DependencyFact {
        DependencyFact {
            from: a.into(),
            to: b.into(),
            kind: DependencyKind::Imports,
            evidence: DependencyEvidence {
                path: a.into(),
                line: 1,
                bytes: [0, 1],
                provider: "fixture".into(),
            },
        }
    }
    #[test]
    fn traversal_handles_reverse_cycles_depth_and_limits() {
        let facts = vec![
            edge("a", "b"),
            edge("b", "c"),
            edge("c", "a"),
            edge("b", "d"),
        ];
        let mut options = DependencyOptions::default();
        let one = traverse("a", group(facts.clone()), &options);
        assert_eq!(one.files, BTreeSet::from(["a".into(), "b".into()]));
        assert!(one.depth_limited);
        options.direction = Direction::Incoming;
        assert_eq!(
            traverse("a", group(facts.clone()), &options).relationships,
            group(vec![edge("c", "a")])
        );
        options.direction = Direction::Outgoing;
        options.depth = 4;
        let all = traverse("a", group(facts.clone()), &options);
        assert_eq!(all.relationships.len(), 4);
        assert!(!all.depth_limited);
        options.limit = 1;
        let limited = traverse("a", group(facts), &options);
        assert_eq!(
            limited.relationships.len(),
            4,
            "page size does not limit graph exploration"
        );
    }
    #[test]
    fn grouping_preserves_counts_kinds_and_bounded_evidence() {
        let mut facts = vec![edge("caller", "other")];
        for line in (1..=2000).rev() {
            let mut fact = edge("caller", "popular");
            fact.kind = DependencyKind::Calls;
            fact.evidence.line = line;
            fact.evidence.bytes = [line, line + 1];
            facts.push(fact.clone());
            facts.push(fact); // The same occurrence must not be counted twice.
        }
        let mut reference = edge("caller", "popular");
        reference.kind = DependencyKind::References;
        facts.push(reference);
        let groups = group(facts);
        assert_eq!(groups.len(), 2);
        let popular = groups.iter().find(|g| g.to == "popular").unwrap();
        assert_eq!(popular.kinds.len(), 2);
        let calls = popular
            .kinds
            .iter()
            .find(|k| k.kind == DependencyKind::Calls)
            .unwrap();
        assert_eq!(calls.occurrences, 2000);
        assert_eq!(
            calls.evidence.iter().map(|e| e.line).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(calls.evidence_truncated);
        let refs = popular
            .kinds
            .iter()
            .find(|k| k.kind == DependencyKind::References)
            .unwrap();
        assert_eq!(refs.occurrences, 1);
        assert!(!refs.evidence_truncated);
        let incoming = traverse(
            "popular",
            groups,
            &DependencyOptions {
                direction: Direction::Incoming,
                ..Default::default()
            },
        );
        assert_eq!(incoming.relationships.len(), 1);
        assert_eq!(incoming.relationships[0].from, "caller");
    }

    #[test]
    fn semantic_projection_requires_current_provider_inputs() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("lib.rs"),
            "mod a; fn caller() { a::run(); } use std::fs; use std::io;",
        )
        .unwrap();
        fs::write(root.join("a.rs"), "pub fn run() {}").unwrap();
        let db = temp.path().join("index.sqlite");
        crate::index(&root, &db).unwrap();
        let conn = crate::store::open(&db, true).unwrap();
        let snapshot = crate::store::metadata(&conn, "snapshot").unwrap().unwrap();
        let inputs = crate::languages::rust::inputs::semantic_inputs(&root).unwrap();
        for (key, value) in [
            ("semantic_snapshot", snapshot.as_str()),
            ("semantic_inputs", inputs.as_str()),
            ("semantic_policy", "sonar-scip-utf8-source-calls-v1"),
            ("semantic_profile", "fixture-compiler"),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO metadata VALUES (?1,?2)",
                rusqlite::params![key, value],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO semantic_edges SELECT a.id,b.id,'calls','lib.rs',1,27,30
            FROM locations a,locations b WHERE a.symbol='caller' AND b.symbol='run'",
            [],
        )
        .unwrap();
        drop(conn);
        let response = dependencies(&db, "lib.rs", &Default::default()).unwrap();
        let call = response
            .relationships
            .iter()
            .find(|r| r.kinds.iter().any(|k| k.kind == DependencyKind::Calls))
            .unwrap();
        assert_eq!(call.to, "a.rs");
        assert_eq!(
            call.kinds
                .iter()
                .find(|k| k.kind == DependencyKind::Calls)
                .unwrap()
                .evidence[0]
                .provider,
            "rust:fixture-compiler"
        );
        let first = dependencies(
            &db,
            "lib.rs",
            &DependencyOptions {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let continuation = DependencyOptions {
            limit: 1,
            cursor: first.next_cursor,
            ..Default::default()
        };
        assert!(continuation.cursor.is_some());
        // Re-resolution with a new profile invalidates an existing continuation.
        let conn = crate::store::open(&db, true).unwrap();
        conn.execute(
            "UPDATE metadata SET value='new-profile' WHERE key='semantic_profile'",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(dependencies(&db, "lib.rs", &continuation).is_err());
        let first = dependencies(
            &db,
            "lib.rs",
            &DependencyOptions {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let continuation = DependencyOptions {
            limit: 1,
            cursor: first.next_cursor,
            ..Default::default()
        };
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"changed\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        assert!(
            dependencies(&db, "lib.rs", &continuation).is_err(),
            "stale compiler inputs invalidate continuations"
        );
        let response = dependencies(&db, "lib.rs", &Default::default()).unwrap();
        assert!(
            !response
                .relationships
                .iter()
                .any(|r| r.kinds.iter().any(|k| k.kind == DependencyKind::Calls))
        );
        assert!(response.relationships.iter().any(|r| {
            r.kinds
                .iter()
                .any(|k| k.kind == DependencyKind::DeclaresModule)
        }));
    }
}
