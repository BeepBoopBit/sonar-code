use rusqlite::Connection;
use sonar::{
    BriefOptions, FocusOptions, FocusPolicy, NavigationPolicy, RankingOptions, RetrievalMode,
    SearchEngine, SelectionPolicy,
};
use std::{fs, path::Path};

fn put(root: &Path, path: &str, source: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, source).unwrap();
}
fn index(root: &Path, db: &Path) {
    sonar::index_with_resources(
        root,
        db,
        sonar::Chunking::Lines,
        sonar::ResourcePolicy::Yaml,
    )
    .unwrap();
}
fn engine(db: &Path, policy: NavigationPolicy) -> SearchEngine {
    SearchEngine::open(db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_navigation(policy)
        .with_ranking(RankingOptions {
            selection: SelectionPolicy::Ranked,
            ..Default::default()
        })
}

#[test]
fn excluded_document_targets_cannot_return_as_owners_or_supporting_links() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    put(
        &root,
        "guide.md",
        "# Worker\nworker settings [old](history/worker.yaml) [current](worker.yaml)\n",
    );
    put(&root, "history/worker.yaml", "worker: legacy\n");
    put(&root, "worker.yaml", "worker: current\n");
    index(&root, &db);
    sonar::index_navigation(&db).unwrap();
    let e = engine(&db, NavigationPolicy::Owners)
        .with_anchors(true)
        .with_exclusions(vec!["history".into()]);
    let opts = BriefOptions {
        limit: 1,
        related: 10,
        ..Default::default()
    };
    let d = e.diagnose("explain worker settings", &opts).unwrap();
    assert!(d.candidates.iter().all(|c| !c.path.starts_with("history/")));
    let b = e.brief("explain worker settings", &opts).unwrap();
    assert!(!b.starts.is_empty());
    assert!(b.files.iter().all(|f| !f.path.starts_with("history/")));
    for link in b.links {
        assert!(!link.evidence.path.starts_with("history/"));
        assert!(
            b.files
                .iter()
                .flat_map(|f| &f.locations)
                .any(|l| l.id == link.from)
        );
        assert!(
            b.files
                .iter()
                .flat_map(|f| &f.locations)
                .any(|l| l.id == link.to)
        );
    }
}
fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}
fn edges(db: &Path) -> Vec<(String, String, String, String, usize, usize)> {
    let c = Connection::open(db).unwrap();
    c.prepare("SELECT e.kind,af.path,bf.path,e.path,e.start_byte,e.end_byte FROM navigation_edges e JOIN locations a ON a.id=e.from_id JOIN files af ON af.id=a.file_id JOIN locations b ON b.id=e.to_id JOIN files bf ON bf.id=b.file_id ORDER BY e.kind,e.path,e.start_byte")
        .unwrap().query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,u32>(4)? as usize,r.get::<_,u32>(5)? as usize))).unwrap().collect::<Result<_,_>>().unwrap()
}

#[test]
fn literal_resource_edges_use_smallest_owner_and_real_utf8_spans() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    let source = "// é\nconst PROFILE: &str = include_str!(r#\"profiles/worker.yaml\"#);\nfn load_bytes() { let _ = std::include_bytes!(\"profiles/worker.yaml\"); }\nfn dynamic() { let _ = include_str!(concat!(\"profiles/\", \"worker.yaml\")); }\n";
    put(&root, "lib.rs", source);
    put(&root, "profiles/worker.yaml", "name: worker\n");
    put(&root, ".gitignore", "hidden.yaml\n");
    put(&root, "hidden.yaml", "hidden: yes\n");
    index(&root, &db);
    let a = sonar::index_navigation(&db).unwrap();
    let b = sonar::index_navigation(&db).unwrap();
    assert_eq!(json(a), json(b));
    let es = edges(&db);
    assert_eq!(es.len(), 2);
    for (kind, from, to, path, a, b) in es {
        assert_eq!(kind, "embeds");
        assert_eq!(from, "lib.rs");
        assert_eq!(to, "profiles/worker.yaml");
        assert_eq!(path, "lib.rs");
        assert!(source[a..b].contains("worker.yaml"));
    }
    let c = Connection::open(&db).unwrap();
    let owners:Vec<String>=c.prepare("SELECT l.symbol FROM navigation_edges e JOIN locations l ON l.id=e.from_id ORDER BY e.line").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(owners, vec!["PROFILE", "load_bytes"]);
}

