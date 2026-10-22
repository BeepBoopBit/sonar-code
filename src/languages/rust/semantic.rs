//! Import compiler-resolved occurrences, then classify only source-supported call sites.
use crate::{store, workspace};

const IMPORT_POLICY: &str = "sonar-scip-utf8-source-calls-v1";
use anyhow::{Context, Result, ensure};
use protobuf::Message;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path},
    process::Command,
    time::Instant,
};
use tree_sitter::{Node, Parser};

#[derive(Debug, Serialize, Deserialize)]
pub struct SemanticManifest {
    pub root: String,
    pub snapshot: String,
    pub inputs: String,
    pub analyzer: String,
    pub toolchain: String,
    pub config: serde_json::Value,
    pub scip_hash: String,
}

#[derive(Debug, Serialize)]
pub struct SemanticReport {
    pub snapshot: String,
    pub profile: String,
    pub documents: usize,
    pub mapped_definitions: usize,
    pub ambiguous_symbols: usize,
    pub skipped_occurrences: usize,
    pub references: usize,
    pub calls: usize,
    pub elapsed_ms: u128,
}

/// Run analysis explicitly, with locked/offline Cargo and build outputs outside the source repository.
pub fn resolve(db: &Path, analyzer: &Path, config: Option<&Path>) -> Result<SemanticReport> {
    let started = Instant::now();
    let conn = store::open(db, false)?;
    let root = store::metadata(&conn, "root")?.context("index sources first")?;
    let snapshot = store::metadata(&conn, "snapshot")?.context("index sources first")?;
    ensure!(
        workspace::snapshot(&workspace::scan_index(&conn, Path::new(&root))?) == snapshot,
        "sources changed; run sonar index before resolve"
    );
    let inputs = crate::languages::rust::inputs::semantic_inputs(Path::new(&root))?;
    let parent = db
        .canonicalize()?
        .parent()
        .context("database has no parent")?
        .to_owned();
    let work = tempfile::Builder::new()
        .prefix("resolve-")
        .tempdir_in(&parent)?;
    let target = parent.join("analysis-target");
    fs::create_dir_all(&target)?;
    let version = |program: &Path, args: &[&str], directory: Option<&Path>| -> Result<String> {
        let mut command = Command::new(program);
        command.args(args);
        if let Some(directory) = directory {
            command.current_dir(directory);
        }
        let out = command.output()?;
        ensure!(
            out.status.success(),
            "{} failed: {}",
            program.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8(out.stdout)?.trim().to_owned())
    };
    let analyzer_version = version(analyzer, &["--version"], None)?;
    // Honor repository toolchain selection even when Sonar runs from another directory.
    let toolchain = version(Path::new("rustc"), &["-vV"], Some(Path::new(&root)))?;
    let mut settings = if let Some(path) = config {
        serde_json::from_slice::<serde_json::Value>(&fs::read(path)?)?
    } else {
        serde_json::json!({"cargo": {"allTargets": true}})
    };
    ensure!(
        settings.is_object(),
        "analyzer config must be a JSON object"
    );
    let cargo = settings
        .as_object_mut()
        .unwrap()
        .entry("cargo")
        .or_insert_with(|| serde_json::json!({}));
    let cargo = cargo
        .as_object_mut()
        .context("cargo settings must be an object")?;
    let extra = cargo
        .entry("extraArgs")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .context("extraArgs must be an array")?;
    for flag in ["--locked", "--offline"] {
        if !extra.iter().any(|v| v == flag) {
            extra.push(flag.into());
        }
    }
    cargo.insert("targetDir".into(), serde_json::json!(target));
    let config_path = work.path().join("config.json");
    fs::write(&config_path, serde_json::to_vec_pretty(&settings)?)?;
    let scip_path = work.path().join("index.scip");
    let log_path = parent.join("resolve.log");
    let log = fs::File::create(&log_path)?;
    let status = Command::new(analyzer)
        .args([
            "scip",
            &root,
            "--exclude-vendored-libraries",
            "--num-threads",
            "4",
            "--output",
        ])
        .arg(&scip_path)
        .arg("--config-path")
        .arg(&config_path)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_NET_OFFLINE", "true")
        .stdout(log.try_clone()?)
        .stderr(log)
        .status()
        .context("running rust-analyzer scip")?;
    ensure!(
        status.success(),
        "analysis failed; see {}",
        log_path.display()
    );
    // The exporter can exit successfully after Cargo failed and it fell back to --no-deps.
    // Do not silently publish that reduced dependency view as the requested analysis profile.
    let log_text = fs::read_to_string(&log_path)?;
    ensure!(
        !log_text.contains("Loading cargo metadata: failed exit status:")
            && !log_text.contains("`cargo metadata` failed and returning"),
        "Cargo metadata failed; restore the repository's local dependencies and inspect {}",
        log_path.display()
    );
    ensure!(
        crate::languages::rust::inputs::semantic_inputs(Path::new(&root))? == inputs,
        "analysis inputs changed during export; retry resolve"
    );
    let manifest = SemanticManifest {
        root,
        snapshot,
        inputs,
        analyzer: analyzer_version,
        toolchain,
        config: settings,
        scip_hash: blake3::hash(&fs::read(&scip_path)?).to_hex().to_string(),
    };
    let manifest_path = work.path().join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    let mut result = import_scip(db, &scip_path, &manifest_path)?;
    // Keep a reproducible pair for inspection after a successful import.
    fs::copy(scip_path, parent.join("index.scip"))?;
    fs::copy(manifest_path, parent.join("semantic-manifest.json"))?;
    result.elapsed_ms = started.elapsed().as_millis();
    Ok(result)
}

