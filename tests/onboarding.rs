use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Project {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}
impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repository with spaces");
        let home = temp.path().join("home");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&home).unwrap();
        fs::write(
            root.join("lib.rs"),
            "/// Authenticate requests.\nfn authenticate_request() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("README.md"),
            "# Authentication\n[code](lib.rs#L2)\n",
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            home,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sonar"));
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.home.join("codex"))
            .env_remove("CLAUDE_CONFIG_DIR");
        command
    }
    fn run(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
fn failed(output: Output) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn init_remembers_custom_database_and_resources_and_adopts_existing_indexes() {
    let project = Project::new();
    assert_eq!(project.run(&["status"])["state"], "not_initialized");
    failed(
        project
            .command()
            .args(["brief", "authentication"])
            .output()
            .unwrap(),
    );
    assert!(!project.root.join(".sonar").exists());
    fs::write(
        project.root.join("settings.yaml"),
        "timeout: authentication\n",
    )
    .unwrap();
    let db = project.home.join("custom index.sqlite");
    let initialized = project.run(&["--db", db.to_str().unwrap(), "init", "--resources", "yaml"]);
    assert_eq!(initialized["initialized"], true);
    assert!(db.is_file());
    assert!(!project.root.join(".sonar/index.sqlite").exists());
    let config = fs::read_to_string(project.root.join(".sonar/project.json")).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&config).unwrap()["resources"],
        "yaml"
    );
    fs::write(project.root.join("new.yaml"), "authentication: enabled\n").unwrap();
    project.run(&["init"]);
    assert_eq!(project.run(&["status"])["database"], db.to_str().unwrap());
    assert_eq!(
        project.run(&["status"])["index"]["syntax_freshness"],
        "current"
    );
    let brief = project.run(&["brief", "authentication", "--scope", "new.yaml"]);
    assert!(
        brief["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "new.yaml")
    );
    assert_eq!(
        fs::read_to_string(project.root.join(".gitignore")).unwrap(),
        "/.sonar/\n"
    );
    let child = project.root.join("nested");
    fs::create_dir(&child).unwrap();
    let output = project
        .command()
        .current_dir(&child)
        .args(["brief", "authenticate_request"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Legacy indexes remain usable without a separate initialization marker.
    fs::remove_file(project.root.join(".sonar/project.json")).unwrap();
    assert_eq!(
        project.run(&["--db", db.to_str().unwrap(), "status"])["initialized"],
        true
    );
}

#[test]
fn codex_and_claude_setup_preserve_configuration_and_are_repeatable() {
    let project = Project::new();
    fs::create_dir(project.root.join(".codex")).unwrap();
    fs::write(
        project.root.join(".codex/config.toml"),
        "# Keep settings\nmodel = 'keep'\n[mcp_servers.other]\ncommand = 'other'\n",
    )
    .unwrap();
    fs::write(
        project.root.join(".mcp.json"),
        r#"{"mcpServers":{"other":{"command":"other"}},"custom":true}"#,
    )
    .unwrap();
    fs::write(
        project.root.join("AGENTS.md"),
        "Preserve project guidance.\n",
    )
    .unwrap();
    fs::write(
        project.root.join("CLAUDE.md"),
        "Preserve Claude guidance.\n",
    )
    .unwrap();
    for client in ["codex", "claude"] {
        let report = project.run(&["setup", client]);
        assert_eq!(report["mcp_verified"], true);
        assert_eq!(report["scope"], "project");
        let config = Path::new(report["configuration"].as_str().unwrap());
        let instructions = Path::new(report["instructions"].as_str().unwrap());
        let before = fs::read(config).unwrap();
        let guidance = fs::read_to_string(instructions).unwrap();
        assert_eq!(guidance.matches("<!-- sonar:begin -->").count(), 1);
        assert!(guidance.contains("Preserve"));
        project.run(&["setup", client]);
        assert_eq!(fs::read(config).unwrap(), before);
        assert_eq!(fs::read_to_string(instructions).unwrap(), guidance);
        assert_eq!(
            project.run(&["status"])["index"]["syntax_freshness"],
            "current"
        );
    }
    let toml = fs::read_to_string(project.root.join(".codex/config.toml")).unwrap();
    assert!(toml.contains("# Keep settings"));
    assert!(toml.contains("model = 'keep'"));
    assert!(toml.contains("mcp_servers.other"));
    let claude: Value =
        serde_json::from_slice(&fs::read(project.root.join(".mcp.json")).unwrap()).unwrap();
    assert_eq!(claude["custom"], true);
    assert_eq!(claude["mcpServers"]["other"]["command"], "other");
    assert!(fs::read_dir(&project.home).unwrap().next().is_none());
}

#[test]
fn setup_conflicts_and_failed_preparation_preserve_client_files_and_instructions() {
    let project = Project::new();
    let conflict = r#"{"mcpServers":{"sonar":{"command":"another"}}}"#;
    fs::write(project.root.join(".mcp.json"), conflict).unwrap();
    fs::write(project.root.join("CLAUDE.md"), "Keep this.\n").unwrap();
    failed(
        project
            .command()
            .args(["setup", "claude"])
            .output()
            .unwrap(),
    );
    assert!(!project.root.join(".sonar").exists());
    assert_eq!(
        fs::read_to_string(project.root.join(".mcp.json")).unwrap(),
        conflict
    );
    assert_eq!(
        fs::read_to_string(project.root.join("CLAUDE.md")).unwrap(),
        "Keep this.\n"
    );
    failed(
        project
            .command()
            .args(["setup", "claude", "--replace", "--model", "missing model"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        fs::read_to_string(project.root.join(".mcp.json")).unwrap(),
        conflict
    );
    assert_eq!(
        fs::read_to_string(project.root.join("CLAUDE.md")).unwrap(),
        "Keep this.\n"
    );
    assert_eq!(
        project.run(&["setup", "claude", "--replace"])["mcp_verified"],
        true
    );
}

#[test]
fn global_registration_uses_distinct_names_and_isolated_user_configuration() {
    let project = Project::new();
    let codex = project.run(&["setup", "codex", "--global"]);
    assert_eq!(codex["scope"], "user");
    assert!(
        codex["server_name"]
            .as_str()
            .unwrap()
            .starts_with("sonar-repository-with-spaces-")
    );
    assert_eq!(
        Path::new(codex["configuration"].as_str().unwrap()),
        project.home.join("codex/config.toml")
    );
    let claude = project.run(&["setup", "claude", "--global"]);
    assert_eq!(
        Path::new(claude["configuration"].as_str().unwrap()),
        project.home.join(".claude.json")
    );
    assert!(!project.root.join(".mcp.json").exists());
    assert!(!project.root.join(".codex").exists());
}

#[test]
fn missing_optional_models_do_not_hide_project_status() {
    let project = Project::new();
    project.run(&["init"]);
    let db = project.root.join(".sonar/index.sqlite");
    let status = sonar::project_status(
        &db,
        Some(&project.home.join("missing model")),
        Some(&project.root),
    )
    .unwrap();
    assert_eq!(status["initialized"], true);
    assert_eq!(status["index"]["syntax_freshness"], "current");
    assert!(status["model_error"].is_string());
}

#[test]
fn moved_project_settings_cannot_redirect_initialization_to_the_original_project() {
    let project = Project::new();
    project.run(&["init"]);
    let path = project.root.join(".sonar/project.json");
    let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let original = project.home.join("original");
    fs::create_dir(&original).unwrap();
    config["root"] = serde_json::json!(original);
    config["db"] = serde_json::json!(original.join("index.sqlite"));
    fs::write(path, serde_json::to_string(&config).unwrap()).unwrap();
    let output = project.command().args(["setup", "codex"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("moved or copied"));
    assert!(fs::read_dir(original).unwrap().next().is_none());
}