#[test]
fn markdown_links_skip_code_remote_ignored_and_invalid_targets() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    put(&root, "lib.rs", "fn first() {}\nfn second() {}\n");
    put(
        &root,
        "guide.md",
        "# Guide\n[second](lib.rs#L2)\n[section](details.md#retry)\n[remote](https://example.com/lib.rs)\n[escape](../lib.rs)\n[ignored](hidden.rs)\n[missing](missing.rs)\n`[inline code](lib.rs)`\n```md\n[code](lib.rs)\n```\n[unknown fragment](lib.rs#nope)\n[bad line](lib.rs#L2-junk)\n``[code span](lib.rs)``\n",
    );
    put(
        &root,
        "details.md",
        "# Intro\ntext\n## Retry\nretry instruction\n",
    );
    put(&root, "hidden.rs", "fn secret() {}\n");
    put(&root, ".gitignore", "hidden.rs\n");
    index(&root, &db);
    sonar::index_navigation(&db).unwrap();
    let es = edges(&db);
    assert_eq!(es.len(), 2);
    let c = Connection::open(&db).unwrap();
    let second:String=c.query_row("SELECT b.symbol FROM navigation_edges e JOIN locations b ON b.id=e.to_id WHERE e.line=2",[],|r|r.get(0)).unwrap();
    assert_eq!(second, "second");
    let line:u32=c.query_row("SELECT min(r.start_line) FROM navigation_edges e JOIN regions r ON r.location_id=e.to_id WHERE e.line=3 AND r.view='combined'",[],|r|r.get(0)).unwrap();
    assert_eq!(line, 3);
}

#[test]
fn module_test_provenance_crosses_files_but_respects_shared_routes() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    put(
        &root,
        "lib.rs",
        "#[cfg(test)]\nmod legacy;\n#[cfg(test)]\n#[path=\"shared.rs\"]\nmod test_shared;\n#[path=\"shared.rs\"]\nmod live_shared;\nmod live;\n",
    );
    put(&root, "legacy.rs", "mod child;\npub fn old_profile() {}\n");
    put(&root, "legacy/child.rs", "pub fn child_profile() {}\n");
    put(&root, "shared.rs", "pub fn shared_profile() {}\n");
    put(&root, "live.rs", "pub fn live_profile() {}\n");
    index(&root, &db);
    let report = sonar::index_navigation(&db).unwrap();
    assert!(report.test_files.contains_key("legacy.rs"));
    assert!(report.test_files.contains_key("legacy/child.rs"));
    assert!(!report.test_files.contains_key("shared.rs"));
    assert!(!report.test_files.contains_key("live.rs"));
    let b = engine(&db, NavigationPolicy::Off)
        .diagnose("old_profile", &BriefOptions::default())
        .unwrap();
    let p = engine(&db, NavigationPolicy::Provenance)
        .diagnose("old_profile", &BriefOptions::default())
        .unwrap();
    assert_eq!(b.candidates[0].source_role, "implementation");
    assert_eq!(p.candidates[0].source_role, "test");
    assert_eq!(p.navigation.unwrap().status, "exact_bypass");
    assert_eq!(
        b.candidates
            .iter()
            .map(|c| (c.location_id, c.score))
            .collect::<Vec<_>>(),
        p.candidates
            .iter()
            .map(|c| (c.location_id, c.score))
            .collect::<Vec<_>>()
    );
}

#[test]
fn nested_inline_module_paths_and_ambiguous_paths_are_conservative() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    put(
        &root,
        "lib.rs",
        "#[cfg(test)]\nmod check { mod inner; }\nmod outer;\n#[cfg(test)]\nmod ambiguous;\n",
    );
    put(&root, "check/inner.rs", "fn test_helper() {}\n");
    put(
        &root,
        "outer.rs",
        "#[cfg(test)]\nmod nested { #[path=\"other.rs\"] mod explicit; }\n",
    );
    put(&root, "outer/nested/other.rs", "fn alternate() {}\n");
    put(&root, "ambiguous.rs", "fn a() {}\n");
    put(&root, "ambiguous/mod.rs", "fn b() {}\n");
    index(&root, &db);
    let r = sonar::index_navigation(&db).unwrap();
    assert!(r.test_files.contains_key("check/inner.rs"));
    assert!(r.test_files.contains_key("outer/nested/other.rs"));
    assert!(!r.test_files.contains_key("ambiguous.rs"));
    assert!(!r.test_files.contains_key("ambiguous/mod.rs"));
    assert!(r.unsupported_modules > 0);
}

