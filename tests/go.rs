use serde_json::Value;
use sonar::{
    BriefOptions, FieldUseOptions, FollowOptions, InvestigationOptions, Investigator,
    NeighborOptions, SearchEngine, SourceRole,
};
use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

const SOURCE: &str = r#"//go:build linux

// Package vault stores temporary shares.
package vault

import (
    "context"
    "errors"
    "sync"
)

// Store holds the one-time retrieval policy.
type Store struct {
    // Once permits exactly one retrieval.
    Once bool `json:"once"`
    Name, Title string
    *sync.Mutex
    Index[int]
    _ string
    Options struct { hidden int }
}

// Paste is the compatibility name for a store.
type Paste = Store

// Grouped declarations retain separate identities.
type (
    Counter[T any] struct { value T }
    // Service describes share retrieval.
    Service interface {
        // Claim retrieves a temporary share.
        Claim(context.Context) error
        other.Service
    }
    Number interface { ~int | ~float64 }
)

// Quota is a named scalar.
type Quota int64

// Lifetimes bound temporary shares.
const (
    // MaxTTL bounds expiry.
    MaxTTL = 86399
    Idle, Busy = 0, 1
    _ = 2
)

var Enabled = true
var (
    Count, Limit int
    _ = 0
)

// NewStore configures a temporary share store.
func NewStore() *Store { return &Store{Once: true} }

// Claim consumes a share only once.
func (s *Store) Claim(ctx context.Context) error {
    type localNoise struct { hidden int }
    const localLimit = 1
    if s.Once {
        return errors.New("one-time share already claimed")
    }
    go cleanup()
    defer persist()
    select {
    case <-ctx.Done():
        return ctx.Err()
    default:
        return nil
    }
}

func (*Store) Reset() {}
func (c Counter[T]) Get() T { return c.value }

