use sonar::{BriefOptions, InvestigationOptions, Investigator, SearchEngine, SourceRole};
use std::{fs, path::PathBuf, process::Command};

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
        fs::write(root.join("policy.py"), "def enforce_policy(value):\n    \"\"\"Reject expired cursor values.\"\"\"\n    return value\n\ndef test_local_policy():\n    assert enforce_policy('expired cursor')\n").unwrap();
        fs::write(
            root.join("guide.md"),
            "# Expired cursor\nReject expired cursor values.\n",
        )
        .unwrap();
        fs::write(
            root.join("lib.rs"),
            "#[cfg(test)]\n#[path = \"support.rs\"]\nmod support;\nfn enforce_policy() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("support.rs"),
            "fn test_support() { /* expired cursor */ }\n",
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
    fn engine(&self, role: SourceRole) -> SearchEngine {
        SearchEngine::open_navigation(&self.db, None)
            .unwrap()
            .with_source_role(Some(role))
    }
}

#[test]
fn roles_apply_before_lexical_limits_and_investigation_retains_primary() {
    let f = Fixture::new();
    let noise = (0..620)
        .map(|i| {
            format!("def test_cursor_{i}():\n    '''Reject expired cursor values.'''\n    pass\n")
        })
        .collect::<String>();
    fs::write(f.root.join("test_many.py"), noise).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let query = "reject expired cursor values";
    let options = BriefOptions {
        related: 0,
        ..Default::default()
    };
    let brief = f
        .engine(SourceRole::Implementation)
        .with_kind(Some("function".into()))
        .brief(query, &options)
        .unwrap();
    assert!(!brief.starts.is_empty());
    assert!(
        brief
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .all(|l| l.source_role.as_deref() == Some("implementation") && l.kind == "function")
    );
    assert!(
        brief
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .any(|l| l.symbol.as_deref() == Some("enforce_policy"))
    );
    let unfiltered = SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .brief(query, &options)
        .unwrap();
    let investigation = Investigator::open(&f.db, None)
        .unwrap()
        .investigate(query, &InvestigationOptions::default())
        .unwrap();
    let functions = investigation
        .panels
        .iter()
        .find(|p| p.purpose == "functions")
        .unwrap();
    assert!(!functions.leads.is_empty());
    for lead in &functions.leads {
        let location = &investigation
            .locations
            .iter()
            .find(|l| l.id == lead.location)
            .unwrap()
            .location;
        assert_eq!(location.source_role, "implementation");
        assert_eq!(location.kind, "function");
    }
    let old_handles: Vec<_> = unfiltered
        .starts
        .iter()
        .map(|id| {
            unfiltered
                .files
                .iter()
                .flat_map(|f| &f.locations)
                .find(|l| l.id == *id)
                .unwrap()
                .handle
                .as_ref()
                .unwrap()
        })
        .collect();
    let new_handles: Vec<_> = investigation.panels[0]
        .leads
        .iter()
        .map(|lead| {
            &investigation
                .locations
                .iter()
                .find(|l| l.id == lead.location)
                .unwrap()
                .location
                .handle
        })
        .collect();
    assert_eq!(old_handles, new_handles);
}

#[test]
fn role_filters_honor_current_module_proofs_scope_exclusions_and_exact_identity() {
    let f = Fixture::new();
    let options = BriefOptions {
        related: 0,
        ..Default::default()
    };
    let implementation = f
        .engine(SourceRole::Implementation)
        .brief("test_support", &options)
        .unwrap();
    assert!(
        !implementation
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .any(|l| l.symbol.as_deref() == Some("test_support"))
    );
    let tests = f
        .engine(SourceRole::Test)
        .brief("test_support", &options)
        .unwrap();
    assert!(
        tests
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .any(|l| l.symbol.as_deref() == Some("test_support")
                && l.source_role.as_deref() == Some("test"))
    );
    let excluded = f
        .engine(SourceRole::Test)
        .with_exclusions(vec!["support.rs".into()])
        .brief("test_support", &options)
        .unwrap();
    assert!(!excluded.files.iter().any(|f| f.path == "support.rs"));
    let scoped = f
        .engine(SourceRole::Implementation)
        .with_kind(Some("function".into()))
        .brief(
            "enforce_policy",
            &BriefOptions {
                scope: Some("policy.py".into()),
                related: 0,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(scoped.files.iter().all(|f| f.path == "policy.py"));
    assert_eq!(scoped.starts.len(), 1);
    let docs = f
        .engine(SourceRole::Documentation)
        .brief("expired cursor", &options)
        .unwrap();
    assert!(!docs.starts.is_empty());
    assert!(
        docs.files
            .iter()
            .flat_map(|f| &f.locations)
            .all(|l| l.source_role.as_deref() == Some("documentation"))
    );
    // Stale module proofs cannot determine a role after source changes.
    fs::write(f.root.join("lib.rs"), "mod support;\n").unwrap();
    let stale = f
        .engine(SourceRole::Implementation)
        .brief("test_support", &options)
        .unwrap();
    assert!(
        stale
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .any(|l| l.symbol.as_deref() == Some("test_support")
                && l.source_role.as_deref() == Some("implementation"))
    );
}

#[test]
fn cli_role_filter_and_diagnostics_match_the_api_and_keep_database_read_only() {
    let f = Fixture::new();
    let before = fs::read(&f.db).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .arg("--db")
        .arg(&f.db)
        .args([
            "brief",
            "expired cursor",
            "--role",
            "implementation",
            "--kind",
            "function",
            "--related",
            "0",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = SearchEngine::open(&f.db, sonar::RetrievalMode::Lexical, None)
        .unwrap()
        .with_source_role(Some(SourceRole::Implementation))
        .with_kind(Some("function".into()))
        .brief(
            "expired cursor",
            &BriefOptions {
                related: 0,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    let diagnostic = f
        .engine(SourceRole::Implementation)
        .diagnose("expired cursor", &BriefOptions::default())
        .unwrap();
    assert_eq!(diagnostic.source_role, Some(SourceRole::Implementation));
    let invalid = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .arg("--db")
        .arg(&f.db)
        .args(["brief", "cursor", "--role", "guessed_owner"])
        .output()
        .unwrap();
    assert!(!invalid.status.success());
    assert_eq!(before, fs::read(&f.db).unwrap());
}