#[test]
fn navigation_discovery_is_bounded_preserves_scores_and_has_valid_links() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    put(
        &root,
        "lib.rs",
        "/// worker configuration\nconst CONFIG: &str = include_str!(\"worker.yaml\");\nfn worker_load() {}\n",
    );
    put(&root, "worker.yaml", "name: worker\nmode: configuration\n");
    index(&root, &db);
    sonar::index_navigation(&db).unwrap();
    let options = BriefOptions::default();
    let q = "explain worker configuration";
    let base = engine(&db, NavigationPolicy::Off)
        .diagnose(q, &options)
        .unwrap();
    for policy in [NavigationPolicy::Owners, NavigationPolicy::Siblings] {
        let engine = engine(&db, policy);
        let d = engine.diagnose(q, &options).unwrap();
        let nav = d.navigation.as_ref().unwrap();
        assert_eq!(nav.status, "active");
        assert!(nav.channel.len() <= 32);
        for c in &d.candidates {
            let old = base
                .candidates
                .iter()
                .find(|old| old.location_id == c.location_id);
            let added = nav.channel.iter().find(|x| x.location_id == c.location_id);
            assert_eq!(
                c.score,
                old.map_or(0.0, |c| c.score) + added.map_or(0.0, |a| a.addition)
            );
            if let Some(old) = old {
                assert_eq!(json(&old.winning_hit), json(&c.winning_hit));
            }
        }
        let brief = engine.brief(q, &options).unwrap();
        assert!(brief.links.len() <= 6);
        assert_eq!(json(&brief), json(engine.brief(q, &options).unwrap()));
        if policy == NavigationPolicy::Owners {
            assert!(brief.links.iter().any(|l| l.kind == "embeds"));
        }
    }
    let scope = BriefOptions {
        scope: Some("worker.yaml".into()),
        ..Default::default()
    };
    let d = engine(&db, NavigationPolicy::Owners)
        .diagnose(q, &scope)
        .unwrap();
    assert!(d.navigation.unwrap().edges.is_empty());
    assert!(d.candidates.iter().all(|c| c.path == "worker.yaml"));
}

#[test]
fn missing_changed_deleted_and_reindexed_facts_fall_back_without_stale_edges() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    put(
        &root,
        "lib.rs",
        "const VALUE:&str=include_str!(\"worker.yaml\");\n",
    );
    put(&root, "worker.yaml", "worker: yes\n");
    index(&root, &db);
    let q = "explain worker";
    let options = BriefOptions::default();
    let e = engine(&db, NavigationPolicy::Owners);
    assert_eq!(
        e.diagnose(q, &options).unwrap().navigation.unwrap().status,
        "unavailable"
    );
    sonar::index_navigation(&db).unwrap();
    put(&root, "worker.yaml", "worker: modified\n");
    assert!(sonar::index_navigation(&db).is_err());
    assert_eq!(
        e.diagnose(q, &options).unwrap().navigation.unwrap().status,
        "unavailable"
    );
    index(&root, &db);
    assert_eq!(
        e.diagnose(q, &options).unwrap().navigation.unwrap().status,
        "unavailable"
    );
    sonar::index_navigation(&db).unwrap();
    fs::remove_file(root.join("worker.yaml")).unwrap();
    assert_eq!(
        e.diagnose(q, &options).unwrap().navigation.unwrap().status,
        "unavailable"
    );
}

