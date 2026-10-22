use sonar::{Direction, NeighborOptions, Relation};
use std::{fs, path::PathBuf};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("lib.rs"), "// π🙂\nstruct S { count: usize }\nimpl S {\n fn first(&self) {\n  let 值 = 1;\n }\n fn second(&self) {}\n}\nconst PROFILE: &str = include_str!(\"profile.yaml\");\n").unwrap();
        fs::write(
            root.join("guide.md"),
            "# Guide\n[method](lib.rs#L4)\n## More\ntext\n",
        )
        .unwrap();
        fs::write(root.join("profile.yaml"), "enabled: true\n").unwrap();
        let db = temp.path().join("index.sqlite");
        sonar::index_with_resources(
            &root,
            &db,
            sonar::Chunking::Lines,
            sonar::ResourcePolicy::Yaml,
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
    fn handle(&self, line: usize) -> String {
        sonar::locate(&self.db, "lib.rs", line, None)
            .unwrap()
            .locations
            .remove(0)
            .handle
    }
}

#[test]
fn kind_filter_applies_before_the_lexical_pool_limit_and_keeps_supporting_parents() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let mut source = String::from("struct Config {\n");
    for i in 0..600 {
        source.push_str(&format!("needle_{i}: u64,\n"));
    }
    source.push_str("}\nimpl Config {\n/// needle\nfn policy(&self) {\n");
    for i in 0..200 {
        source.push_str(&format!("let unrelated_{i} = {i};\n"));
    }
    source.push_str("}\n}\n");
    fs::write(root.join("lib.rs"), source).unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let options = sonar::BriefOptions::default();
    let original = sonar::SearchEngine::open(&db, sonar::RetrievalMode::Lexical, None)
        .unwrap()
        .diagnose("needle", &options)
        .unwrap();
    assert_eq!(original.candidates.len(), 512);
    assert!(
        !original
            .candidates
            .iter()
            .any(|c| c.symbol.as_deref() == Some("Config::policy"))
    );
    let engine = sonar::SearchEngine::open(&db, sonar::RetrievalMode::Lexical, None)
        .unwrap()
        .with_kind(Some("function".into()));
    let d = engine.diagnose("needle", &options).unwrap();
    assert_eq!(d.kind.as_deref(), Some("function"));
    assert_eq!(d.candidates.len(), 1);
    assert_eq!(d.candidates[0].symbol.as_deref(), Some("Config::policy"));
    let b = engine.brief("needle", &options).unwrap();
    assert_eq!(b.starts.len(), 1);
    assert!(b.files[0].locations.iter().any(|l| l.kind == "impl"));
    for kind in ["", "Function", "function OR struct"] {
        assert!(
            sonar::SearchEngine::open(&db, sonar::RetrievalMode::Lexical, None)
                .unwrap()
                .with_kind(Some(kind.into()))
                .brief("needle", &options)
                .is_err()
        );
    }
}

#[test]
fn locate_respects_scope_unicode_positions_and_smallest_declaration() {
    let f = Fixture::new();
    let found = sonar::locate(&f.db, "./lib.rs", 5, None).unwrap();
    assert_eq!(found.locations[0].symbol.as_deref(), Some("S::first"));
    assert_eq!(found.locations[0].range, [4, 6]);
    assert!(sonar::locate(&f.db, "lib.rs", 1, Some(5)).is_err()); // inside π
    assert!(sonar::locate(&f.db, "lib.rs", 0, None).is_err());
    assert!(sonar::locate(&f.db, "lib.rs", 1, Some(100)).is_err());
    assert!(sonar::locate(&f.db, "lib.rs", 100, None).is_err());
    assert!(sonar::locate(&f.db, "../lib.rs", 1, None).is_err());
    assert!(sonar::locate(&f.db, "Cargo.toml", 1, None).is_err());
    let field = sonar::locate(&f.db, "lib.rs", 2, Some(12)).unwrap();
    assert_eq!(field.locations[0].symbol.as_deref(), Some("S::count"));
}

#[test]
fn outlines_paginate_in_source_order_and_bind_cursors_to_request() {
    let f = Fixture::new();
    let all = sonar::outline(&f.db, "lib.rs", 100, None).unwrap();
    let mut handles = Vec::new();
    let mut cursor = None;
    loop {
        let page = sonar::outline(&f.db, "lib.rs", 2, cursor.as_deref()).unwrap();
        handles.extend(page.locations.into_iter().map(|l| l.handle));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        handles,
        all.locations
            .iter()
            .map(|l| l.handle.clone())
            .collect::<Vec<_>>()
    );
    let page = sonar::outline(&f.db, "lib.rs", 1, None).unwrap();
    assert!(sonar::outline(&f.db, "guide.md", 1, page.next_cursor.as_deref()).is_err());
    assert!(sonar::outline(&f.db, "lib.rs", 0, None).is_err());
    assert!(sonar::outline(&f.db, "lib.rs", 101, None).is_err());
    let docs = sonar::outline(&f.db, "guide.md", 10, None).unwrap();
    assert_eq!(docs.locations[0].title.as_deref(), Some("Guide"));
    assert_eq!(docs.locations[1].source_role, "documentation");
    assert_eq!(docs.locations[1].range, [3, 4]);
    assert_eq!(
        sonar::locate(&f.db, "guide.md", 4, None).unwrap().locations[0].handle,
        docs.locations[1].handle
    );
}

