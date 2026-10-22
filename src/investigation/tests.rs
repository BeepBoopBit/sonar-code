use super::*;
use protobuf::Message;
use scip::types::{Document, Index, Metadata, Occurrence, PositionEncoding};
use std::fs;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Fixture {
    /// Import source-bound SCIP occurrences rather than fabricate graph rows.
    fn new(files: &[(&str, &str)], semantic: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='investigation_fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        for (path, source) in files {
            fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            fs::write(root.join(path), source).unwrap();
        }
        let db = temp.path().join("index.sqlite");
        let indexed = crate::index(&root, &db).unwrap();
        if semantic {
            let names: Vec<_> = files
                .iter()
                .filter(|(p, _)| p.ends_with(".rs"))
                .flat_map(|(_, s)| s.lines())
                .filter_map(|l| l.strip_prefix("fn "))
                .map(|l| l.split('(').next().unwrap())
                .collect();
            let documents = files
                .iter()
                .filter(|(p, _)| p.ends_with(".rs"))
                .map(|(path, source)| {
                    let mut occurrences = Vec::new();
                    for (line, body) in source
                        .lines()
                        .enumerate()
                        .filter(|(_, l)| l.starts_with("fn "))
                    {
                        for name in &names {
                            for (column, _) in body.match_indices(name) {
                                let word = |c: char| c.is_alphanumeric() || c == '_';
                                if body[..column].chars().next_back().is_some_and(word)
                                    || body[column + name.len()..].chars().next().is_some_and(word)
                                {
                                    continue;
                                }
                                occurrences.push(Occurrence {
                                    range: vec![
                                        line as i32,
                                        column as i32,
                                        (column + name.len()) as i32,
                                    ],
                                    symbol: format!("fixture {name}"),
                                    symbol_roles: i32::from(column == 3),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    Document {
                        relative_path: (*path).into(),
                        language: "rust".into(),
                        position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                        occurrences,
                        ..Default::default()
                    }
                })
                .collect();
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
            let path = temp.path().join("manifest.json");
            fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            crate::import_scip(&db, &scip, &path).unwrap();
        }
        Self {
            _temp: temp,
            root,
            db,
        }
    }

    fn handle(&self, path: &str, line: usize) -> String {
        crate::locate(&self.db, path, line, None)
            .unwrap()
            .locations
            .remove(0)
            .handle
    }

    fn run(&self, query: &str, options: InvestigationOptions) -> Investigation {
        Investigator::open(&self.db, None)
            .unwrap()
            .investigate(query, &options)
            .unwrap()
    }
}

fn panel<'a>(r: &'a Investigation, name: &str) -> &'a InvestigationPanel {
    r.panels.iter().find(|p| p.purpose == name).unwrap()
}

fn target<'a>(r: &'a Investigation, lead: &InvestigationLead) -> &'a InvestigationLocation {
    r.locations.iter().find(|l| l.id == lead.location).unwrap()
}

fn audit(r: &Investigation, root: &Path) {
    let locations: BTreeSet<_> = r.locations.iter().map(|l| &l.id).collect();
    let mut used = BTreeSet::new();
    for p in &r.panels {
        for lead in &p.leads {
            assert!(locations.contains(&lead.location));
            if let Some(anchor) = &r.anchor {
                let mut current = anchor;
                let mut seen = BTreeSet::from([current]);
                for &step in &lead.steps {
                    used.insert(step);
                    let e = &r.links[step];
                    assert!(locations.contains(&e.from) && locations.contains(&e.to));
                    current = if current == &e.from {
                        &e.to
                    } else {
                        assert_eq!(current, &e.to);
                        &e.from
                    };
                    assert!(seen.insert(current));
                    assert!(
                        e.evidence.line
                            <= fs::read_to_string(root.join(&e.evidence.path))
                                .unwrap()
                                .lines()
                                .count()
                    );
                }
                assert_eq!(current, &lead.location);
            }
        }
    }
    assert_eq!(used.len(), r.links.len());
    for l in &r.locations {
        if let Some([lo, hi]) = l.focus {
            let source = fs::read_to_string(root.join(&l.location.path)).unwrap();
            assert!(lo > 0 && lo <= hi && hi <= source.lines().count());
            assert!(hi - lo < 16);
            assert!(
                source
                    .lines()
                    .skip(lo - 1)
                    .take(hi - lo + 1)
                    .map(|s| s.len() + 1)
                    .sum::<usize>()
                    <= 4096
            );
        }
    }
}

