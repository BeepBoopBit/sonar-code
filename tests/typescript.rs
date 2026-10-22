use serde_json::Value;
use sonar::{BriefOptions, InvestigationOptions, Investigator, NeighborOptions};
use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

const SOURCE: &str = r#"/** Cursor storage and validation. */
throw new Error('indexing must never execute this module')

/** Repository session catalogue. */
@register
export class Catalog<T> {
  #capacity = 10
  /** Reject an expired pagination cursor. */
  validate = async (cursor: T) => {
    if (isExpired(cursor)) {
      throw new Error('pagination cursor expired')
    }
    return cursor
  }
  /** Prepare a cursor label. */
  prepare(value: string): string
  prepare(value: number): string
  prepare(value: unknown): string {
    function nested() { return 'π' }
    return nested()
  }
  get capacity() { return this.#capacity }
}

export interface Page {
  cursor?: string
  fetch(cursor: string): Promise<Page>
}
export type Pagination = { expires: number }
export enum State { Pending, Ready = 'ready' }
export namespace API {
  export const LIMIT = 200
  export const load = <T>(value: T): T => value
}
export const wrapped = ((value: string) => value) satisfies Handler
export const named = function internalName(value: string) { return value }
export const Worker = class { run() { return 1 } }
export function* items() { yield 1 }
describe('cursor', () => {
  const localNoise = 1
  const testHelper = () => localNoise
})
export function annotated(value: { inlineMember: string }) { return value }
export const pairOne = () => 1, pairTwo = () => 2
export const typed: Handler = (value: string) => value
export declare const external: string
export const caf\u00e9: string = 'π'
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
        fs::write(root.join("catalog.ts"), SOURCE).unwrap();
        fs::write(root.join("view.tsx"), "/** Render a cursor badge. */\nexport const Badge = ({cursor}: {cursor: string}) => <span>{cursor}</span>\n").unwrap();
        fs::write(root.join("api.d.ts"), "declare namespace Client {\n  function fetch(cursor: string): Promise<string>\n  interface Options { timeout: number }\n}\n").unwrap();
        fs::write(root.join("lib.rs"), "fn shared_token() {}\n").unwrap();
        fs::write(root.join("client.py"), "def shared_token(): pass\n").unwrap();
        fs::write(
            root.join("shared.ts"),
            "export const shared_token = () => 1\n",
        )
        .unwrap();
        fs::write(
            root.join("guide.md"),
            "# Pagination\n[validate](catalog.ts#L11)\n",
        )
        .unwrap();
        let db = temp.path().join("index.sqlite");
        assert_eq!(sonar::index(&root, &db).unwrap().parse_error_files, 0);
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

#[test]
fn modern_declarations_are_qualified_and_locate_the_implementation() {
    let f = Fixture::new();
    let outline = sonar::outline(&f.db, "catalog.ts", 100, None).unwrap();
    for (symbol, kind) in [
        ("Catalog", "class"),
        ("Catalog.#capacity", "field"),
        ("Catalog.validate", "function"),
        ("Catalog.prepare", "function"),
        ("Catalog.prepare.nested", "function"),
        ("Catalog.capacity", "function"),
        ("Page", "interface"),
        ("Page.cursor", "field"),
        ("Page.fetch", "function"),
        ("Pagination", "type"),
        ("Pagination.expires", "field"),
        ("State", "enum"),
        ("State.Pending", "variant"),
        ("State.Ready", "variant"),
        ("API", "module"),
        ("API.LIMIT", "constant"),
        ("API.load", "function"),
        ("wrapped", "function"),
        ("named", "function"),
        ("Worker", "class"),
        ("Worker.run", "function"),
        ("items", "function"),
        ("testHelper", "function"),
        ("pairOne", "function"),
        ("pairTwo", "function"),
        ("typed", "function"),
        ("external", "constant"),
        ("caf\\u00e9", "constant"),
    ] {
        assert!(
            outline
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol) && l.kind == kind),
            "missing {symbol}: {outline:?}"
        );
    }
    assert!(
        outline
            .locations
            .iter()
            .all(|l| l.language.as_deref() == Some("typescript"))
    );
    assert!(
        !outline.locations.iter().any(|l| l
            .symbol
            .as_deref()
            .is_some_and(|s| s.contains("localNoise")
                || s.contains("inlineMember")
                || s.contains("internalName")))
    );
    let prepared: Vec<_> = outline
        .locations
        .iter()
        .filter(|l| l.symbol.as_deref() == Some("Catalog.prepare"))
        .collect();
    assert_eq!(
        prepared.len(),
        1,
        "overloads must lead to their implementation"
    );
    assert_eq!(prepared[0].definition, Some(18));
    assert_eq!(prepared[0].range, [16, 21]);
    assert_eq!(
        sonar::locate(&f.db, "catalog.ts", 16, None)
            .unwrap()
            .locations[0]
            .handle,
        prepared[0].handle
    );
    let class = outline
        .locations
        .iter()
        .find(|l| l.symbol.as_deref() == Some("Catalog"))
        .unwrap();
    assert_eq!(class.range[0], 5, "decorators are included");
    assert_eq!(prepared[0].parent.as_ref(), Some(&class.handle));
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let docs: String = conn.query_row("SELECT r.source FROM regions r JOIN locations l ON l.id=r.location_id WHERE l.symbol='Catalog.prepare' AND r.view='description' LIMIT 1", [], |r| r.get(0)).unwrap();
    assert!(docs.contains("Prepare a cursor label"));
}

