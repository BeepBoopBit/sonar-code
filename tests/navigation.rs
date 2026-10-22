use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use sonar::{Brief, BriefOptions, brief, index};
use tempfile::TempDir;

struct Project {
    _temp: TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let db = temp.path().join("index.sqlite");
        Self {
            _temp: temp,
            root,
            db,
        }
    }

    fn write(&self, name: &str, source: &str) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn query(&self, query: &str) -> Brief {
        brief(&self.db, query, &BriefOptions::default()).unwrap()
    }
}

fn starting_symbols(response: &Brief) -> Vec<&str> {
    response
        .starts
        .iter()
        .filter_map(|id| {
            response
                .files
                .iter()
                .flat_map(|file| &file.locations)
                .find(|location| &location.id == id)
                .and_then(|location| location.symbol.as_deref())
        })
        .collect()
}

#[test]
fn markdown_sections_are_navigable_and_invalidated_on_edit() {
    let project = Project::new();
    project.write("guide.md", "# Getting started\nordinary introduction\n\n# Credentials\nKernel-supplied identity establishes trust.\n```rust\n# not a heading\n```\n");
    project.write("lib.rs", "/// Caller identity.\nfn verify() {}\n");
    index(&project.root, &project.db).unwrap();
    let brief = project.query("Kernel supplied trust");
    let docs = brief.files.iter().find(|f| f.path == "guide.md").unwrap();
    assert!(
        docs.locations
            .iter()
            .all(|l| l.symbol.is_none() && l.definition.is_none())
    );
    assert!(
        docs.locations
            .iter()
            .any(|l| l.focus.is_some_and(|r| r[0] <= 5 && r[1] >= 5))
    );
    let conn = rusqlite::Connection::open(&project.db).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM locations WHERE kind='documentation'",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        2
    );
    project.write("guide.md", "# Replaced\nNo retained credentials text.\n");
    assert!(
        project
            .query("Kernel supplied trust")
            .files
            .iter()
            .all(|f| f.path != "guide.md")
    );
}

#[test]
fn navigates_to_symbols_and_emits_only_evidenced_containment() {
    let project = Project::new();
    project.write("src/lib.rs", "struct SpawnPool;\nimpl SpawnPool {\n    /// Start configured spawn workers.\n    fn start_workers(&self) {}\n}\nfn caller() { SpawnPool.start_workers(); }\n");
    index(&project.root, &project.db).unwrap();
    let response = project.query("SpawnPool start_workers");
    assert_eq!(starting_symbols(&response)[0], "SpawnPool::start_workers");
    assert_eq!(response.index.syntax_freshness, "current");
    assert_eq!(response.index.semantic_freshness, "unavailable");
    assert!(response.index.semantic_profile.is_none());
    assert!(!response.links.is_empty());
    for link in &response.links {
        assert_eq!(link.kind, "contains");
        let locations: Vec<_> = response
            .files
            .iter()
            .flat_map(|file| &file.locations)
            .collect();
        assert!(locations.iter().any(|location| location.id == link.from));
        assert!(locations.iter().any(|location| location.id == link.to));
        assert_eq!(link.evidence.path, "src/lib.rs");
    }
    let encoded = serde_json::to_value(&response).unwrap();
    assert!(encoded["files"][0]["locations"][0].get("source").is_none());
    assert!(encoded.get("score").is_none());
}

#[test]
fn distinguishes_trait_impls_and_inline_module_scopes() {
    let project = Project::new();
    project.write("src/lib.rs", "mod nested {\nstruct Config { spawn_workers: usize }\nimpl Default for Config {\n    fn default() -> Self { Self { spawn_workers: 8 } }\n}\n}\n");
    index(&project.root, &project.db).unwrap();
    let response = project.query("Config default");
    assert!(starting_symbols(&response).contains(&"nested::<Config as Default>::default"));
    assert_eq!(project.query("spawn_workers").files[0].path, "src/lib.rs");
}

#[test]
fn finds_a_focus_region_deep_inside_a_long_function() {
    let project = Project::new();
    let mut source = "fn assemble() {\n".to_owned();
    source.push_str(&"    do_work();\n".repeat(90));
    source.push_str("    let quarantine_marker = true;\n}\n");
    project.write("src/large.rs", &source);
    index(&project.root, &project.db).unwrap();
    let response = project.query("quarantine_marker");
    let location = response
        .files
        .iter()
        .flat_map(|file| &file.locations)
        .find(|location| location.id == response.starts[0])
        .unwrap();
    assert_eq!(location.symbol.as_deref(), Some("assemble"));
    assert_eq!(location.definition, Some(1));
    let [start, end] = location.focus.unwrap();
    assert!(start <= 92 && end >= 92);
    assert!(end - start < 8);
}

