use sonar::{
    Direction, FieldAccess, FieldUseOptions, GoOptions, InvestigationOptions, Investigator,
    NeighborOptions, Relation,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[path = "../src/test_toolchains.rs"]
mod test_toolchains;

fn compiler() -> Option<PathBuf> {
    test_toolchains::go()
}
const COUNTER: &str = r#"package counter

type Counter[T ~int] struct { Value T; Callback func() }
func (c *Counter[T]) Add(delta T) T { c.Value += delta; return c.Value }
func (c *Counter[T]) Reset() { c.Value = 0 }
func (c *Counter[T]) Bump() { c.Value++ }
func (c *Counter[T]) Borrow() *T { return &c.Value }
func (c *Counter[T]) Invoke() { c.Callback() }
func New() *Counter[int] { return &Counter[int]{Value: 1} }
type Adder interface { Add(int) int }
"#;
const APP: &str = r#"package app
import c "example.test/app/internal/counter"
func Execute() int { v := c.New(); v.Reset(); defer v.Bump(); go v.Bump(); return v.Add(2) }
func Shadow() { var v struct { Add func(int) int }; v.Add(3) }
func Unknown(callback func()) { callback() }
func Dynamic(a c.Adder) int { return a.Add(1) }
func Literal() c.Counter[int] { return c.Counter[int]{Value: 3} }
"#;
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    options: GoOptions,
}
impl Fixture {
    fn new(go: &Path) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("internal/counter")).unwrap();
        fs::write(root.join("go.mod"), "module example.test/app\n\ngo 1.26\n").unwrap();
        fs::write(root.join("internal/counter/counter.go"), COUNTER).unwrap();
        fs::write(root.join("app.go"), APP).unwrap();
        fs::write(root.join("app_test.go"),"package app_test\nimport (\"testing\"; app \"example.test/app\")\nfunc TestExecute(t *testing.T) { app.Execute() }\n").unwrap();
        fs::write(
            root.join("feature.go"),
            "//go:build extra\n\npackage app\nfunc Feature() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("platform_windows.go"),
            "package app\nfunc WindowsOnly() {}\n",
        )
        .unwrap();
        fs::write(root.join("guide.md"), "# Execution\n[Execute](app.go#L3)\n").unwrap();
        let db = temp.path().join("index.sqlite");
        sonar::index(&root, &db).unwrap();
        sonar::index_navigation(&db).unwrap();
        let options = GoOptions {
            command: go.to_owned(),
            ..Default::default()
        };
        Self {
            _temp: temp,
            root,
            db,
            options,
        }
    }
    fn resolve(&self) -> sonar::GoReport {
        sonar::resolve_go(&self.db, &self.options).unwrap()
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
    fn calls(&self, handle: &str, direction: Direction) -> sonar::NavigationResponse {
        sonar::neighbors(
            &self.db,
            handle,
            &NeighborOptions {
                relation: Relation::Calls,
                direction,
                ..Default::default()
            },
        )
        .unwrap()
    }
}

