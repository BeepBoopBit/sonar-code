use sonar::{BriefOptions, RankingOptions, RetrievalMode, SearchEngine, SelectionPolicy};
use std::{fs, path::Path};

fn engine(db: &Path, enabled: bool) -> SearchEngine {
    SearchEngine::open(db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_anchors(enabled)
        .with_ranking(RankingOptions {
            selection: SelectionPolicy::Ranked,
            ..Default::default()
        })
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

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn embedded_names_keep_scores_and_smallest_source_owners() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("lib.rs"),
        "fn one() { log(\"read_failed\"); } fn two() { log(\"write_failed\"); }\n",
    )
    .unwrap();
    let db = temp.path().join("db");
    index(&root, &db);
    let options = BriefOptions::default();
    let baseline = engine(&db, false)
        .diagnose("why write_failed?", &options)
        .unwrap();
    let anchored = engine(&db, true)
        .diagnose("why write_failed?", &options)
        .unwrap();
    let evidence = anchored.anchors.as_ref().unwrap();
    assert_eq!(evidence.status, "active");
    assert_eq!(evidence.anchors.len(), 1);
    assert_eq!(evidence.channel.len(), 1);
    let target = anchored
        .candidates
        .iter()
        .find(|c| c.location_id == evidence.channel[0].location_id)
        .unwrap();
    assert_eq!(target.symbol.as_deref(), Some("two"));
    for candidate in &anchored.candidates {
        let old = baseline
            .candidates
            .iter()
            .find(|c| c.location_id == candidate.location_id)
            .unwrap();
        let addition = evidence
            .channel
            .iter()
            .find(|c| c.location_id == candidate.location_id)
            .map_or(0.0, |c| c.addition);
        assert_eq!(candidate.score, old.score + addition);
        assert_eq!(json(&candidate.winning_hit), json(&old.winning_hit));
        assert!(!candidate.exact_symbol_match);
    }
    let repeated = engine(&db, true)
        .diagnose("why write_failed then write_failed?", &options)
        .unwrap();
    assert_eq!(repeated.anchors.unwrap().channel.len(), 1);
    let brief = json(
        engine(&db, true)
            .brief("why write_failed?", &options)
            .unwrap(),
    );
    assert!(brief.get("anchors").is_none());
}

#[test]
fn ambiguous_names_are_counted_after_scope_and_ignored_sources_stay_out() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    for i in 0..5 {
        fs::create_dir(root.join(format!("mod{i}"))).unwrap();
        fs::write(root.join(format!("mod{i}/lib.rs")), "fn parse_value() {}\n").unwrap();
    }
    let db = temp.path().join("db");
    index(&root, &db);
    let broad = engine(&db, true)
        .diagnose("explain parse_value", &BriefOptions::default())
        .unwrap();
    assert_eq!(broad.anchors.unwrap().status, "no_anchors");
    let scope = BriefOptions {
        scope: Some("mod1".into()),
        ..Default::default()
    };
    let one = engine(&db, true)
        .diagnose("explain parse_value", &scope)
        .unwrap();
    assert_eq!(one.anchors.as_ref().unwrap().channel.len(), 1);
    assert!(one.candidates.iter().all(|c| c.path.starts_with("mod1/")));
    fs::write(root.join(".gitignore"), "mod1/\n").unwrap();
    let ignored = engine(&db, true)
        .diagnose("explain parse_value", &scope)
        .unwrap();
    assert!(ignored.candidates.is_empty());
    assert_eq!(ignored.anchors.unwrap().status, "no_anchors");
}