#[test]
fn discovery_preserves_primary_and_surfaces_kind_leads_before_pool_caps() {
    let clutter = (0..550)
        .map(|i| format!("## Policy {i}\npolicy policy\n"))
        .collect::<String>();
    let f = Fixture::new(
        &[
            ("noise.md", &clutter),
            ("lib.rs", "fn policy_owner() { /* policy */ }\n"),
            ("guide.md", "# Contract\npolicy behavior\n"),
        ],
        false,
    );
    let r = f.run("explain policy behavior", Default::default());
    let expected = SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .brief(
            "explain policy behavior",
            &BriefOptions {
                related: 0,
                ..Default::default()
            },
        )
        .unwrap();
    let ids: BTreeMap<_, _> = expected
        .files
        .iter()
        .flat_map(|f| &f.locations)
        .map(|l| (&l.id, l.handle.as_ref().unwrap()))
        .collect();
    assert_eq!(
        panel(&r, "primary")
            .leads
            .iter()
            .map(|l| &target(&r, l).location.handle)
            .collect::<Vec<_>>(),
        expected.starts.iter().map(|id| ids[id]).collect::<Vec<_>>()
    );
    assert_eq!(
        target(&r, &panel(&r, "functions").leads[0])
            .location
            .symbol
            .as_deref(),
        Some("policy_owner")
    );
    assert!(r.locations.len() <= 11);
    assert!(r.links.is_empty());
    let exact = f.run("policy_owner", Default::default());
    assert_eq!(exact.panels.len(), 1);
    let filtered = f.run(
        "explain policy behavior",
        InvestigationOptions {
            exclusions: vec!["noise.md".into()],
            ..Default::default()
        },
    );
    assert!(
        filtered
            .locations
            .iter()
            .all(|l| l.location.path != "noise.md")
    );
    let empty = f.run("xylophonium", Default::default());
    assert!(empty.locations.is_empty());
    assert!(empty.notices.iter().any(|n| n.code == "no_matches"));
    audit(&r, &f.root);
}

