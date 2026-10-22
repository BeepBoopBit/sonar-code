use sonar::{BriefOptions, RetrievalMode, SearchEngine};
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn exclusions_fill_the_lexical_pool_before_limits_and_preserve_sibling_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("reports")).unwrap();
    fs::create_dir_all(root.join("reports-old")).unwrap();
    fs::create_dir_all(root.join("odd%dir")).unwrap();
    fs::write(
        root.join("reports/noise.rs"),
        (0..600)
            .map(|i| format!("/// needle operation\nfn needle_operation_{i}() {{}}\n"))
            .collect::<String>(),
    )
    .unwrap();
    fs::write(
        root.join("reports-old/keep.rs"),
        "/// needle\nfn useful() {}\n",
    )
    .unwrap();
    fs::write(root.join("odd%dir/omit.rs"), "fn needle_operation() {}\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let base = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_kind(Some("function".into()));
    let options = BriefOptions::default();
    assert!(
        !base
            .diagnose("needle operation detail", &options)
            .unwrap()
            .candidates
            .iter()
            .any(|c| c.path == "reports-old/keep.rs")
    );
    let filtered = base.with_exclusions(vec![
        "./reports/".into(),
        "reports".into(),
        "odd%dir".into(),
    ]);
    let d = filtered
        .diagnose("needle operation detail", &options)
        .unwrap();
    assert_eq!(d.exclusions, ["odd%dir", "reports"]);
    assert_eq!(d.candidates.len(), 1);
    assert_eq!(d.candidates[0].symbol.as_deref(), Some("useful"));
    let b = filtered.brief("needle operation detail", &options).unwrap();
    assert_eq!(b.index.syntax_freshness, "current");
    assert!(b.files.iter().all(|f| f.path == "reports-old/keep.rs"));
    assert!(b.notices.iter().any(|n| n.code == "path_exclusions"));
    let scoped = BriefOptions {
        scope: Some("reports".into()),
        ..Default::default()
    };
    assert!(
        filtered
            .brief("needle operation", &scoped)
            .unwrap()
            .starts
            .is_empty()
    );
    assert!(
        filtered
            .brief("reports/noise.rs", &options)
            .unwrap()
            .files
            .iter()
            .all(|f| f.path != "reports/noise.rs")
    );
    // Excluding a changed file must not hide whole-index staleness.
    fs::write(root.join("reports/noise.rs"), "fn changed() {}\n").unwrap();
    assert_eq!(
        filtered
            .brief("needle", &options)
            .unwrap()
            .index
            .syntax_freshness,
        "stale"
    );
}

#[test]
fn invalid_exclusions_are_rejected_and_unmatched_paths_are_reported() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn useful() {}\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    for path in ["", ".", "./", "../outside", "/absolute", "dir\\file"] {
        let e = SearchEngine::open(&db, RetrievalMode::Lexical, None)
            .unwrap()
            .with_exclusions(vec![path.into()]);
        assert!(
            e.brief("useful", &BriefOptions::default()).is_err(),
            "{path}"
        );
    }
    let e = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_exclusions(vec!["absent".into()]);
    let b = e.brief("useful", &BriefOptions::default()).unwrap();
    assert!(!b.starts.is_empty());
    assert!(
        b.notices
            .iter()
            .any(|n| n.code == "path_exclusions" && n.message.starts_with("0 indexed"))
    );
}

#[test]
fn cli_batch_and_diagnostics_share_repeatable_exclusions() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    for path in ["keep.rs", "one.rs", "two.rs"] {
        fs::write(root.join(path), "fn shared() {}\n").unwrap();
    }
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    for diagnostic in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sonar"));
        command
            .arg("--db")
            .arg(&db)
            .args(["batch", "--exclude", "one.rs", "--exclude", "two.rs"]);
        if diagnostic {
            command.arg("--diagnostics");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"\"shared\"\n\"shared\"\n")
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let rows: Vec<serde_json::Value> = String::from_utf8(result.stdout)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], rows[1]);
        let locations = rows[0][if diagnostic { "candidates" } else { "files" }]
            .as_array()
            .unwrap();
        assert!(!locations.is_empty());
        assert!(locations.iter().all(|l| l["path"] == "keep.rs"));
    }
}
