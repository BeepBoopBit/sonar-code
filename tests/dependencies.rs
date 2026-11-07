use sonar::{
    Direction,
    analysis::{DependencyKind, DependencyOptions, dependencies},
};
use std::{fs, path::PathBuf, process::Command};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        for (path, source) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        let db = temp.path().join("index.sqlite");
        sonar::index(&root, &db).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
}

#[test]
fn module_dependencies_reverse_paths_and_unresolved_imports_have_evidence() {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "mod a;\nuse crate::a::run;\npub use crate::a::run as execute;\nmod inline { mod child; }\n",
        ),
        ("src/a.rs", "mod b;\npub fn run() {}\n"),
        ("src/a/b.rs", "pub fn leaf() {}\n"),
        ("src/inline/child.rs", "pub fn child() {}\n"),
    ]);
    let before = fs::read(&f.db).unwrap();
    let response = dependencies(
        &f.db,
        "./src/lib.rs",
        &DependencyOptions {
            depth: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(response.relationships.len(), 3);
    assert!(response.files.contains(&"src/a/b.rs".into()));
    assert_eq!(response.unresolved_total, 2);
    assert_eq!(response.unresolved[0].kind, DependencyKind::Imports);
    assert_eq!(response.unresolved[1].kind, DependencyKind::ReExports);
    assert_eq!(
        response
            .relationships
            .iter()
            .find(|r| r.to == "src/a.rs")
            .unwrap()
            .kinds[0]
            .evidence[0]
            .line,
        1
    );
    for fact in &response.relationships {
        let evidence = &fact.kinds[0].evidence[0];
        let source = fs::read_to_string(f.root.join(&evidence.path)).unwrap();
        assert!(source[evidence.bytes[0]..evidence.bytes[1]].starts_with("mod "));
    }
    let reverse = dependencies(
        &f.db,
        "src/a/b.rs",
        &DependencyOptions {
            direction: Direction::Incoming,
            depth: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(reverse.files.contains(&"src/lib.rs".into()));
    assert_eq!(reverse.relationships.len(), 2);
    assert_eq!(fs::read(&f.db).unwrap(), before, "queries are read only");
}

#[test]
fn unresolved_layout_parse_errors_and_language_gaps_are_explicit() {
    let f = Fixture::new(&[
        (
            "src/lib.rs",
            "mod absent;\nmod ambiguous;\n#[path = \"other.rs\"] mod custom;\n",
        ),
        ("src/ambiguous.rs", ""),
        ("src/ambiguous/mod.rs", ""),
        ("src/other.rs", ""),
        ("src/broken.rs", "fn {"),
        ("app.py", "import os\n"),
    ]);
    let result = dependencies(&f.db, "src/lib.rs", &Default::default()).unwrap();
    assert!(result.relationships.is_empty());
    let reasons: Vec<_> = result
        .unresolved
        .iter()
        .map(|u| u.reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        vec![
            "target_not_indexed",
            "ambiguous_module_target",
            "attribute_driven_module_layout_unsupported"
        ]
    );
    assert_eq!(
        result
            .coverage
            .iter()
            .find(|c| c.language == "rust")
            .unwrap()
            .skipped_files,
        vec!["src/broken.rs"]
    );
    assert!(result.coverage.iter().any(|c| c.language == "python"));
    assert!(
        !result
            .notices
            .iter()
            .any(|n| n.code == "syntax_dependencies_unavailable" && n.message.contains("python"))
    );
}

#[test]
fn stale_sources_are_rejected_and_reindex_replaces_dependency_facts() {
    let f = Fixture::new(&[("lib.rs", "mod a;"), ("a.rs", "")]);
    fs::write(f.root.join("lib.rs"), "mod b;").unwrap();
    fs::write(f.root.join("b.rs"), "").unwrap();
    assert!(
        dependencies(&f.db, "lib.rs", &Default::default())
            .unwrap_err()
            .to_string()
            .contains("sources changed")
    );
    sonar::index(&f.root, &f.db).unwrap();
    let result = dependencies(&f.db, "lib.rs", &Default::default()).unwrap();
    assert_eq!(result.relationships.len(), 1);
    assert_eq!(result.relationships[0].to, "b.rs");
    fs::remove_file(f.root.join("b.rs")).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let result = dependencies(&f.db, "lib.rs", &Default::default()).unwrap();
    assert!(result.relationships.is_empty());
    assert_eq!(result.unresolved[0].reason, "target_not_indexed");
}

#[test]
fn validates_requests_limits_and_legacy_indexes() {
    let f = Fixture::new(&[
        ("lib.rs", "mod a; mod b; use std::fs; use std::io;"),
        ("a.rs", ""),
        ("b.rs", ""),
    ]);
    for path in ["../lib.rs", "/lib.rs", "missing.rs"] {
        assert!(dependencies(&f.db, path, &Default::default()).is_err());
    }
    for options in [
        DependencyOptions {
            depth: 0,
            ..Default::default()
        },
        DependencyOptions {
            limit: 0,
            ..Default::default()
        },
    ] {
        assert!(dependencies(&f.db, "lib.rs", &options).is_err());
    }
    let limited = dependencies(
        &f.db,
        "lib.rs",
        &DependencyOptions {
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(limited.truncated);
    assert_eq!(limited.relationships.len(), 1);
    assert_eq!(limited.unresolved.len(), 1);
    assert_eq!(limited.unresolved_total, 2);
    rusqlite::Connection::open(&f.db)
        .unwrap()
        .execute("DROP TABLE analysis_batches", [])
        .unwrap();
    assert!(
        dependencies(&f.db, "lib.rs", &Default::default())
            .unwrap_err()
            .to_string()
            .contains("dependency facts unavailable")
    );
    sonar::index(&f.root, &f.db).unwrap();
    assert!(dependencies(&f.db, "lib.rs", &Default::default()).is_ok());
}

#[test]
fn cli_dependencies_and_dependents_match_library() {
    let f = Fixture::new(&[("lib.rs", "mod a;"), ("a.rs", "")]);
    for (command, path, direction) in [
        ("dependencies", "lib.rs", Direction::Outgoing),
        ("dependents", "a.rs", Direction::Incoming),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_sonar"))
            .args(["--db", f.db.to_str().unwrap(), command, path])
            .current_dir(&f.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let expected = dependencies(
            &f.db,
            path,
            &DependencyOptions {
                direction,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(actual, serde_json::to_value(expected).unwrap());
    }
}

#[test]
fn pagination_exhausts_groups_unresolved_and_skipped_files_without_duplicates() {
    let f = Fixture::new(&[
        (
            "lib.rs",
            "mod a; mod b; mod c; use one; use two; use three; use four; use five;",
        ),
        ("a.rs", "mod nested; use six;"),
        ("a/nested.rs", ""),
        ("b.rs", ""),
        ("c.rs", ""),
        ("broken1.rs", "fn {"),
        ("broken2.rs", "fn {"),
        ("broken3.rs", "fn {"),
    ]);
    let before = fs::read(&f.db).unwrap();
    for direction in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
        let path = if matches!(direction, Direction::Incoming) {
            "a/nested.rs"
        } else {
            "lib.rs"
        };
        let expected = dependencies(
            &f.db,
            path,
            &DependencyOptions {
                direction,
                depth: 2,
                ..Default::default()
            },
        )
        .unwrap();
        let mut options = DependencyOptions {
            direction,
            depth: 2,
            limit: 2,
            ..Default::default()
        };
        let (mut groups, mut unresolved, mut skipped) = (vec![], vec![], vec![]);
        let mut pages = 0;
        loop {
            let response = dependencies(&f.db, path, &options).unwrap();
            assert_eq!(response.version, 2);
            assert_eq!(response.relationships_total, expected.relationships.len());
            assert_eq!(response.reachable_files, expected.reachable_files);
            assert_eq!(response.unresolved_total, expected.unresolved.len());
            assert_eq!(response.truncated, response.next_cursor.is_some());
            assert!(response.relationships.len() <= 2 && response.unresolved.len() <= 2);
            assert!(response.coverage.iter().all(|c| c.skipped_files.len() <= 2));
            groups.extend(response.relationships);
            unresolved.extend(response.unresolved);
            skipped.extend(response.coverage.into_iter().flat_map(|c| c.skipped_files));
            pages += 1;
            assert!(pages <= 4, "continuation must terminate");
            options.cursor = response.next_cursor;
            if options.cursor.is_none() {
                break;
            }
        }
        assert_eq!(groups, expected.relationships);
        assert_eq!(
            serde_json::to_value(unresolved).unwrap(),
            serde_json::to_value(expected.unresolved).unwrap()
        );
        assert_eq!(
            skipped,
            expected
                .coverage
                .into_iter()
                .flat_map(|c| c.skipped_files)
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(fs::read(&f.db).unwrap(), before);
}

#[test]
fn cursors_bind_options_repository_and_source_generation() {
    let files = [("lib.rs", "mod a; mod b;"), ("a.rs", ""), ("b.rs", "")];
    let f = Fixture::new(&files);
    let other = Fixture::new(&files);
    let options = DependencyOptions {
        limit: 1,
        ..Default::default()
    };
    let first = dependencies(&f.db, "lib.rs", &options).unwrap();
    let cursor = first.next_cursor.unwrap();
    let next = DependencyOptions {
        cursor: Some(cursor.clone()),
        ..options.clone()
    };
    assert!(dependencies(&f.db, "./lib.rs", &next).is_ok());
    for changed in [
        DependencyOptions {
            limit: 2,
            ..next.clone()
        },
        DependencyOptions {
            depth: 2,
            ..next.clone()
        },
        DependencyOptions {
            direction: Direction::Incoming,
            ..next.clone()
        },
    ] {
        assert!(dependencies(&f.db, "lib.rs", &changed).is_err());
    }
    assert!(dependencies(&f.db, "a.rs", &next).is_err());
    assert!(dependencies(&other.db, "lib.rs", &next).is_err());
    for bad in [
        "invalid".into(),
        "x".repeat(129),
        format!("{}:0", cursor.rsplit_once(':').unwrap().0),
        format!(
            "{}:9999999999999999999999999999999999999",
            cursor.rsplit_once(':').unwrap().0
        ),
    ] {
        assert!(
            dependencies(
                &f.db,
                "lib.rs",
                &DependencyOptions {
                    cursor: Some(bad),
                    ..options.clone()
                }
            )
            .is_err()
        );
    }
    fs::write(f.root.join("a.rs"), "fn added() {}").unwrap();
    assert!(dependencies(&f.db, "lib.rs", &next).is_err());
    sonar::index(&f.root, &f.db).unwrap();
    assert!(dependencies(&f.db, "lib.rs", &next).is_err());
}

#[test]
fn cli_continuations_match_library_pages() {
    let f = Fixture::new(&[("lib.rs", "mod a; mod b;"), ("a.rs", ""), ("b.rs", "")]);
    let first = dependencies(
        &f.db,
        "lib.rs",
        &DependencyOptions {
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let cursor = first.next_cursor.unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .args([
            "--db",
            f.db.to_str().unwrap(),
            "dependencies",
            "lib.rs",
            "--limit",
            "1",
            "--cursor",
            &cursor,
        ])
        .current_dir(&f.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let expected = dependencies(
        &f.db,
        "lib.rs",
        &DependencyOptions {
            limit: 1,
            cursor: Some(cursor),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(actual, serde_json::to_value(expected).unwrap());
}
