use sonar::analysis::{ReviewPackOptions, ReviewPackResponse, review_pack};
use std::{collections::BTreeSet, fs, path::PathBuf, process::Command};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        for (path, text) in files {
            let p = root.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        let f = Self {
            db: temp.path().join("index.sqlite"),
            _temp: temp,
            root,
        };
        f.git(&["init", "-q"]);
        f.git(&["add", "--all"]);
        f.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "base",
        ]);
        f.index();
        f
    }
    fn git(&self, args: &[&str]) -> String {
        let p = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
        String::from_utf8(p.stdout).unwrap()
    }
    fn index(&self) {
        sonar::index(&self.root, &self.db).unwrap();
        sonar::index_navigation(&self.db).unwrap();
    }
    fn validate(&self, response: &ReviewPackResponse) {
        for pack in &response.packs {
            assert_eq!(
                serde_json::to_vec(pack).unwrap().len(),
                pack.estimated_tokens
            );
            assert!(pack.estimated_tokens <= pack.max_tokens);
            assert!(pack.reading_path.len() <= pack.max_files);
            let mut seen = BTreeSet::new();
            for file in &pack.reading_path {
                assert!(seen.insert((&file.snapshot, &file.path)));
                let source = if file.snapshot == "current" {
                    fs::read_to_string(self.root.join(&file.path)).unwrap()
                } else {
                    self.git(&[
                        "show",
                        &format!("{}:{}", pack.base_commit.as_ref().unwrap(), file.path),
                    ])
                };
                let lines = source.split_inclusive('\n').collect::<Vec<_>>();
                let mut included = 0;
                for excerpt in &file.excerpts {
                    let [a, b] = excerpt.range;
                    assert!(a > 0 && a <= b && b <= lines.len());
                    assert_eq!(excerpt.text, lines[a - 1..b].concat());
                    included += b - a + 1;
                }
                assert_eq!(included, file.included_lines);
                assert!(included <= file.requested_lines);
            }
        }
    }
}

#[test]
fn audits_page_all_areas_and_bind_continuations_to_budget_and_snapshot() {
    let f = Fixture::new(&[
        ("a.py", "def alpha():\n    return 1\n"),
        ("b.ts", "export function beta() { return 2; }\n"),
        ("c.go", "package c\nfunc Gamma() {}\n"),
    ]);
    let mut options = ReviewPackOptions {
        max_files: 1,
        limit: 1,
        ..Default::default()
    };
    let first = review_pack(&f.db, &options).unwrap();
    f.validate(&first);
    assert_eq!(first.packs_total, 3);
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(review_pack(&f.db, &options).unwrap()).unwrap()
    );
    let mut ids = BTreeSet::new();
    let mut response = first;
    loop {
        for p in &response.packs {
            assert!(ids.insert(p.id.clone()));
        }
        let Some(cursor) = response.next_cursor else {
            break;
        };
        options.cursor = Some(cursor);
        response = review_pack(&f.db, &options).unwrap();
        f.validate(&response);
    }
    assert_eq!(ids.len(), 3);
    let mut changed_budget = options.clone();
    changed_budget.max_tokens += 1;
    assert!(
        review_pack(&f.db, &changed_budget)
            .unwrap_err()
            .to_string()
            .contains("different options")
    );
    fs::write(f.root.join("a.py"), "def alpha():\n    return 4\n").unwrap();
    assert!(review_pack(&f.db, &options).is_err());
    f.index();
    assert!(
        review_pack(&f.db, &options)
            .unwrap_err()
            .to_string()
            .contains("another query")
    );
}

#[test]
fn excerpts_are_exact_and_bounded_for_unicode_crlf_and_oversized_lines() {
    let text = format!(
        "def alpha():\r\n    # {}\r\n    return '雪\\\"'\r\n",
        "雪\\\"".repeat(8000)
    );
    let f = Fixture::new(&[("a.py", &text)]);
    let options = ReviewPackOptions {
        seed: Some("a.py".into()),
        max_tokens: 2048,
        ..Default::default()
    };
    let response = review_pack(&f.db, &options).unwrap();
    f.validate(&response);
    let p = &response.packs[0];
    assert!(p.truncated);
    assert_eq!(p.reading_path.len(), 1);
    assert!(!p.reading_path[0].excerpts.is_empty());
    assert!(p.reading_path[0].included_lines < p.reading_path[0].requested_lines);
    let larger = review_pack(
        &f.db,
        &ReviewPackOptions {
            max_tokens: 65536,
            ..options
        },
    )
    .unwrap();
    assert_ne!(p.id, larger.packs[0].id);
    f.validate(&larger);
}

