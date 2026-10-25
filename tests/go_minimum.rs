//! A Go 1.18 fixture for validating the embedded helper's documented minimum.
use sonar::{Direction, FieldAccess, FieldUseOptions, GoOptions, NeighborOptions, Relation};
use std::fs;

#[path = "../src/test_toolchains.rs"]
mod test_toolchains;

#[test]
fn go_minimum_toolchain_resolves_calls_fields_and_interfaces() {
    let Some(command) = test_toolchains::go() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("go.mod"),
        "module example.test/minimum\n\ngo 1.18\n",
    )
    .unwrap();
    fs::write(
        root.join("counter.go"),
        r#"package counter
type Resetter interface { Reset() }
type Counter[T ~int] struct { Value T }
func (c *Counter[T]) Reset() { c.Value = 0 }
func Run() { c := Counter[int]{1}; reset := c.Reset; reset() }
"#,
    )
    .unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let report = sonar::resolve_go(
        &db,
        &GoOptions {
            command,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.diagnostic_count, 0, "{report:?}");
    let outline = sonar::outline(&db, "counter.go", 100, None).unwrap();
    let handle = |symbol: &str| {
        outline
            .locations
            .iter()
            .find(|location| location.symbol.as_deref() == Some(symbol))
            .unwrap()
            .handle
            .clone()
    };
    let before = fs::read(&db).unwrap();
    let calls = sonar::neighbors(
        &db,
        &handle("Run"),
        &NeighborOptions {
            relation: Relation::Calls,
            direction: Direction::Outgoing,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        calls
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Counter.Reset")),
        "{calls:?}"
    );
    let implementations = sonar::neighbors(
        &db,
        &handle("Resetter"),
        &NeighborOptions {
            relation: Relation::Implements,
            direction: Direction::Incoming,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        implementations
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("Counter")),
        "{implementations:?}"
    );
    let uses =
        sonar::field_uses(&db, &handle("Counter.Value"), &FieldUseOptions::default()).unwrap();
    assert!(
        uses.uses
            .iter()
            .any(|use_| use_.access == FieldAccess::Initialize),
        "{uses:?}"
    );
    assert_eq!(fs::read(&db).unwrap(), before);
}