#[derive(Clone)]
struct SourceLocation {
    id: i64,
    start: usize,
    end: usize,
    name_start: usize,
    name_end: usize,
    kind: String,
}

pub fn import_scip(db: &Path, scip_path: &Path, manifest_path: &Path) -> Result<SemanticReport> {
    let started = Instant::now();
    let manifest: SemanticManifest = serde_json::from_slice(&fs::read(manifest_path)?)?;
    let bytes = fs::read(scip_path)?;
    ensure!(
        blake3::hash(&bytes).to_hex().as_str() == manifest.scip_hash,
        "SCIP bytes do not match manifest"
    );
    let index = scip::types::Index::parse_from_bytes(&bytes)?;
    let mut conn = store::open(db, true)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        store::metadata(&tx, "root")?.as_deref() == Some(manifest.root.as_str()),
        "semantic export belongs to another root"
    );
    ensure!(
        store::metadata(&tx, "snapshot")?.as_deref() == Some(manifest.snapshot.as_str()),
        "semantic export belongs to another generation"
    );
    let files = workspace::scan_index(&tx, Path::new(&manifest.root))?;
    ensure!(
        workspace::snapshot(&files) == manifest.snapshot
            && crate::languages::rust::inputs::semantic_inputs(Path::new(&manifest.root))?
                == manifest.inputs,
        "semantic export is stale"
    );
    let metadata = index.metadata.as_ref().context("SCIP metadata missing")?;
    ensure!(
        metadata.project_root == format!("file://{}", manifest.root),
        "SCIP project root does not match manifest"
    );
    let sources: BTreeMap<_, _> = files
        .iter()
        .map(|file| (file.path.as_str(), file.source.as_str()))
        .collect();
    let mut locations: BTreeMap<String, Vec<SourceLocation>> = BTreeMap::new();
    {
        let mut stmt = tx.prepare("SELECT f.path, l.id, l.start_byte, l.end_byte, l.name_start, l.name_end, l.kind FROM locations l JOIN files f ON f.id=l.file_id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                SourceLocation {
                    id: row.get(1)?,
                    start: row.get::<_, i64>(2)? as usize,
                    end: row.get::<_, i64>(3)? as usize,
                    name_start: row.get::<_, i64>(4)? as usize,
                    name_end: row.get::<_, i64>(5)? as usize,
                    kind: row.get(6)?,
                },
            ))
        })?;
        for row in rows {
            let (path, loc) = row?;
            locations.entry(path).or_default().push(loc);
        }
    }
    let key = |path: &str, symbol: &str| {
        if symbol.starts_with("local ") {
            format!("{path}\0{symbol}")
        } else {
            symbol.to_owned()
        }
    };
    let mut definitions: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    let mut valid_documents = 0;
    let mut skipped = 0;
    for doc in &index.documents {
        ensure!(
            Path::new(&doc.relative_path)
                .components()
                .all(|p| matches!(p, Component::Normal(_))),
            "invalid SCIP document path"
        );
        let (Some(source), Some(locs)) = (
            sources.get(doc.relative_path.as_str()),
            locations.get(&doc.relative_path),
        ) else {
            continue;
        };
        if doc.language != "rust" {
            continue;
        }
        ensure!(
            doc.position_encoding.enum_value_or_default()
                == scip::types::PositionEncoding::UTF8CodeUnitOffsetFromLineStart,
            "only UTF-8 SCIP positions are supported"
        );
        valid_documents += 1;
        for occurrence in &doc.occurrences {
            if occurrence.symbol_roles & 1 == 0 {
                continue;
            }
            let Some((start, end)) = byte_range(source, &occurrence.range) else {
                skipped += 1;
                continue;
            };
            let ids: Vec<_> = locs
                .iter()
                .filter(|l| l.name_start == start && l.name_end == end && l.kind != "file")
                .map(|l| l.id)
                .collect();
            let entry = definitions
                .entry(key(&doc.relative_path, &occurrence.symbol))
                .or_default();
            if ids.len() == 1 {
                entry.insert(ids[0]);
            } else {
                entry.insert(-1);
            }
        }
    }
    let ambiguous = definitions.values().filter(|ids| ids.len() > 1).count();
    let resolved: BTreeMap<_, _> = definitions
        .into_iter()
        .filter_map(|(key, ids)| {
            if ids.len() == 1 && !ids.contains(&-1) {
                Some((key, *ids.first().unwrap()))
            } else {
                None
            }
        })
        .collect();
    let kinds: BTreeMap<_, _> = locations
        .values()
        .flatten()
        .map(|l| (l.id, l.kind.as_str()))
        .collect();
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let mut edges = BTreeSet::new();
    for doc in &index.documents {
        let (Some(source), Some(locs)) = (
            sources.get(doc.relative_path.as_str()),
            locations.get(&doc.relative_path),
        ) else {
            continue;
        };
        if doc.language != "rust" {
            continue;
        }
        let tree = parser
            .parse(source, None)
            .context("source parsing failed")?;
        for occurrence in &doc.occurrences {
            if occurrence.symbol_roles & 1 != 0 {
                continue;
            }
            let Some(&target) = resolved.get(&key(&doc.relative_path, &occurrence.symbol)) else {
                skipped += 1;
                continue;
            };
            let Some((start, end)) = byte_range(source, &occurrence.range) else {
                skipped += 1;
                continue;
            };
            let Some(owner) = locs
                .iter()
                .filter(|l| l.start <= start && l.end >= end)
                .min_by_key(|l| l.end - l.start)
            else {
                skipped += 1;
                continue;
            };
            let kind = if kinds[&target] == "function" && is_call(tree.root_node(), start, end) {
                "calls"
            } else {
                "references"
            };
            edges.insert((
                owner.id,
                target,
                kind,
                doc.relative_path.clone(),
                occurrence.range[0] as usize + 1,
                start,
                end,
            ));
        }
    }
    tx.execute("DELETE FROM semantic_edges WHERE from_id IN (SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path GLOB '*.rs')", [])?;
    tx.execute("DELETE FROM metadata WHERE key='usage_policy'", [])?;
    for (from, to, kind, path, line, start, end) in &edges {
        tx.execute("INSERT INTO semantic_edges(from_id,to_id,kind,path,line,start_byte,end_byte) VALUES (?1,?2,?3,?4,?5,?6,?7)", params![from, to, kind, path, *line as i64, *start as i64, *end as i64])?;
    }
    let json = serde_json::to_string(&manifest)?;
    let mut profile_hash = blake3::Hasher::new();
    profile_hash.update(IMPORT_POLICY.as_bytes());
    profile_hash.update(json.as_bytes());
    let profile = profile_hash.finalize().to_hex().to_string();
    for (key, value) in [
        ("semantic_profile", profile.as_str()),
        ("semantic_manifest", json.as_str()),
        ("semantic_inputs", manifest.inputs.as_str()),
        ("semantic_snapshot", manifest.snapshot.as_str()),
        ("semantic_policy", IMPORT_POLICY),
    ] {
        tx.execute("INSERT INTO metadata(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key, value])?;
    }
    ensure!(
        crate::languages::rust::inputs::semantic_inputs(Path::new(&manifest.root))?
            == manifest.inputs,
        "analysis inputs changed while importing"
    );
    tx.commit()?;
    Ok(SemanticReport {
        snapshot: manifest.snapshot,
        profile,
        documents: valid_documents,
        mapped_definitions: resolved.len(),
        ambiguous_symbols: ambiguous,
        skipped_occurrences: skipped,
        references: edges.iter().filter(|e| e.2 == "references").count(),
        calls: edges.iter().filter(|e| e.2 == "calls").count(),
        elapsed_ms: started.elapsed().as_millis(),
    })
}

