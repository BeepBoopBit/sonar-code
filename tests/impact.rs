use sonar::analysis::{ImpactOptions, impact};
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
        let fixture = Self {
            db: temp.path().join("index.sqlite"),
            _temp: temp,
            root,
        };
        fixture.git(&["init", "-q"]);
        fixture.git(&["add", "--all"]);
        fixture.git(&[
            "-c",
            "user.name=Sonar Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "base",
        ]);
        fixture
    }
    fn git(&self, args: &[&str]) -> Vec<u8> {
        let p = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        p.stdout
    }
    fn index(&self) {
        sonar::index(&self.root, &self.db).unwrap();
    }
}

#[test]
fn maps_worktree_and_staged_changes_to_current_declaration_candidates() {
    let f = Fixture::new(&[
        ("lib.rs", "mod a; mod b;\n"),
        (
            "a.rs",
            "pub fn value() -> u32 {\n 1\n}\npub fn unchanged() {}\n",
        ),
        ("b.rs", "pub fn caller() { a::value(); }\n"),
    ]);
    fs::write(
        f.root.join("a.rs"),
        "pub fn value() -> u32 {\n 2\n}\npub fn unchanged() {}\n",
    )
    .unwrap();
    f.git(&["add", "a.rs"]);
    fs::write(
        f.root.join("a.rs"),
        "pub fn value() -> u32 {\n 3\n}\npub fn unchanged() {}\n",
    )
    .unwrap();
    f.index();
    let before_db = fs::read(&f.db).unwrap();
    let before_git = f.git(&["diff", "HEAD"]);
    let result = impact(
        &f.db,
        &ImpactOptions {
            max_files: 2,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(result.changes_total, 1);
    assert!(result.baseline_analysis);
    let change = &result.changes[0];
    assert_eq!(change.path, "a.rs");
    assert_eq!(change.status, "M");
    assert_eq!(
        change
            .declarations
            .iter()
            .filter_map(|d| d.symbol.as_deref())
            .collect::<Vec<_>>(),
        vec!["value"]
    );
    assert!(
        change
            .gaps
            .contains(&"current_compiler_consumers_unavailable".into())
    );
    assert!(change.cluster.as_ref().unwrap().files.len() <= 2);
    assert_eq!(change.hunks[0].new_start, 2);
    assert_eq!(f.git(&["diff", "HEAD"]), before_git);
    assert_eq!(fs::read(&f.db).unwrap(), before_db);
}

#[test]
fn added_deleted_non_code_and_literal_paths_have_explicit_results() {
    let f = Fixture::new(&[
        ("lib.rs", "fn first() {}\nfn removed() {}\n"),
        ("deleted.rs", "fn gone() {}\n"),
        ("odd [x]\nname.rs", "fn odd() { let x=1; }\n"),
        ("README.md", "# Before\n"),
    ]);
    fs::write(f.root.join("lib.rs"), "fn first() {}\n").unwrap();
    fs::remove_file(f.root.join("deleted.rs")).unwrap();
    fs::write(f.root.join("new.rs"), "fn added() {}\n").unwrap();
    fs::write(f.root.join("odd [x]\nname.rs"), "fn odd() { let x=2; }\n").unwrap();
    fs::write(f.root.join("README.md"), "# After\n").unwrap();
    f.index();
    let report = impact(
        &f.db,
        &ImpactOptions {
            limit: 20,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.changes_total, 5);
    let deleted = report
        .changes
        .iter()
        .find(|c| c.path == "deleted.rs")
        .unwrap();
    assert!(deleted.cluster.is_none() && deleted.declarations.is_empty());
    assert!(
        deleted
            .baseline_declarations
            .iter()
            .any(|d| d.symbol.as_deref() == Some("gone"))
    );
    assert!(deleted.baseline_cluster.is_some());
    let lines = report.changes.iter().find(|c| c.path == "lib.rs").unwrap();
    assert!(
        lines.declarations.is_empty(),
        "deletion boundary must not accuse the neighboring function"
    );
    assert!(
        lines
            .baseline_declarations
            .iter()
            .any(|d| d.symbol.as_deref() == Some("removed"))
    );
    assert!(
        lines
            .declaration_changes
            .iter()
            .any(|d| d.status == "removed")
    );
    let added = report.changes.iter().find(|c| c.path == "new.rs").unwrap();
    assert_eq!(added.status, "untracked");
    assert_eq!(added.declarations[0].symbol.as_deref(), Some("added"));
    let odd = report
        .changes
        .iter()
        .find(|c| c.path == "odd [x]\nname.rs")
        .unwrap();
    assert_eq!(odd.declarations[0].symbol.as_deref(), Some("odd"));
    let docs = report
        .changes
        .iter()
        .find(|c| c.path == "README.md")
        .unwrap();
    assert!(docs.cluster.is_none());
    assert!(docs.gaps.contains(&"non_code_change_not_analyzed".into()));
}

#[test]
fn pagination_is_exact_and_bound_to_diff_options_repository_and_analysis() {
    let files = [("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")];
    let f = Fixture::new(&files);
    fs::write(f.root.join("a.rs"), "fn a() { let x=1; }\n").unwrap();
    fs::write(f.root.join("b.rs"), "fn b() { let x=1; }\n").unwrap();
    f.index();
    let first = impact(
        &f.db,
        &ImpactOptions {
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let cursor = first.next_cursor.unwrap();
    let next = ImpactOptions {
        limit: 1,
        cursor: Some(cursor),
        ..Default::default()
    };
    let second = impact(&f.db, &next).unwrap();
    assert_ne!(first.changes[0].path, second.changes[0].path);
    assert!(second.next_cursor.is_none());
    for options in [
        ImpactOptions {
            limit: 2,
            ..next.clone()
        },
        ImpactOptions {
            max_files: 1,
            ..next.clone()
        },
        ImpactOptions {
            scope: Some("a.rs".into()),
            ..next.clone()
        },
        ImpactOptions {
            base: "HEAD~0".into(),
            ..next.clone()
        },
    ] {
        assert!(impact(&f.db, &options).is_err());
    }
    let other = Fixture::new(&files);
    other.index();
    assert!(impact(&other.db, &next).is_err());
    fs::write(f.root.join("b.rs"), "fn b() { let x=2; }\n").unwrap();
    assert!(impact(&f.db, &next).is_err());
    f.index();
    assert!(impact(&f.db, &next).is_err());
}

#[test]
fn scope_clean_trees_detail_caps_and_invalid_options_are_explicit() {
    let f = Fixture::new(&[("lib.rs", "fn root() {}\n")]);
    f.index();
    assert_eq!(impact(&f.db, &Default::default()).unwrap().changes_total, 0);
    for options in [
        ImpactOptions {
            max_files: 0,
            ..Default::default()
        },
        ImpactOptions {
            limit: 0,
            ..Default::default()
        },
        ImpactOptions {
            base: "--invalid".into(),
            ..Default::default()
        },
    ] {
        assert!(impact(&f.db, &options).is_err());
    }
    let source = (0..120)
        .map(|i| format!("fn function{i}() {{}}\n"))
        .collect::<String>();
    fs::write(f.root.join("large.rs"), source).unwrap();
    f.index();
    let report = impact(
        &f.db,
        &ImpactOptions {
            scope: Some("large.rs".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.changes_total, 1);
    assert_eq!(report.changes[0].declarations_total, 120);
    assert_eq!(report.changes[0].declarations.len(), 100);
    assert!(report.changes[0].details_truncated);
    assert_eq!(
        impact(
            &f.db,
            &ImpactOptions {
                scope: Some("lib.rs".into()),
                ..Default::default()
            }
        )
        .unwrap()
        .changes_total,
        0
    );
}

#[test]
fn cli_impact_and_cursor_match_library() {
    let f = Fixture::new(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
    fs::write(f.root.join("a.rs"), "fn a() { let x=1; }\n").unwrap();
    fs::write(f.root.join("b.rs"), "fn b() { let x=1; }\n").unwrap();
    f.index();
    let first = impact(
        &f.db,
        &ImpactOptions {
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    for cursor in [None, first.next_cursor] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sonar"));
        command
            .arg("--db")
            .arg(&f.db)
            .args(["impact", "--base", "HEAD", "--limit", "1"]);
        if let Some(cursor) = &cursor {
            command.args(["--cursor", cursor]);
        }
        let output = command.current_dir(&f.root).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = impact(
            &f.db,
            &ImpactOptions {
                limit: 1,
                cursor,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}

#[test]
fn non_git_roots_are_rejected_and_parse_errors_remain_visible() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn root() {}\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    assert!(impact(&db, &Default::default()).is_err());
    let f = Fixture::new(&[("lib.rs", "fn valid() {}\n")]);
    fs::write(f.root.join("lib.rs"), "fn {\n").unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    assert!(report.changes[0].gaps.contains(&"parse_errors".into()));
    assert!(report.changes[0].cluster.is_none());
}

#[test]
fn baseline_signatures_and_deleted_body_lines_map_to_the_original_identity() {
    let f = Fixture::new(&[(
        "lib.rs",
        "fn value(x: u32) -> u32 {\n let unused = 1;\n x\n}\n",
    )]);
    fs::write(f.root.join("lib.rs"), "fn value(x: u64) -> u64 {\n x\n}\n").unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    let change = &report.changes[0];
    let delta = change
        .declaration_changes
        .iter()
        .find(|d| {
            d.before
                .as_ref()
                .is_some_and(|d| d.symbol.as_deref() == Some("value"))
        })
        .unwrap();
    assert_eq!(delta.status, "modified");
    assert!(!delta.uncertain);
    assert_eq!(delta.signature_changed, Some(true));
    assert!(delta.before.as_ref().unwrap().signature.contains("u32"));
    assert!(delta.after.as_ref().unwrap().signature.contains("u64"));
    assert_eq!(delta.before.as_ref().unwrap().snapshot, "baseline");
    assert_eq!(
        delta.before.as_ref().unwrap().commit.as_deref(),
        Some(report.base_commit.as_str())
    );
    assert!(
        change
            .gaps
            .contains(&"baseline_compiler_consumers_unavailable".into())
    );
    let f = Fixture::new(&[("lib.rs", "fn value() {\n let removed = 1;\n}\n")]);
    fs::write(f.root.join("lib.rs"), "fn value() {\n}\n").unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    let change = &report.changes[0];
    assert!(change.hunks.iter().all(|h| h.new_lines == 0));
    assert_eq!(change.declarations[0].symbol.as_deref(), Some("value"));
    assert_eq!(change.declaration_changes[0].signature_changed, Some(false));
}

#[test]
fn rename_move_candidates_remain_uncertain_and_duplicate_bodies_are_not_matched() {
    let f = Fixture::new(&[("old.rs", "fn original(x: u32) -> u32 { x + 1 }\n")]);
    fs::remove_file(f.root.join("old.rs")).unwrap();
    fs::write(
        f.root.join("new.rs"),
        "fn renamed(x: u32) -> u32 { x + 1 }\n",
    )
    .unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    for change in &report.changes {
        let delta = change
            .declaration_changes
            .iter()
            .find(|d| d.status == "possible_rename_or_move")
            .unwrap();
        assert!(delta.uncertain);
        assert_eq!(delta.before.as_ref().unwrap().path, "old.rs");
        assert_eq!(delta.after.as_ref().unwrap().path, "new.rs");
    }
    let f = Fixture::new(&[
        ("a.rs", "fn first() -> u32 { 42 }\n"),
        ("b.rs", "fn second() -> u32 { 42 }\n"),
    ]);
    fs::remove_file(f.root.join("a.rs")).unwrap();
    fs::remove_file(f.root.join("b.rs")).unwrap();
    fs::write(f.root.join("new.rs"), "fn third() -> u32 { 42 }\n").unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    assert!(
        !report
            .changes
            .iter()
            .flat_map(|c| &c.declaration_changes)
            .any(|d| d.status == "possible_rename_or_move")
    );
}

#[test]
fn baseline_syntax_supports_mixed_languages_and_reports_unparseable_old_files() {
    let f = Fixture::new(&[
        ("client.py", "def run():\n    return 1\n"),
        (
            "module.ts",
            "export function value(): number { return 1; }\n",
        ),
        ("main.go", "package main\nfunc value() int { return 1 }\n"),
    ]);
    fs::write(f.root.join("client.py"), "def run():\n    return 2\n").unwrap();
    fs::write(
        f.root.join("module.ts"),
        "export function value(): number { return 2; }\n",
    )
    .unwrap();
    fs::write(
        f.root.join("main.go"),
        "package main\nfunc value() int { return 2 }\n",
    )
    .unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    assert_eq!(report.changes_total, 3);
    for change in &report.changes {
        assert!(
            change
                .declaration_changes
                .iter()
                .any(|d| d.status == "modified"
                    && !d.uncertain
                    && d.signature_changed == Some(false))
        );
    }
    let f = Fixture::new(&[("lib.rs", "fn {\n")]);
    fs::write(f.root.join("lib.rs"), "fn valid() {}\n").unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    assert_eq!(report.baseline.parse_error_files, 1);
    assert_eq!(
        report.changes[0].declaration_changes[0].status,
        "before_parse_unavailable"
    );
}

#[test]
fn baseline_results_are_stable_and_never_register_or_switch_worktrees() {
    let f = Fixture::new(&[("lib.rs", "fn value() -> u32 { 1 }\n")]);
    fs::write(f.root.join("lib.rs"), "fn value() -> u32 { 2 }\n").unwrap();
    f.index();
    let worktrees = f.git(&["worktree", "list", "--porcelain"]);
    let head = f.git(&["rev-parse", "HEAD"]);
    let index = fs::read(f.root.join(".git/index")).unwrap();
    let db = fs::read(&f.db).unwrap();
    let first = impact(&f.db, &Default::default()).unwrap();
    let second = impact(&f.db, &Default::default()).unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    assert_eq!(worktrees, f.git(&["worktree", "list", "--porcelain"]));
    assert_eq!(head, f.git(&["rev-parse", "HEAD"]));
    assert_eq!(index, fs::read(f.root.join(".git/index")).unwrap());
    assert_eq!(db, fs::read(&f.db).unwrap());
}

#[cfg(unix)]
#[test]
fn baseline_reports_symlinks_without_following_them() {
    let f = Fixture::new(&[("lib.rs", "fn value() -> u32 { 1 }\n")]);
    let outside = f.root.parent().unwrap().join("outside.rs");
    fs::write(&outside, "fn outside() {}\n").unwrap();
    std::os::unix::fs::symlink(&outside, f.root.join("linked.rs")).unwrap();
    f.git(&["add", "linked.rs"]);
    f.git(&[
        "-c",
        "user.name=Sonar Test",
        "-c",
        "user.email=test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-qm",
        "link",
    ]);
    fs::write(f.root.join("lib.rs"), "fn value() -> u32 { 2 }\n").unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    assert_eq!(report.baseline.excluded_entries, vec!["linked.rs"]);
    assert_eq!(fs::read_to_string(outside).unwrap(), "fn outside() {}\n");
}

#[test]
fn unsupported_or_failed_baseline_compilation_preserves_syntax_results() {
    let f = Fixture::new(&[("client.py", "def run():\n    return 1\n")]);
    fs::write(f.root.join("client.py"), "def run():\n    return 2\n").unwrap();
    f.index();
    let report = impact(
        &f.db,
        &ImpactOptions {
            baseline_resolve: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.baseline.compilers[0].status, "unavailable");
    assert!(report.changes[0].baseline_declarations_total > 0);
    assert!(
        report.changes[0]
            .gaps
            .contains(&"baseline_compiler_consumers_unavailable".into())
    );
    let f = Fixture::new(&[("lib.rs", "fn run() -> u32 { 1 }\n")]);
    fs::write(f.root.join("lib.rs"), "fn run() -> u32 { 2 }\n").unwrap();
    f.index();
    let report = impact(
        &f.db,
        &ImpactOptions {
            baseline_resolve: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.baseline.compilers[0].status, "failed");
    assert!(report.changes[0].baseline_declarations_total > 0);
}

#[test]
fn overloads_with_unique_signatures_keep_structural_identity() {
    let before = "export function value(x: string): string;\nexport function value(x: number): number;\nexport function value(x: any) {\n return x;\n}\n";
    let after = "export function value(x: string): string;\nexport function value(x: number): number;\nexport function value(x: any) {\n return x ?? 0;\n}\n";
    let f = Fixture::new(&[("module.ts", before)]);
    fs::write(f.root.join("module.ts"), after).unwrap();
    f.index();
    let report = impact(&f.db, &Default::default()).unwrap();
    assert!(
        report.changes[0]
            .declaration_changes
            .iter()
            .any(|d| d.status == "modified" && !d.uncertain && d.signature_changed == Some(false)),
        "{:?}",
        report.changes[0].declaration_changes
    );
}
