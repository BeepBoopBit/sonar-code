//! Exercise the real rust-analyzer exporter, in addition to the SCIP import fixtures.
use sonar::{Direction, FieldAccess, FieldUseOptions, NeighborOptions, Relation};
use std::{fs, process::Command};

#[path = "../src/test_toolchains.rs"]
mod test_toolchains;

#[test]
fn real_rust_analyzer_resolves_calls_references_and_field_uses_offline() {
    let Some(analyzer) = test_toolchains::rust_analyzer() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"sonar-rust-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"pub struct Counter { pub value: u32 }
pub fn bump(counter: &mut Counter) { counter.value += 1; }
pub fn execute() -> u32 {
    let mut counter = Counter { value: 0 };
    bump(&mut counter);
    counter.value
}
#[cfg(test)]
mod tests {
    #[test]
    fn check_execution() { assert_eq!(super::execute(), 1); }
}
"#,
    )
    .unwrap();
    let lockfile = Command::new("cargo")
        .args(["generate-lockfile", "--offline"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        lockfile.status.success(),
        "{}",
        String::from_utf8_lossy(&lockfile.stderr)
    );
    let lock_before = fs::read(root.join("Cargo.lock")).unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    let report = sonar::resolve(&db, &analyzer, None).unwrap_or_else(|error| {
        let log = fs::read_to_string(temp.path().join("resolve.log")).unwrap_or_default();
        panic!("Rust analyzer preparation failed: {error:#}\n{log}");
    });
    assert!(report.documents > 0 && report.calls > 0, "{report:?}");
    let outline = sonar::outline(&db, "src/lib.rs", 100, None).unwrap();
    let handle = |symbol: &str| {
        outline
            .locations
            .iter()
            .find(|location| location.symbol.as_deref() == Some(symbol))
            .unwrap_or_else(|| panic!("{symbol}: {outline:?}"))
            .handle
            .clone()
    };
    let before = fs::read(&db).unwrap();
    let calls = sonar::neighbors(
        &db,
        &handle("execute"),
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
            .any(|l| l.symbol.as_deref() == Some("bump")),
        "{calls:?}"
    );
    let references = sonar::neighbors(
        &db,
        &handle("Counter::value"),
        &NeighborOptions {
            relation: Relation::References,
            direction: Direction::Incoming,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        references
            .locations
            .iter()
            .any(|l| l.symbol.as_deref() == Some("execute")),
        "{references:?}"
    );
    let uses =
        sonar::field_uses(&db, &handle("Counter::value"), &FieldUseOptions::default()).unwrap();
    for access in [
        FieldAccess::Read,
        FieldAccess::ReadWrite,
        FieldAccess::Initialize,
    ] {
        assert!(
            uses.uses.iter().any(|use_| use_.access == access),
            "{access:?}: {uses:?}"
        );
    }
    assert_eq!(fs::read(root.join("Cargo.lock")).unwrap(), lock_before);
    assert_eq!(
        fs::read(&db).unwrap(),
        before,
        "queries must not write the index"
    );
}
