use serde_json::Value;
use sonar::{BriefOptions, InvestigationOptions, Investigator, NeighborOptions, SearchEngine};
use std::{fs, path::PathBuf, process::Command};

const PYTHON: &str = r#""""Cursor storage and validation."""
raise RuntimeError("indexing must never execute this module")

@dataclass
class Catalog:
    """Repository session catalogue."""
    capacity: int = 10

    @staticmethod
    async def validate(cursor):
        """Reject an expired pagination cursor."""
        if cursor.expired:
            raise ValueError("pagination cursor expired")
        return cursor

    def prepare(self):
        def nested():
            return "π"
        return nested()

MAX_RESULTS = 200
"#;

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
        fs::write(root.join("catalog.py"), PYTHON).unwrap();
        fs::write(root.join("lib.rs"), "fn rust_owner() {}\n").unwrap();
        fs::write(
            root.join("api.pyi"),
            "class Session:\n    def wait(self) -> str: ...\n",
        )
        .unwrap();
        fs::write(
            root.join("guide.md"),
            "# Pagination\n[validate](catalog.py#L10)\n",
        )
        .unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(
            root.join("tests/test_catalog.py"),
            "def helper():\n    return 1\n\ndef test_expiry():\n    assert helper() == 1\n",
        )
        .unwrap();
        let db = temp.path().join("index.sqlite");
        sonar::index(&root, &db).unwrap();
        sonar::index_navigation(&db).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
    fn cli(&self, args: &[&str]) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_sonar"))
            .arg("--db")
            .arg(&self.db)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn python_decorators_nesting_fields_stubs_and_test_roles_are_source_bound() {
    let f = Fixture::new();
    let outline = sonar::outline(&f.db, "catalog.py", 100, None).unwrap();
    let names: Vec<_> = outline
        .locations
        .iter()
        .filter_map(|l| l.symbol.as_deref())
        .collect();
    for expected in [
        "Catalog",
        "Catalog.capacity",
        "Catalog.validate",
        "Catalog.prepare",
        "Catalog.prepare.nested",
        "MAX_RESULTS",
    ] {
        assert!(names.contains(&expected), "{names:?}");
    }
    assert_eq!(
        names.len(),
        6,
        "decorated definitions must not be duplicated"
    );
    let method = outline
        .locations
        .iter()
        .find(|l| l.symbol.as_deref() == Some("Catalog.validate"))
        .unwrap();
    assert_eq!(method.kind, "function");
    assert_eq!(method.definition, Some(10));
    assert_eq!(method.range, [9, 14]);
    let class = outline
        .locations
        .iter()
        .find(|l| l.kind == "class")
        .unwrap();
    assert_eq!(method.parent.as_ref(), Some(&class.handle));
    let located = sonar::locate(&f.db, "catalog.py", 13, None).unwrap();
    assert_eq!(located.locations[0].handle, method.handle);
    let nested = sonar::locate(&f.db, "catalog.py", 18, None).unwrap();
    assert_eq!(
        nested.locations[0].symbol.as_deref(),
        Some("Catalog.prepare.nested")
    );
    assert!(
        sonar::outline(&f.db, "api.pyi", 100, None)
            .unwrap()
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Session.wait"))
    );
    let tests = sonar::outline(&f.db, "tests/test_catalog.py", 100, None).unwrap();
    assert!(tests.locations.iter().all(|l| l.source_role == "test"));
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let docs: String = conn.query_row("SELECT r.source FROM regions r JOIN locations l ON l.id=r.location_id WHERE l.symbol='Catalog.validate' AND view='description' LIMIT 1", [], |r|r.get(0)).unwrap();
    assert!(docs.contains("Reject an expired pagination cursor"));
}