fn byte_range(source: &str, range: &[i32]) -> Option<(usize, usize)> {
    let (row, col, end_row, end_col) = match range {
        [r, c, e] => (*r, *c, *r, *e),
        [r, c, er, ec] => (*r, *c, *er, *ec),
        _ => return None,
    };
    if [row, col, end_row, end_col].iter().any(|n| *n < 0) {
        return None;
    }
    let offset = |row: i32, col: i32| {
        let mut lines = source.split_inclusive('\n');
        let start = lines
            .by_ref()
            .take(row as usize)
            .map(str::len)
            .sum::<usize>();
        let line = lines.next()?;
        let len = line.trim_end_matches(['\r', '\n']).len();
        if col as usize > len || !line.is_char_boundary(col as usize) {
            return None;
        }
        Some(start + col as usize)
    };
    let start = offset(row, col)?;
    let end = offset(end_row, end_col)?;
    (start < end).then_some((start, end))
}

fn is_call(root: Node<'_>, start: usize, end: usize) -> bool {
    let Some(mut node) = root.descendant_for_byte_range(start, end) else {
        return false;
    };
    loop {
        if node.kind() == "call_expression" {
            let Some(function) = node.child_by_field_name("function") else {
                return false;
            };
            return callee_name(function)
                .is_some_and(|name| name.start_byte() == start && name.end_byte() == end);
        }
        if matches!(
            node.kind(),
            "arguments" | "type_arguments" | "macro_invocation" | "block"
        ) {
            return false;
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        node = parent;
    }
}

pub(crate) fn callee_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "identifier" | "field_identifier" => Some(node),
        "scoped_identifier" => callee_name(node.child_by_field_name("name")?),
        "generic_function" => callee_name(node.child_by_field_name("function")?),
        "field_expression" => callee_name(node.child_by_field_name("field")?),
        _ => None,
    }
}