#[test]
fn changes_include_before_after_and_deleted_source_with_snapshot_provenance() {
    let f = Fixture::new(&[
        ("lib.rs", "mod service;\nmod removed;\n"),
        ("service.rs", "pub fn value() -> u32 { 1 }\n"),
        ("removed.rs", "pub fn old() {}\n"),
    ]);
    fs::write(f.root.join("service.rs"), "pub fn value() -> u64 { 2 }\n").unwrap();
    fs::remove_file(f.root.join("removed.rs")).unwrap();
    f.index();
    let before = f.git(&["status", "--porcelain=v1"]);
    let db_before = fs::read(&f.db).unwrap();
    let options = ReviewPackOptions {
        base: Some("HEAD".into()),
        max_files: 2,
        ..Default::default()
    };
    let response = review_pack(&f.db, &options).unwrap();
    f.validate(&response);
    assert_eq!(response.packs_total, 2);
    let changed = response
        .packs
        .iter()
        .find(|p| p.change_path.as_deref() == Some("service.rs"))
        .unwrap();
    assert!(
        changed
            .declaration_changes
            .iter()
            .any(|d| d.signature_changed == Some(true))
    );
    assert!(changed.reading_path.iter().any(|f| f.snapshot == "current"));
    assert!(
        changed
            .reading_path
            .iter()
            .any(|f| f.snapshot == "baseline")
    );
    let removed = response
        .packs
        .iter()
        .find(|p| p.change_path.as_deref() == Some("removed.rs"))
        .unwrap();
    assert!(
        removed
            .reading_path
            .iter()
            .any(|f| f.snapshot == "baseline" && !f.excerpts.is_empty())
    );
    assert_eq!(before, f.git(&["status", "--porcelain=v1"]));
    assert_eq!(db_before, fs::read(&f.db).unwrap());
    assert_eq!(
        serde_json::to_value(&response).unwrap(),
        serde_json::to_value(review_pack(&f.db, &options).unwrap()).unwrap()
    );
    let tight = review_pack(
        &f.db,
        &ReviewPackOptions {
            max_files: 1,
            max_tokens: 2048,
            ..options
        },
    )
    .unwrap();
    f.validate(&tight);
    assert!(tight.packs.iter().all(|p| p.reading_path.len() <= 1));
}

#[test]
fn validates_options_and_cli_produces_the_same_pack() {
    let f = Fixture::new(&[("a.py", "def alpha():\n    pass\n")]);
    for options in [
        ReviewPackOptions {
            max_tokens: 1,
            ..Default::default()
        },
        ReviewPackOptions {
            seed: Some("a.py".into()),
            base: Some("HEAD".into()),
            ..Default::default()
        },
        ReviewPackOptions {
            baseline_resolve: true,
            ..Default::default()
        },
        ReviewPackOptions {
            max_files: 51,
            ..Default::default()
        },
    ] {
        assert!(review_pack(&f.db, &options).is_err());
    }
    let p = Command::new(env!("CARGO_BIN_EXE_sonar"))
        .args([
            "--db",
            f.db.to_str().unwrap(),
            "review-pack",
            "--seed",
            "a.py",
            "--max-tokens",
            "4096",
        ])
        .current_dir(&f.root)
        .output()
        .unwrap();
    assert!(p.status.success(), "{}", String::from_utf8_lossy(&p.stderr));
    let cli: serde_json::Value = serde_json::from_slice(&p.stdout).unwrap();
    let api = review_pack(
        &f.db,
        &ReviewPackOptions {
            seed: Some("a.py".into()),
            max_tokens: 4096,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cli, serde_json::to_value(api).unwrap());
}