#[test]
fn python_discovery_focus_and_context_use_the_existing_contract() {
    let f = Fixture::new();
    let engine = Investigator::open(&f.db, None).unwrap();
    let result = engine
        .investigate(
            "expired pagination cursor",
            &InvestigationOptions {
                scope: Some("catalog.py".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let location = result
        .locations
        .iter()
        .find(|l| l.location.symbol.as_deref() == Some("Catalog.validate"))
        .unwrap();
    let focus = location.focus.unwrap();
    assert!(focus[0] <= 13 && focus[1] >= 13 && focus[1] - focus[0] < 16);
    assert!(result.index.languages.iter().any(|l| l.language == "python"
        && l.semantic_freshness == "unsupported"
        && l.capabilities.syntax_focus));
    let syntax = f.cli(&[
        "brief",
        "pagination cursor expired",
        "--scope",
        "catalog.py",
        "--kind",
        "function",
        "--focus",
        "syntax",
        "--focus-lines",
        "5",
    ]);
    let text = syntax.to_string();
    assert!(text.contains("Catalog.validate"));
    let context = engine
        .investigate(
            "expired pagination cursor",
            &InvestigationOptions {
                from: Some(location.location.handle.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        context
            .notices
            .iter()
            .any(|n| n.code == "language_analysis_unavailable")
    );
    assert!(context.links.iter().any(|l| l.kind == "documents"));
    assert!(
        !context
            .links
            .iter()
            .any(|l| l.kind == "calls" || l.kind == "references")
    );
    let neighbors = sonar::neighbors(
        &f.db,
        &location.location.handle,
        &NeighborOptions::default(),
    )
    .unwrap();
    assert!(neighbors.links.iter().any(|l| l.kind == "contains"));
    assert!(
        neighbors
            .notices
            .iter()
            .any(|n| n.code == "language_analysis_unavailable")
    );
    let follow = SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .follow(
            &location.location.handle,
            "cursor",
            &sonar::FollowOptions::default(),
        )
        .unwrap();
    assert!(follow.matches.is_empty());
    assert!(
        follow
            .notices
            .iter()
            .any(|n| n.code == "language_analysis_unavailable")
    );
    let uses = sonar::field_uses(
        &f.db,
        &location.location.handle,
        &sonar::FieldUseOptions::default(),
    )
    .unwrap();
    assert!(
        uses.notices
            .iter()
            .any(|n| n.code == "language_analysis_unavailable")
    );
}

#[test]
fn per_language_syntax_freshness_and_reindexing_invalidate_old_handles() {
    let f = Fixture::new();
    let handle = sonar::locate(&f.db, "catalog.py", 13, None)
        .unwrap()
        .locations
        .remove(0)
        .handle;
    fs::write(f.root.join("catalog.py"), format!("{PYTHON}\n# updated\n")).unwrap();
    let brief = sonar::brief(&f.db, "rust_owner", &BriefOptions::default()).unwrap();
    let rust = brief
        .index
        .languages
        .iter()
        .find(|l| l.language == "rust")
        .unwrap();
    let python = brief
        .index
        .languages
        .iter()
        .find(|l| l.language == "python")
        .unwrap();
    assert_eq!(rust.syntax_freshness, "current");
    assert_eq!(python.syntax_freshness, "stale");
    assert_eq!(python.semantic_freshness, "unsupported");
    assert!(sonar::neighbors(&f.db, &handle, &NeighborOptions::default()).is_err());
    sonar::index(&f.root, &f.db).unwrap();
    assert!(sonar::neighbors(&f.db, &handle, &NeighborOptions::default()).is_err());
    assert!(sonar::locate(&f.db, "catalog.py", 13, None).is_ok());
}

#[test]
fn legacy_indexes_keep_their_boundary_until_explicit_upgrade() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn stable() {}\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DELETE FROM metadata WHERE key='languages'", [])
        .unwrap();
    conn.execute(
        "UPDATE metadata SET value='4' WHERE key='extraction_version'",
        [],
    )
    .unwrap();
    let handle = sonar::locate(&db, "lib.rs", 1, None)
        .unwrap()
        .locations
        .remove(0)
        .handle;
    fs::write(root.join("new.py"), "def python_owner():\n    pass\n").unwrap();
    assert!(sonar::neighbors(&db, &handle, &NeighborOptions::default()).is_ok());
    let old = sonar::brief(&db, "python_owner", &BriefOptions::default()).unwrap();
    assert_eq!(old.index.languages.len(), 2);
    let python = old
        .index
        .languages
        .iter()
        .find(|l| l.language == "python")
        .unwrap();
    assert_eq!(
        (python.files, python.detected_files, python.unindexed_files),
        (0, 1, 1)
    );
    assert_eq!(python.syntax_freshness, "unindexed");
    assert_eq!(old.index.syntax_freshness, "current");
    assert!(old.notices.iter().any(|n| n.code == "unindexed_language"));
    assert!(!old.files.iter().any(|f| f.path == "new.py"));
    assert_eq!(
        sonar::brief(&db, "stable", &BriefOptions::default())
            .unwrap()
            .files[0]
            .language
            .as_deref(),
        Some("rust")
    );
    sonar::index(&root, &db).unwrap();
    assert!(sonar::neighbors(&db, &handle, &NeighborOptions::default()).is_err());
    assert_eq!(
        sonar::locate(&db, "new.py", 1, None).unwrap().locations[0]
            .symbol
            .as_deref(),
        Some("python_owner")
    );
}

#[test]
fn unscoped_briefs_search_both_languages_and_label_navigation_without_reindexing() {
    let f = Fixture::new();
    fs::write(f.root.join("same.rs"), "fn shared_navigation_token() {}\n").unwrap();
    fs::write(
        f.root.join("same.py"),
        "def shared_navigation_token():\n    pass\n",
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let before = fs::read(&f.db).unwrap();
    for brief in [
        sonar::brief(&f.db, "shared_navigation_token", &BriefOptions::default()).unwrap(),
        SearchEngine::open_navigation(&f.db, None)
            .unwrap()
            .brief("shared_navigation_token", &BriefOptions::default())
            .unwrap(),
    ] {
        let languages: std::collections::BTreeSet<_> = brief
            .files
            .iter()
            .filter(|f| f.locations.iter().any(|l| brief.starts.contains(&l.id)))
            .filter_map(|f| f.language.as_deref())
            .collect();
        assert_eq!(languages, ["python", "rust"].into_iter().collect());
        assert!(
            brief
                .index
                .languages
                .iter()
                .all(|l| l.detected_files == l.files && l.unindexed_files == 0)
        );
    }
    let result = Investigator::open(&f.db, None)
        .unwrap()
        .investigate("shared_navigation_token", &InvestigationOptions::default())
        .unwrap();
    assert!(
        result
            .locations
            .iter()
            .any(|l| l.location.language.as_deref() == Some("python"))
    );
    assert!(
        result
            .locations
            .iter()
            .any(|l| l.location.language.as_deref() == Some("rust"))
    );
    let outline = sonar::outline(&f.db, "same.py", 100, None).unwrap();
    assert!(
        outline
            .locations
            .iter()
            .all(|l| l.language.as_deref() == Some("python"))
    );
    assert_eq!(
        fs::read(&f.db).unwrap(),
        before,
        "briefs and navigation must stay read-only"
    );
}

#[test]
fn language_detection_reports_new_sources_and_respects_legacy_ignore_rules() {
    let f = Fixture::new();
    fs::write(f.root.join("added.py"), "def added(): pass\n").unwrap();
    let added = sonar::brief(&f.db, "Catalog", &BriefOptions::default()).unwrap();
    let python = added
        .index
        .languages
        .iter()
        .find(|l| l.language == "python")
        .unwrap();
    assert_eq!(
        (python.files, python.detected_files, python.unindexed_files),
        (3, 4, 1)
    );
    assert_eq!(python.syntax_freshness, "stale");
    assert!(added.notices.iter().any(|n| n.code == "unindexed_sources"));

    // Simulate a Rust-only policy and detection of a new language. Excluded paths
    // may even contain invalid UTF-8; detection reads names only, not their bytes.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn stable() {}\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("UPDATE metadata SET value='rust' WHERE key='languages'", [])
        .unwrap();
    fs::write(root.join(".ignore"), "ignored.py\n").unwrap();
    fs::write(root.join("ignored.py"), "def ignored(): pass\n").unwrap();
    fs::write(root.join("visible.py"), [0xff]).unwrap();
    for path in ["venv", "site-packages", "__pycache__", ".hidden"] {
        fs::create_dir(root.join(path)).unwrap();
        fs::write(root.join(path).join("hidden.py"), "def hidden(): pass\n").unwrap();
    }
    let before = fs::read(&db).unwrap();
    let result = sonar::brief(&db, "stable", &BriefOptions::default()).unwrap();
    let python = result
        .index
        .languages
        .iter()
        .find(|l| l.language == "python")
        .unwrap();
    assert_eq!(
        (python.files, python.detected_files, python.unindexed_files),
        (0, 1, 1)
    );
    assert_eq!(fs::read(&db).unwrap(), before);
}

#[test]
fn python_environment_directories_and_other_languages_are_excluded() {
    let f = Fixture::new();
    for directory in ["venv", "__pycache__", "site-packages"] {
        fs::create_dir(f.root.join(directory)).unwrap();
        fs::write(
            f.root.join(directory).join("noise.py"),
            "def noisy(): pass\n",
        )
        .unwrap();
    }
    fs::write(f.root.join("unsupported.js"), "function ignored() {}\n").unwrap();
    let report = sonar::index(&f.root, &f.db).unwrap();
    assert_eq!(report.files, 5);
    assert_eq!(
        sonar::supported_languages(),
        vec!["rust", "python", "typescript"]
    );
    assert_eq!(sonar::source_language("pkg/model.pyi"), Some("python"));
}

#[test]
fn python_unicode_names_and_crlf_preserve_source_offsets() {
    let f = Fixture::new();
    let source = "# π before the declaration\r\n@register\r\nasync def café():\r\n    \"\"\"Return the label.\"\"\"\r\n    return 'π'\r\n";
    fs::write(f.root.join("unicode.py"), source).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let located = sonar::locate(&f.db, "unicode.py", 5, None).unwrap();
    let location = &located.locations[0];
    assert_eq!(location.symbol.as_deref(), Some("café"));
    assert_eq!(location.definition, Some(3));
    assert_eq!(location.range, [2, 5]);
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let (start, end): (i64, i64) = conn
        .query_row(
            "SELECT name_start,name_end FROM locations WHERE symbol='café'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(&source[start as usize..end as usize], "café");
    assert!(sonar::neighbors(&f.db, &location.handle, &NeighborOptions::default()).is_ok());
}

#[test]
fn malformed_python_is_reported_and_resolution_does_not_execute_it() {
    let f = Fixture::new();
    fs::write(f.root.join("broken.py"), "def broken(\n").unwrap();
    assert_eq!(sonar::index(&f.root, &f.db).unwrap().parse_error_files, 1);
    let brief = sonar::brief(&f.db, "Catalog", &BriefOptions::default()).unwrap();
    assert!(brief.notices.iter().any(|n| n.code == "parse_errors"));
    fs::remove_file(f.root.join("lib.rs")).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let error = sonar::resolve(&f.db, std::path::Path::new("must-not-run"), None).unwrap_err();
    assert!(error.to_string().contains("no resolved-analysis provider"));
}