pub(crate) fn current(
    conn: &Connection,
    root: &Path,
    snapshot: &str,
    syntax_current: bool,
) -> Result<bool> {
    if !syntax_current
        || store::metadata(conn, "semantic_snapshot")?.as_deref() != Some(snapshot)
        || store::metadata(conn, "semantic_policy")?.as_deref() != Some(IMPORT_POLICY)
    {
        return Ok(false);
    }
    Ok(store::metadata(conn, "semantic_inputs")?.as_deref()
        == Some(crate::languages::rust::inputs::semantic_inputs(root)?.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BriefOptions, RetrievalMode, SearchEngine};
    use scip::types::{Document, Index, Metadata, Occurrence, PositionEncoding};

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        db: PathBuf,
        scip: PathBuf,
        manifest: PathBuf,
    }
    use std::path::PathBuf;

    fn fixture(ambiguous: bool) -> Fixture {
        fixture_languages(ambiguous, false)
    }

    fn fixture_languages(ambiguous: bool, python: bool) -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let source = "fn accept() {}\nfn unrelated() {}\n/// credentials supplied by the kernel\n#[test]\nfn scenario() { let _s = \"π\"; let value = accept; accept(); }\nfn other() { unrelated(); }\n";
        fs::write(root.join("lib.rs"), source).unwrap();
        if python {
            fs::write(
                root.join("client.py"),
                "def accept():\n    return 1\n\ndef scenario():\n    return accept()\n",
            )
            .unwrap();
        }
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        let db = temp.path().join("index.sqlite");
        let indexed = crate::index(&root, &db).unwrap();
        let occurrence = |line: usize, word: &str, last: bool, symbol: &str, definition: bool| {
            let text = source.lines().nth(line).unwrap();
            let column = if last {
                text.rfind(word)
            } else {
                text.find(word)
            }
            .unwrap();
            Occurrence {
                range: vec![line as i32, column as i32, (column + word.len()) as i32],
                symbol: symbol.into(),
                symbol_roles: i32::from(definition),
                ..Default::default()
            }
        };
        let index = Index {
            metadata: Some(Metadata {
                project_root: format!("file://{}", root.display()),
                ..Default::default()
            })
            .into(),
            documents: vec![Document {
                relative_path: "lib.rs".into(),
                language: "rust".into(),
                position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                occurrences: vec![
                    occurrence(0, "accept", false, "resolved accept", true),
                    occurrence(
                        1,
                        "unrelated",
                        false,
                        if ambiguous {
                            "resolved accept"
                        } else {
                            "resolved unrelated"
                        },
                        true,
                    ),
                    occurrence(4, "scenario", false, "resolved scenario", true),
                    occurrence(5, "other", false, "resolved other", true),
                    occurrence(4, "accept", false, "resolved accept", false),
                    occurrence(4, "accept", true, "resolved accept", false),
                    occurrence(5, "unrelated", false, "resolved unrelated", false),
                ],
                ..Default::default()
            }],
            ..Default::default()
        };
        let scip = temp.path().join("index.scip");
        fs::write(&scip, index.write_to_bytes().unwrap()).unwrap();
        let manifest = temp.path().join("manifest.json");
        let record = SemanticManifest {
            root: root.to_str().unwrap().into(),
            snapshot: indexed.snapshot,
            inputs: crate::languages::rust::inputs::semantic_inputs(&root).unwrap(),
            analyzer: "test fixture".into(),
            toolchain: "fixture".into(),
            config: serde_json::json!({}),
            scip_hash: blake3::hash(&fs::read(&scip).unwrap()).to_hex().to_string(),
        };
        fs::write(&manifest, serde_json::to_vec(&record).unwrap()).unwrap();
        Fixture {
            _temp: temp,
            root,
            db,
            scip,
            manifest,
        }
    }

    #[test]
    fn mixed_providers_preserve_each_other_and_hide_only_stale_language_edges() {
        let compiler = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(".sonar/typescript/oracle/node_modules/typescript");
        if !compiler.exists() {
            return;
        }
        let f = fixture(false);
        fs::write(f.root.join("module.ts"), "import type {Missing} from 'missing-library'\nexport function tsAccept(): Missing { return {} as Missing }\nexport function tsCall() { return tsAccept() }\n").unwrap();
        fs::write(
            f.root.join("tsconfig.json"),
            "{\"compilerOptions\":{\"module\":\"ESNext\",\"moduleResolution\":\"Bundler\"}}",
        )
        .unwrap();
        let indexed = crate::index(&f.root, &f.db).unwrap();
        let mut record: SemanticManifest =
            serde_json::from_slice(&fs::read(&f.manifest).unwrap()).unwrap();
        record.snapshot = indexed.snapshot;
        record.inputs = crate::languages::rust::inputs::semantic_inputs(&f.root).unwrap();
        fs::write(&f.manifest, serde_json::to_vec(&record).unwrap()).unwrap();
        crate::resolve_typescript(&f.db, Path::new("node"), Some(&compiler), None).unwrap();
        import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        let call = crate::outline(&f.db, "module.ts", 20, None)
            .unwrap()
            .locations
            .into_iter()
            .find(|l| l.symbol.as_deref() == Some("tsCall"))
            .unwrap();
        let options = crate::NeighborOptions {
            relation: crate::Relation::Calls,
            ..Default::default()
        };
        assert!(
            !crate::neighbors(&f.db, &call.handle, &options)
                .unwrap()
                .links
                .is_empty()
        );
        // A missing package becoming available affects only TypeScript inputs.
        let package = f.root.join("node_modules/missing-library");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("index.d.ts"),
            "export interface Missing { id: number }\n",
        )
        .unwrap();
        let result = crate::neighbors(&f.db, &call.handle, &options).unwrap();
        assert!(result.links.is_empty());
        assert_eq!(result.index.semantic_freshness, "partial");
        assert!(
            result
                .index
                .languages
                .iter()
                .any(|l| l.language == "rust" && l.semantic_freshness == "current")
        );
        assert!(
            result
                .index
                .languages
                .iter()
                .any(|l| l.language == "typescript" && l.semantic_freshness == "stale")
        );
        let brief = SearchEngine::open_navigation(&f.db, None)
            .unwrap()
            .brief("tsAccept", &Default::default())
            .unwrap();
        assert!(
            !brief
                .links
                .iter()
                .any(|l| matches!(l.kind.as_str(), "calls" | "references"))
        );
        let rust = crate::outline(&f.db, "lib.rs", 20, None)
            .unwrap()
            .locations
            .into_iter()
            .find(|l| l.symbol.as_deref() == Some("accept"))
            .unwrap();
        assert!(
            !crate::neighbors(&f.db, &rust.handle, &options)
                .unwrap()
                .links
                .is_empty()
        );
        // The inverse import also retains the Rust graph.
        crate::resolve_typescript(&f.db, Path::new("node"), Some(&compiler), None).unwrap();
        assert!(
            !crate::neighbors(&f.db, &rust.handle, &options)
                .unwrap()
                .links
                .is_empty()
        );
        // Current TypeScript cannot certify Rust-specific derived analysis.
        fs::write(
            f.root.join("Cargo.toml"),
            "[package]\nname='changed'\nversion='0.1.0'\n",
        )
        .unwrap();
        let current_ts = crate::neighbors(&f.db, &call.handle, &options).unwrap();
        assert!(!current_ts.links.is_empty());
        assert!(
            current_ts
                .index
                .languages
                .iter()
                .any(|l| l.language == "rust" && l.semantic_freshness == "stale")
        );
        assert!(
            crate::neighbors(&f.db, &rust.handle, &options)
                .unwrap()
                .links
                .is_empty()
        );
        let structure = crate::structure(&f.db, None).unwrap();
        assert!(!structure.semantic_current);
        assert!(
            structure.locations.iter().any(
                |l| l.symbol.as_deref() == Some("tsCall") && l.status == "unsupported_language"
            )
        );
    }

    #[test]
    fn mixed_language_graph_coverage_keeps_rust_edges_without_claiming_python_resolution() {
        let f = fixture_languages(false, true);
        import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        let brief = crate::brief(&f.db, "accept", &BriefOptions::default()).unwrap();
        assert_eq!(brief.index.semantic_freshness, "partial");
        let rust = brief
            .index
            .languages
            .iter()
            .find(|l| l.language == "rust")
            .unwrap();
        let python = brief
            .index
            .languages
            .iter()
            .find(|l| l.language == "python")
            .unwrap();
        assert_eq!(rust.semantic_freshness, "current");
        assert!(rust.semantic_profile.is_some());
        assert_eq!(python.semantic_freshness, "unsupported");
        assert!(python.semantic_profile.is_none());
        let rust = crate::locate(&f.db, "lib.rs", 1, None)
            .unwrap()
            .locations
            .remove(0)
            .handle;
        let python = crate::locate(&f.db, "client.py", 1, None)
            .unwrap()
            .locations
            .remove(0)
            .handle;
        let rust = crate::neighbors(&f.db, &rust, &crate::NeighborOptions::default()).unwrap();
        assert!(rust.links.iter().any(|l| l.kind == "calls"));
        let python = crate::neighbors(&f.db, &python, &crate::NeighborOptions::default()).unwrap();
        assert!(
            !python
                .links
                .iter()
                .any(|l| l.kind == "calls" || l.kind == "references")
        );
        assert!(
            python
                .notices
                .iter()
                .any(|n| n.code == "language_analysis_unavailable")
        );
        fs::write(f.root.join("client.py"), "def changed(): pass\n").unwrap();
        let brief = crate::brief(&f.db, "accept", &BriefOptions::default()).unwrap();
        assert_eq!(
            brief
                .index
                .languages
                .iter()
                .find(|l| l.language == "rust")
                .unwrap()
                .syntax_freshness,
            "current"
        );
        assert_eq!(
            brief
                .index
                .languages
                .iter()
                .find(|l| l.language == "python")
                .unwrap()
                .syntax_freshness,
            "stale"
        );
        // Compiler edges remain conservatively tied to the complete indexed generation.
        assert_eq!(brief.index.semantic_freshness, "stale");
    }

    #[test]
    fn explicit_neighbors_use_resolved_calls_and_invalidate_semantic_cursors() {
        let f = fixture(false);
        import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        let target = crate::locate(&f.db, "lib.rs", 1, None)
            .unwrap()
            .locations
            .remove(0);
        let options = crate::NeighborOptions {
            relation: crate::Relation::Calls,
            direction: crate::Direction::Incoming,
            ..Default::default()
        };
        let response = crate::neighbors(&f.db, &target.handle, &options).unwrap();
        assert_eq!(response.links.len(), 1);
        assert_eq!(response.links[0].evidence.line, 5);
        assert_eq!(response.locations[0].symbol.as_deref(), Some("scenario"));
        let caller = response.locations[0].handle.clone();
        let page = crate::neighbors(
            &f.db,
            &caller,
            &crate::NeighborOptions {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(page.next_cursor.is_some());
        fs::write(
            f.root.join("Cargo.toml"),
            "[package]\nname='changed'\nversion='0.1.0'\n",
        )
        .unwrap();
        let stale = crate::neighbors(&f.db, &target.handle, &options).unwrap();
        assert!(stale.links.is_empty());
        assert_eq!(stale.index.semantic_freshness, "stale");
        assert!(
            crate::neighbors(
                &f.db,
                &caller,
                &crate::NeighborOptions {
                    limit: 1,
                    cursor: page.next_cursor,
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn resolved_calls_are_distinct_from_references_and_expand_without_word_overlap() {
        let f = fixture(false);
        let report = import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        assert_eq!(
            (report.mapped_definitions, report.calls, report.references),
            (4, 2, 1)
        );
        let engine = SearchEngine::open(&f.db, RetrievalMode::Lexical, None)
            .unwrap()
            .with_graph(true);
        let diagnostic = engine
            .diagnose("credentials", &BriefOptions::default())
            .unwrap();
        let accept = diagnostic
            .candidates
            .iter()
            .find(|c| c.symbol.as_deref() == Some("accept"))
            .unwrap();
        assert_eq!(accept.lexical_rank, None);
        assert!(accept.graph_seed_count > 0);
        let brief = engine
            .brief("credentials", &BriefOptions::default())
            .unwrap();
        assert_eq!(brief.index.semantic_freshness, "current");
        assert!(
            brief
                .links
                .iter()
                .any(|l| l.kind == "calls" && l.evidence.line == 5)
        );
        assert!(
            brief
                .links
                .iter()
                .any(|l| l.kind == "references" && l.evidence.line == 5)
        );
        let empty = engine
            .brief(
                "credentials",
                &BriefOptions {
                    scope: Some("outside/".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(empty.starts.is_empty() && empty.links.is_empty());
    }

    #[test]
    fn context_budget_serves_multiple_starts_and_new_graph_methods_preserve_evidence() {
        let f = fixture(false);
        import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        let conn = store::open(&f.db, false).unwrap();
        let lookup = |symbol: &str| {
            conn.query_row("SELECT id FROM locations WHERE symbol=?1", [symbol], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        let mut selected = vec![
            crate::search::load_parent(&conn, lookup("scenario")).unwrap(),
            crate::search::load_parent(&conn, lookup("other")).unwrap(),
        ];
        let mut ids = std::collections::BTreeMap::from([
            (selected[0].id, "s1".into()),
            (selected[1].id, "s2".into()),
        ]);
        let (links, _, _) = crate::assembly::related(
            &conn,
            &mut selected,
            &mut ids,
            2,
            "",
            &BTreeSet::new(),
            true,
            None,
            &[],
        )
        .unwrap();
        assert!(
            links
                .iter()
                .any(|l| l.from == "s1" && l.kind == "calls" && l.evidence.line == 5)
        );
        assert!(
            links
                .iter()
                .any(|l| l.from == "s2" && l.kind == "calls" && l.evidence.line == 6)
        );
        for method in [
            crate::GraphMethod::Propagation,
            crate::GraphMethod::PageRank,
        ] {
            let engine = SearchEngine::open(&f.db, RetrievalMode::Lexical, None)
                .unwrap()
                .with_graph(true)
                .with_ranking(crate::RankingOptions {
                    graph_method: method,
                    ..Default::default()
                });
            let d = engine
                .diagnose("credentials", &BriefOptions::default())
                .unwrap();
            assert_eq!(d.graph_method, Some(method));
            assert!(
                d.candidates
                    .iter()
                    .any(|c| c.symbol.as_deref() == Some("accept") && c.graph_score > 0.0)
            );
            let b = engine
                .brief(
                    "credentials",
                    &BriefOptions {
                        scope: Some("outside".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(b.starts.is_empty() && b.links.is_empty());
        }
    }

    #[test]
    fn stale_build_inputs_disable_graph_and_failed_import_preserves_generation() {
        let f = fixture(false);
        import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        let engine = SearchEngine::open(&f.db, RetrievalMode::Lexical, None)
            .unwrap()
            .with_graph(true);
        fs::write(
            f.root.join("Cargo.toml"),
            "[package]\nname='changed'\nversion='0.1.0'\n",
        )
        .unwrap();
        let brief = engine
            .brief("credentials", &BriefOptions::default())
            .unwrap();
        assert_eq!(brief.index.syntax_freshness, "current");
        assert_eq!(brief.index.semantic_freshness, "stale");
        assert!(brief.links.iter().all(|l| l.kind == "contains"));
        assert!(import_scip(&f.db, &f.scip, &f.manifest).is_err());
        let conn = store::open(&f.db, false).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM semantic_edges", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            3
        );
    }

    #[test]
    fn ambiguous_compiler_symbols_are_never_bound_by_name() {
        let f = fixture(true);
        let report = import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        assert_eq!(report.ambiguous_symbols, 1);
        assert_eq!((report.calls, report.references), (0, 0));
        fs::write(&f.scip, b"changed export").unwrap();
        assert!(import_scip(&f.db, &f.scip, &f.manifest).is_err());
    }

    #[test]
    fn extractor_rebuilds_and_import_policy_changes_invalidate_semantic_bindings() {
        let f = fixture(false);
        import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        let conn = store::open(&f.db, true).unwrap();
        conn.execute(
            "UPDATE metadata SET value='old-policy' WHERE key='semantic_policy'",
            [],
        )
        .unwrap();
        let engine = SearchEngine::open(&f.db, RetrievalMode::Lexical, None)
            .unwrap()
            .with_graph(true);
        assert_eq!(
            engine
                .brief("credentials", &BriefOptions::default())
                .unwrap()
                .index
                .semantic_freshness,
            "stale"
        );
        import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        conn.execute(
            "UPDATE metadata SET value='old-extractor' WHERE key='extraction_version'",
            [],
        )
        .unwrap();
        assert_eq!(crate::index(&f.root, &f.db).unwrap().changed, 1);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM semantic_edges", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            engine
                .brief("credentials", &BriefOptions::default())
                .unwrap()
                .index
                .semantic_freshness,
            "stale"
        );
    }

    #[test]
    fn positions_and_call_classification_reject_unproven_calls() {
        assert_eq!(byte_range("π fn x() {}\r\n", &[0, 0, 2]), Some((0, 2)));
        assert_eq!(byte_range("π fn x() {}\n", &[0, 1, 2]), None);
        assert_eq!(byte_range("fn x() {}\n", &[8, 0, 1]), None);
        let source =
            "fn x() { target::<Type>(); target(other); object.method(); helper!{ target() }; }";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        for (word, expected) in [
            ("target", true),
            ("Type", false),
            ("other", false),
            ("method", true),
            ("helper", false),
        ] {
            let start = source.find(word).unwrap();
            assert_eq!(
                is_call(tree.root_node(), start, start + word.len()),
                expected,
                "{word}"
            );
        }
        let start = source.rfind("target").unwrap();
        assert!(!is_call(tree.root_node(), start, start + 6));
    }
}
