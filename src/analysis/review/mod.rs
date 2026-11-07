//! Static, bounded review units. No language-specific clustering or generated findings.
mod engine;
mod model;
pub use model::*;

use super::{
    DependencyCoverage, DependencyKind, DependencyRelationship,
    graph::{self, GraphData},
};
use crate::{Notice, explore, search};
use anyhow::{Result, ensure};
use engine::{Engine, Selection};
use rusqlite::params;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const POLICY: &str = "review-clusters-v2";
const AUXILIARY_LIMIT: usize = 20;

struct Inventory {
    code: BTreeSet<String>,
    eligible: BTreeSet<String>,
    unassigned: Vec<ReviewUnassigned>,
    non_code: usize,
    test_files: BTreeSet<String>,
    mixed_test_files: BTreeSet<String>,
    test_links: BTreeMap<(String, String), ReviewConnection>,
    shared: BTreeSet<String>,
}

fn inventory(s: &explore::Snapshot<'_>, scope: &str) -> Result<Inventory> {
    let mut inventory = Inventory {
        code: BTreeSet::new(),
        eligible: BTreeSet::new(),
        unassigned: vec![],
        non_code: 0,
        test_files: BTreeSet::new(),
        mixed_test_files: BTreeSet::new(),
        test_links: BTreeMap::new(),
        shared: BTreeSet::new(),
    };
    let mut stmt = s.conn.prepare("SELECT f.path,f.parse_errors,
        coalesce(min(CASE WHEN l.kind!='file' THEN l.is_test END),0),
        coalesce(max(l.is_test),0) FROM files f LEFT JOIN locations l ON l.file_id=f.id GROUP BY f.id ORDER BY f.path")?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, bool>(1)?,
            r.get::<_, bool>(2)?,
            r.get::<_, bool>(3)?,
        ))
    })? {
        let (path, errors, all_test, any_test) = row?;
        if all_test {
            inventory.test_files.insert(path.clone());
        }
        if any_test {
            inventory.mixed_test_files.insert(path.clone());
        }
        if !search::in_scope(&path, scope) {
            continue;
        }
        if crate::source_language(&path).is_none() {
            inventory.non_code += 1;
            continue;
        }
        inventory.code.insert(path.clone());
        if errors {
            inventory.unassigned.push(ReviewUnassigned {
                path,
                reason: "parse_errors".into(),
            });
        } else {
            inventory.eligible.insert(path);
        }
    }
    if s.navigation_current {
        let mut stmt = s
            .conn
            .prepare("SELECT path FROM navigation_test_files ORDER BY path")?;
        for path in stmt.query_map([], |r| r.get::<_, String>(0))? {
            inventory.test_files.insert(path?);
        }
    }
    let mut stmt = s.conn.prepare(
        "SELECT a.path,b.path,e.kind,e.path,e.line,e.start_byte,e.end_byte FROM semantic_edges e
        JOIN locations x ON x.id=e.from_id JOIN files a ON a.id=x.file_id
        JOIN locations y ON y.id=e.to_id JOIN files b ON b.id=y.file_id
        WHERE x.is_test=1 OR y.is_test=1 ORDER BY a.path,b.path,e.path,e.start_byte,e.end_byte,e.kind",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, u32>(4)?,
            r.get::<_, u32>(5)?,
            r.get::<_, u32>(6)?,
        ))
    })? {
        let (from, to, kind, path, line, start, end) = row?;
        let kind = match kind.as_str() {
            "calls" => DependencyKind::Calls,
            "references" => DependencyKind::References,
            "implements" => DependencyKind::Implements,
            _ => continue,
        };
        let Some(language) = crate::source_language(&from) else {
            continue;
        };
        let profile = s
            .index
            .languages
            .iter()
            .find(|l| l.language == language)
            .and_then(|l| l.semantic_profile.as_deref())
            .unwrap_or("compiler");
        inventory
            .test_links
            .entry((from.clone(), to.clone()))
            .or_insert(ReviewConnection {
                from,
                to,
                kind,
                occurrences: 1,
                evidence: super::DependencyEvidence {
                    path,
                    line: line as usize,
                    bytes: [start as usize, end as usize],
                    provider: format!("{language}:{profile}"),
                },
            });
    }
    Ok(inventory)
}

