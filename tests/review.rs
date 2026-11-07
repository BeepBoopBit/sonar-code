use sonar::analysis::{
    ReviewAreasOptions, ReviewCluster, ReviewContextOptions, review_areas, review_context,
};
use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

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
        sonar::index_navigation(&db).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
    fn standard() -> Self {
        Self::new(&[
            ("src/lib.rs", "mod a;\nmod b;\n#[cfg(test)] mod tests;\n"),
            ("src/a.rs", "pub fn run() {}\nmod nested;\n"),
            ("src/a/nested.rs", "pub fn leaf() {}\n"),
            ("src/b.rs", "pub fn run() {}\n"),
            ("src/tests.rs", "#[test]\nfn check() {}\n"),
            ("scripts/check.py", "def inspect():\n    pass\n"),
            ("src/broken.rs", "fn {"),
            ("README.md", "# Repo\n"),
        ])
    }
}

fn check_cluster(f: &Fixture, cluster: &ReviewCluster, max_files: usize) {
    assert!(!cluster.files.is_empty() && cluster.files.len() <= max_files);
    assert_eq!(cluster.files.len(), cluster.reading_path.len());
    let mut seen = BTreeSet::new();
    for step in &cluster.reading_path {
        assert!(cluster.files.contains(&step.file));
        if let Some(parent) = &step.parent_file {
            assert!(seen.contains(parent));
            let via = step.via.as_ref().unwrap();
            assert!(
                (&via.from == parent && via.to == step.file)
                    || (&via.to == parent && via.from == step.file)
            );
            let source = fs::read(f.root.join(&via.evidence.path)).unwrap();
            let [start, end] = via.evidence.bytes;
            assert!(start < end && end <= source.len());
            assert_eq!(
                source[..start].iter().filter(|b| **b == b'\n').count() + 1,
                via.evidence.line
            );
        } else {
            assert!(seen.is_empty());
            assert!(step.via.is_none());
        }
        let lines = fs::read_to_string(f.root.join(&step.file))
            .unwrap()
            .lines()
            .count()
            .max(1);
        assert!(!step.ranges.is_empty() && step.ranges.len() <= 3);
        for [start, end] in &step.ranges {
            assert!(start <= end && *start >= 1 && *end <= lines);
        }
        assert!(seen.insert(step.file.clone()));
    }
    for boundary in &cluster.boundary {
        assert!(!cluster.files.contains(&boundary.outside_file));
        let r = &boundary.relationship;
        assert!(cluster.files.contains(&r.from) || cluster.files.contains(&r.to));
    }
}