#[test]
fn symbol_name_matches_do_not_move_focus_to_the_short_tail() {
    let project = Project::new();
    let mut source = "fn start_workers() {\n    let configured_workers = 8;\n".to_owned();
    source.push_str(&"    do_work();\n".repeat(30));
    source.push_str("}\n");
    project.write("lib.rs", &source);
    index(&project.root, &project.db).unwrap();
    let response = project.query("start_workers configured");
    let location = response
        .files
        .iter()
        .flat_map(|file| &file.locations)
        .find(|location| location.id == response.starts[0])
        .unwrap();
    assert_eq!(location.symbol.as_deref(), Some("start_workers"));
    let [start, end] = location.focus.unwrap();
    assert!(start <= 2 && end >= 2);
}

#[test]
fn incremental_updates_remove_stale_fts_rows_and_reuse_unchanged_files() {
    let project = Project::new();
    project.write("a.rs", "fn obsolete_marker() {}\n");
    project.write("b.rs", "fn remove_marker() {}\n");
    let first = index(&project.root, &project.db).unwrap();
    let unchanged = index(&project.root, &project.db).unwrap();
    assert_eq!(unchanged.changed, 0);
    assert_eq!(unchanged.snapshot, first.snapshot);
    project.write("a.rs", "fn replacement_marker() {}\n");
    fs::remove_file(project.root.join("b.rs")).unwrap();
    project.write("c.rs", "fn added_marker() {}\n");
    let updated = index(&project.root, &project.db).unwrap();
    assert_eq!((updated.changed, updated.removed, updated.files), (2, 1, 2));
    assert_ne!(updated.snapshot, first.snapshot);
    assert!(!starting_symbols(&project.query("replacement_marker")).is_empty());
    let conn = rusqlite::Connection::open(&project.db).unwrap();
    let orphaned: i64 = conn
        .query_row(
            "SELECT count(*) FROM search WHERE rowid NOT IN (SELECT id FROM regions)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphaned, 0);
    let old_tokens: i64 = conn
        .query_row(
            "SELECT count(*) FROM search WHERE search MATCH 'obsolete OR remove'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_tokens, 0);
}

#[test]
fn stale_files_are_omitted_and_additions_invalidate_freshness() {
    let project = Project::new();
    project.write("a.rs", "fn alpha() {}\n");
    project.write("b.rs", "fn beta() {}\n");
    index(&project.root, &project.db).unwrap();
    project.write("a.rs", "fn gamma() {}\n");
    let stale = project.query("alpha beta");
    assert_eq!(stale.index.syntax_freshness, "stale");
    assert!(stale.files.iter().all(|file| file.path != "a.rs"));
    assert!(starting_symbols(&stale).contains(&"beta"));
    index(&project.root, &project.db).unwrap();
    project.write("c.rs", "fn delta() {}\n");
    let stale = project.query("delta");
    assert_eq!(stale.index.syntax_freshness, "stale");
    assert!(stale.starts.is_empty());
}

#[test]
fn ignores_build_vendor_and_ignored_files_without_requiring_git() {
    let project = Project::new();
    project.write(".gitignore", "ignored.rs\n");
    for path in [
        "src/lib.rs",
        "ignored.rs",
        "target/debug/build.rs",
        "vendor/lib.rs",
        ".hidden/file.rs",
    ] {
        project.write(path, "fn needle() {}\n");
    }
    let report = index(&project.root, &project.db).unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(project.query("needle").files.len(), 1);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            project.root.join("src/lib.rs"),
            project.root.join("link.rs"),
        )
        .unwrap();
        assert_eq!(index(&project.root, &project.db).unwrap().files, 1);
    }
}