fn validate_budget(max_files: usize) -> Result<()> {
    ensure!(
        (1..=50).contains(&max_files),
        "max_files must be between 1 and 50"
    );
    Ok(())
}

fn seed(s: &explore::Snapshot<'_>, value: &str, scope: &str) -> Result<ReviewSeed> {
    let location = if value.starts_with("sn1:") {
        Some(s.location(s.resolve(value)?)?)
    } else if search::normalize_scope(Some(value))
        .ok()
        .is_some_and(|p| s.sources.contains_key(&p))
    {
        None
    } else {
        let mut stmt = s.conn.prepare("SELECT l.id,f.path FROM locations l JOIN files f ON f.id=l.file_id WHERE l.symbol=?1 ORDER BY f.path,l.start_byte")?;
        let matches: Vec<_> = stmt
            .query_map([value], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, path)| search::in_scope(path, scope))
            .collect();
        ensure!(
            matches.len() == 1,
            "review seed must be an indexed file, full handle, or unambiguous exact symbol; use outline/locate for a handle"
        );
        Some(s.location(matches[0].0)?)
    };
    let seed = match location {
        Some(location) => ReviewSeed {
            path: location.path,
            symbol: location.symbol,
            range: Some(location.range),
        },
        None => ReviewSeed {
            path: search::normalize_scope(Some(value))?,
            symbol: None,
            range: None,
        },
    };
    ensure!(
        search::in_scope(&seed.path, scope),
        "review seed is outside scope"
    );
    ensure!(
        crate::source_language(&seed.path).is_some(),
        "review seed must be a supported code file"
    );
    Ok(seed)
}

fn connection(relationship: &DependencyRelationship) -> ReviewConnection {
    let kind = relationship
        .kinds
        .iter()
        .max_by_key(|k| (engine::kind_weight(k.kind), std::cmp::Reverse(k.kind)))
        .expect("grouped relationship has a kind");
    ReviewConnection {
        from: relationship.from.clone(),
        to: relationship.to.clone(),
        kind: kind.kind,
        occurrences: kind.occurrences,
        evidence: kind.evidence[0].clone(),
    }
}

fn area_id(s: &explore::Snapshot<'_>, selection: &Selection) -> Result<String> {
    let files: BTreeSet<_> = selection.order.iter().map(|(f, _)| f).collect();
    Ok(format!(
        "ra1:{}",
        explore::request((POLICY, &s.generation, files))?
    ))
}

