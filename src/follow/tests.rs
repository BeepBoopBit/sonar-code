use super::*;
use protobuf::Message;
use scip::types::{Document, Index, Metadata, Occurrence, PositionEncoding};
use std::{fs, path::PathBuf};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Fixture {
    /// Feed real source occurrences through the production SCIP importer. Each declaration
    /// is a single-line function; repeated calls and function-value references stay distinct.
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='follow_fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        let names: Vec<_> = files
            .iter()
            .flat_map(|(_, source)| source.lines())
            .filter_map(|line| {
                line.strip_prefix("fn ")
                    .or_else(|| line.strip_prefix("struct "))
            })
            .map(|line| line.split(['(', ';', ' ', '{']).next().unwrap())
            .collect();
        let mut documents = vec![];
        for (path, source) in files {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), source).unwrap();
            let mut occurrences = vec![];
            for (line, text) in source
                .lines()
                .enumerate()
                .filter(|(_, text)| text.starts_with("fn ") || text.starts_with("struct "))
            {
                for name in &names {
                    for (column, _) in text.match_indices(name) {
                        let word = |c: char| c.is_alphanumeric() || c == '_';
                        if text[..column].chars().next_back().is_some_and(word)
                            || text[column + name.len()..].chars().next().is_some_and(word)
                        {
                            continue;
                        }
                        occurrences.push(Occurrence {
                            range: vec![line as i32, column as i32, (column + name.len()) as i32],
                            symbol: format!("fixture {name}"),
                            symbol_roles: i32::from(
                                column == if text.starts_with("fn ") { 3 } else { 7 },
                            ),
                            ..Default::default()
                        });
                    }
                }
            }
            documents.push(Document {
                relative_path: (*path).into(),
                language: "rust".into(),
                position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                occurrences,
                ..Default::default()
            });
        }
        let db = temp.path().join("index.sqlite");
        let indexed = crate::index(&root, &db).unwrap();
        let bytes = Index {
            metadata: Some(Metadata {
                project_root: format!("file://{}", root.display()),
                ..Default::default()
            })
            .into(),
            documents,
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();
        let scip = temp.path().join("index.scip");
        fs::write(&scip, &bytes).unwrap();
        let manifest = crate::SemanticManifest {
            root: root.to_str().unwrap().into(),
            snapshot: indexed.snapshot,
            inputs: crate::languages::rust::inputs::semantic_inputs(&root).unwrap(),
            analyzer: "fixture".into(),
            toolchain: "fixture".into(),
            config: serde_json::json!({}),
            scip_hash: blake3::hash(&bytes).to_hex().to_string(),
        };
        let manifest_path = temp.path().join("manifest.json");
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        crate::import_scip(&db, &scip, &manifest_path).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }

    fn anchor(&self, path: &str, line: usize) -> String {
        crate::locate(&self.db, path, line, None)
            .unwrap()
            .locations
            .remove(0)
            .handle
    }

    fn follow(&self, anchor: &str, query: &str, options: &FollowOptions) -> FollowResponse {
        crate::SearchEngine::open(&self.db, RetrievalMode::Lexical, None)
            .unwrap()
            .follow(anchor, query, options)
            .unwrap()
    }
}

fn target<'a>(response: &'a FollowResponse, matched: &FollowMatch) -> &'a NavigationLocation {
    response
        .locations
        .iter()
        .find(|l| l.handle == matched.target)
        .unwrap()
}

fn assert_routes(response: &FollowResponse) {
    let locations: BTreeSet<_> = response
        .locations
        .iter()
        .map(|l| &l.handle)
        .chain([&response.anchor.handle])
        .collect();
    let mut used = BTreeSet::new();
    for matched in &response.matches {
        let mut current = &response.anchor.handle;
        let mut visited = BTreeSet::from([current]);
        assert!(!matched.steps.is_empty());
        assert!(matched.steps.len() <= response.traversal.hops);
        for &step in &matched.steps {
            used.insert(step);
            let link = &response.links[step];
            assert!(locations.contains(&link.from) && locations.contains(&link.to));
            current = if current == &link.from {
                &link.to
            } else {
                assert_eq!(current, &link.to);
                &link.from
            };
            assert!(visited.insert(current), "cycle in output trail");
        }
        assert_eq!(current, &matched.target);
    }
    assert_eq!(used.len(), response.links.len());
}