func annotated(value struct { hidden int }) {}
func TestNameOutsideTestFile() {}
func init() { panic("indexing must never execute project code") }
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
        fs::write(root.join("store.go"), SOURCE).unwrap();
        fs::write(root.join("mirror.go"), "package vault\ntype Mirror struct{}\nfunc (*Mirror) Claim() {}\nfunc shared_token() {}\nfunc ( /* receiver comment */ s * /* pointer comment */ Store) Elsewhere() {}\n").unwrap();
        fs::write(root.join("store_test.go"), "package vault_test\nfunc helper() {}\nfunc TestClaim() {}\nfunc BenchmarkClaim() {}\nfunc FuzzClaim() {}\nfunc ExampleClaim() {}\ntype fixture struct { value int }\n").unwrap();
        fs::write(root.join("lib.rs"), "fn shared_token() {}\n").unwrap();
        fs::write(root.join("client.py"), "def shared_token(): pass\n").unwrap();
        fs::write(
            root.join("client.ts"),
            "export function shared_token() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("guide.md"),
            format!(
                "# One-time retrieval\n[Claim](store.go#L{})\n",
                line("func (s *Store) Claim")
            ),
        )
        .unwrap();
        let db = temp.path().join("index.sqlite");
        let report = sonar::index(&root, &db).unwrap();
        assert_eq!(report.parse_error_files, 0);
        sonar::index_navigation(&db).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }

    fn cli(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_sonar"))
            .arg("--db")
            .arg(&self.db)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn line(text: &str) -> usize {
    SOURCE.lines().position(|l| l.contains(text)).unwrap() + 1
}

#[test]
fn go_declarations_preserve_receivers_members_generics_and_grouped_names() {
    let f = Fixture::new();
    assert_eq!(
        sonar::source_language("internal/vault/store.go"),
        Some("go")
    );
    let outline = sonar::outline(&f.db, "store.go", 100, None).unwrap();
    let expected = [
        ("Store", "struct"),
        ("Store.Once", "field"),
        ("Store.Name", "field"),
        ("Store.Title", "field"),
        ("Store.Mutex", "field"),
        ("Store.Index", "field"),
        ("Store.Options", "field"),
        ("Paste", "type"),
        ("Counter", "struct"),
        ("Counter.value", "field"),
        ("Service", "interface"),
        ("Service.Claim", "function"),
        ("Number", "interface"),
        ("Quota", "type"),
        ("MaxTTL", "constant"),
        ("Idle", "constant"),
        ("Busy", "constant"),
        ("Enabled", "variable"),
        ("Count", "variable"),
        ("Limit", "variable"),
        ("NewStore", "function"),
        ("Store.Claim", "function"),
        ("Store.Reset", "function"),
        ("Counter.Get", "function"),
        ("annotated", "function"),
        ("TestNameOutsideTestFile", "function"),
        ("init", "function"),
    ];
    let declarations: Vec<_> = outline
        .locations
        .iter()
        .filter(|l| l.symbol.is_some())
        .collect();
    assert_eq!(declarations.len(), expected.len(), "{outline:?}");
    for (symbol, kind) in expected {
        assert!(
            declarations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol) && l.kind == kind),
            "missing {symbol}: {outline:?}"
        );
    }
    assert!(
        outline
            .locations
            .iter()
            .all(|l| l.language.as_deref() == Some("go") && l.source_role == "implementation")
    );
    let store = declarations
        .iter()
        .find(|l| l.symbol.as_deref() == Some("Store"))
        .unwrap();
    let field = declarations
        .iter()
        .find(|l| l.symbol.as_deref() == Some("Store.Once"))
        .unwrap();
    assert_eq!(field.parent.as_ref(), Some(&store.handle));
    let claim = sonar::locate(
        &f.db,
        "store.go",
        line("one-time share already claimed"),
        None,
    )
    .unwrap();
    assert_eq!(claim.locations[0].symbol.as_deref(), Some("Store.Claim"));
    let file = outline.anchor.as_ref().unwrap();
    assert_eq!(claim.locations[0].parent.as_ref(), Some(&file.handle));
    let mirror = sonar::locate(&f.db, "mirror.go", 3, None).unwrap();
    assert_eq!(mirror.locations[0].symbol.as_deref(), Some("Mirror.Claim"));
    let elsewhere = sonar::locate(&f.db, "mirror.go", 5, None).unwrap();
    assert_eq!(
        elsewhere.locations[0].symbol.as_deref(),
        Some("Store.Elsewhere")
    );

    let conn = rusqlite::Connection::open(&f.db).unwrap();
    for (symbol, spelling) in [
        ("Store.Claim", "Claim"),
        ("Counter.Get", "Get"),
        ("Store.Mutex", "Mutex"),
        ("Busy", "Busy"),
    ] {
        let (start, end): (i64, i64) = conn
            .query_row(
                "SELECT name_start,name_end FROM locations WHERE symbol=?1",
                [symbol],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(&SOURCE[start as usize..end as usize], spelling);
    }
    let signature: String = conn.query_row("SELECT source FROM regions JOIN locations ON location_id=locations.id WHERE symbol='Store.Claim' AND view='identity'", [], |r| r.get(0)).unwrap();
    assert_eq!(
        signature,
        "func (s *Store) Claim(ctx context.Context) error"
    );
    let docs: String = conn.query_row("SELECT source FROM regions JOIN locations ON location_id=locations.id WHERE symbol='MaxTTL' AND view='description'", [], |r| r.get(0)).unwrap();
    assert!(docs.contains("MaxTTL bounds expiry"));
}