#[test]
fn context_connects_recorded_tests_callers_callees_and_containing_documentation() {
    let f = Fixture::new(
        &[
            (
                "core.rs",
                "fn policy() { helper(); }\nfn helper() {}\nfn bridge() { policy(); policy(); }\n#[test]\nfn verifies_policy() { bridge(); }\nfn reference() { let _ = policy; }\n",
            ),
            (
                "unrelated.rs",
                "#[test]\nfn policy_test_by_name_only() {}\n",
            ),
            ("guide.md", "# Contract\n[source](core.rs)\n"),
        ],
        true,
    );
    crate::index_navigation(&f.db).unwrap();
    let opts = InvestigationOptions {
        from: Some(f.handle("core.rs", 1)),
        ..Default::default()
    };
    let r = f.run("explain policy", opts.clone());
    assert_eq!(
        target(&r, &panel(&r, "callers").leads[0])
            .location
            .symbol
            .as_deref(),
        Some("bridge")
    );
    assert_eq!(
        target(&r, &panel(&r, "callees").leads[0])
            .location
            .symbol
            .as_deref(),
        Some("helper")
    );
    assert_eq!(
        target(&r, &panel(&r, "references").leads[0])
            .location
            .symbol
            .as_deref(),
        Some("reference")
    );
    let test = &panel(&r, "test_code").leads[0];
    assert_eq!(test.steps.len(), 2);
    assert_eq!(
        target(&r, test).location.symbol.as_deref(),
        Some("verifies_policy")
    );
    assert!(
        !r.locations
            .iter()
            .any(|l| l.location.path == "unrelated.rs")
    );
    assert_eq!(r.links[test.steps[0]].occurrences, 2);
    let doc = &panel(&r, "documentation").leads[0];
    assert_eq!(target(&r, doc).location.path, "guide.md");
    assert_eq!(doc.steps.len(), 2);
    assert_eq!(r.links[doc.steps[0]].kind, "contains");
    assert_eq!(r.links[doc.steps[1]].kind, "documents");
    audit(&r, &f.root);
    let filtered = f.run(
        "policy",
        InvestigationOptions {
            exclusions: vec!["guide.md".into()],
            ..opts.clone()
        },
    );
    assert!(panel(&filtered, "documentation").leads.is_empty());
    assert!(filtered.links.iter().all(|e| e.evidence.path != "guide.md"));
    let doc_anchor = f.run(
        "policy",
        InvestigationOptions {
            from: Some(f.handle("guide.md", 2)),
            ..Default::default()
        },
    );
    assert_eq!(
        target(&doc_anchor, &panel(&doc_anchor, "linked_sources").leads[0])
            .location
            .path,
        "core.rs"
    );
    audit(&doc_anchor, &f.root);
    fs::write(f.root.join("core.rs"), "fn changed() {}\n").unwrap();
    assert!(
        Investigator::open(&f.db, None)
            .unwrap()
            .investigate("policy", &opts)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    crate::index(&f.root, &f.db).unwrap();
    assert!(
        Investigator::open(&f.db, None)
            .unwrap()
            .investigate("policy", &opts)
            .is_err()
    );
}

#[test]
fn excluded_callers_do_not_consume_edge_budget_and_missing_analysis_is_explicit() {
    let noise = (0..70)
        .map(|i| format!("fn caller_{i}() {{ policy(); }}\n"))
        .collect::<String>();
    let f = Fixture::new(
        &[
            ("policy.rs", "fn policy() {}\n"),
            ("old/noise.rs", &noise),
            ("keep.rs", "#[test]\nfn verifies_policy() { policy(); }\n"),
        ],
        true,
    );
    let opts = InvestigationOptions {
        from: Some(f.handle("policy.rs", 1)),
        ..Default::default()
    };
    let r = f.run("policy", opts.clone());
    assert!(r.traversal.as_ref().unwrap().edge_limit_hits > 0);
    let filtered = f.run(
        "policy",
        InvestigationOptions {
            exclusions: vec!["old".into()],
            ..opts.clone()
        },
    );
    assert_eq!(filtered.traversal.as_ref().unwrap().edge_limit_hits, 0);
    assert_eq!(panel(&filtered, "test_code").leads.len(), 1);
    assert!(
        filtered
            .locations
            .iter()
            .all(|l| !l.location.path.starts_with("old/"))
    );
    assert!(
        filtered
            .notices
            .iter()
            .any(|n| n.code == "navigation_unavailable")
    );
    let bad = InvestigationOptions {
        exclusions: vec!["policy.rs".into()],
        ..opts.clone()
    };
    assert!(
        Investigator::open(&f.db, None)
            .unwrap()
            .investigate("policy", &bad)
            .is_err()
    );
    // A semantic-input change invalidates graph evidence without altering source handles.
    fs::write(
        f.root.join("Cargo.toml"),
        "[package]\nname='changed'\nversion='0.1.0'\n",
    )
    .unwrap();
    let missing = f.run("policy", opts);
    assert!(missing.panels.iter().all(|p| p.leads.is_empty()));
    assert!(missing.links.is_empty());
    assert!(
        missing
            .notices
            .iter()
            .any(|n| n.code == "graph_unavailable")
    );
    assert_eq!(missing.index.semantic_freshness, "stale");
    audit(&filtered, &f.root);
}

#[test]
fn overlap_describes_actual_focus_and_identity_without_inventing_confidence() {
    let f = Fixture::new(&[("lib.rs", "fn alpha() { let beta = 1; }\n")], false);
    let r = f.run("alpha beta gamma", Default::default());
    let a = r
        .locations
        .iter()
        .find(|l| l.location.symbol.as_deref() == Some("alpha"))
        .unwrap();
    assert_eq!(a.overlap.identity, vec!["alpha"]);
    assert_eq!(a.overlap.focus, vec!["alpha", "beta"]);
    assert_eq!(a.overlap.unseen, vec!["gamma"]);
    let r = f.run(
        "gamma",
        InvestigationOptions {
            from: Some(f.handle("lib.rs", 1)),
            ..Default::default()
        },
    );
    assert!(r.notices.iter().any(|n| n.code == "no_focus_term_overlap"));
    assert!(r.notices.iter().any(|n| n.code == "graph_unavailable"));
    assert!(r.notices.iter().any(|n| n.code == "navigation_unavailable"));
    for query in [" ", &"a".repeat(16_385)] {
        assert!(
            Investigator::open(&f.db, None)
                .unwrap()
                .investigate(query, &Default::default())
                .is_err()
        );
    }
    audit(&r, &f.root);
}

#[test]
fn incoming_test_walk_stops_at_its_node_and_depth_budgets() {
    let bridges = (0..64)
        .map(|i| format!("fn bridge_{i}() {{ policy(); }}\n"))
        .collect::<String>();
    let callers = (0..64)
        .flat_map(|i| {
            (0..3).map(move |j| format!("#[test]\nfn verify_{i}_{j}() {{ bridge_{i}(); }}\n"))
        })
        .collect::<String>();
    let f = Fixture::new(
        &[
            ("policy.rs", "fn policy() {}\n"),
            ("bridge.rs", &bridges),
            ("tests.rs", &callers),
        ],
        true,
    );
    let r = f.run(
        "policy",
        InvestigationOptions {
            from: Some(f.handle("policy.rs", 1)),
            ..Default::default()
        },
    );
    let traversal = r.traversal.as_ref().unwrap();
    assert!(traversal.test_node_limit_hit);
    assert_eq!(traversal.test_nodes_visited, 128);
    assert!(r.locations.len() <= 30 && r.links.len() <= 29);
    assert!(r.panels.iter().all(|p| p.leads.len() <= 3));
    assert!(
        panel(&r, "test_code")
            .leads
            .iter()
            .all(|l| l.steps.len() == 2)
    );
    audit(&r, &f.root);
}
