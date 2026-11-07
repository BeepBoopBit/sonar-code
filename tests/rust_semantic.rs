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

#[test]
fn real_compiler_resolves_import_leaves_aliases_and_reexports_without_resolving_globs() {
    let Some(analyzer) = test_toolchains::rust_analyzer() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"sonar-import-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"),"pub mod service;\nmod consumer;\npub use service::{one as exported, Thing as PublicThing};\npub use service::*;\n").unwrap();
    fs::write(
        root.join("src/service.rs"),
        "pub fn one() -> u32 { 1 }\npub struct Thing;\n",
    )
    .unwrap();
    fs::write(root.join("src/consumer.rs"),"use crate::service::{one as renamed, Thing};\nuse std::fmt::Debug;\npub fn run() -> u32 { let _ = Thing; renamed() }\n").unwrap();
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    sonar::resolve(&db, &analyzer, None).unwrap();
    let before = fs::read(&db).unwrap();
    let response =
        sonar::analysis::dependencies(&db, "src/consumer.rs", &Default::default()).unwrap();
    let service = response
        .relationships
        .iter()
        .find(|g| g.to == "src/service.rs")
        .unwrap();
    let imports = service
        .kinds
        .iter()
        .find(|k| k.kind == sonar::analysis::DependencyKind::Imports)
        .unwrap();
    assert_eq!(imports.occurrences, 2, "{response:?}");
    for evidence in &imports.evidence {
        let source = fs::read_to_string(root.join(&evidence.path)).unwrap();
        assert!(["one", "Thing"].contains(&&source[evidence.bytes[0]..evidence.bytes[1]]));
        assert!(evidence.provider.starts_with("rust:"));
    }
    assert_eq!(
        response.unresolved.len(),
        1,
        "external Debug target must remain explicit"
    );
    let exports = sonar::analysis::dependencies(&db, "src/lib.rs", &Default::default()).unwrap();
    let service = exports
        .relationships
        .iter()
        .find(|g| g.to == "src/service.rs")
        .unwrap();
    assert_eq!(
        service
            .kinds
            .iter()
            .find(|k| k.kind == sonar::analysis::DependencyKind::ReExports)
            .unwrap()
            .occurrences,
        2
    );
    assert!(
        exports
            .unresolved
            .iter()
            .any(|u| u.reason == "glob_expansion_unavailable")
    );
    assert_eq!(before, fs::read(&db).unwrap());
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"changed-import-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    let stale = sonar::analysis::dependencies(&db, "src/consumer.rs", &Default::default()).unwrap();
    assert!(
        !stale
            .relationships
            .iter()
            .flat_map(|g| &g.kinds)
            .any(|k| k.kind == sonar::analysis::DependencyKind::Imports)
    );
    assert_eq!(stale.unresolved.len(), 3);
}

#[test]
fn real_compiler_impact_maps_changed_symbols_and_their_consumers() {
    let Some(analyzer) = test_toolchains::rust_analyzer() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"sonar-impact-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "mod service; mod consumer;\n").unwrap();
    fs::write(
        root.join("src/service.rs"),
        "pub fn value() -> u32 {\n 1\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/consumer.rs"),
        "pub fn run() -> u32 { crate::service::value() }\n",
    )
    .unwrap();
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    for args in [
        vec!["init", "-q"],
        vec!["add", "--all"],
        vec![
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
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(
        root.join("src/service.rs"),
        "pub fn value() -> u32 {\n 2\n}\n",
    )
    .unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    sonar::resolve(&db, &analyzer, None).unwrap();
    let report = sonar::analysis::impact(
        &db,
        &sonar::analysis::ImpactOptions {
            max_files: 3,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.changes_total, 1);
    let change = &report.changes[0];
    assert_eq!(change.declarations[0].symbol.as_deref(), Some("value"));
    let caller = change
        .consumers
        .iter()
        .find(|c| c.location.symbol.as_deref() == Some("run"))
        .unwrap();
    assert_eq!(caller.kind, "calls");
    assert_eq!(caller.location.path, "src/consumer.rs");
    assert_eq!(caller.target, change.declarations[0].handle);
    assert!(
        change
            .cluster
            .as_ref()
            .unwrap()
            .files
            .contains(&"src/consumer.rs".into())
    );
    assert!(
        !change
            .gaps
            .contains(&"current_compiler_consumers_unavailable".into())
    );
}

#[test]
fn baseline_compiler_finds_consumers_of_a_removed_function() {
    let Some(analyzer) = test_toolchains::rust_analyzer() else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname=\"sonar-baseline-fixture\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), "mod service; mod consumer;\n").unwrap();
    fs::write(root.join("src/service.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    let old_caller = "pub fn run() -> u32 { crate::service::value() }\n";
    fs::write(root.join("src/consumer.rs"), old_caller).unwrap();
    assert!(
        Command::new("cargo")
            .args(["generate-lockfile", "--offline"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    for args in [
        vec!["init", "-q"],
        vec!["add", "--all"],
        vec![
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
        ],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(root.join("src/lib.rs"), "mod consumer;\n").unwrap();
    fs::remove_file(root.join("src/service.rs")).unwrap();
    fs::write(root.join("src/consumer.rs"), "pub fn run() -> u32 { 0 }\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    sonar::resolve(&db, &analyzer, None).unwrap();
    let before = fs::read(&db).unwrap();
    let report = sonar::analysis::impact(
        &db,
        &sonar::analysis::ImpactOptions {
            max_files: 3,
            baseline_resolve: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        report
            .baseline
            .compilers
            .iter()
            .any(|c| c.language == "rust" && c.status == "prepared"),
        "{:?}",
        report.baseline.compilers
    );
    let change = report
        .changes
        .iter()
        .find(|c| c.path == "src/service.rs")
        .unwrap();
    let caller = change
        .baseline_consumers
        .iter()
        .find(|c| c.location.symbol.as_deref() == Some("run") && c.kind == "calls")
        .unwrap();
    assert_eq!(caller.location.snapshot, "baseline");
    assert_eq!(
        caller.location.commit.as_deref(),
        Some(report.base_commit.as_str())
    );
    let [start, end] = caller.evidence.bytes;
    assert_eq!(&old_caller[start..end], "value");
    assert!(!caller.location.id.starts_with("sn1:"));
    assert!(change.declarations.is_empty());
    assert!(
        change
            .baseline_cluster
            .as_ref()
            .unwrap()
            .files
            .contains(&"src/consumer.rs".into())
    );
    assert!(change.relationship_changes.iter().any(|r| r.kind
        == sonar::analysis::DependencyKind::Calls
        && r.status == "observed_removed"));
    let first = sonar::analysis::impact(
        &db,
        &sonar::analysis::ImpactOptions {
            baseline_resolve: true,
            limit: 1,
            ..Default::default()
        },
    )
    .unwrap();
    let cursor = first.next_cursor.unwrap();
    let second = sonar::analysis::impact(
        &db,
        &sonar::analysis::ImpactOptions {
            baseline_resolve: true,
            limit: 1,
            cursor: Some(cursor),
            ..Default::default()
        },
    )
    .unwrap();
    assert_ne!(
        first.changes[0].path, second.changes[0].path,
        "compiler-backed continuations must survive new temporary roots"
    );
    assert_eq!(before, fs::read(&db).unwrap());
}
