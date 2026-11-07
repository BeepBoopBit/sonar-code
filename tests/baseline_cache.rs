use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

#[path = "../src/test_toolchains.rs"]
mod test_toolchains;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    cache: PathBuf,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        for (path, source) in files {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, source).unwrap();
        }
        let f = Self {
            db: temp.path().join("index.sqlite"),
            cache: temp.path().join("cache"),
            _temp: temp,
            root,
        };
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
            let p = Command::new("git")
                .arg("-C")
                .arg(&f.root)
                .args(args)
                .output()
                .unwrap();
            assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        }
        f
    }
    fn standard() -> Self {
        let f = Self::new(&[("lib.rs", "fn value() -> u32 { 1 }\n")]);
        fs::write(f.root.join("lib.rs"), "fn value() -> u32 { 2 }\n").unwrap();
        sonar::index(&f.root, &f.db).unwrap();
        f
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sonar"));
        command
            .arg("--db")
            .arg(&self.db)
            .args(args)
            .current_dir(&self.root)
            .env("SONAR_BASELINE_CACHE", &self.cache);
        command
    }
    fn run(&self, args: &[&str]) -> Value {
        let p = self.command(args).output().unwrap();
        assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        serde_json::from_slice(&p.stdout).unwrap()
    }
    fn entry(&self, mode: &str) -> Value {
        self.run(&["baseline-cache", "list"])["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["mode"] == mode)
            .unwrap()
            .clone()
    }
}

#[test]
fn warm_syntax_reports_are_identical_and_corruption_or_policy_changes_rebuild() {
    let f = Fixture::standard();
    let before = fs::read(&f.db).unwrap();
    let cold = f.run(&["impact"]);
    let entry = f.entry("syntax");
    let key = entry["key"].as_str().unwrap();
    let path = f.cache.join("entries").join(key);
    let old_index = fs::read(path.join("index.sqlite")).unwrap();
    let warm = f.run(&["impact"]);
    assert_eq!(cold, warm);
    assert_eq!(old_index, fs::read(path.join("index.sqlite")).unwrap());
    assert_eq!(entry["created_ms"], f.entry("syntax")["created_ms"]);
    fs::write(path.join("source/lib.rs"), "fn corrupted() {}\n").unwrap();
    assert_eq!(cold, f.run(&["impact"]));
    assert_eq!(
        fs::read_to_string(path.join("source/lib.rs")).unwrap(),
        "fn value() -> u32 { 1 }\n"
    );
    let mut ready: Value =
        serde_json::from_slice(&fs::read(path.join("ready.json")).unwrap()).unwrap();
    ready["policy"] = json!("obsolete-analysis");
    fs::write(path.join("ready.json"), serde_json::to_vec(&ready).unwrap()).unwrap();
    assert_eq!(
        f.run(&["baseline-cache", "list"])["entries"][0]["state"],
        "obsolete"
    );
    assert_eq!(cold, f.run(&["impact"]));
    assert_eq!(before, fs::read(&f.db).unwrap());
}

#[test]
fn independent_processes_share_one_complete_entry_and_cleanup_is_explicit() {
    let f = Fixture::standard();
    let first = f
        .command(&["impact"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let second = f.command(&["impact"]).output().unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let first = first.wait_with_output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&first.stdout).unwrap(),
        serde_json::from_slice::<Value>(&second.stdout).unwrap()
    );
    let list = f.run(&["baseline-cache", "list"]);
    assert_eq!(list["entries"].as_array().unwrap().len(), 1);
    assert_eq!(list["entries"][0]["state"], "ready");
    let prune = f.run(&["baseline-cache", "prune", "--all"]);
    assert_eq!(prune["entries_remaining"], 0);
    assert!(prune["freed_bytes"].as_u64().unwrap() > 0);
    assert!(
        f.run(&["baseline-cache", "list"])["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn maintenance_requires_no_project_initialization_and_respects_limits() {
    let f = Fixture::new(&[("lib.rs", "fn value() {}\n")]);
    assert!(
        f.run(&["baseline-cache", "list"])["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fs::write(f.root.join("lib.rs"), "fn value() { let x=1; }\n").unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    f.run(&["impact"]);
    let prune = f.run(&["baseline-cache", "prune", "--max-bytes", "1"]);
    assert_eq!(prune["entries_remaining"], 0);
    assert_eq!(prune["over_limit"], false);
}

#[test]
fn compiler_inputs_and_environment_invalidate_without_rebuilding_syntax() {
    let Some(compiler) = test_toolchains::typescript() else {
        return;
    };
    let f = Fixture::new(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"target":"ESNext","module":"ESNext","moduleResolution":"Bundler"}}"#,
        ),
        (
            "service.ts",
            "export function value(): number { return 1; }\n",
        ),
        (
            "consumer.ts",
            "import {value} from './service';\nexport function run() { return value(); }\n",
        ),
    ]);
    let wrapper = f.root.parent().unwrap().join("compiler.js");
    fs::write(
        &wrapper,
        format!(
            "module.exports = require({});\n",
            serde_json::to_string(&compiler).unwrap()
        ),
    )
    .unwrap();
    fs::write(
        f.root.join("service.ts"),
        "export function value(): number { return 2; }\n",
    )
    .unwrap();
    sonar::index(&f.root, &f.db).unwrap();
    sonar::resolve_typescript(&f.db, Path::new("node"), Some(&wrapper), None).unwrap();
    let cold = f.run(&["impact", "--baseline-resolve"]);
    let syntax = f.entry("syntax");
    let compiled = f.entry("compiler");
    assert_eq!(cold, f.run(&["impact", "--baseline-resolve"]));
    assert_eq!(compiled["created_ms"], f.entry("compiler")["created_ms"]);
    let key = compiled["key"].as_str().unwrap();
    let ready_path = f.cache.join("entries").join(key).join("ready.json");
    let old: Value = serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
    fs::write(
        &wrapper,
        format!(
            "module.exports = require({}); // changed input\n",
            serde_json::to_string(&compiler).unwrap()
        ),
    )
    .unwrap();
    let rebuilt = f.run(&["impact", "--baseline-resolve"]);
    assert_eq!(rebuilt["baseline"]["compilers"][0]["status"], "prepared");
    let new: Value = serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
    assert_ne!(old["created_ms"], new["created_ms"]);
    assert_eq!(syntax["created_ms"], f.entry("syntax")["created_ms"]);
    let p = f
        .command(&["impact", "--baseline-resolve"])
        .env("SONAR_CACHE_TEST_BUILD_REVISION", "changed")
        .output()
        .unwrap();
    assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
    let entries = f.run(&["baseline-cache", "list"]);
    assert_eq!(
        entries["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["mode"] == "compiler")
            .count(),
        2
    );
    let p = f
        .command(&["impact", "--baseline-resolve"])
        .env("SONAR_BASELINE_CACHE_COMPILER", "0")
        .output()
        .unwrap();
    assert!(p.status.success());
    assert_eq!(
        f.run(&["baseline-cache", "list"])["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // A version-readable compiler that fails actual preparation must not be published.
    fs::write(
        &wrapper,
        "module.exports = { version: 'broken-test-compiler' };\n",
    )
    .unwrap();
    let failed = f.run(&["impact", "--baseline-resolve"]);
    assert_eq!(failed["baseline"]["compilers"][0]["status"], "failed");
    assert_eq!(
        f.run(&["baseline-cache", "list"])["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}