#[test]
fn go_discovery_focus_and_document_navigation_share_the_existing_contract() {
    let f = Fixture::new();
    let before = fs::read(&f.db).unwrap();
    let engine = Investigator::open(&f.db, None).unwrap();
    let found = engine
        .investigate(
            "one-time share already claimed",
            &InvestigationOptions {
                scope: Some("store.go".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let claim = found
        .locations
        .iter()
        .find(|l| l.location.symbol.as_deref() == Some("Store.Claim"))
        .unwrap();
    let focus = claim.focus.unwrap();
    let error_line = line("one-time share already claimed");
    assert!(
        focus[0] <= error_line && focus[1] >= error_line && focus[1] - focus[0] < 16,
        "{focus:?}"
    );
    let go = found
        .index
        .languages
        .iter()
        .find(|l| l.language == "go")
        .unwrap();
    assert_eq!((go.files, go.detected_files, go.unindexed_files), (3, 3, 0));
    assert_eq!(go.syntax_freshness, "current");
    assert_eq!(go.semantic_freshness, "unavailable");
    assert!(go.capabilities.declarations && go.capabilities.syntax_focus);
    assert!(go.capabilities.resolved_references && go.capabilities.field_uses);
    let context = engine
        .investigate(
            "claim",
            &InvestigationOptions {
                from: Some(claim.location.handle.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(context.links.iter().any(|l| l.kind == "documents"));
    assert!(
        !context
            .links
            .iter()
            .any(|l| l.kind == "calls" || l.kind == "references")
    );
    assert!(
        context
            .notices
            .iter()
            .any(|n| n.code == "graph_unavailable")
    );
    let navigation =
        sonar::neighbors(&f.db, &claim.location.handle, &NeighborOptions::default()).unwrap();
    assert!(navigation.links.iter().any(|l| l.kind == "contains"));
    assert!(
        navigation
            .notices
            .iter()
            .any(|n| n.code == "graph_unavailable")
    );
    let follow = SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .follow(&claim.location.handle, "persist", &FollowOptions::default())
        .unwrap();
    assert!(follow.matches.is_empty());
    assert!(follow.notices.iter().any(|n| n.code == "graph_unavailable"));
    let uses =
        sonar::field_uses(&f.db, &claim.location.handle, &FieldUseOptions::default()).unwrap();
    assert!(uses.uses.is_empty());
    assert!(uses.notices.iter().any(|n| n.code == "graph_unavailable"));
    let syntax = f.cli(&[
        "brief",
        "one-time share already claimed",
        "--scope",
        "store.go",
        "--kind",
        "function",
        "--focus",
        "syntax",
        "--focus-lines",
        "5",
    ]);
    assert!(syntax.to_string().contains("Store.Claim"));
    assert_eq!(
        fs::read(&f.db).unwrap(),
        before,
        "queries must remain read-only"
    );
}

#[test]
fn go_test_helpers_external_packages_and_mixed_languages_are_filtered_correctly() {
    let f = Fixture::new();
    let tests = sonar::outline(&f.db, "store_test.go", 100, None).unwrap();
    assert!(tests.locations.iter().all(|l| l.source_role == "test"));
    for role in [SourceRole::Implementation, SourceRole::Test] {
        let result = SearchEngine::open_navigation(&f.db, None)
            .unwrap()
            .with_source_role(Some(role))
            .with_kind(Some("function".into()))
            .brief("helper", &BriefOptions::default())
            .unwrap();
        let helpers = result
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .filter(|l| l.symbol.as_deref() == Some("helper"))
            .count();
        assert_eq!(helpers, usize::from(role == SourceRole::Test));
    }
    let result = sonar::brief(&f.db, "shared_token", &BriefOptions::default()).unwrap();
    let languages: BTreeSet<_> = result
        .files
        .iter()
        .filter(|f| f.locations.iter().any(|l| result.starts.contains(&l.id)))
        .filter_map(|f| f.language.as_deref())
        .collect();
    assert_eq!(
        languages,
        ["rust", "python", "typescript", "go"].into_iter().collect()
    );
    let hidden = f.cli(&["brief", "Store.Claim", "--exclude", "store.go"]);
    assert!(
        !hidden["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["path"] == "store.go")
    );
}

#[test]
fn go_unicode_crlf_and_documentation_boundaries_preserve_source_evidence() {
    let f = Fixture::new();
    let source = "// Package café documents storage.\r\npackage café\r\n\r\n// Café stores a label.\r\ntype Café struct { État string }\r\n\r\n/* Lire returns the label. */\r\nfunc (c *Café) Lire() string { return c.État }\r\n\r\n// Detached text is not documentation.\r\n\r\nvar Plain = 1 // Trailing text is not documentation.\r\nvar Next = 2\r\n";
    fs::write(f.root.join("unicode.go"), source).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let located = sonar::locate(&f.db, "unicode.go", 8, None).unwrap();
    assert_eq!(located.locations[0].symbol.as_deref(), Some("Café.Lire"));
    assert_eq!(located.locations[0].definition, Some(8));
    assert_eq!(located.locations[0].range, [8, 8]);
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let (start, end): (i64, i64) = conn
        .query_row(
            "SELECT name_start,name_end FROM locations WHERE symbol='Café.État'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(&source[start as usize..end as usize], "État");
    let descriptions: Vec<(Option<String>, String)> = conn.prepare("SELECT l.symbol,r.source FROM regions r JOIN locations l ON r.location_id=l.id JOIN files f ON l.file_id=f.id WHERE f.path='unicode.go' AND r.view='description'").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
    assert!(descriptions.iter().any(|(s,d)| s.as_deref() == Some("Café.Lire") && d.contains("Lire returns the label")));
    assert!(
        descriptions
            .iter()
            .any(|(s, d)| s.is_none() && d.contains("Package café"))
    );
    assert!(
        !descriptions
            .iter()
            .any(|(_, d)| d.contains("Detached") || d.contains("Trailing"))
    );
}

#[test]
fn go_syntax_focus_keeps_concurrency_statements_and_case_guards() {
    let f = Fixture::new();
    let source = "package vault\nfunc Dispatch(out chan<- string) {\n    go publish(\"background delivery\")\n    defer release(\"cleanup resources\")\n    select {\n    case out <- \"queued delivery\":\n        record(\"queue accepted\")\n    default:\n        record(\"queue full\")\n    }\n    switch state {\n    case \"expired\":\n        report(\"share expired\")\n    }\n}\n";
    fs::write(f.root.join("dispatch_windows.go"), source).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let engine = SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .with_kind(Some("function".into()))
        .with_focus(sonar::FocusOptions {
            policy: sonar::FocusPolicy::Syntax,
            max_lines: 8,
        });
    let options = BriefOptions {
        scope: Some("dispatch_windows.go".into()),
        ..Default::default()
    };
    for (query, first, last) in [
        ("background delivery", 3, 3),
        ("cleanup resources", 4, 4),
        ("queue accepted", 6, 7),
        ("share expired", 12, 13),
    ] {
        let diagnostic = engine.diagnose(query, &options).unwrap();
        let focus = diagnostic.focus.first().unwrap();
        assert_eq!(focus.reason, "syntax_unit", "{diagnostic:?}");
        let range = focus.range.unwrap();
        assert!(
            range[0] <= first && range[1] >= last && focus.lines <= 8,
            "{query}: {focus:?}"
        );
    }
}

#[test]
fn go_legacy_indexes_require_explicit_refresh_and_stale_handles_are_rejected() {
    // Use a fresh index containing only the legacy language boundary.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn stable() {}\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE metadata SET value='rust,python,typescript' WHERE key='languages'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE metadata SET value='7-typescript' WHERE key='extraction_version'",
        [],
    )
    .unwrap();
    fs::write(root.join("new.go"), "package vault\nfunc Added() {}\n").unwrap();
    let before = fs::read(&db).unwrap();
    let old = sonar::brief(&db, "stable", &BriefOptions::default()).unwrap();
    let go = old
        .index
        .languages
        .iter()
        .find(|l| l.language == "go")
        .unwrap();
    assert_eq!((go.files, go.detected_files, go.unindexed_files), (0, 1, 1));
    assert_eq!(go.syntax_freshness, "unindexed");
    assert_eq!(old.index.syntax_freshness, "current");
    assert!(old.notices.iter().any(|n| n.code == "unindexed_language"));
    assert_eq!(fs::read(&db).unwrap(), before);
    sonar::index(&root, &db).unwrap();
    let handle = sonar::locate(&db, "new.go", 2, None).unwrap().locations[0]
        .handle
        .clone();
    fs::write(
        root.join("new.go"),
        "package vault\nfunc Added() { println(1) }\n",
    )
    .unwrap();
    assert!(sonar::neighbors(&db, &handle, &NeighborOptions::default()).is_err());
    sonar::index(&root, &db).unwrap();
    assert!(sonar::neighbors(&db, &handle, &NeighborOptions::default()).is_err());
    assert_eq!(
        sonar::locate(&db, "new.go", 2, None).unwrap().locations[0]
            .symbol
            .as_deref(),
        Some("Added")
    );
}

#[test]
fn go_ignore_rules_parse_errors_and_ast_refresh_are_supported() {
    let f = Fixture::new();
    fs::write(f.root.join(".ignore"), "ignored.go\n").unwrap();
    fs::write(f.root.join("ignored.go"), [0xff]).unwrap();
    for directory in ["vendor", "node_modules", ".hidden"] {
        fs::create_dir(f.root.join(directory)).unwrap();
        fs::write(f.root.join(directory).join("noise.go"), [0xff]).unwrap();
    }
    fs::write(f.root.join("broken.go"), "package vault\nfunc broken(\n").unwrap();
    assert_eq!(sonar::index(&f.root, &f.db).unwrap().parse_error_files, 1);
    assert_eq!(
        sonar::index_with(&f.root, &f.db, sonar::Chunking::Ast)
            .unwrap()
            .parse_error_files,
        1
    );
    let result = sonar::brief(&f.db, "Store.Claim", &BriefOptions::default()).unwrap();
    assert!(result.notices.iter().any(|n| n.code == "parse_errors"));
    assert_eq!(
        result
            .index
            .languages
            .iter()
            .find(|l| l.language == "go")
            .unwrap()
            .files,
        4
    );
    assert!(
        sonar::outline(&f.db, "store.go", 100, None)
            .unwrap()
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Store.Claim"))
    );
    fs::remove_file(f.root.join("lib.rs")).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    assert!(
        sonar::resolve(&f.db, std::path::Path::new("must-not-run"), None)
            .unwrap_err()
            .to_string()
            .contains("no resolved-analysis provider")
    );
}