#[test]
fn dialects_declaration_files_and_unicode_keep_exact_source_spans() {
    let f = Fixture::new();
    for extension in ["ts", "mts", "cts", "d.ts", "d.mts", "d.cts"] {
        assert_eq!(
            sonar::source_language(&format!("api.{extension}")),
            Some("typescript")
        );
    }
    assert_eq!(sonar::source_language("component.tsx"), Some("typescript"));
    assert_eq!(sonar::source_language("component.jsx"), None);
    let source = "// π\r\n/** Return a label. */\r\nexport const café = (label: string) => <string>label\r\n";
    fs::write(f.root.join("unicode.mts"), source).unwrap();
    fs::write(
        f.root.join("worker.cts"),
        "export const ready = () => true\n",
    )
    .unwrap();
    assert_eq!(sonar::index(&f.root, &f.db).unwrap().parse_error_files, 0);
    let located = sonar::locate(&f.db, "unicode.mts", 3, None).unwrap();
    assert_eq!(located.locations[0].symbol.as_deref(), Some("café"));
    assert_eq!(located.locations[0].definition, Some(3));
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    let (start, end): (i64, i64) = conn
        .query_row(
            "SELECT name_start,name_end FROM locations WHERE symbol='café'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(&source[start as usize..end as usize], "café");
    for (path, symbol) in [
        ("api.d.ts", "Client.fetch"),
        ("view.tsx", "Badge"),
        ("worker.cts", "ready"),
    ] {
        assert!(
            sonar::outline(&f.db, path, 100, None)
                .unwrap()
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol))
        );
    }
}