#[test]
fn go_resolves_package_aliases_generics_receivers_tests_and_interface_declarations() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    let report = f.resolve();
    assert_eq!(report.diagnostic_count, 0, "{:?}", report.diagnostics);
    assert!(report.analysis.calls >= 7, "{report:?}");
    assert!(report.excluded_files.contains(&"feature.go".into()));
    assert!(
        report
            .excluded_files
            .contains(&"platform_windows.go".into())
    );
    let before = fs::read(&f.db).unwrap();
    let execute = f.handle("app.go", "Execute");
    let calls = f.calls(&execute, Direction::Outgoing);
    let symbols: Vec<_> = calls
        .locations
        .iter()
        .filter_map(|l| l.symbol.as_deref())
        .collect();
    for name in ["New", "Counter.Reset", "Counter.Bump", "Counter.Add"] {
        assert!(symbols.contains(&name), "{calls:?}");
    }
    assert!(calls.links.iter().all(|l| l.evidence.path == "app.go"));
    let callers = f.calls(&execute, Direction::Incoming);
    assert!(
        callers
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("TestExecute") && l.source_role == "test"),
        "{callers:?}"
    );
    for symbol in ["Shadow", "Unknown"] {
        assert!(
            f.calls(&f.handle("app.go", symbol), Direction::Outgoing)
                .links
                .is_empty()
        );
    }
    let dynamic = f.calls(&f.handle("app.go", "Dynamic"), Direction::Outgoing);
    assert!(
        dynamic
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Adder.Add"))
    );
    assert!(
        !dynamic
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Counter.Add"))
    );
    let investigation = Investigator::open(&f.db, None)
        .unwrap()
        .investigate(
            "counter value",
            &InvestigationOptions {
                from: Some(execute.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(investigation.links.iter().any(|l| l.kind == "calls"));
    assert!(
        investigation
            .notices
            .iter()
            .any(|n| n.code == "go_analysis_coverage")
    );
    let follow = sonar::SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .follow(&execute, "Counter.Add", &Default::default())
        .unwrap();
    assert!(!follow.matches.is_empty(), "{follow:?}");
    assert_eq!(
        fs::read(&f.db).unwrap(),
        before,
        "queries must not write the database"
    );
}

#[test]
fn go_function_values_resolve_local_aliases_methods_generics_and_source_evidence() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    let source = r#"package app
import c "example.test/app/internal/counter"
func firstTarget() {}
func secondTarget() {}
func genericTarget[T any](value T) {}
func genericPair[A, B any](a A, b B) {}
type Action func()
func LocalAlias() { invoke := (firstTarget)
invoke()
}
func Declaration() { var invoke func() = firstTarget; invoke() }
func AliasChain() { first := firstTarget; second := first; third := second; third() }
func MultipleValues() { first, second := firstTarget, secondTarget; first(); second() }
func MethodValue(v *c.Counter[int]) { invoke := v.Reset; invoke() }
func MethodExpression(v *c.Counter[int]) { invoke := (*c.Counter[int]).Reset; invoke(v) }
func ImportedValue() { invoke := c.New; _ = invoke() }
func GenericValue() { invoke := genericTarget[int]; invoke(1) }
func GenericPairValue() { invoke := genericPair[int, string]; invoke(1, "x") }
func ConvertedValue() { invoke := Action(firstTarget); invoke() }
func ConvertedCall() { Action(firstTarget)() }
func DeferredValue() { invoke := firstTarget; defer invoke(); go invoke() }
func ShadowedValue() { invoke := firstTarget; { invoke := secondTarget; invoke() }; invoke() }
func CapturedValue() { invoke := firstTarget; func() { invoke() }() }
func LoopValue(condition bool) { for invoke := firstTarget; condition; { invoke() } }
func InterfaceValue(v c.Adder) int { invoke := v.Add; return invoke(1) }
func UnicodeValue() { Exécuter := firstTarget; Exécuter() }
"#
    .replace('\n', "\r\n");
    fs::write(f.root.join("function_values.go"), &source).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert_eq!(report.diagnostic_count, 0, "{:?}", report.diagnostics);
    assert_eq!(report.analysis.ambiguous_symbols, 0);
    let before = fs::read(&f.db).unwrap();
    let conn =
        rusqlite::Connection::open_with_flags(&f.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let occurrence = |symbol: &str, kind: &str| -> (usize, usize, usize) {
        conn.query_row(
            "SELECT e.start_byte,e.end_byte,e.line FROM semantic_edges e
             JOIN locations l ON l.id=e.from_id
             WHERE e.path='function_values.go' AND l.symbol=?1 AND e.kind=?2",
            rusqlite::params![symbol, kind],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, i64>(2)? as usize,
                ))
            },
        )
        .unwrap()
    };
    for (symbol, target, evidence) in [
        ("LocalAlias", "firstTarget", "invoke"),
        ("Declaration", "firstTarget", "invoke"),
        ("AliasChain", "firstTarget", "third"),
        ("MethodValue", "Counter.Reset", "invoke"),
        ("MethodExpression", "Counter.Reset", "invoke"),
        ("ImportedValue", "New", "invoke"),
        ("GenericValue", "genericTarget", "invoke"),
        ("GenericPairValue", "genericPair", "invoke"),
        ("ConvertedValue", "firstTarget", "invoke"),
        ("ConvertedCall", "firstTarget", "Action(firstTarget)"),
        ("CapturedValue", "firstTarget", "invoke"),
        ("LoopValue", "firstTarget", "invoke"),
        ("InterfaceValue", "Adder.Add", "invoke"),
        ("UnicodeValue", "firstTarget", "Exécuter"),
    ] {
        let calls = f.calls(&f.handle("function_values.go", symbol), Direction::Outgoing);
        assert_eq!(calls.links.len(), 1, "{symbol}: {calls:?}");
        assert!(
            calls
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(target)),
            "{symbol}: {calls:?}"
        );
        let link = &calls.links[0];
        assert_eq!(link.evidence.path, "function_values.go");
        let (start, end, line) = occurrence(symbol, "calls");
        assert_eq!(&source[start..end], evidence);
        assert_eq!(link.evidence.line, line);
        assert_eq!(
            source[..start].bytes().filter(|b| *b == b'\n').count() + 1,
            line
        );
        if symbol == "InterfaceValue" {
            assert!(
                !calls
                    .locations
                    .iter()
                    .any(|l| l.symbol.as_deref() == Some("Counter.Add"))
            );
        }
    }
    for symbol in ["MultipleValues", "ShadowedValue"] {
        let calls = f.calls(&f.handle("function_values.go", symbol), Direction::Outgoing);
        assert_eq!(calls.links.len(), 2, "{symbol}: {calls:?}");
        for target in ["firstTarget", "secondTarget"] {
            assert!(
                calls
                    .locations
                    .iter()
                    .any(|l| l.symbol.as_deref() == Some(target)),
                "{symbol}: {calls:?}"
            );
        }
    }
    let deferred = f.calls(
        &f.handle("function_values.go", "DeferredValue"),
        Direction::Outgoing,
    );
    assert_eq!(deferred.links.len(), 1, "{deferred:?}");
    assert_eq!(deferred.links[0].occurrences, 2);
    let local = f.handle("function_values.go", "LocalAlias");
    let references = sonar::neighbors(
        &f.db,
        &local,
        &NeighborOptions {
            relation: Relation::References,
            direction: Direction::Outgoing,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(references.links.len(), 1, "{references:?}");
    let (start, end, line) = occurrence("LocalAlias", "references");
    assert_eq!(&source[start..end], "firstTarget");
    assert_eq!(references.links[0].evidence.line, line);
    let callers = f.calls(
        &f.handle("function_values.go", "firstTarget"),
        Direction::Incoming,
    );
    assert!(
        callers.locations.iter().any(|l| l.handle == local),
        "{callers:?}"
    );
    assert_eq!(fs::read(&f.db).unwrap(), before);
}

#[test]
fn go_function_values_omit_mutation_escapes_globals_and_unknown_callbacks() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    fs::write(f.root.join("dynamic_values.go"), r#"package app
func alpha() {}
func beta() {}
var packageCallback = alpha
func GlobalValue() { packageCallback() }
func CopiedGlobal() { invoke := packageCallback; invoke() }
func CallbackParameter(invoke func()) { invoke() }
func FieldValue(v struct { Invoke func() }) { invoke := v.Invoke; invoke() }
func IndexValue() { callbacks := []func(){alpha}; invoke := callbacks[0]; invoke() }
func ReassignedValue() { invoke := alpha; invoke = beta; invoke() }
func BranchValue(change bool) { invoke := alpha; if change { invoke = beta }; invoke() }
func RedeclaredValue() { invoke := alpha; invoke, ok := beta, true; _ = ok; invoke() }
func AddressedValue() { invoke := alpha; _ = &(invoke); invoke() }
func PointerWrite() { invoke := alpha; pointer := &invoke; *pointer = beta; invoke() }
func RangeAssigned() { invoke := alpha; for _, invoke = range []func(){beta} {}; invoke() }
func ClosureWrite() { invoke := alpha; func() { invoke = beta }(); invoke() }
func ConcurrentWrite() { invoke := alpha; go func() { invoke = beta }(); invoke() }
func AssignedLater() { var invoke func(); invoke = alpha; invoke() }
func UnknownReturn(factory func() func()) { invoke := factory(); invoke() }
func FunctionLiteral() { invoke := func() {}; invoke() }
func NilValue() { var invoke func() = nil; invoke() }
func DirtyChain() { first := alpha; second := first; first = beta; second() }
func ParallelSwap() { first, second := alpha, beta; first, second = second, first; first(); second() }
"#).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert_eq!(report.diagnostic_count, 0, "{:?}", report.diagnostics);
    for symbol in [
        "GlobalValue",
        "CopiedGlobal",
        "CallbackParameter",
        "FieldValue",
        "IndexValue",
        "ReassignedValue",
        "BranchValue",
        "RedeclaredValue",
        "AddressedValue",
        "PointerWrite",
        "RangeAssigned",
        "ClosureWrite",
        "ConcurrentWrite",
        "AssignedLater",
        "UnknownReturn",
        "FunctionLiteral",
        "NilValue",
        "DirtyChain",
        "ParallelSwap",
    ] {
        let calls = f.calls(&f.handle("dynamic_values.go", symbol), Direction::Outgoing);
        assert!(calls.links.is_empty(), "{symbol}: {calls:?}");
    }
}

#[test]
fn go_invalid_function_value_initializers_do_not_create_call_targets() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    fs::write(
        f.root.join("invalid_functions.go"),
        r#"package app
func needsArgument(int) {}
func ordinaryFunction() {}
type InvalidAction func()
func MismatchedAlias() { var invoke func() = needsArgument; invoke() }
func BadValue() { var invoke func() = 1; invoke() }
func MissingAlias() { invoke := undefinedFunction; invoke() }
func InvalidConversion() { invoke := InvalidAction(123); invoke() }
func InvalidInstance() { invoke := ordinaryFunction[int]; invoke() }
"#,
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert!(report.diagnostic_count > 0);
    for symbol in [
        "MismatchedAlias",
        "BadValue",
        "MissingAlias",
        "InvalidConversion",
        "InvalidInstance",
    ] {
        let calls = f.calls(
            &f.handle("invalid_functions.go", symbol),
            Direction::Outgoing,
        );
        assert!(calls.links.is_empty(), "{symbol}: {calls:?}");
    }
    assert!(
        !f.calls(&f.handle("app.go", "Execute"), Direction::Outgoing)
            .links
            .is_empty()
    );
}

#[test]
fn go_implementations_connect_complete_method_sets_and_observed_generic_instances() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    fs::create_dir(f.root.join("contracts")).unwrap();
    fs::write(
        f.root.join("contracts/contracts.go"),
        r#"package contracts
type Sealed interface { seal() }
type Token struct{}
func (Token) seal() {}
type Argument struct{}
type Consumer interface { Consume(Argument) }
"#,
    )
    .unwrap();
    let source = r#"package app
import "example.test/app/contracts"
type Resettable interface { Reset() }
type Combined interface { Resettable; Add(int) int }
type ContractAlias = Combined
type Anything interface{}
type NumberConstraint interface { ~int; Reset() }
type GenericContract[T any] interface { Put(T) }
type Concrete struct{}
func (Concrete) Reset() {}
func (*Concrete) Add(n int) int { return n }
type ConcreteAlias = Concrete
type PointerAlias = *Concrete
type DefinedPointer *Concrete
type Partial struct{}
func (Partial) Add(n int) int { return n }
type Wrong struct{}
func (Wrong) Reset(int) {}
type WrongResult struct{}
func (WrongResult) Reset() {}
func (WrongResult) Add(int) string { return "" }
type Other struct{}
func (Other) Reset() {}
type Embedded struct { Concrete }
type Ambiguous struct { Concrete; Other }
type InterfaceHolder struct { Resettable }
type Box[T any] struct{}
func (Box[T]) Put(T) {}
type BoxAlias[T any] = Box[T]
type IntBox = Box[int]
type StringOnly struct{}
func (StringOnly) Put(string) {}
type Unused[T any] struct{}
func (Unused[T]) Put(T) {}
var _ GenericContract[int] = Box[int]{}
var _ GenericContract[int] = BoxAlias[int]{}
var _ = Box[string]{}
type LocalToken struct{}
func (LocalToken) seal() {}
type Consumer struct{}
func (Consumer) Consume(contracts.Argument) {}
func DynamicCombined(value Combined) int { value.Reset(); return value.Add(1) }
"#
    .replace('\n', "\r\n");
    fs::write(f.root.join("implementations.go"), &source).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert_eq!(report.diagnostic_count, 0, "{:?}", report.diagnostics);
    assert!(report.implementations > 0, "{report:?}");
    assert_eq!(report.analysis.ambiguous_symbols, 0);
    let before = fs::read(&f.db).unwrap();
    let query = |path: &str, symbol: &str, direction| {
        sonar::neighbors(
            &f.db,
            &f.handle(path, symbol),
            &NeighborOptions {
                relation: Relation::Implements,
                direction,
                limit: 100,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let incoming = |symbol| query("implementations.go", symbol, Direction::Incoming);
    let combined = incoming("Combined");
    let symbols: Vec<_> = combined
        .locations
        .iter()
        .filter_map(|l| l.symbol.as_deref())
        .collect();
    for symbol in [
        "Concrete",
        "ConcreteAlias",
        "PointerAlias",
        "Embedded",
        "Counter",
    ] {
        assert!(symbols.contains(&symbol), "missing {symbol}: {combined:?}");
    }
    for symbol in [
        "Partial",
        "Wrong",
        "WrongResult",
        "Other",
        "Ambiguous",
        "DefinedPointer",
        "InterfaceHolder",
    ] {
        assert!(
            !symbols.contains(&symbol),
            "unexpected {symbol}: {combined:?}"
        );
    }
    let resetters = incoming("Resettable");
    for symbol in ["Concrete", "Other", "Embedded", "InterfaceHolder"] {
        assert!(
            resetters
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol)),
            "{resetters:?}"
        );
    }
    for symbol in ["Wrong", "Ambiguous", "DefinedPointer"] {
        assert!(
            !resetters
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol)),
            "{resetters:?}"
        );
    }
    let methods = incoming("Combined.Add");
    assert!(
        methods
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Concrete.Add")),
        "{methods:?}"
    );
    for symbol in ["Partial.Add", "WrongResult.Add"] {
        assert!(
            !methods
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol)),
            "{methods:?}"
        );
    }
    let contracts = query("implementations.go", "Concrete.Add", Direction::Outgoing);
    for symbol in ["Combined.Add", "Adder.Add"] {
        assert!(
            contracts
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol)),
            "{contracts:?}"
        );
    }
    assert_eq!(contracts.links.len(), 2, "{contracts:?}");
    let generic = incoming("GenericContract");
    for symbol in ["Box", "BoxAlias", "IntBox"] {
        assert!(
            generic
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol)),
            "{generic:?}"
        );
    }
    for symbol in ["StringOnly", "Unused"] {
        assert!(
            !generic
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol)),
            "{generic:?}"
        );
    }
    let generic_methods = incoming("GenericContract.Put");
    assert_eq!(generic_methods.locations.len(), 1, "{generic_methods:?}");
    assert_eq!(
        generic_methods.locations[0].symbol.as_deref(),
        Some("Box.Put")
    );
    for symbol in ["Anything", "NumberConstraint"] {
        assert!(incoming(symbol).links.is_empty(), "{symbol}");
    }
    let sealed = query("contracts/contracts.go", "Sealed", Direction::Incoming);
    assert_eq!(sealed.locations.len(), 1, "{sealed:?}");
    assert_eq!(sealed.locations[0].symbol.as_deref(), Some("Token"));
    let consumer = query("contracts/contracts.go", "Consumer", Direction::Incoming);
    assert_eq!(consumer.locations.len(), 1, "{consumer:?}");
    assert_eq!(consumer.locations[0].path, "implementations.go");
    let scoped = sonar::neighbors(
        &f.db,
        &f.handle("contracts/contracts.go", "Consumer"),
        &NeighborOptions {
            relation: Relation::Implements,
            scope: Some("contracts".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(scoped.links.is_empty());
    let dynamic = f.calls(
        &f.handle("implementations.go", "DynamicCombined"),
        Direction::Outgoing,
    );
    assert!(dynamic.links.iter().all(|l| l.kind == "calls"));
    assert!(
        dynamic
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Combined.Add"))
    );
    assert!(
        !dynamic
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Concrete.Add"))
    );
    let mut paged = std::collections::BTreeSet::new();
    let mut cursor = None;
    loop {
        let page = sonar::neighbors(
            &f.db,
            &f.handle("implementations.go", "Resettable"),
            &NeighborOptions {
                relation: Relation::Implements,
                direction: Direction::Incoming,
                limit: 1,
                cursor,
                ..Default::default()
            },
        )
        .unwrap();
        for link in page.links {
            assert!(paged.insert(link.from));
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(paged.len(), resetters.links.len());
    let follow = sonar::SearchEngine::open_navigation(&f.db, None)
        .unwrap()
        .follow(
            &f.handle("implementations.go", "Combined.Add"),
            "Concrete",
            &sonar::FollowOptions {
                relation: sonar::FollowRelation::Implements,
                direction: Direction::Incoming,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!follow.matches.is_empty(), "{follow:?}");
    assert!(follow.links.iter().all(|l| l.kind == "implements"));
    assert!(
        follow
            .notices
            .iter()
            .any(|n| n.code == "go_analysis_coverage")
    );
    let conn =
        rusqlite::Connection::open_with_flags(&f.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut stmt = conn.prepare("SELECT e.start_byte,e.end_byte,e.line,l.symbol FROM semantic_edges e JOIN locations l ON l.id=e.from_id WHERE e.kind='implements' AND e.path='implementations.go'").unwrap();
    for row in stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as usize,
                r.get::<_, i64>(1)? as usize,
                r.get::<_, i64>(2)? as usize,
                r.get::<_, String>(3)?,
            ))
        })
        .unwrap()
    {
        let (start, end, line, symbol) = row.unwrap();
        assert_eq!(&source[start..end], symbol.rsplit('.').next().unwrap());
        assert_eq!(
            source[..start].bytes().filter(|b| *b == b'\n').count() + 1,
            line
        );
    }
    assert_eq!(fs::read(&f.db).unwrap(), before);
}

#[test]
fn go_implementation_queries_report_missing_stale_and_failed_type_checking() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    fs::write(f.root.join("implementation.go"), "package app\ntype Resetter interface { Reset() }\ntype Concrete struct{}\nfunc (Concrete) Reset() {}\n").unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let handle = f.handle("implementation.go", "Resetter");
    let options = NeighborOptions {
        relation: Relation::Implements,
        direction: Direction::Incoming,
        ..Default::default()
    };
    let unavailable = sonar::neighbors(&f.db, &handle, &options).unwrap();
    assert!(unavailable.links.is_empty());
    assert!(
        unavailable
            .notices
            .iter()
            .any(|n| n.code == "graph_unavailable")
    );
    assert!(f.resolve().implementations > 0);
    let before = fs::read(&f.db).unwrap();
    fs::write(
        f.root.join("go.mod"),
        "module example.test/app\n\ngo 1.26\n// changed analysis configuration\n",
    )
    .unwrap();
    let stale = sonar::neighbors(&f.db, &handle, &options).unwrap();
    assert!(stale.links.is_empty());
    assert!(stale.notices.iter().any(|n| n.code == "graph_unavailable"));
    assert_eq!(fs::read(&f.db).unwrap(), before);
    fs::write(
        f.root.join("broken.go"),
        "package app\ntype Incomplete interface { Missing(UndefinedType) }\n",
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert!(report.diagnostic_count > 0);
    assert_eq!(report.implementations, 0);
    assert!(
        sonar::neighbors(&f.db, &f.handle("implementation.go", "Resetter"), &options)
            .unwrap()
            .links
            .is_empty()
    );
    assert!(
        !f.calls(&f.handle("app.go", "Execute"), Direction::Outgoing)
            .links
            .is_empty()
    );
}

#[test]
fn go_field_uses_distinguish_reads_writes_borrows_initializers_and_callable_fields() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    let unicode =
        "package app\r\ntype Café struct { État int }\r\nfunc (c *Café) Update() { c.État++ }\r\n";
    fs::write(f.root.join("unicode.go"), unicode).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    f.resolve();
    let unicode_field = f.handle("unicode.go", "Café.État");
    let unicode_uses =
        sonar::field_uses(&f.db, &unicode_field, &FieldUseOptions::default()).unwrap();
    assert_eq!(unicode_uses.uses.len(), 1);
    let span = &unicode_uses.uses[0].evidence.span;
    assert_eq!(&unicode[span.start_byte..span.end_byte], "État");
    assert_eq!(unicode_uses.uses[0].access, FieldAccess::ReadWrite);
    let field = f.handle("internal/counter/counter.go", "Counter.Value");
    let uses = sonar::field_uses(&f.db, &field, &FieldUseOptions::default()).unwrap();
    for access in [
        FieldAccess::Read,
        FieldAccess::Write,
        FieldAccess::ReadWrite,
        FieldAccess::Borrow,
        FieldAccess::Initialize,
    ] {
        assert!(
            uses.uses.iter().any(|u| u.access == access),
            "missing {access:?}: {uses:?}"
        );
    }
    let source = COUNTER;
    for use_ in uses
        .uses
        .iter()
        .filter(|u| u.evidence.path == "internal/counter/counter.go")
    {
        assert_eq!(
            &source[use_.evidence.span.start_byte..use_.evidence.span.end_byte],
            "Value"
        );
        assert!(use_.context.is_some());
    }
    let scoped = sonar::field_uses(
        &f.db,
        &field,
        &FieldUseOptions {
            scope: Some("app.go".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(scoped.uses.len(), 1);
    assert_eq!(scoped.uses[0].access, FieldAccess::Initialize);
    let callback = f.handle("internal/counter/counter.go", "Counter.Callback");
    assert!(
        sonar::field_uses(&f.db, &callback, &FieldUseOptions::default())
            .unwrap()
            .uses
            .iter()
            .any(|u| u.access == FieldAccess::Receiver)
    );
    assert!(
        f.calls(
            &f.handle("internal/counter/counter.go", "Counter.Invoke"),
            Direction::Outgoing
        )
        .links
        .is_empty()
    );
}

#[test]
fn go_positional_initializers_preserve_field_identity_and_value_references() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    let source = r#"package app
import c "example.test/app/internal/counter"
const DefaultPort = 8080
func deliver() {}
type Config struct { Port int; Handler func() }
type ConfigAlias = Config
type Embedded struct { Config; Enabled bool }
type Pair[T any] struct { First, Second T }
type PairAlias[T any] = Pair[T]
type Padding struct { First int; _ int; Last int }
type Unicode struct { Café string; Ready bool }
var packageValue = Config{DefaultPort, deliver}
func Positional() Config { return Config{DefaultPort, deliver} }
func Keyed() Config { return Config{Handler: deliver, Port: DefaultPort} }
func AliasLiteral() ConfigAlias { return ConfigAlias{DefaultPort, deliver} }
func FromOtherPackage() c.Counter[int] { return c.Counter[int]{DefaultPort, nil} }
func Generic() Pair[int] { return Pair[int]{1, 2} }
func GenericAlias() PairAlias[int] { return PairAlias[int]{3, 4} }
func Nested() Embedded { return Embedded{Config{5, nil}, true} }
func Elided() []*Config { return []*Config{{6, nil}, {7, deliver}} }
func ArrayValues() [1]Config { return [1]Config{{8, nil}} }
func MapValues() map[string]Config { return map[string]Config{"a": {9, nil}} }
func BlankField() Padding { return Padding{10, 0, 11} }
func UnicodeLiteral() Unicode { return Unicode{"π", true} }
func Empty() Config { return Config{} }
func Arrays() []int { return []int{12, 13} }
"#
    .replace('\n', "\r\n");
    fs::write(f.root.join("positional.go"), &source).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert_eq!(report.diagnostic_count, 0, "{report:?}");
    assert_eq!(report.analysis.ambiguous_symbols, 0, "{report:?}");
    let before = fs::read(&f.db).unwrap();
    let query = |symbol| {
        sonar::field_uses(
            &f.db,
            &f.handle("positional.go", symbol),
            &FieldUseOptions::default(),
        )
        .unwrap()
    };
    let port = query("Config.Port");
    let values: Vec<_> = port
        .uses
        .iter()
        .map(|u| {
            assert_eq!(u.access, FieldAccess::Initialize);
            let span = &u.evidence.span;
            assert!(u.context.is_some());
            assert_eq!(
                span.start_line,
                source[..span.start_byte]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1
            );
            &source[span.start_byte..span.end_byte]
        })
        .collect();
    assert_eq!(
        values,
        [
            "DefaultPort",
            "DefaultPort",
            "Port",
            "DefaultPort",
            "5",
            "6",
            "7",
            "8",
            "9"
        ]
    );
    for (symbol, expected) in [
        ("Pair.First", vec!["1", "3"]),
        ("Pair.Second", vec!["2", "4"]),
        ("Padding.First", vec!["10"]),
        ("Padding.Last", vec!["11"]),
        ("Embedded.Config", vec!["Config{5, nil}"]),
        ("Embedded.Enabled", vec!["true"]),
        ("Unicode.Café", vec!["\"π\""]),
    ] {
        let uses = query(symbol);
        let actual: Vec<_> = uses
            .uses
            .iter()
            .map(|u| &source[u.evidence.span.start_byte..u.evidence.span.end_byte])
            .collect();
        assert_eq!(actual, expected, "{symbol}: {uses:?}");
        assert!(
            uses.uses
                .iter()
                .all(|u| u.access == FieldAccess::Initialize)
        );
    }
    let external = sonar::field_uses(
        &f.db,
        &f.handle("internal/counter/counter.go", "Counter.Value"),
        &FieldUseOptions {
            scope: Some("positional.go".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(external.uses.len(), 1);
    let span = &external.uses[0].evidence.span;
    assert_eq!(&source[span.start_byte..span.end_byte], "DefaultPort");
    assert_eq!(external.uses[0].access, FieldAccess::Initialize);
    let positional = f.handle("positional.go", "Positional");
    let references = sonar::neighbors(
        &f.db,
        &positional,
        &NeighborOptions {
            relation: Relation::References,
            direction: Direction::Outgoing,
            ..Default::default()
        },
    )
    .unwrap();
    for symbol in ["DefaultPort", "deliver", "Config.Port", "Config.Handler"] {
        assert!(
            references
                .locations
                .iter()
                .any(|l| l.symbol.as_deref() == Some(symbol)),
            "{references:?}"
        );
    }
    assert!(f.calls(&positional, Direction::Outgoing).links.is_empty());
    let first = sonar::field_uses(
        &f.db,
        &positional,
        &FieldUseOptions {
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(first.uses.len(), 1);
    let second = sonar::field_uses(
        &f.db,
        &positional,
        &FieldUseOptions {
            limit: 1,
            cursor: first.next_cursor.clone(),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(first.next_cursor.is_some());
    assert_eq!(second.uses.len(), 1);
    assert!(second.next_cursor.is_none());
    assert_ne!(first.uses[0].field, second.uses[0].field);
    for symbol in ["Empty", "Arrays"] {
        assert!(
            query(symbol).uses.is_empty(),
            "{symbol} must not acquire positional field uses"
        );
    }
    assert_eq!(
        fs::read(&f.db).unwrap(),
        before,
        "field and reference queries must remain read-only"
    );
}

#[test]
fn go_invalid_positional_literals_keep_value_reads_without_inventing_initializers() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    fs::create_dir(f.root.join("private")).unwrap();
    fs::write(
        f.root.join("private/private.go"),
        "package private\ntype Hidden struct { hidden int; Exported int }\n",
    )
    .unwrap();
    fs::write(
        f.root.join("invalid.go"),
        r#"package app
import "example.test/app/private"
type Broken struct { First, Second int }
type Source struct { Value int }
var value Source
func TooFew() { _ = Broken{value.Value} }
func TooMany() { _ = Broken{value.Value, 2, 3} }
func Mixed() { _ = Broken{value.Value, Second: 2} }
func Inaccessible() { _ = private.Hidden{1, 2} }
func UnknownType() { _ = MissingType{1, 2} }
"#,
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert!(report.diagnostic_count > 0);
    for symbol in ["TooFew", "TooMany", "Mixed", "Inaccessible", "UnknownType"] {
        let uses = sonar::field_uses(
            &f.db,
            &f.handle("invalid.go", symbol),
            &FieldUseOptions::default(),
        )
        .unwrap();
        assert!(
            !uses
                .uses
                .iter()
                .any(|u| u.access == FieldAccess::Initialize),
            "{symbol}: {uses:?}"
        );
        if matches!(symbol, "TooFew" | "TooMany" | "Mixed") {
            assert!(
                uses.uses.iter().any(|u| u.access == FieldAccess::Read),
                "{symbol}: {uses:?}"
            );
        }
    }
}

#[test]
fn go_build_tags_changes_and_failed_preparation_preserve_only_current_graphs() {
    let Some(go) = compiler() else { return };
    let mut f = Fixture::new(&go);
    f.resolve();
    f.options.tags = vec!["extra".into()];
    let report = f.resolve();
    assert!(report.included_files.contains(&"feature.go".into()));
    f.options.goos = Some("windows".into());
    let report = f.resolve();
    assert!(
        report
            .included_files
            .contains(&"platform_windows.go".into())
    );
    assert_eq!(report.build_context["GOOS"], "windows");
    f.options.goos = None;
    f.resolve();
    let execute = f.handle("app.go", "Execute");
    let before = fs::read(&f.db).unwrap();
    let mut broken = f.options.clone();
    broken.command = f.root.join("missing-go");
    assert!(sonar::resolve_go(&f.db, &broken).is_err());
    assert_eq!(fs::read(&f.db).unwrap(), before);
    fs::write(
        f.root.join("go.mod"),
        "module example.test/app\n\ngo 1.26\n// configuration changed\n",
    )
    .unwrap();
    let stale = f.calls(&execute, Direction::Outgoing);
    assert!(stale.links.is_empty());
    assert!(stale.notices.iter().any(|n| n.code == "graph_unavailable"));
    assert_eq!(
        stale
            .index
            .languages
            .iter()
            .find(|l| l.language == "go")
            .unwrap()
            .semantic_freshness,
        "stale"
    );
    f.resolve();
    assert!(!f.calls(&execute, Direction::Outgoing).links.is_empty());
    fs::write(f.root.join("app.go"), format!("{APP}\nfunc Added() {{}}\n")).unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    assert!(
        f.calls(&f.handle("app.go", "Execute"), Direction::Outgoing)
            .links
            .is_empty()
    );
    fs::write(
        f.root.join("missing.go"),
        "package app\nfunc Missing() { unresolved.Call() }\n",
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert!(report.diagnostic_count > 0);
    assert!(
        !f.calls(&f.handle("app.go", "Execute"), Direction::Outgoing)
            .links
            .is_empty()
    );
    assert!(
        f.calls(&f.handle("missing.go", "Missing"), Direction::Outgoing)
            .links
            .is_empty()
    );
}

#[test]
fn go_workspaces_and_replaced_dependency_sources_are_tracked_without_indexing_external_code() {
    let Some(go) = compiler() else { return };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    for (dir, module, source) in [
        (
            "one",
            "example.test/one",
            "package one\nimport t \"example.test/two\"\nfunc Run() int { return t.Value() }\n",
        ),
        (
            "two",
            "example.test/two",
            "package two\nfunc Value() int { return 1 }\n",
        ),
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
        fs::write(
            root.join(dir).join("go.mod"),
            format!("module {module}\n\ngo 1.26\n"),
        )
        .unwrap();
        fs::write(root.join(dir).join("lib.go"), source).unwrap();
    }
    fs::write(root.join("go.work"), "go 1.26\nuse (\n ./one\n ./two\n)\n").unwrap();
    let db = temp.path().join("workspace.sqlite");
    sonar::index(&root, &db).unwrap();
    let options = GoOptions {
        command: go.clone(),
        ..Default::default()
    };
    let report = sonar::resolve_go(&db, &options).unwrap();
    assert_eq!(report.diagnostic_count, 0, "{report:?}");
    let run = sonar::outline(&db, "one/lib.go", 100, None)
        .unwrap()
        .locations
        .into_iter()
        .find(|l| l.symbol.as_deref() == Some("Run"))
        .unwrap()
        .handle;
    let calls = sonar::neighbors(
        &db,
        &run,
        &NeighborOptions {
            relation: Relation::Calls,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        calls
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Value") && l.path == "two/lib.go"),
        "{calls:?}"
    );
    fs::write(root.join("go.work.sum"), "// changed workspace inputs\n").unwrap();
    assert!(
        sonar::neighbors(
            &db,
            &run,
            &NeighborOptions {
                relation: Relation::Calls,
                ..Default::default()
            }
        )
        .unwrap()
        .links
        .is_empty()
    );

    let f = Fixture::new(&go);
    let dep = f.root.parent().unwrap().join("dep");
    fs::create_dir(&dep).unwrap();
    fs::write(dep.join("go.mod"), "module example.test/dep\n\ngo 1.26\n").unwrap();
    fs::write(
        dep.join("dep.go"),
        "package dep\nfunc Number() int { return 1 }\n",
    )
    .unwrap();
    fs::write(f.root.join("go.mod"),"module example.test/app\n\ngo 1.26\nrequire example.test/dep v0.0.0\nreplace example.test/dep => ../dep\n").unwrap();
    fs::write(
        f.root.join("dependency.go"),
        "package app\nimport \"example.test/dep\"\nfunc Dependency() int { return dep.Number() }\n",
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    let report = f.resolve();
    assert_eq!(report.diagnostic_count, 0, "{report:?}");
    let execute = f.handle("app.go", "Execute");
    assert!(!f.calls(&execute, Direction::Outgoing).links.is_empty());
    let original_modified = fs::metadata(dep.join("dep.go"))
        .unwrap()
        .modified()
        .unwrap();
    fs::write(
        dep.join("dep.go"),
        "package dep\nfunc Number() int { return 2 }\n",
    )
    .unwrap();
    fs::File::options()
        .write(true)
        .open(dep.join("dep.go"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(original_modified))
        .unwrap();
    assert!(f.calls(&execute, Direction::Outgoing).links.is_empty());

    // Vendored dependencies are also loaded without downloading or indexing their symbols.
    let vendor = Command::new(&go)
        .args(["mod", "vendor"])
        .current_dir(&f.root)
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .output()
        .unwrap();
    assert!(
        vendor.status.success(),
        "{}",
        String::from_utf8_lossy(&vendor.stderr)
    );
    let report = f.resolve();
    assert_eq!(report.diagnostic_count, 0, "{report:?}");
    assert!(!f.calls(&execute, Direction::Outgoing).links.is_empty());
    let vendor_source = f.root.join("vendor/example.test/dep/dep.go");
    fs::write(
        vendor_source,
        "package dep\nfunc Number() int { return 3 }\n",
    )
    .unwrap();
    assert!(f.calls(&execute, Direction::Outgoing).links.is_empty());
}

#[test]
fn go_cli_init_saves_the_build_profile_and_refreshes_it() {
    let Some(go) = compiler() else { return };
    let f = Fixture::new(&go);
    let cli = |args: &[&str]| {
        let result = Command::new(env!("CARGO_BIN_EXE_sonar"))
            .arg("--db")
            .arg(&f.db)
            .args(args)
            .current_dir(&f.root)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()
    };
    cli(&[
        "init",
        "--go",
        "--go-command",
        go.to_str().unwrap(),
        "--go-tags",
        "extra",
    ]);
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(f.root.join(".sonar/project.json")).unwrap())
            .unwrap();
    assert_eq!(saved["go"]["tags"], serde_json::json!(["extra"]));
    assert_eq!(saved["go"]["command"], go.to_str().unwrap());
    fs::write(f.root.join("app.go"), format!("{APP}\n// refreshed\n")).unwrap();
    cli(&["init"]);
    let status = cli(&["status"]);
    assert_eq!(
        status["index"]["languages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["language"] == "go")
            .unwrap()["semantic_freshness"],
        "current"
    );
    let result = cli(&["resolve", "--go"]);
    assert!(result["calls"].as_u64().unwrap() > 0);
    let changed = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .arg("--db")
        .arg(&f.db)
        .arg("status")
        .current_dir(&f.root)
        .env("GOFLAGS", "-tags=changed")
        .output()
        .unwrap();
    assert!(changed.status.success());
    let changed: serde_json::Value = serde_json::from_slice(&changed.stdout).unwrap();
    assert_eq!(
        changed["index"]["languages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["language"] == "go")
            .unwrap()["semantic_freshness"],
        "stale"
    );
}
