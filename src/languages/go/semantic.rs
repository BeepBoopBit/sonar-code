//! Explicit Go analysis. Queries use persisted evidence and never start a Go process.
use crate::semantic::inputs::hash as file_hash;
use crate::{store, workspace};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

const SCRIPT: &str = include_str!("analyzer.go");
const POLICY: &str = "go-compiler-v1";
const ENVIRONMENT: &[&str] = &[
    "PATH",
    "GOENV",
    "GOROOT",
    "GOPATH",
    "GOMODCACHE",
    "GOWORK",
    "GO111MODULE",
    "GOOS",
    "GOARCH",
    "GOFLAGS",
    "CGO_ENABLED",
    "GOEXPERIMENT",
    "GOAMD64",
    "GOARM",
    "GOARM64",
    "GOTOOLCHAIN",
    "CC",
    "CXX",
    "CGO_CFLAGS",
    "CGO_CPPFLAGS",
    "CGO_CXXFLAGS",
    "CGO_LDFLAGS",
];
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS go_field_uses (
 owner INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
 field INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
 path TEXT NOT NULL,start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,data TEXT NOT NULL,
 PRIMARY KEY(owner,field,path,start_byte,end_byte));";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GoOptions {
    pub command: PathBuf,
    pub tags: Vec<String>,
    pub goos: Option<String>,
    pub goarch: Option<String>,
}
impl Default for GoOptions {
    fn default() -> Self {
        Self {
            command: "go".into(),
            tags: vec![],
            goos: None,
            goarch: None,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct GoReport {
    #[serde(flatten)]
    pub analysis: crate::SemanticReport,
    pub compiler: String,
    pub build_context: BTreeMap<String, String>,
    pub tags: Vec<String>,
    pub modules: Vec<String>,
    pub included_files: Vec<String>,
    pub excluded_files: Vec<String>,
    pub diagnostic_count: usize,
    pub diagnostics: Vec<String>,
    pub field_uses: usize,
    pub implementations: usize,
}
#[derive(Deserialize, Serialize)]
struct Input {
    path: PathBuf,
    hash: Option<String>,
    #[serde(default)]
    directory: bool,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    stamp: Option<crate::semantic::inputs::FileStamp>,
}
#[derive(Deserialize, Serialize)]
struct Edge {
    from: i64,
    to: i64,
    kind: String,
    path: String,
    line: usize,
    start: usize,
    end: usize,
    text: String,
}
#[derive(Deserialize, Serialize)]
struct Field {
    #[serde(flatten)]
    edge: Edge,
    access: crate::FieldAccess,
    context: Option<[usize; 2]>,
}
#[derive(Deserialize)]
struct Export {
    compiler: String,
    modules: Vec<String>,
    included_files: Vec<String>,
    excluded_files: Vec<String>,
    diagnostics: Vec<String>,
    diagnostic_count: usize,
    mapped_definitions: usize,
    ambiguous_symbols: usize,
    skipped_occurrences: usize,
    edges: Vec<Edge>,
    field_uses: Vec<Field>,
    inputs: Vec<Input>,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    script: String,
    environment: BTreeMap<String, Option<String>>,
    options: GoOptions,
    compiler: String,
    build_context: BTreeMap<String, String>,
    modules: Vec<String>,
    included_files: Vec<String>,
    excluded_files: Vec<String>,
    diagnostic_count: usize,
    inputs: Vec<Input>,
}
fn process_environment() -> BTreeMap<String, Option<String>> {
    ENVIRONMENT
        .iter()
        .map(|key| ((*key).into(), std::env::var(key).ok()))
        .collect()
}
fn relevant(name: &str) -> bool {
    name.ends_with(".go") || matches!(name, "go.mod" | "go.sum" | "go.work" | "go.work.sum")
}

fn input_current(input: &Input) -> Result<bool> {
    if input.directory {
        let entries = match fs::read_dir(&input.path) {
            Ok(entries) => entries,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if relevant(&name) {
                files.push(name);
            }
        }
        files.sort();
        Ok(files == input.files)
    } else {
        if input.hash.is_none() {
            return Ok(!input.path.try_exists()?);
        }
        if let Some(expected) = input.stamp
            && crate::semantic::inputs::stamp(&input.path)? == Some(expected)
        {
            return Ok(true);
        }
        Ok(file_hash(&input.path)? == input.hash)
    }
}
pub(crate) fn current(conn: &Connection, snapshot: &str, fresh: bool) -> Result<bool> {
    if !fresh
        || store::metadata(conn, "go_semantic_snapshot")?.as_deref() != Some(snapshot)
        || store::metadata(conn, "go_semantic_policy")?.as_deref() != Some(POLICY)
    {
        return Ok(false);
    }
    let Some(manifest) = store::metadata(conn, "go_semantic_manifest")? else {
        return Ok(false);
    };
    let manifest: Manifest = serde_json::from_str(&manifest)?;
    if manifest.script != blake3::hash(SCRIPT.as_bytes()).to_hex().as_str()
        || manifest.environment != process_environment()
    {
        return Ok(false);
    }
    for input in &manifest.inputs {
        if !input_current(input)? {
            return Ok(false);
        }
    }
    Ok(true)
}
pub(crate) fn coverage_notice(conn: &Connection) -> Result<Option<crate::Notice>> {
    let Some(manifest) = store::metadata(conn, "go_semantic_manifest")? else {
        return Ok(None);
    };
    let manifest: Manifest = serde_json::from_str(&manifest)?;
    Ok(Some(crate::Notice::new(
        "go_analysis_coverage",
        format!(
            "Go analysis covers {} indexed files ({}/{}, tags [{}]); {} files were excluded and {} diagnostics reported. Calls through local function values require single initializers without reassignment or address-taking. Interface calls target interface declarations. Implementation links compare indexed types and observed generic instances within each package load, including value/pointer method sets; loads with type errors, empty interfaces, and type-set constraints are omitted. Runtime dispatch and unknown callback targets are not inferred. External, generated, and unmapped targets are omitted.",
            manifest.included_files.len(),
            manifest
                .build_context
                .get("GOOS")
                .map_or("?", String::as_str),
            manifest
                .build_context
                .get("GOARCH")
                .map_or("?", String::as_str),
            manifest.options.tags.join(","),
            manifest.excluded_files.len(),
            manifest.diagnostic_count
        ),
    )))
}

pub fn resolve(db: &Path, options: &GoOptions) -> Result<GoReport> {
    let started = Instant::now();
    let conn = store::open(db, false)?;
    let root = store::metadata(&conn, "root")?.context("index sources first")?;
    let snapshot = store::metadata(&conn, "snapshot")?.context("index snapshot missing")?;
    let generation = crate::explore::generation(&conn)?;
    let files = workspace::scan_index(&conn, Path::new(&root))?;
    ensure!(
        workspace::snapshot(&files) == snapshot,
        "sources changed; run sonar index before resolve"
    );
    let typed: Vec<_> = files
        .iter()
        .filter(|f| crate::source_language(&f.path) == Some("go"))
        .collect();
    ensure!(!typed.is_empty(), "no indexed Go sources");
    ensure!(
        options.tags.iter().all(|t| !t.is_empty()
            && t.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))),
        "Go build tags must be nonempty names"
    );
    let command = if options.command.components().count() > 1 {
        std::path::absolute(&options.command)?
    } else {
        options.command.clone()
    };
    let mut env_command = Command::new(&command);
    env_command
        .args(["env", "-json"])
        .current_dir(&root)
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .env("GOSUMDB", "off");
    if let Some(goos) = &options.goos {
        env_command.env("GOOS", goos);
    }
    if let Some(goarch) = &options.goarch {
        env_command.env("GOARCH", goarch);
    }
    let output = env_command
        .output()
        .context("starting Go; install Go or set --go-command")?;
    ensure!(
        output.status.success(),
        "Go environment failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let environment: BTreeMap<String, String> = serde_json::from_slice(&output.stdout)?;
    let build_context = environment
        .iter()
        .filter(|(key, _)| {
            matches!(
                key.as_str(),
                "GOOS"
                    | "GOARCH"
                    | "CGO_ENABLED"
                    | "GOFLAGS"
                    | "GOEXPERIMENT"
                    | "GOWORK"
                    | "GOMOD"
                    | "GOROOT"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let executable = Path::new(
        environment
            .get("GOROOT")
            .context("Go did not report GOROOT")?,
    )
    .join("bin")
    .join(if cfg!(windows) { "go.exe" } else { "go" });
    let mut stmt=conn.prepare("SELECT l.id,f.path,l.kind,l.start_byte,l.end_byte,l.name_start,l.name_end FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path GLOB '*.go'")?;
    let locations:Vec<serde_json::Value>=stmt.query_map([],|r|Ok(serde_json::json!({"id":r.get::<_,i64>(0)?,"path":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"start":r.get::<_,i64>(3)?,"end":r.get::<_,i64>(4)?,"name_start":r.get::<_,i64>(5)?,"name_end":r.get::<_,i64>(6)?})))?.collect::<rusqlite::Result<_>>()?;
    let temp = tempfile::tempdir()?;
    fs::write(temp.path().join("analyzer.go"), SCRIPT)?;
    let request = serde_json::to_vec(
        &serde_json::json!({"root":root,"command":executable,"environment":environment,"tags":options.tags,"files":typed.iter().map(|f|&f.path).collect::<Vec<_>>(),"locations":locations}),
    )?;
    let mut child = Command::new(&executable)
        .args(["run", "analyzer.go"])
        .current_dir(temp.path())
        .env("GO111MODULE", "off")
        .env("GOWORK", "off")
        .env("GOFLAGS", "")
        .env("GOTOOLCHAIN", "local")
        .env("GOPROXY", "off")
        .env("GOSUMDB", "off")
        .env(
            "GOOS",
            environment.get("GOHOSTOS").context("Go host OS missing")?,
        )
        .env(
            "GOARCH",
            environment
                .get("GOHOSTARCH")
                .context("Go host architecture missing")?,
        )
        .env("CGO_ENABLED", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting embedded Go analyzer")?;
    child
        .stdin
        .take()
        .context("Go analyzer stdin missing")?
        .write_all(&request)?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "Go analysis failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut export: Export =
        serde_json::from_slice(&output.stdout).context("invalid Go analyzer response")?;
    export.inputs.push(Input {
        hash: file_hash(&executable)?,
        path: executable,
        directory: false,
        files: vec![],
        stamp: None,
    });
    let command_path = if command.is_absolute() {
        Some(command.clone())
    } else {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(&command))
                .find(|path| path.is_file())
        })
    };
    if let Some(path) = command_path {
        export.inputs.push(Input {
            hash: file_hash(&path)?,
            path,
            directory: false,
            files: vec![],
            stamp: None,
        });
    }
    ensure!(
        !export.included_files.is_empty(),
        "no indexed Go source could be analyzed in this build context: {}",
        export.diagnostics.join("; ")
    );
    let sources: BTreeMap<_, _> = typed
        .iter()
        .map(|f| (f.path.as_str(), f.source.as_str()))
        .collect();
    let ids: BTreeMap<_, _> = locations
        .iter()
        .map(|l| (l["id"].as_i64().unwrap(), l))
        .collect();
    for edge in &export.edges {
        let source = sources
            .get(edge.path.as_str())
            .context("Go occurrence outside indexed sources")?;
        ensure!(
            matches!(edge.kind.as_str(), "calls" | "references" | "implements")
                && edge.start < edge.end
                && edge.end <= source.len()
                && source.is_char_boundary(edge.start)
                && source.is_char_boundary(edge.end)
                && source[edge.start..edge.end] == edge.text,
            "invalid Go occurrence span"
        );
        let owner = ids
            .get(&edge.from)
            .context("Go occurrence owner is not indexed")?;
        let target = ids.get(&edge.to).context("Go target is not indexed")?;
        ensure!(
            owner["path"] == edge.path
                && owner["start"].as_u64().unwrap() as usize <= edge.start
                && owner["end"].as_u64().unwrap() as usize >= edge.end,
            "invalid Go occurrence owner"
        );
        ensure!(
            edge.kind != "calls" || target["kind"] == "function",
            "Go call target is not callable"
        );
        if edge.kind == "implements" {
            let from = owner["kind"].as_str().unwrap_or_default();
            let to = target["kind"].as_str().unwrap_or_default();
            ensure!(
                (from == "function" && to == "function")
                    || (matches!(from, "struct" | "type") && matches!(to, "interface" | "type")),
                "invalid Go implementation endpoints"
            );
        }
        ensure!(
            source[..edge.start].bytes().filter(|b| *b == b'\n').count() + 1 == edge.line,
            "invalid Go occurrence line"
        );
    }
    let recorded: BTreeSet<_> = export
        .edges
        .iter()
        .map(|e| {
            (
                e.from,
                e.to,
                e.path.as_str(),
                e.start,
                e.end,
                e.kind.as_str(),
            )
        })
        .collect();
    for field in &export.field_uses {
        let e = &field.edge;
        ensure!(
            recorded.contains(&(e.from, e.to, e.path.as_str(), e.start, e.end, "references"))
                && ids[&e.to]["kind"] == "field",
            "Go field use has no validated field reference"
        );
        if let Some([start, end]) = field.context {
            let source = sources[e.path.as_str()];
            ensure!(
                start <= e.start
                    && e.end <= end
                    && end <= source.len()
                    && source.is_char_boundary(start)
                    && source.is_char_boundary(end)
                    && end - start <= 4096
                    && source[start..end].lines().count() <= 16,
                "invalid Go field context"
            );
        }
    }
    for input in &mut export.inputs {
        let before = if input.directory {
            None
        } else {
            crate::semantic::inputs::stamp(&input.path)?
        };
        ensure!(
            input_current(input)?,
            "Go analysis inputs changed during analysis"
        );
        if !input.directory {
            let after = crate::semantic::inputs::stamp(&input.path)?;
            ensure!(
                before == after,
                "Go input changed while validating its hash"
            );
            input.stamp = after;
        }
    }
    ensure!(
        workspace::snapshot(&workspace::scan_index(&conn, Path::new(&root))?) == snapshot,
        "sources changed during Go analysis"
    );
    let manifest = serde_json::to_string(&Manifest {
        script: blake3::hash(SCRIPT.as_bytes()).to_hex().to_string(),
        environment: process_environment(),
        options: options.clone(),
        compiler: export.compiler.clone(),
        build_context: build_context.clone(),
        modules: export.modules.clone(),
        included_files: export.included_files.clone(),
        excluded_files: export.excluded_files.clone(),
        diagnostic_count: export.diagnostic_count,
        inputs: export.inputs,
    })?;
    let profile = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let mut writer = store::open(db, true)?;
    let tx = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        crate::explore::generation(&tx)? == generation,
        "index changed during Go analysis"
    );
    tx.execute_batch(SCHEMA)?;
    tx.execute("DELETE FROM semantic_edges WHERE from_id IN (SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path GLOB '*.go')",[])?;
    tx.execute("DELETE FROM go_field_uses", [])?;
    for e in &export.edges {
        tx.execute(
            "INSERT OR IGNORE INTO semantic_edges VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                e.from,
                e.to,
                e.kind,
                e.path,
                e.line as i64,
                e.start as i64,
                e.end as i64
            ],
        )?;
    }
    for field in &export.field_uses {
        let e = &field.edge;
        tx.execute(
            "INSERT OR REPLACE INTO go_field_uses VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                e.from,
                e.to,
                e.path,
                e.start as i64,
                e.end as i64,
                serde_json::to_string(field)?
            ],
        )?;
    }
    for (key, value) in [
        ("go_semantic_profile", profile.clone()),
        ("go_semantic_snapshot", snapshot.clone()),
        ("go_semantic_policy", POLICY.into()),
        ("go_semantic_manifest", manifest.clone()),
        (
            "go_baseline_command",
            command.to_string_lossy().into_owned(),
        ),
    ] {
        tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES (?1,?2)",
            [key, value.as_str()],
        )?;
    }
    ensure!(
        workspace::snapshot(&workspace::scan_index(&tx, Path::new(&root))?) == snapshot,
        "sources changed during Go import"
    );
    for input in &serde_json::from_str::<Manifest>(&manifest)?.inputs {
        ensure!(input_current(input)?, "Go inputs changed during import");
    }
    tx.commit()?;
    Ok(GoReport {
        analysis: crate::SemanticReport {
            snapshot,
            profile,
            documents: export.included_files.len(),
            mapped_definitions: export.mapped_definitions,
            ambiguous_symbols: export.ambiguous_symbols,
            skipped_occurrences: export.skipped_occurrences,
            references: export
                .edges
                .iter()
                .filter(|e| e.kind == "references")
                .count(),
            calls: export.edges.iter().filter(|e| e.kind == "calls").count(),
            elapsed_ms: started.elapsed().as_millis(),
        },
        compiler: export.compiler,
        build_context,
        tags: options.tags.clone(),
        modules: export.modules,
        included_files: export.included_files,
        excluded_files: export.excluded_files,
        diagnostic_count: export.diagnostic_count,
        diagnostics: export.diagnostics,
        field_uses: export.field_uses.len(),
        implementations: export
            .edges
            .iter()
            .filter(|e| e.kind == "implements")
            .count(),
    })
}

pub(crate) fn enrich(
    s: &crate::explore::Snapshot<'_>,
    owner: i64,
    field: i64,
    path: &str,
    bytes: [usize; 2],
    use_: &mut crate::FieldUse,
) -> Result<()> {
    let data:String=s.conn.query_row("SELECT data FROM go_field_uses WHERE owner=?1 AND field=?2 AND path=?3 AND start_byte=?4 AND end_byte=?5",params![owner,field,path,bytes[0] as i64,bytes[1] as i64],|r|r.get(0))?;
    let record: Field = serde_json::from_str(&data)?;
    use_.access = record.access;
    let source = &s.sources[path].source;
    use_.context = record.context.map(|[start, end]| crate::SourceSpan {
        start_byte: start,
        end_byte: end,
        start_line: source[..start].bytes().filter(|b| *b == b'\n').count() + 1,
        end_line: source[..end.saturating_sub(1)]
            .bytes()
            .filter(|b| *b == b'\n')
            .count()
            + 1,
    });
    use_.limitations
        .push("access_labels_describe_syntax_not_runtime_mutation".into());
    Ok(())
}