#[test]
fn exact_quantities_and_unsupported_settings_do_not_expand() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    put(
        &root,
        "lib.rs",
        "fn parse_value() {let buffer_bytes=65536;}\n",
    );
    index(&root, &db);
    sonar::index_navigation(&db).unwrap();
    for (q, status) in [
        ("parse_value", "exact_bypass"),
        ("64 KiB buffer", "quantity_bypass"),
    ] {
        let a = engine(&db, NavigationPolicy::Off)
            .brief(q, &BriefOptions::default())
            .unwrap();
        let e = engine(&db, NavigationPolicy::Owners);
        let b = e.brief(q, &BriefOptions::default()).unwrap();
        assert_eq!(json(a), json(b));
        assert_eq!(
            e.diagnose(q, &BriefOptions::default())
                .unwrap()
                .navigation
                .unwrap()
                .status,
            status
        );
    }
    assert!(
        SearchEngine::open(&db, RetrievalMode::Lexical, None)
            .unwrap()
            .with_navigation(NavigationPolicy::Owners)
            .brief("buffer", &BriefOptions::default())
            .is_err()
    );
    assert!(
        engine(&db, NavigationPolicy::Owners)
            .with_graph(true)
            .brief("buffer", &BriefOptions::default())
            .is_err()
    );
}

#[test]
fn evidence_focus_reconsiders_location_without_changing_navigation() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    let mut lines = vec!["background"; 45];
    lines[0] = "# Recovery";
    lines[2] = "retry";
    lines[24] = "retry";
    lines[35] = "abort";
    put(&root, "guide.md", &lines.join("\n"));
    index(&root, &db);
    let opts = BriefOptions::default();
    let q = "retry abort";
    let window = engine(&db, NavigationPolicy::Off).with_focus(FocusOptions {
        policy: FocusPolicy::Window,
        max_lines: 16,
    });
    let evidence = engine(&db, NavigationPolicy::Off).with_focus(FocusOptions {
        policy: FocusPolicy::Evidence,
        max_lines: 16,
    });
    let mut a = json(window.brief(q, &opts).unwrap());
    let mut b = json(evidence.brief(q, &opts).unwrap());
    let range: &serde_json::Value = &b["files"][0]["locations"][0]["focus"];
    assert!(range[0].as_u64().unwrap() <= 25 && range[1].as_u64().unwrap() >= 36);
    for value in [&mut a, &mut b] {
        for f in value["files"].as_array_mut().unwrap() {
            for l in f["locations"].as_array_mut().unwrap() {
                l.as_object_mut().unwrap().remove("focus");
            }
        }
    }
    assert_eq!(a, b);
    let d = evidence.diagnose(q, &opts).unwrap();
    assert!(d.focus.iter().all(|f| f.lines <= 16 && f.bytes <= 4096));
    assert_eq!(
        json(evidence.brief(q, &opts).unwrap()),
        json(evidence.brief(q, &opts).unwrap())
    );
}

#[test]
fn high_degree_links_are_truncated_and_reported() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    let mut guide = String::from("# Hub anchor\n");
    for i in 0..40 {
        let path = format!("resource{i}.yaml");
        put(&root, &path, "value: ordinary\n");
        guide.push_str(&format!("[details]({path})\n"));
    }
    put(&root, "guide.md", &guide);
    index(&root, &db);
    sonar::index_navigation(&db).unwrap();
    let d = engine(&db, NavigationPolicy::Owners)
        .diagnose("explain hub anchor", &BriefOptions::default())
        .unwrap();
    let n = d.navigation.unwrap();
    assert!(n.truncated);
    assert_eq!(n.edges.len(), 32);
    assert!(n.channel.len() <= 32);
}

#[test]
fn evidence_focus_keeps_link_occurrence_in_related_document() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    let db = t.path().join("db");
    let mut source = String::from("# Guide\nworker\n");
    source.push_str(&"background\n".repeat(45));
    source.push_str("[details](worker.yaml)\n");
    put(&root, "guide.md", &source);
    put(&root, "worker.yaml", "name: worker\nmode: worker\n");
    index(&root, &db);
    sonar::index_navigation(&db).unwrap();
    let e = engine(&db, NavigationPolicy::Owners).with_focus(FocusOptions {
        policy: FocusPolicy::Evidence,
        max_lines: 16,
    });
    let options = BriefOptions {
        limit: 1,
        ..Default::default()
    };
    let d = e.diagnose("explain worker", &options).unwrap();
    let proof = d
        .focus
        .iter()
        .find(|f| f.reason == "evidence_with_link_site")
        .unwrap();
    let range = proof.range.unwrap();
    assert!(range[0] <= 48 && range[1] >= 48);
    assert!(proof.lines <= 16 && proof.bytes <= 4096);
}