#[test]
fn scope_is_a_literal_path_and_query_cannot_inject_fts_operators() {
    let project = Project::new();
    project.write("src/lib.rs", "fn HTTPServer() {}\n");
    project.write("src_extra/lib.rs", "fn HTTPServer() {}\n");
    project.write("odd%dir/lib.rs", "fn HTTPServer() {}\n");
    index(&project.root, &project.db).unwrap();
    for scope in ["src", "odd%dir"] {
        let options = BriefOptions {
            scope: Some(scope.into()),
            ..Default::default()
        };
        let response = brief(&project.db, "http server\" OR name:* NOT \"", &options).unwrap();
        assert!(!response.starts.is_empty());
        assert!(
            response
                .files
                .iter()
                .all(|file| file.path.starts_with(&format!("{scope}/")))
        );
    }
    assert!(project.query("\"***:").starts.is_empty());
    assert!(
        brief(
            &project.db,
            "needle",
            &BriefOptions {
                scope: Some("../escape".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[test]
fn reports_parse_errors_and_handles_empty_repositories() {
    let project = Project::new();
    assert_eq!(index(&project.root, &project.db).unwrap().files, 0);
    assert!(project.query("authentication").starts.is_empty());
    project.write("bad.rs", "fn recoverable() {}\nfn broken(\n");
    assert_eq!(
        index(&project.root, &project.db).unwrap().parse_error_files,
        1
    );
    assert!(
        project
            .query("recoverable")
            .notices
            .iter()
            .any(|notice| notice.code == "parse_errors")
    );
}

#[test]
fn failed_update_preserves_the_last_committed_generation() {
    let project = Project::new();
    project.write("a.rs", "fn original() {}\n");
    let before = index(&project.root, &project.db).unwrap();
    project.write("a.rs", "fn replacement() {}\n");
    fs::write(project.root.join("invalid.rs"), [0xff, 0xfe]).unwrap();
    assert!(index(&project.root, &project.db).is_err());
    let conn = rusqlite::Connection::open(&project.db).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'snapshot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, before.snapshot);
}

#[test]
fn refuses_other_databases_and_roots() {
    let project = Project::new();
    let conn = rusqlite::Connection::open(&project.db).unwrap();
    conn.execute("CREATE TABLE unrelated(value TEXT)", [])
        .unwrap();
    assert!(index(&project.root, &project.db).is_err());
    let other_db = project._temp.path().join("sonar.sqlite");
    index(&project.root, &other_db).unwrap();
    let other_repo = project._temp.path().join("other");
    fs::create_dir(&other_repo).unwrap();
    assert!(index(&other_repo, &other_db).is_err());
}

fn cli(db: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sonar"))
        .arg("--db")
        .arg(db)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_produces_json_and_missing_index_does_not_create_a_database() {
    let project = Project::new();
    let missing = cli(&project.db, &["brief", "authentication"]);
    assert!(!missing.status.success());
    assert!(!project.db.exists());
    assert!(missing.stdout.is_empty());
    project.write("lib.rs", "fn authenticate() {}\n");
    let indexed = cli(&project.db, &["index", project.root.to_str().unwrap()]);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let output = cli(
        &project.db,
        &["brief", "authenticate", "--limit", "1", "--related", "0"],
    );
    assert!(output.status.success());
    let response: Brief = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response.starts.len(), 1);
    assert_eq!(starting_symbols(&response), vec!["authenticate"]);
}

#[test]
fn lexical_budget_counts_locations_instead_of_repeated_regions() {
    let project = Project::new();
    let mut source = format!(
        "fn giant() {{\n{}\n}}\n",
        "checksum_protocol();\n".repeat(14_000)
    );
    for i in 0..560 {
        source.push_str(&format!("fn operation_{i}() {{ checksum_protocol(); }}\n"));
    }
    project.write("lib.rs", &source);
    index(&project.root, &project.db).unwrap();
    let engine =
        sonar::SearchEngine::open(&project.db, sonar::RetrievalMode::Lexical, None).unwrap();
    let diagnostic = engine
        .diagnose("checksum protocol", &BriefOptions::default())
        .unwrap();
    assert_eq!(diagnostic.candidates.len(), 512);
    let ids: std::collections::BTreeSet<_> = diagnostic
        .candidates
        .iter()
        .map(|c| c.location_id)
        .collect();
    assert_eq!(ids.len(), 512);
}

#[test]
fn exact_file_lookups_and_metadata_are_source_bound() {
    let project = Project::new();
    project.write("src/weird_name.rs", "fn operation() {}\n");
    project.write("guide.md", "# Actual `heading`\nBody.\n");
    index(&project.root, &project.db).unwrap();
    let response = project.query("src/weird_name.rs");
    assert_eq!(response.files[0].path, "src/weird_name.rs");
    assert_eq!(response.files[0].locations[0].kind, "file");
    let docs = project.query("guide.md");
    let location = &docs.files[0].locations[0];
    assert_eq!(location.title.as_deref(), Some("Actual `heading`"));
    assert_eq!(location.source_role.as_deref(), Some("documentation"));
}

#[test]
fn changing_chunk_policy_reindexes_unchanged_sources_and_invalidates_vectors() {
    let project = Project::new();
    project.write(
        "lib.rs",
        &format!("fn x() {{\n{}\n}}", "if ready { step(); }\n".repeat(60)),
    );
    index(&project.root, &project.db).unwrap();
    let ast = sonar::index_with(&project.root, &project.db, sonar::Chunking::Ast).unwrap();
    assert_eq!(ast.changed, 1);
    assert_eq!(
        sonar::index_with(&project.root, &project.db, sonar::Chunking::Ast)
            .unwrap()
            .changed,
        0
    );
    assert_eq!(index(&project.root, &project.db).unwrap().changed, 1);
}

#[test]
fn syntax_focus_bounds_documentation_and_malformed_source_without_hiding_staleness() {
    let project = Project::new();
    project.write(
        "guide.md",
        "# Credentials\nKernel identity establishes trust.\nMore context.\n",
    );
    project.write("broken.rs", "fn verify() {\n    credentials(\n}\n");
    index(&project.root, &project.db).unwrap();
    let engine = sonar::SearchEngine::open(&project.db, sonar::RetrievalMode::Lexical, None)
        .unwrap()
        .with_focus(sonar::FocusOptions {
            policy: sonar::FocusPolicy::Syntax,
            max_lines: 2,
        });
    let d = engine
        .diagnose("credentials", &BriefOptions::default())
        .unwrap();
    assert!(
        d.focus
            .iter()
            .any(|f| f.path == "guide.md" && f.reason == "descriptive_anchor")
    );
    assert!(
        d.focus
            .iter()
            .any(|f| f.path == "broken.rs" && f.reason == "parse_error_window")
    );
    assert!(d.focus.iter().all(|f| f.lines <= 2 && f.bytes <= 4096));
    let b = engine
        .brief(
            "credentials",
            &BriefOptions {
                scope: Some("guide.md".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(b.files.iter().all(|f| f.path == "guide.md"));
    project.write("guide.md", "# Replaced\nNo old instructions.\n");
    let b = engine
        .brief(
            "credentials",
            &BriefOptions {
                scope: Some("guide.md".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(b.starts.is_empty());
    assert_eq!(b.index.syntax_freshness, "stale");
}

#[test]
fn yaml_resources_are_opt_in_source_bound_and_have_no_invented_symbols() {
    use sonar::{Chunking, ResourcePolicy, index_with_resources};
    let p = Project::new();
    p.write("lib.rs", "fn launch() {}\n");
    p.write(
        "profiles/agent.yaml",
        "name: helper\npatterns:\n  - 'unexpected capacity exhausted'\n",
    );
    let baseline = index(&p.root, &p.db).unwrap();
    assert!(p.query("capacity exhausted").starts.is_empty());
    let enabled =
        index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).unwrap();
    assert_ne!(enabled.snapshot, baseline.snapshot);
    let response = p.query("capacity exhausted");
    assert_eq!(response.index.syntax_freshness, "current");
    let file = response
        .files
        .iter()
        .find(|f| f.path == "profiles/agent.yaml")
        .unwrap();
    assert!(
        file.locations
            .iter()
            .all(|l| l.kind == "file" && l.symbol.is_none() && l.definition.is_none())
    );
    assert!(
        file.locations
            .iter()
            .any(|l| l.focus.is_some_and(|r| r[0] <= 3 && r[1] >= 3))
    );
    assert!(response.links.is_empty());
    assert!(sonar::structure(&p.db, None).is_ok());
    let unchanged =
        index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).unwrap();
    assert_eq!(unchanged.changed, 0);
    p.write("profiles/agent.yaml", "name: replaced\n");
    assert!(p.query("capacity exhausted").starts.is_empty());
    assert!(sonar::structure(&p.db, None).is_err());
    index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).unwrap();
    assert_eq!(p.query("replaced").files[0].path, "profiles/agent.yaml");
    fs::remove_file(p.root.join("profiles/agent.yaml")).unwrap();
    assert!(p.query("replaced").starts.is_empty());
    p.write("profiles/new.yml", "name: newmarker\n");
    index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).unwrap();
    assert_eq!(p.query("newmarker").files[0].path, "profiles/new.yml");
    index(&p.root, &p.db).unwrap();
    assert!(p.query("newmarker").starts.is_empty());
}

#[test]
fn yaml_resource_failures_are_bounded_and_atomic() {
    use sonar::{Chunking, ResourcePolicy, index_with_resources};
    let p = Project::new();
    p.write("lib.rs", "fn original_marker() {}\n");
    index(&p.root, &p.db).unwrap();
    let old = fs::read(&p.db).unwrap();
    p.write("large.yaml", &"x".repeat(256 * 1024 + 1));
    let error =
        index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).unwrap_err();
    assert!(error.to_string().contains("256 KiB"));
    assert_eq!(fs::read(&p.db).unwrap(), old);
    fs::write(p.root.join("large.yaml"), [0xff]).unwrap();
    assert!(index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).is_err());
    assert_eq!(fs::read(&p.db).unwrap(), old);
    assert_eq!(
        starting_symbols(&p.query("original_marker"))[0],
        "original_marker"
    );
}

#[cfg(unix)]
#[test]
fn yaml_resources_respect_ignored_directories_and_symlinks() {
    use sonar::{Chunking, ResourcePolicy, index_with_resources};
    let p = Project::new();
    p.write("lib.rs", "fn source() {}\n");
    p.write(".hidden.yaml", "secret_marker: true\n");
    p.write("target/build.yaml", "secret_marker: true\n");
    p.write("vendor/config.yml", "secret_marker: true\n");
    p.write(".gitignore", "ignored.yaml\n");
    p.write("ignored.yaml", "secret_marker: true\n");
    std::os::unix::fs::symlink(p.root.join(".hidden.yaml"), p.root.join("linked.yaml")).unwrap();
    let report =
        index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).unwrap();
    assert_eq!(report.files, 1);
    assert!(p.query("secret_marker").starts.is_empty());
}

#[test]
fn standalone_file_ranking_preserves_container_exact_scope_and_freshness_rules() {
    use sonar::{
        Chunking, FileRankingPolicy, RankingOptions, ResourcePolicy, RetrievalMode, SearchEngine,
        index_with_resources,
    };
    let p = Project::new();
    p.write("profiles/live.yaml", "capacity_exhausted: retry_later\n");
    p.write("notes.rs", "// capacity_exhausted retry_later\n");
    p.write(
        "lib.rs",
        "/// capacity_exhausted retry_later\nfn handler() {}\n",
    );
    index_with_resources(&p.root, &p.db, Chunking::Lines, ResourcePolicy::Yaml).unwrap();
    let old = SearchEngine::open(&p.db, RetrievalMode::Lexical, None).unwrap();
    let new = SearchEngine::open(&p.db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_ranking(RankingOptions {
            file_ranking: FileRankingPolicy::Standalone,
            ..Default::default()
        });
    let options = BriefOptions::default();
    let a = old
        .diagnose("capacity_exhausted retry_later", &options)
        .unwrap();
    let b = new
        .diagnose("capacity_exhausted retry_later", &options)
        .unwrap();
    for c in &a.candidates {
        let after = b
            .candidates
            .iter()
            .find(|x| x.location_id == c.location_id)
            .unwrap();
        if c.path == "lib.rs" {
            assert_eq!(
                after.lexical_score, c.lexical_score,
                "separately represented declarations and their container keep their weights"
            );
        } else {
            assert!(
                after.lexical_score > c.lexical_score,
                "childless resources and comment-only Rust are both eligible"
            );
        }
    }
    for query in ["handler", "profiles/live.yaml"] {
        let result = new.brief(query, &options).unwrap();
        let first = result
            .files
            .iter()
            .flat_map(|f| f.locations.iter().map(move |l| (f, l)))
            .find(|(_, l)| l.id == result.starts[0])
            .unwrap();
        assert!(first.0.path == query || first.1.symbol.as_deref() == Some(query));
    }
    let scoped = new
        .brief(
            "capacity_exhausted",
            &BriefOptions {
                scope: Some("profiles".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(scoped.files.iter().all(|f| f.path.starts_with("profiles/")));
    p.write("profiles/live.yaml", "changed: true\n");
    assert!(
        new.brief("capacity_exhausted", &options)
            .unwrap()
            .files
            .iter()
            .all(|f| f.path != "profiles/live.yaml")
    );
}