#[test]
fn resource_names_require_standalone_files_and_current_indexed_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("worker.yaml"), "name: worker\n").unwrap();
    fs::write(root.join("worker.rs"), "fn task() {}\n").unwrap();
    let db = temp.path().join("db");
    index(&root, &db);
    let opts = BriefOptions::default();
    let d = engine(&db, true)
        .diagnose("change worker profile", &opts)
        .unwrap();
    assert_eq!(
        d.anchors.as_ref().unwrap().anchors[0].owners[0].path,
        "worker.yaml"
    );
    assert_eq!(d.anchors.unwrap().channel.len(), 1);
    fs::write(root.join("worker.yaml"), "name: modified\n").unwrap();
    assert_eq!(
        engine(&db, true)
            .diagnose("change worker profile", &opts)
            .unwrap()
            .anchors
            .unwrap()
            .status,
        "no_anchors"
    );
    index(&root, &db);
    assert_eq!(
        engine(&db, true)
            .diagnose("change worker profile", &opts)
            .unwrap()
            .anchors
            .unwrap()
            .status,
        "active"
    );
    fs::remove_file(root.join("worker.yaml")).unwrap();
    fs::write(root.join("newfile.yaml"), "name: newfile\n").unwrap();
    assert_eq!(
        engine(&db, true)
            .diagnose("worker and newfile", &opts)
            .unwrap()
            .anchors
            .unwrap()
            .status,
        "no_anchors"
    );
}

#[test]
fn exact_quantity_and_no_anchor_queries_preserve_complete_briefs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("worker.yaml"), "name: worker\n").unwrap();
    fs::write(
        root.join("lib.rs"),
        "fn parse_value() { let output_bytes = 65536; }\n",
    )
    .unwrap();
    let db = temp.path().join("db");
    index(&root, &db);
    let opts = BriefOptions::default();
    for query in [
        "worker.yaml",
        "parse_value",
        "64 KiB worker",
        "ordinary unrelated question",
    ] {
        assert_eq!(
            json(engine(&db, false).brief(query, &opts).unwrap()),
            json(engine(&db, true).brief(query, &opts).unwrap())
        );
    }
    assert_eq!(
        engine(&db, true)
            .diagnose("64 KiB worker", &opts)
            .unwrap()
            .anchors
            .unwrap()
            .status,
        "quantity_bypass"
    );
    assert_eq!(
        engine(&db, true)
            .diagnose("64kb worker", &opts)
            .unwrap()
            .anchors
            .unwrap()
            .status,
        "active"
    );
}

#[test]
fn quotes_match_complete_content_and_caps_bound_distinct_anchors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("lib.rs"),
        "fn a() { log(\"library can't load\"); }\nfn b() { log(\"library can't load\"); }\n",
    )
    .unwrap();
    let mut names = Vec::new();
    for c in 'a'..='j' {
        let name = format!("letter{c}");
        fs::write(root.join(format!("{name}.yaml")), "value: 1\n").unwrap();
        names.push(name);
    }
    let db = temp.path().join("db");
    index(&root, &db);
    let opts = BriefOptions::default();
    let d = engine(&db, true)
        .diagnose("why doesn't \"library can't load\" work?", &opts)
        .unwrap();
    assert_eq!(d.anchors.unwrap().channel.len(), 2);
    let d = engine(&db, true)
        .diagnose("why \"library can't load now\"?", &opts)
        .unwrap();
    assert!(d.anchors.unwrap().channel.is_empty());
    let query = names.join(" ");
    let d = engine(&db, true).diagnose(&query, &opts).unwrap();
    assert_eq!(d.anchors.as_ref().unwrap().anchors.len(), 8);
    assert_eq!(d.anchors.unwrap().channel.len(), 8);
}

#[test]
fn unsupported_ranking_combinations_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn foo() {}\n").unwrap();
    let db = temp.path().join("db");
    index(&root, &db);
    let opts = BriefOptions::default();
    let default_selection = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_anchors(true);
    assert!(
        default_selection
            .brief("foo", &opts)
            .unwrap_err()
            .to_string()
            .contains("anchors require")
    );
    assert!(
        engine(&db, true)
            .with_graph(true)
            .brief("foo", &opts)
            .is_err()
    );
    assert!(
        engine(&db, true)
            .with_channel_coverage(true)
            .brief("foo", &opts)
            .is_err()
    );
}