#[test]
fn follows_two_hop_routes_preserving_direction_repetition_and_intermediates() {
    let f = Fixture::new(&[(
        "lib.rs",
        "fn entry() { bridge(); bridge(); entry(); }\nfn bridge() { policy(); }\nfn policy() { bridge(); }\nfn disconnected_policy() {}\n",
    )]);
    let anchor = f.anchor("lib.rs", 1);
    let options = FollowOptions {
        direction: Direction::Outgoing,
        relation: FollowRelation::Calls,
        ..Default::default()
    };
    let response = f.follow(&anchor, "policy", &options);
    assert_eq!(
        target(&response, &response.matches[0]).symbol.as_deref(),
        Some("policy")
    );
    assert_eq!(response.matches[0].steps.len(), 2);
    assert_eq!(response.links[0].occurrences, 2);
    assert!(response.links.iter().all(|e| e.kind == "calls"));
    assert!(
        !response
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("disconnected_policy"))
    );
    assert_routes(&response);
    let once = f.follow(
        &anchor,
        "policy",
        &FollowOptions {
            hops: 1,
            ..options.clone()
        },
    );
    assert!(
        !once
            .matches
            .iter()
            .any(|m| target(&once, m).symbol.as_deref() == Some("policy"))
    );
    let reverse = f.follow(
        &f.anchor("lib.rs", 3),
        "entry",
        &FollowOptions {
            direction: Direction::Incoming,
            ..options
        },
    );
    assert_eq!(
        target(&reverse, &reverse.matches[0]).symbol.as_deref(),
        Some("entry")
    );
    assert_eq!(reverse.matches[0].steps.len(), 2);
    let mut cursor = &reverse.anchor.handle;
    for step in &reverse.matches[0].steps {
        let edge = &reverse.links[*step];
        assert_eq!(cursor, &edge.to);
        cursor = &edge.from;
    }
    assert_routes(&reverse);
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        serde_json::to_value(f.follow(
            &anchor,
            "policy",
            &FollowOptions {
                direction: Direction::Outgoing,
                relation: FollowRelation::Calls,
                ..Default::default()
            }
        ))
        .unwrap()
    );
}

#[test]
fn scope_applies_to_the_entire_route_and_references_are_not_calls() {
    let f = Fixture::new(&[
        (
            "inside/a.rs",
            "fn entry() { bridge(); let callback = policy; }\nfn policy() {}\n",
        ),
        ("outside.rs", "fn bridge() { policy(); }\n"),
    ]);
    let anchor = f.anchor("inside/a.rs", 1);
    let base = FollowOptions {
        direction: Direction::Outgoing,
        relation: FollowRelation::Calls,
        ..Default::default()
    };
    let all = f.follow(&anchor, "policy", &base);
    assert_eq!(all.matches[0].steps.len(), 2);
    let scoped = f.follow(
        &anchor,
        "policy",
        &FollowOptions {
            scope: Some("inside".into()),
            ..base.clone()
        },
    );
    assert!(scoped.matches.is_empty());
    let references = f.follow(
        &anchor,
        "policy",
        &FollowOptions {
            scope: Some("inside".into()),
            relation: FollowRelation::References,
            ..base.clone()
        },
    );
    assert_eq!(references.matches[0].steps.len(), 1);
    assert!(references.links.iter().all(|e| e.kind == "references"));
    assert!(
        references
            .locations
            .iter()
            .all(|l| l.path.starts_with("inside/"))
    );
    assert_routes(&references);
    assert!(
        crate::SearchEngine::open(&f.db, RetrievalMode::Lexical, None)
            .unwrap()
            .follow(
                &anchor,
                "policy",
                &FollowOptions {
                    scope: Some("outside.rs".into()),
                    ..base
                }
            )
            .is_err()
    );
}

#[test]
fn stale_semantics_cannot_produce_trails_and_source_edits_invalidate_handles() {
    let f = Fixture::new(&[("lib.rs", "fn entry() { policy(); }\nfn policy() {}\n")]);
    let anchor = f.anchor("lib.rs", 1);
    fs::write(
        f.root.join("Cargo.toml"),
        "[package]\nname='changed'\nversion='0.1.0'\n",
    )
    .unwrap();
    let result = f.follow(&anchor, "policy", &Default::default());
    assert!(result.matches.is_empty() && result.links.is_empty());
    assert_eq!(result.index.semantic_freshness, "stale");
    assert_eq!(result.traversal.expanded, 0);
    assert!(result.notices.iter().any(|n| n.code == "graph_unavailable"));
    fs::write(f.root.join("lib.rs"), "fn replacement() {}\n").unwrap();
    let engine = crate::SearchEngine::open(&f.db, RetrievalMode::Lexical, None).unwrap();
    assert!(
        engine
            .follow(&anchor, "policy", &Default::default())
            .is_err()
    );
    crate::index(&f.root, &f.db).unwrap();
    assert!(
        engine
            .follow(&anchor, "policy", &Default::default())
            .is_err()
    );
    let new = f.anchor("lib.rs", 1);
    assert!(
        f.follow(&new, "policy", &Default::default())
            .notices
            .iter()
            .any(|n| n.code == "graph_unavailable")
    );
}