#[test]
fn selected_context_honors_budget_proofs_tests_and_seed_forms() {
    let f = Fixture::standard();
    let before = fs::read(&f.db).unwrap();
    let response = review_context(
        &f.db,
        "src/lib.rs",
        &ReviewContextOptions {
            max_files: 2,
            ..Default::default()
        },
    )
    .unwrap();
    check_cluster(&f, &response.cluster, 2);
    assert_eq!(response.cluster.files.len(), 2);
    assert!(response.cluster.omitted_files_total >= 2);
    assert!(
        response
            .cluster
            .related_tests
            .iter()
            .any(|t| t.path == "src/tests.rs" && t.basis == "test_file_dependency")
    );
    let full = review_context(
        &f.db,
        "src/lib.rs",
        &ReviewContextOptions {
            max_files: 12,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(full.cluster.files.len(), 5);
    check_cluster(&f, &full.cluster, 12);
    let symbol = review_context(&f.db, "leaf", &Default::default()).unwrap();
    assert_eq!(symbol.cluster.seed.path, "src/a/nested.rs");
    assert_eq!(symbol.cluster.seed.symbol.as_deref(), Some("leaf"));
    let handle = sonar::locate(&f.db, "src/a/nested.rs", 1, None)
        .unwrap()
        .locations
        .remove(0)
        .handle;
    let by_handle = review_context(&f.db, &handle, &Default::default()).unwrap();
    assert_eq!(
        serde_json::to_value(symbol).unwrap(),
        serde_json::to_value(by_handle).unwrap()
    );
    assert!(
        review_context(&f.db, "run", &Default::default()).is_err(),
        "ambiguous symbols need handles"
    );
    assert_eq!(fs::read(&f.db).unwrap(), before);
}

#[test]
fn repository_areas_partition_all_code_and_paginate_without_overlap() {
    let f = Fixture::standard();
    let before = fs::read(&f.db).unwrap();
    let mut options = ReviewAreasOptions {
        max_files: 2,
        limit: 1,
        ..Default::default()
    };
    let (mut files, mut areas, mut shared, mut unassigned) =
        (BTreeSet::new(), vec![], BTreeSet::new(), BTreeSet::new());
    loop {
        let page = review_areas(&f.db, &options).unwrap();
        assert_eq!(page.coverage.indexed_code_files, 7);
        assert_eq!(page.coverage.excluded_non_code_files, 1);
        assert_eq!(
            page.coverage.assigned_files
                + page.coverage.shared_files_total
                + page.coverage.unassigned_total,
            7
        );
        assert_eq!(page.truncated, page.next_cursor.is_some());
        for area in &page.areas {
            check_cluster(&f, area, 2);
            for file in &area.files {
                assert!(files.insert(file.clone()));
            }
        }
        areas.extend(page.areas);
        shared.extend(page.coverage.shared_files);
        unassigned.extend(page.coverage.unassigned.into_iter().map(|u| {
            assert_eq!(u.reason, "parse_errors");
            u.path
        }));
        options.cursor = page.next_cursor;
        if options.cursor.is_none() {
            break;
        }
    }
    assert!(
        files.contains("scripts/check.py"),
        "isolated supported files have singleton areas"
    );
    assert!(unassigned.contains("src/broken.rs"));
    assert_eq!(files.len() + shared.len() + unassigned.len(), 7);
    let ids: BTreeSet<_> = areas.iter().map(|a| a.id.as_str()).collect();
    for area in &areas {
        for b in &area.boundary {
            if let Some(other) = &b.other_area {
                assert!(ids.contains(other.as_str()));
                assert_ne!(other, &area.id);
            }
        }
    }
    assert_eq!(fs::read(&f.db).unwrap(), before);
}

#[test]
fn scope_cursor_and_invalid_inputs_are_explicit() {
    let f = Fixture::standard();
    let first = review_areas(
        &f.db,
        &ReviewAreasOptions {
            max_files: 1,
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let next = ReviewAreasOptions {
        max_files: 1,
        limit: 1,
        cursor: first.next_cursor,
        ..Default::default()
    };
    assert!(next.cursor.is_some());
    assert!(review_areas(&f.db, &next).is_ok());
    for changed in [
        ReviewAreasOptions {
            max_files: 2,
            ..next.clone()
        },
        ReviewAreasOptions {
            scope: Some("src".into()),
            ..next.clone()
        },
        ReviewAreasOptions {
            limit: 2,
            ..next.clone()
        },
    ] {
        assert!(review_areas(&f.db, &changed).is_err());
    }
    for value in ["README.md", "../src/lib.rs", "missing.rs", "src/broken.rs"] {
        assert!(review_context(&f.db, value, &Default::default()).is_err());
    }
    for max_files in [0, 51] {
        assert!(
            review_context(
                &f.db,
                "src/lib.rs",
                &ReviewContextOptions {
                    max_files,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    let scoped = review_context(
        &f.db,
        "src/a.rs",
        &ReviewContextOptions {
            scope: Some("src/a".into()),
            ..Default::default()
        },
    );
    assert!(
        scoped.is_err(),
        "literal directory scope does not match a.rs"
    );
    let scoped = review_context(
        &f.db,
        "src/a.rs",
        &ReviewContextOptions {
            scope: Some("src/a.rs".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(scoped.cluster.files, vec!["src/a.rs"]);
    assert!(
        scoped
            .cluster
            .boundary
            .iter()
            .all(|b| b.classification == "outside_scope")
    );
    fs::write(f.root.join("src/a.rs"), "pub fn changed() {}\n").unwrap();
    assert!(review_areas(&f.db, &next).is_err());
    sonar::index(&f.root, &f.db).unwrap();
    assert!(review_areas(&f.db, &next).is_err());
}

#[test]
fn cli_workflows_and_continuation_match_api() {
    let f = Fixture::standard();
    for (args, expected) in [
        (
            vec!["review-context", "src/lib.rs", "--max-files", "2"],
            serde_json::to_value(
                review_context(
                    &f.db,
                    "src/lib.rs",
                    &ReviewContextOptions {
                        max_files: 2,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
            .unwrap(),
        ),
        (
            vec!["review-areas", "--max-files", "2", "--limit", "1"],
            serde_json::to_value(
                review_areas(
                    &f.db,
                    &ReviewAreasOptions {
                        max_files: 2,
                        limit: 1,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
            .unwrap(),
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_sonar"))
            .arg("--db")
            .arg(&f.db)
            .args(args)
            .current_dir(&f.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            expected
        );
    }
    let first = review_areas(
        &f.db,
        &ReviewAreasOptions {
            max_files: 2,
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let cursor = first.next_cursor.unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .arg("--db")
        .arg(&f.db)
        .args([
            "review-areas",
            "--max-files",
            "2",
            "--limit",
            "1",
            "--cursor",
            &cursor,
        ])
        .current_dir(&f.root)
        .output()
        .unwrap();
    assert!(output.status.success());
    let expected = review_areas(
        &f.db,
        &ReviewAreasOptions {
            max_files: 2,
            limit: 1,
            cursor: Some(cursor),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn empty_and_document_only_repositories_have_complete_empty_coverage() {
    for files in [&[][..], &[("README.md", "# Documentation\n")][..]] {
        let f = Fixture::new(files);
        let response = review_areas(&f.db, &Default::default()).unwrap();
        assert_eq!(response.areas_total, 0);
        assert!(response.areas.is_empty());
        assert_eq!(response.coverage.indexed_code_files, 0);
        assert_eq!(response.coverage.excluded_non_code_files, files.len());
        assert!(response.next_cursor.is_none());
    }
}

#[test]
fn module_reading_paths_retain_header_contracts_before_the_first_declaration() {
    let f = Fixture::new(&[
        ("app.py", "from policy import REQUIRED\n"),
        (
            "policy.py",
            "# Every listed role is mandatory.\nREQUIRED = ('reader', 'exporter')\n",
        ),
    ]);
    let response = review_context(
        &f.db,
        "app.py",
        &ReviewContextOptions {
            max_files: 2,
            ..Default::default()
        },
    )
    .unwrap();
    let step = response
        .cluster
        .reading_path
        .iter()
        .find(|s| s.file == "policy.py")
        .unwrap();
    assert!(step.ranges.iter().any(|r| r[0] == 1 && r[1] >= 2));
    assert_eq!(response.policy, "review-clusters-v2");
}