#[test]
fn discovery_focus_and_document_context_work_for_arrow_methods() {
    let f = Fixture::new();
    let result = Investigator::open(&f.db, None)
        .unwrap()
        .investigate(
            "pagination cursor expired",
            &InvestigationOptions {
                scope: Some("catalog.ts".into()),
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
    assert!(focus[0] <= 11 && focus[1] >= 11 && focus[1] - focus[0] < 16);
    let syntax = f.cli(&[
        "diagnose",
        "throw new Error",
        "--scope",
        "catalog.ts",
        "--kind",
        "function",
        "--focus",
        "syntax",
        "--focus-lines",
        "5",
    ]);
    let diagnostic = syntax["focus"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["symbol"] == "Catalog.validate")
        .unwrap();
    assert_eq!(diagnostic["reason"], "syntax_unit", "{syntax}");
    assert_eq!(diagnostic["syntax_kind"], "if_statement");
    let context = Investigator::open(&f.db, None)
        .unwrap()
        .investigate(
            "cursor expired",
            &InvestigationOptions {
                from: Some(location.location.handle.clone()),
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
    let state = result
        .index
        .languages
        .iter()
        .find(|l| l.language == "typescript")
        .unwrap();
    assert_eq!(state.semantic_freshness, "unavailable");
    assert!(state.capabilities.declarations && state.capabilities.syntax_focus);
    assert!(!state.capabilities.contextual_focus && state.capabilities.resolved_references);
}

#[test]
fn mixed_language_search_and_role_filters_share_a_read_only_index() {
    let f = Fixture::new();
    fs::write(
        f.root.join("catalog.test.ts"),
        "const helper = () => 'pagination cursor expired'\n",
    )
    .unwrap();
    fs::write(
        f.root.join("catalog.spec.tsx"),
        "const App = () => <span />\n",
    )
    .unwrap();
    fs::create_dir(f.root.join("__tests__")).unwrap();
    fs::write(
        f.root.join("__tests__/helper.ts"),
        "export const helper = () => 1\n",
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    for path in ["catalog.test.ts", "catalog.spec.tsx", "__tests__/helper.ts"] {
        assert!(
            sonar::outline(&f.db, path, 100, None)
                .unwrap()
                .locations
                .iter()
                .all(|l| l.source_role == "test")
        );
    }
    let before = fs::read(&f.db).unwrap();
    let brief = sonar::brief(&f.db, "shared_token", &BriefOptions::default()).unwrap();
    let languages: BTreeSet<_> = brief
        .files
        .iter()
        .filter(|f| f.locations.iter().any(|l| brief.starts.contains(&l.id)))
        .filter_map(|f| f.language.as_deref())
        .collect();
    assert_eq!(
        languages,
        ["rust", "python", "typescript"].into_iter().collect()
    );
    let filtered = f.cli(&[
        "brief",
        "pagination cursor expired",
        "--kind",
        "function",
        "--role",
        "implementation",
    ]);
    assert!(filtered.to_string().contains("Catalog.validate"));
    assert!(!filtered.to_string().contains("catalog.test.ts"));
    assert_eq!(fs::read(&f.db).unwrap(), before);
}

#[test]
fn ordinary_brief_finds_code_after_misleading_docs_and_keeps_multiple_helpers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let source = format!(
        "/**\n{}*/\nexport function process(input: string) {{\n  const label = input\n  if (input === 'expired cursor') {{\n    throw new Error('pagination cursor rejected')\n  }}\n  return label\n}}\nexport const validatePaginationCursor = (cursor: string) => process(cursor)\nexport const rejectPaginationCursor = (cursor: string) => validatePaginationCursor(cursor)\n",
        " * pagination cursor rejected\n".repeat(30)
    );
    fs::write(root.join("cursor.ts"), &source).unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let brief = sonar::brief(&db, "pagination cursor rejected", &Default::default()).unwrap();
    let functions: Vec<_> = brief
        .files
        .iter()
        .flat_map(|f| &f.locations)
        .filter(|l| brief.starts.contains(&l.id) && l.kind == "function")
        .collect();
    assert!(
        functions.len() >= 2,
        "relevant helpers in one file must remain visible: {brief:?}"
    );
    let owner = functions
        .iter()
        .find(|l| l.symbol.as_deref() == Some("process"))
        .unwrap();
    let range = owner.focus.unwrap();
    let guard = source
        .lines()
        .position(|l| l.contains("if (input"))
        .unwrap()
        + 1;
    assert!(range[0] <= guard && guard <= range[1], "{brief:?}");
    assert!(range[0] > 32 && range[1] - range[0] < 16);
}

#[test]
fn older_indexes_detect_typescript_until_reindex_and_invalidate_old_handles() {
    let f = Fixture::new();
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    conn.execute(
        "DELETE FROM files WHERE path LIKE '%.ts' OR path LIKE '%.tsx'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE metadata SET value='rust,python' WHERE key='languages'",
        [],
    )
    .unwrap();
    conn.execute(
        "UPDATE metadata SET value='6-adapters' WHERE key='extraction_version'",
        [],
    )
    .unwrap();
    drop(conn);
    let before = fs::read(&f.db).unwrap();
    let brief = sonar::brief(&f.db, "Catalog", &BriefOptions::default()).unwrap();
    let state = brief
        .index
        .languages
        .iter()
        .find(|l| l.language == "typescript")
        .unwrap();
    assert_eq!(
        (state.files, state.detected_files, state.unindexed_files),
        (0, 4, 4)
    );
    assert_eq!(state.syntax_freshness, "unindexed");
    assert!(brief.notices.iter().any(|n| n.code == "unindexed_language"));
    assert_eq!(fs::read(&f.db).unwrap(), before);
    sonar::index(&f.root, &f.db).unwrap();
    let handle = sonar::locate(&f.db, "catalog.ts", 11, None)
        .unwrap()
        .locations[0]
        .handle
        .clone();
    fs::write(f.root.join("catalog.ts"), format!("{SOURCE}\n// changed\n")).unwrap();
    let brief = sonar::brief(&f.db, "Catalog", &BriefOptions::default()).unwrap();
    assert_eq!(
        brief
            .index
            .languages
            .iter()
            .find(|l| l.language == "typescript")
            .unwrap()
            .syntax_freshness,
        "stale"
    );
    assert_eq!(
        brief
            .index
            .languages
            .iter()
            .find(|l| l.language == "rust")
            .unwrap()
            .syntax_freshness,
        "current"
    );
    assert!(sonar::neighbors(&f.db, &handle, &NeighborOptions::default()).is_err());
    sonar::index(&f.root, &f.db).unwrap();
    assert!(sonar::neighbors(&f.db, &handle, &NeighborOptions::default()).is_err());
    assert_eq!(sonar::index(&f.root, &f.db).unwrap().changed, 0);
}

#[test]
fn ignored_dependencies_and_malformed_sources_obey_the_common_contract() {
    let f = Fixture::new();
    fs::create_dir(f.root.join("node_modules")).unwrap();
    fs::write(
        f.root.join("node_modules/noise.d.ts"),
        "export declare function noise(): void\n",
    )
    .unwrap();
    fs::write(f.root.join(".gitignore"), "generated/\n").unwrap();
    fs::create_dir(f.root.join("generated")).unwrap();
    fs::write(
        f.root.join("generated/noise.ts"),
        "export const noise = () => 1\n",
    )
    .unwrap();
    fs::write(f.root.join("broken.ts"), "export function broken( {\n").unwrap();
    let report = sonar::index(&f.root, &f.db).unwrap();
    assert_eq!(report.files, 8);
    assert_eq!(report.parse_error_files, 1);
    assert!(
        sonar::brief(&f.db, "Catalog", &BriefOptions::default())
            .unwrap()
            .notices
            .iter()
            .any(|n| n.code == "parse_errors")
    );
    fs::remove_file(f.root.join("lib.rs")).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let error = sonar::resolve(&f.db, std::path::Path::new("must-not-run"), None).unwrap_err();
    assert!(error.to_string().contains("no resolved-analysis provider"));
}

#[test]
fn semicolonless_generic_overloads_do_not_hide_following_declarations() {
    let f = Fixture::new();
    fs::write(
        f.root.join("factory.ts"),
        r#"
export interface Handler {
  <T>(value: T): T
  <T, U>(first: T, second: U): [T, U]
}
export type * from './api'
export class Factory {
  create = <const T>(value: T) => value
}
export const createFactory: Handler = <T>(value: T): T => value
"#,
    )
    .unwrap();
    assert_eq!(sonar::index(&f.root, &f.db).unwrap().parse_error_files, 0);
    let outline = sonar::outline(&f.db, "factory.ts", 100, None).unwrap();
    for name in ["Handler", "Factory", "Factory.create", "createFactory"] {
        assert!(
            outline
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(name)),
            "{outline:?}"
        );
    }
}
