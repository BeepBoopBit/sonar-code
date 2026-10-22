use sonar::{
    Direction, FieldAccess, FieldUseOptions, InvestigationOptions, Investigator, NeighborOptions,
    Relation, SearchEngine,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn compiler() -> Option<PathBuf> {
    let path = std::env::var_os("SONAR_TEST_TYPESCRIPT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".sonar/typescript/oracle/node_modules/typescript")
        });
    if path.exists() {
        Some(path)
    } else {
        eprintln!("Set SONAR_TEST_TYPESCRIPT to run compiler integration checks");
        None
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new(compiler: &Path) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("tsconfig.json"),r#"{"compilerOptions":{"strict":true,"target":"ESNext","module":"ESNext","moduleResolution":"Bundler","jsx":"preserve"}}"#).unwrap();
        fs::write(
            root.join("lib.ts"),
            r#"
export class Counter {
  value = 0
  #café = 0
  add(delta: number): number
  add(delta: number) { this.value += delta; this.#café++; return this.value }
  read() { const {value} = this; return value }
  reset() { this.value = 0 }
}
export const accumulate = (counter: Counter, delta: number) => counter.add(delta)
"#,
        )
        .unwrap();
        fs::write(
            root.join("barrel.ts"),
            "export {Counter, accumulate as increment} from './lib'\n",
        )
        .unwrap();
        fs::write(root.join("main.ts"),r#"
import {Counter, increment} from './barrel'
const counter = new Counter()
export function execute() { increment(counter, 1); counter.add(2); const method = counter.add; return method }
export function other() { const counter = {add: (n: number) => n * 2}; return counter.add(3) }
export function unknown(callback: any) { return callback(1) }
export const initialized: Counter = {value: 1} as Counter
import type {Missing} from 'missing-library'
export type Pending = Missing
"#).unwrap();
        fs::write(root.join("view.tsx"),"import {execute} from './main'\nexport const Badge = () => { execute(); return <span /> }\n").unwrap();
        let db = temp.path().join("index.sqlite");
        sonar::index(&root, &db).unwrap();
        let result =
            sonar::resolve_typescript(&db, Path::new("node"), Some(compiler), None).unwrap();
        assert!(result.analysis.calls >= 5);
        Self {
            _temp: temp,
            root,
            db,
        }
    }
    fn handle(&self, path: &str, symbol: &str) -> String {
        sonar::outline(&self.db, path, 100, None)
            .unwrap()
            .locations
            .into_iter()
            .find(|l| l.symbol.as_deref() == Some(symbol))
            .unwrap()
            .handle
    }
}

#[test]
fn compiler_aliases_overloads_shadowing_and_tsx_have_evidenced_navigation() {
    let Some(compiler) = compiler() else { return };
    let f = Fixture::new(&compiler);
    let before = fs::read(&f.db).unwrap();
    let execute = f.handle("main.ts", "execute");
    let edges = sonar::neighbors(
        &f.db,
        &execute,
        &NeighborOptions {
            relation: Relation::Calls,
            direction: Direction::Outgoing,
            ..Default::default()
        },
    )
    .unwrap();
    let symbols: Vec<_> = edges
        .locations
        .iter()
        .filter_map(|l| l.symbol.as_deref())
        .collect();
    assert!(
        symbols.contains(&"accumulate") && symbols.contains(&"Counter.add"),
        "{edges:?}"
    );
    for e in &edges.links {
        assert_eq!(e.kind, "calls");
        assert_eq!(e.evidence.path, "main.ts");
    }
    let added = f.handle("lib.ts", "Counter.add");
    let callers = sonar::neighbors(
        &f.db,
        &added,
        &NeighborOptions {
            relation: Relation::Calls,
            direction: Direction::Incoming,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        !callers
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("other")),
        "shadowed member must not bind Counter.add"
    );
    let unknown = f.handle("main.ts", "unknown");
    assert!(
        sonar::neighbors(
            &f.db,
            &unknown,
            &NeighborOptions {
                relation: Relation::Calls,
                direction: Direction::Outgoing,
                ..Default::default()
            }
        )
        .unwrap()
        .links
        .is_empty()
    );
    let context = Investigator::open(&f.db, None)
        .unwrap()
        .investigate(
            "increment counter",
            &InvestigationOptions {
                from: Some(execute.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(context.links.iter().any(|l| l.kind == "calls"));
    assert!(
        context
            .locations
            .iter()
            .any(|l| l.location.symbol.as_deref() == Some("Badge"))
    );
    let follow = SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .follow(&execute, "Counter.add", &Default::default())
        .unwrap();
    assert!(!follow.matches.is_empty());
    assert_eq!(
        fs::read(&f.db).unwrap(),
        before,
        "queries must remain read-only"
    );
}

#[test]
fn compiler_fields_distinguish_compound_writes_patterns_and_unicode() {
    let Some(compiler) = compiler() else { return };
    let f = Fixture::new(&compiler);
    let value = f.handle("lib.ts", "Counter.value");
    let uses = sonar::field_uses(&f.db, &value, &FieldUseOptions::default()).unwrap();
    for access in [
        FieldAccess::Read,
        FieldAccess::Write,
        FieldAccess::ReadWrite,
        FieldAccess::Pattern,
        FieldAccess::Initialize,
    ] {
        assert!(
            uses.uses.iter().any(|u| u.access == access),
            "missing {access:?}: {uses:?}"
        );
    }
    let private = f.handle("lib.ts", "Counter.#café");
    let uses = sonar::field_uses(&f.db, &private, &Default::default()).unwrap();
    assert!(uses.uses.iter().any(|u| u.access == FieldAccess::ReadWrite));
    for u in &uses.uses {
        let source = fs::read_to_string(f.root.join(&u.evidence.path)).unwrap();
        assert_eq!(
            &source[u.evidence.span.start_byte..u.evidence.span.end_byte],
            "#café"
        );
    }
}

#[test]
fn config_and_dependency_changes_invalidate_compiler_graph_without_source_reindexing() {
    let Some(compiler) = compiler() else { return };
    let f = Fixture::new(&compiler);
    let handle = f.handle("main.ts", "execute");
    fs::write(f.root.join("tsconfig.json"),r#"{"compilerOptions":{"strict":false,"target":"ESNext","module":"ESNext","moduleResolution":"Bundler"}}"#).unwrap();
    let edges = sonar::neighbors(
        &f.db,
        &handle,
        &NeighborOptions {
            relation: Relation::Calls,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(edges.links.is_empty());
    assert_eq!(edges.index.syntax_freshness, "current");
    assert_eq!(edges.index.semantic_freshness, "stale");
    sonar::resolve_typescript(&f.db, Path::new("node"), Some(&compiler), None).unwrap();
    assert!(
        !sonar::neighbors(
            &f.db,
            &handle,
            &NeighborOptions {
                relation: Relation::Calls,
                ..Default::default()
            }
        )
        .unwrap()
        .links
        .is_empty()
    );
    // Installing a previously absent dependency changes analysis while ignored
    // dependency files leave the source index current.
    let missing = f.root.join("node_modules/missing-library");
    fs::create_dir_all(&missing).unwrap();
    fs::write(
        missing.join("index.d.ts"),
        "export interface Missing { id: number }\n",
    )
    .unwrap();
    let unavailable = sonar::neighbors(
        &f.db,
        &handle,
        &NeighborOptions {
            relation: Relation::Calls,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(unavailable.index.syntax_freshness, "current");
    assert_eq!(unavailable.index.semantic_freshness, "stale");
    assert!(unavailable.links.is_empty());
    fs::write(f.root.join("main.ts"), "export function changed() {}\n").unwrap();
    assert!(sonar::neighbors(&f.db, &handle, &Default::default()).is_err());
    sonar::index(&f.root, &f.db).unwrap();
    assert!(sonar::neighbors(&f.db, &handle, &Default::default()).is_err());
}

#[test]
fn new_tsconfig_and_ambient_type_packages_invalidate_inferred_analysis() {
    let Some(compiler) = compiler() else { return };
    let f = Fixture::new(&compiler);
    let handle = f.handle("main.ts", "execute");
    fs::remove_file(f.root.join("tsconfig.json")).unwrap();
    let inferred =
        sonar::resolve_typescript(&f.db, Path::new("node"), Some(&compiler), None).unwrap();
    assert!(inferred.inferred_files > 0);
    // TypeScript 6 requires an explicit wildcard to load ambient packages;
    // TypeScript 5 loads visible @types packages by default.
    let types_option = if inferred.compiler.starts_with("6.") {
        r#","types":["*"]"#
    } else {
        ""
    };
    fs::write(f.root.join("tsconfig.json"), format!(r#"{{"compilerOptions":{{"strict":true,"target":"ESNext","module":"ESNext","moduleResolution":"Bundler"{types_option}}}}}"#)).unwrap();
    let options = NeighborOptions {
        relation: Relation::Calls,
        ..Default::default()
    };
    let stale = sonar::neighbors(&f.db, &handle, &options).unwrap();
    assert_eq!(stale.index.syntax_freshness, "current");
    assert_eq!(stale.index.semantic_freshness, "stale");
    assert!(stale.links.is_empty());
    let types = f.root.join("node_modules/@types");
    fs::create_dir_all(types.join("first")).unwrap();
    fs::write(
        types.join("first/index.d.ts"),
        "declare const firstEnvironment: boolean\n",
    )
    .unwrap();
    sonar::resolve_typescript(&f.db, Path::new("node"), Some(&compiler), None).unwrap();
    fs::create_dir(types.join("second")).unwrap();
    fs::write(
        types.join("second/index.d.ts"),
        "declare const secondEnvironment: boolean\n",
    )
    .unwrap();
    let stale = sonar::neighbors(&f.db, &handle, &options).unwrap();
    assert_eq!(stale.index.syntax_freshness, "current");
    assert_eq!(stale.index.semantic_freshness, "stale");
    assert!(stale.links.is_empty());
}