fn source_ranges(
    s: &explore::Snapshot<'_>,
    file: &str,
    initial: Option<[usize; 2]>,
    links: &[ReviewConnection],
) -> Result<Vec<[usize; 2]>> {
    let lines = s.sources[file].source.lines().count().max(1);
    let mut starts = Vec::new();
    if let Some([start, _]) = initial {
        starts.push(start);
    }
    for link in links {
        if link.evidence.path == file {
            starts.push(link.evidence.line);
        }
        if link.to == file {
            let kind = link.kind.name();
            let mut stmt = s.conn.prepare(
                "SELECT DISTINCT l.definition FROM semantic_edges e
                JOIN locations l ON l.id=e.to_id JOIN files f ON f.id=l.file_id
                WHERE e.from_id IN (SELECT x.id FROM locations x JOIN files a ON a.id=x.file_id WHERE a.path=?1)
                AND e.path=?1 AND e.start_byte=?2 AND e.end_byte=?3 AND e.kind=?4 AND f.path=?5
                AND l.definition IS NOT NULL ORDER BY l.definition LIMIT 3",
            )?;
            let definitions = stmt
                .query_map(
                    params![
                        link.evidence.path,
                        link.evidence.bytes[0] as i64,
                        link.evidence.bytes[1] as i64,
                        kind,
                        file
                    ],
                    |r| r.get::<_, u32>(0),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            // Module-level links identify a file, not a bound declaration. Retain
            // its header contracts/imports instead of starting after them at the
            // first constant or function definition.
            if definitions.is_empty()
                && matches!(
                    link.kind,
                    DependencyKind::Imports
                        | DependencyKind::ReExports
                        | DependencyKind::DeclaresModule
                )
            {
                starts.push(1);
            }
            starts.extend(definitions.into_iter().map(|line| line as usize));
        }
    }
    if starts.is_empty() {
        let line: Option<u32> = s.conn.query_row("SELECT min(l.definition) FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path=?1 AND l.kind!='file'",[file],|r| r.get(0))?;
        starts.push(line.unwrap_or(1) as usize);
    }
    let mut seen = BTreeSet::new();
    Ok(starts
        .into_iter()
        .filter(|start| seen.insert(*start))
        .take(3)
        .map(|start| {
            let start = start.clamp(1, lines);
            [start, (start + 11).min(lines)]
        })
        .collect())
}

fn render(
    s: &explore::Snapshot<'_>,
    data: &GraphData,
    inventory: &Inventory,
    selection: &Selection,
    seed: ReviewSeed,
    max_files: usize,
    assignments: &BTreeMap<String, String>,
) -> Result<ReviewCluster> {
    let selected: BTreeSet<_> = selection.order.iter().map(|(p, _)| p.clone()).collect();
    let links: Vec<_> = selection
        .order
        .iter()
        .filter_map(|(_, parent)| {
            parent
                .as_ref()
                .map(|(_, i)| connection(&data.relationships[*i]))
        })
        .collect();
    let mut reading_path = Vec::new();
    for (file, parent) in &selection.order {
        reading_path.push(ReviewReadingStep {
            file: file.clone(),
            source_role: if inventory.test_files.contains(file) {
                "test"
            } else if inventory.mixed_test_files.contains(file) {
                "implementation_with_tests"
            } else {
                "implementation"
            }
            .into(),
            parent_file: parent.as_ref().map(|(p, _)| p.clone()),
            via: parent
                .as_ref()
                .map(|(_, i)| connection(&data.relationships[*i])),
            ranges: source_ranges(
                s,
                file,
                if file == &seed.path { seed.range } else { None },
                &links,
            )?,
        });
    }
    let mut boundary = Vec::new();
    let mut omitted = BTreeSet::new();
    let mut tests: BTreeMap<String, ReviewTest> = BTreeMap::new();
    for relationship in &data.relationships {
        let from = selected.contains(&relationship.from);
        let to = selected.contains(&relationship.to);
        if !from && !to {
            continue;
        }
        if from != to {
            let outside = if from {
                &relationship.to
            } else {
                &relationship.from
            };
            omitted.insert(outside.clone());
            let classification = if !inventory.code.contains(outside) {
                "outside_scope"
            } else if inventory.shared.contains(outside) {
                "shared_infrastructure"
            } else if assignments.contains_key(outside) {
                "other_area"
            } else if !inventory.eligible.contains(outside) {
                "unassigned"
            } else {
                "outside_budget"
            };
            boundary.push(ReviewBoundary {
                relationship: relationship.clone(),
                outside_file: outside.clone(),
                classification: classification.into(),
                other_area: assignments.get(outside).cloned(),
            });
        }
        for path in [&relationship.from, &relationship.to] {
            let full_test = inventory.test_files.contains(path);
            let test_relation = inventory
                .test_links
                .contains_key(&(relationship.from.clone(), relationship.to.clone()))
                && inventory.mixed_test_files.contains(path);
            if full_test || test_relation {
                tests.entry(path.clone()).or_insert_with(|| ReviewTest {
                    path: path.clone(),
                    selected: selected.contains(path),
                    basis: if full_test {
                        "test_file_dependency"
                    } else {
                        "compiler_relationship_with_test_code"
                    }
                    .into(),
                    connection: if test_relation {
                        inventory.test_links[&(relationship.from.clone(), relationship.to.clone())]
                            .clone()
                    } else {
                        connection(relationship)
                    },
                });
            }
        }
    }
    let boundary_total = boundary.len();
    boundary.truncate(AUXILIARY_LIMIT);
    let related_tests_total = tests.len();
    let title = Path::new(&selection.seed)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_string_lossy()
        .into_owned();
    Ok(ReviewCluster {
        id: area_id(s, selection)?,
        title,
        seed,
        max_files,
        files: selection.order.iter().map(|(f, _)| f.clone()).collect(),
        reading_path,
        related_tests: tests.into_values().take(AUXILIARY_LIMIT).collect(),
        related_tests_total,
        boundary,
        boundary_total,
        omitted_files_total: omitted.len(),
        boundary_truncated: boundary_total > AUXILIARY_LIMIT,
        unresolved_total: data
            .unresolved
            .iter()
            .filter(|u| selected.contains(&u.from))
            .count(),
    })
}

fn analysis_coverage(data: &GraphData) -> Vec<DependencyCoverage> {
    data.coverage
        .iter()
        .map(|c| DependencyCoverage {
            language: c.language.clone(),
            provider: c.provider.clone(),
            analyzed_files: c.analyzed_files,
            skipped_total: c.skipped_total,
            skipped_files: c
                .skipped_files
                .iter()
                .take(AUXILIARY_LIMIT)
                .cloned()
                .collect(),
            limitations: c.limitations.clone(),
        })
        .collect()
}

fn notices(mut notices: Vec<Notice>, shared_threshold: usize) -> Vec<Notice> {
    notices.push(Notice::new("review_heuristics", format!("Clusters and reading order are deterministic static heuristics, not execution traces, confirmed feature boundaries, defect priorities, or measured test coverage. Shared files require at least {shared_threshold} non-module consumers across three directories, and at least twice as many consumers as dependencies. Boundary/test examples are capped at twenty; use dependency queries for more.")));
    notices
}

pub(super) fn context_cluster(
    s: &explore::Snapshot<'_>,
    data: &GraphData,
    seed: ReviewSeed,
    options: &ReviewContextOptions,
    preferred: &BTreeSet<String>,
) -> Result<(ReviewCluster, usize)> {
    validate_budget(options.max_files)?;
    let scope = search::normalize_scope(options.scope.as_deref())?;
    let mut inventory = inventory(s, &scope)?;
    ensure!(
        inventory.eligible.contains(&seed.path),
        "review seed has parse errors or is outside scope"
    );
    let engine = Engine::new(&data.relationships);
    inventory.shared = engine
        .shared
        .intersection(&inventory.eligible)
        .cloned()
        .collect();
    let selection = engine.select_prioritized(
        &seed.path,
        &inventory.eligible,
        options.max_files,
        preferred,
    );
    let cluster = render(
        s,
        data,
        &inventory,
        &selection,
        seed,
        options.max_files,
        &BTreeMap::new(),
    )?;
    Ok((cluster, engine.shared_threshold))
}

pub fn review_context(
    db: &Path,
    value: &str,
    options: &ReviewContextOptions,
) -> Result<ReviewContextResponse> {
    validate_budget(options.max_files)?;
    let scope = search::normalize_scope(options.scope.as_deref())?;
    explore::read(db, |s| {
        let seed = seed(s, value, &scope)?;
        let data = graph::load(s)?;
        let (cluster, shared_threshold) =
            context_cluster(s, &data, seed, options, &BTreeSet::new())?;
        Ok(ReviewContextResponse {
            version: 1,
            policy: POLICY.into(),
            cluster,
            index: s.index.clone(),
            analysis_coverage: analysis_coverage(&data),
            notices: notices(data.notices, shared_threshold),
        })
    })
}

pub fn review_areas(db: &Path, options: &ReviewAreasOptions) -> Result<ReviewAreasResponse> {
    validate_budget(options.max_files)?;
    ensure!(
        (1..=20).contains(&options.limit),
        "review area limit must be between 1 and 20"
    );
    let scope = search::normalize_scope(options.scope.as_deref())?;
    explore::read(db, |s| {
        let data = graph::load(s)?;
        let mut inventory = inventory(s, &scope)?;
        let engine = Engine::new(&data.relationships);
        inventory.shared = engine
            .shared
            .intersection(&inventory.eligible)
            .cloned()
            .collect();
        let selections = engine.partition(&inventory.eligible, options.max_files);
        let mut assignments = BTreeMap::new();
        for selection in &selections {
            let id = area_id(s, selection)?;
            for (file, _) in &selection.order {
                assignments.insert(file.clone(), id.clone());
            }
        }
        let shared: Vec<_> = inventory
            .eligible
            .intersection(&engine.shared)
            .cloned()
            .collect();
        let total = selections
            .len()
            .max(shared.len())
            .max(inventory.unassigned.len());
        let request = explore::request((
            POLICY,
            "areas",
            &s.generation,
            &s.index,
            s.navigation_current,
            &inventory.test_files,
            &scope,
            options.max_files,
            options.limit,
            &data.relationships,
            &data.unresolved,
            &data.coverage,
            &shared,
            &inventory.unassigned,
        ))?;
        let offset =
            super::pagination::offset(options.cursor.as_deref(), &request, options.limit, total)?;
        let next_cursor = super::pagination::next(&request, offset, options.limit, total);
        let areas = selections
            .iter()
            .skip(offset)
            .take(options.limit)
            .map(|selection| {
                let seed = ReviewSeed {
                    path: selection.seed.clone(),
                    symbol: None,
                    range: None,
                };
                render(
                    s,
                    &data,
                    &inventory,
                    selection,
                    seed,
                    options.max_files,
                    &assignments,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            assignments.len() + shared.len() + inventory.unassigned.len() == inventory.code.len(),
            "review coverage partition is incomplete"
        );
        Ok(ReviewAreasResponse {
            version: 1,
            policy: POLICY.into(),
            areas,
            areas_total: selections.len(),
            coverage: ReviewAreaCoverage {
                indexed_code_files: inventory.code.len(),
                assigned_files: assignments.len(),
                shared_files_total: shared.len(),
                shared_files: shared
                    .into_iter()
                    .skip(offset)
                    .take(options.limit)
                    .collect(),
                unassigned_total: inventory.unassigned.len(),
                unassigned: inventory
                    .unassigned
                    .into_iter()
                    .skip(offset)
                    .take(options.limit)
                    .collect(),
                excluded_non_code_files: inventory.non_code,
            },
            index: s.index.clone(),
            analysis_coverage: analysis_coverage(&data),
            truncated: next_cursor.is_some(),
            next_cursor,
            notices: notices(data.notices, engine.shared_threshold),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn mark_current(conn: &rusqlite::Connection, root: &Path) {
        let snapshot = crate::store::metadata(conn, "snapshot").unwrap().unwrap();
        let inputs = crate::languages::rust::inputs::semantic_inputs(root).unwrap();
        for (key, value) in [
            ("semantic_snapshot", snapshot.as_str()),
            ("semantic_inputs", inputs.as_str()),
            ("semantic_policy", "sonar-scip-utf8-source-calls-v1"),
            ("semantic_profile", "review-fixture"),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO metadata VALUES (?1,?2)",
                params![key, value],
            )
            .unwrap();
        }
    }

    #[test]
    fn mixed_test_files_point_to_actual_test_occurrences_and_resolved_targets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("lib.rs"), "mod service; mod checks;\n").unwrap();
        fs::write(root.join("service.rs"), "pub fn target() {}\n").unwrap();
        let source = "fn production() { crate::service::target(); }\n#[test]\nfn verification() { crate::service::target(); }\n";
        fs::write(root.join("checks.rs"), source).unwrap();
        let db = temp.path().join("index.sqlite");
        crate::index(&root, &db).unwrap();
        let conn = crate::store::open(&db, true).unwrap();
        mark_current(&conn, &root);
        for (owner, start, line) in [
            ("production", source.find("target").unwrap(), 1),
            ("verification", source.rfind("target").unwrap(), 3),
        ] {
            conn.execute("INSERT INTO semantic_edges SELECT a.id,b.id,'calls','checks.rs',?1,?2,?3 FROM locations a,locations b WHERE a.symbol=?4 AND b.symbol='target'",
                params![line,start as i64,(start+6) as i64,owner]).unwrap();
        }
        drop(conn);
        let context = review_context(
            &db,
            "service.rs",
            &ReviewContextOptions {
                max_files: 2,
                ..Default::default()
            },
        )
        .unwrap();
        let test = context
            .cluster
            .related_tests
            .iter()
            .find(|t| t.path == "checks.rs")
            .unwrap();
        assert_eq!(test.basis, "compiler_relationship_with_test_code");
        assert_eq!(
            test.connection.evidence.line, 3,
            "production occurrence must not substitute for test evidence"
        );
        let step = context
            .cluster
            .reading_path
            .iter()
            .find(|s| s.file == "checks.rs")
            .unwrap();
        assert_eq!(step.source_role, "implementation_with_tests");
        assert!(
            context
                .cluster
                .reading_path
                .iter()
                .find(|s| s.file == "service.rs")
                .unwrap()
                .ranges
                .iter()
                .any(|r| r[0] == 1)
        );
    }

    #[test]
    fn shared_coverage_boundary_caps_and_analysis_cursors_are_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("ports.rs"), "pub fn target() {}\n").unwrap();
        for i in 0..25 {
            let path = root.join(format!("area{}/consumer{i}.rs", i % 3));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, format!("fn caller{i}() {{ target(); }}\n")).unwrap();
        }
        let db = temp.path().join("index.sqlite");
        crate::index(&root, &db).unwrap();
        let conn = crate::store::open(&db, true).unwrap();
        mark_current(&conn, &root);
        for i in 0..25 {
            let path = format!("area{}/consumer{i}.rs", i % 3);
            let source = fs::read_to_string(root.join(&path)).unwrap();
            let start = source.find("target").unwrap();
            conn.execute("INSERT INTO semantic_edges SELECT a.id,b.id,'calls',?1,1,?2,?3 FROM locations a,locations b WHERE a.symbol=?4 AND b.symbol='target'",
                params![path,start as i64,(start+6) as i64,format!("caller{i}")]).unwrap();
        }
        drop(conn);
        let first = review_areas(
            &db,
            &ReviewAreasOptions {
                limit: 20,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(first.coverage.assigned_files, 25);
        assert_eq!(first.coverage.shared_files, vec!["ports.rs"]);
        assert_eq!(first.coverage.indexed_code_files, 26);
        assert_eq!(first.areas_total, 25);
        assert!(
            first
                .areas
                .iter()
                .all(|a| a.files.len() == 1
                    && a.boundary[0].classification == "shared_infrastructure")
        );
        let context = review_context(
            &db,
            "ports.rs",
            &ReviewContextOptions {
                max_files: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(context.cluster.boundary_total, 25);
        assert_eq!(context.cluster.boundary.len(), 20);
        assert!(context.cluster.boundary_truncated);
        let next = ReviewAreasOptions {
            limit: 20,
            cursor: first.next_cursor,
            ..Default::default()
        };
        assert_eq!(review_areas(&db, &next).unwrap().areas.len(), 5);
        let conn = crate::store::open(&db, true).unwrap();
        conn.execute(
            "UPDATE metadata SET value='updated-review-profile' WHERE key='semantic_profile'",
            [],
        )
        .unwrap();
        drop(conn);
        assert!(review_areas(&db, &next).is_err());
    }
}
