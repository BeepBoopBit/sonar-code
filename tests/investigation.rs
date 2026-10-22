use std::{fs, process::Command};

#[test]
fn cli_discovery_and_chosen_handle_share_the_library_contract_and_filters() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn policy() {}\n").unwrap();
    fs::write(root.join("guide.md"), "# Policy\n[owner](lib.rs#L1)\n").unwrap();
    fs::write(root.join("old.md"), "# Policy\nold policy behavior\n").unwrap();
    let db = temp.path().join("index.sqlite");
    sonar::index(&root, &db).unwrap();
    sonar::index_navigation(&db).unwrap();
    let engine = sonar::Investigator::open(&db, None).unwrap();
    let opts = sonar::InvestigationOptions {
        exclusions: vec!["old.md".into()],
        ..Default::default()
    };
    let expected = engine.investigate("explain policy", &opts).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "investigate",
            "explain policy",
            "--exclude",
            "old.md",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value, serde_json::to_value(expected).unwrap());
    let handle = sonar::locate(&db, "lib.rs", 1, None)
        .unwrap()
        .locations
        .remove(0)
        .handle;
    let result = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "investigate",
            "policy",
            "--from",
            &handle,
            "--scope",
            "lib.rs",
            "--exclude",
            "old.md",
            "--exclude",
            "guide.md",
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    let value: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["anchor"], "n1");
    assert_eq!(value["locations"].as_array().unwrap().len(), 1);
    assert_eq!(value["links"].as_array().unwrap().len(), 0);
    assert!(
        value["notices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["code"] == "graph_unavailable")
    );
    let result = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .args([
            "--db",
            db.to_str().unwrap(),
            "investigate",
            "policy",
            "--exclude",
            "../escape",
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
}