#[test]
fn handles_reject_edits_other_repositories_and_extractor_generations() {
    let f = Fixture::new();
    let other = Fixture::new();
    let h = f.handle(5);
    assert!(sonar::neighbors(&other.db, &h, &Default::default()).is_err());
    let unchanged = sonar::index_with_resources(
        &f.root,
        &f.db,
        sonar::Chunking::Lines,
        sonar::ResourcePolicy::Yaml,
    )
    .unwrap();
    assert_eq!(unchanged.changed, 0);
    assert_eq!(h, f.handle(5));
    sonar::index_with_resources(
        &f.root,
        &f.db,
        sonar::Chunking::Ast,
        sonar::ResourcePolicy::Yaml,
    )
    .unwrap();
    assert!(sonar::neighbors(&f.db, &h, &Default::default()).is_err());
    let h = f.handle(5);
    fs::write(f.root.join("lib.rs"), "fn replacement() {}\n").unwrap();
    assert!(sonar::neighbors(&f.db, &h, &Default::default()).is_err());
    assert!(sonar::locate(&f.db, "lib.rs", 1, None).is_err());
    sonar::index(&f.root, &f.db).unwrap();
    assert!(sonar::neighbors(&f.db, &h, &Default::default()).is_err());
}

#[test]
fn explicit_connections_preserve_direction_evidence_scope_and_pagination() {
    let f = Fixture::new();
    sonar::index_navigation(&f.db).unwrap();
    let first = f.handle(5);
    let incoming = sonar::neighbors(
        &f.db,
        &first,
        &NeighborOptions {
            direction: Direction::Incoming,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(incoming.links.iter().all(|l| l.to == first));
    assert!(
        incoming.links.iter().any(|l| l.kind == "documents"
            && l.evidence.path == "guide.md"
            && l.evidence.line == 2)
    );
    let parent = incoming
        .locations
        .iter()
        .find(|l| l.kind == "impl")
        .unwrap()
        .handle
        .clone();
    let options = NeighborOptions {
        relation: Relation::Contains,
        direction: Direction::Outgoing,
        limit: 1,
        ..Default::default()
    };
    let page = sonar::neighbors(&f.db, &parent, &options).unwrap();
    assert_eq!(page.links.len(), 1);
    assert!(page.next_cursor.is_some());
    let next = sonar::neighbors(
        &f.db,
        &parent,
        &NeighborOptions {
            cursor: page.next_cursor.clone(),
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(next.links.len(), 1);
    assert!(next.next_cursor.is_none());
    assert_ne!(page.links[0].to, next.links[0].to);
    assert!(
        sonar::neighbors(
            &f.db,
            &first,
            &NeighborOptions {
                cursor: page.next_cursor,
                ..options
            }
        )
        .is_err()
    );
    let scoped = sonar::neighbors(
        &f.db,
        &first,
        &NeighborOptions {
            scope: Some("lib.rs".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(scoped.locations.iter().all(|l| l.path == "lib.rs"));
    assert!(!scoped.links.iter().any(|l| l.kind == "documents"));
    assert!(
        sonar::neighbors(
            &f.db,
            &first,
            &NeighborOptions {
                scope: Some("guide.md".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
    let resource = sonar::neighbors(
        &f.db,
        &f.handle(9),
        &NeighborOptions {
            relation: Relation::Embeds,
            direction: Direction::Outgoing,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(resource.links.len(), 1);
    assert_eq!(resource.locations[0].path, "profile.yaml");
}

#[test]
fn absent_analysis_is_explicit_and_handles_are_opt_in() {
    let f = Fixture::new();
    let h = f.handle(5);
    let response = sonar::neighbors(
        &f.db,
        &h,
        &NeighborOptions {
            relation: Relation::Calls,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(response.links.is_empty());
    assert!(
        response
            .notices
            .iter()
            .any(|n| n.code == "graph_unavailable")
    );
    let engine = sonar::SearchEngine::open(&f.db, sonar::RetrievalMode::Lexical, None).unwrap();
    let plain = engine.brief("S::first", &Default::default()).unwrap();
    assert!(
        plain
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .all(|l| l.handle.is_none())
    );
    let with = engine
        .with_handles(true)
        .brief("S::first", &Default::default())
        .unwrap();
    let start = with
        .files
        .iter()
        .flat_map(|f| &f.locations)
        .find(|l| l.id == with.starts[0])
        .unwrap();
    assert_eq!(start.handle.as_deref(), Some(h.as_str()));
    let mut json = serde_json::to_value(with).unwrap();
    for file in json["files"].as_array_mut().unwrap() {
        for l in file["locations"].as_array_mut().unwrap() {
            l.as_object_mut().unwrap().remove("handle");
        }
    }
    assert_eq!(json, serde_json::to_value(plain).unwrap());
}