#[test]
fn bounded_traversal_reports_edge_node_and_result_limits() {
    let mut source = format!(
        "fn entry() {{ {} }}\n",
        (0..70).map(|i| format!("branch{i}();")).collect::<String>()
    );
    for i in 0..70 {
        source.push_str(&format!(
            "fn branch{i}() {{ {} }}\n",
            (0..4)
                .map(|j| format!("leaf_{}();", i * 4 + j))
                .collect::<String>()
        ));
    }
    for i in 0..280 {
        source.push_str(&format!("fn leaf_{i}() {{}}\n"));
    }
    let f = Fixture::new(&[("lib.rs", &source)]);
    let response = f.follow(
        &f.anchor("lib.rs", 1),
        "leaf",
        &FollowOptions {
            direction: Direction::Outgoing,
            relation: FollowRelation::Calls,
            limit: 2,
            ..Default::default()
        },
    );
    assert_eq!(response.traversal.visited, MAX_NODES);
    assert!(response.traversal.node_limit_hit);
    assert_eq!(response.traversal.edge_limit_hits, 1);
    assert_eq!(response.matches.len(), 2);
    assert!(response.traversal.unreturned_matches > 0);
    assert!(response.traversal.depth_frontier > 0);
    assert!(
        response
            .notices
            .iter()
            .any(|n| n.code == "traversal_truncated")
    );
    assert!(
        response
            .notices
            .iter()
            .any(|n| n.code == "results_truncated")
    );
    assert_routes(&response);
}

#[test]
fn lexical_no_match_is_empty_and_invalid_budgets_fail_before_traversal() {
    let f = Fixture::new(&[("lib.rs", "fn entry() { policy(); }\nfn policy() {}\n")]);
    let anchor = f.anchor("lib.rs", 1);
    let none = f.follow(&anchor, "zebra", &Default::default());
    assert!(none.matches.is_empty() && none.locations.is_empty() && none.links.is_empty());
    assert!(none.notices.iter().any(|n| n.code == "no_matches"));
    let engine = crate::SearchEngine::open(&f.db, RetrievalMode::Lexical, None).unwrap();
    for options in [
        FollowOptions {
            hops: 0,
            ..Default::default()
        },
        FollowOptions {
            hops: 5,
            ..Default::default()
        },
        FollowOptions {
            limit: 0,
            ..Default::default()
        },
        FollowOptions {
            limit: 21,
            ..Default::default()
        },
        FollowOptions {
            focus: FocusOptions {
                policy: FocusPolicy::Legacy,
                max_lines: 16,
            },
            ..Default::default()
        },
        FollowOptions {
            focus: FocusOptions {
                policy: FocusPolicy::Contextual,
                max_lines: 16,
            },
            ..Default::default()
        },
    ] {
        assert!(engine.follow(&anchor, "policy", &options).is_err());
    }
    assert!(engine.follow(&anchor, "  ", &Default::default()).is_err());
    assert!(
        engine
            .follow("sn1:bad", "policy", &Default::default())
            .is_err()
    );
}

#[test]
fn endpoint_kind_filter_keeps_intermediate_types_in_the_trail() {
    let f = Fixture::new(&[(
        "lib.rs",
        "fn entry() { let _ = Flag; }\nstruct Flag;\nfn policy(_value: Flag) {}\n",
    )]);
    let response = f.follow(
        &f.anchor("lib.rs", 1),
        "policy",
        &FollowOptions {
            kind: Some("function".into()),
            ..Default::default()
        },
    );
    assert_eq!(response.matches.len(), 1);
    assert_eq!(
        target(&response, &response.matches[0]).symbol.as_deref(),
        Some("policy")
    );
    assert_eq!(response.matches[0].steps.len(), 2);
    assert!(
        response
            .locations
            .iter()
            .any(|l| l.kind == "struct" && l.symbol.as_deref() == Some("Flag"))
    );
    assert_routes(&response);
}
