//! Explicit compiler analysis; subsequent queries consume only validated local records.
use crate::{store, workspace};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

const POLICY: &str = "typescript-compiler-v1";
const SCRIPT: &str = include_str!("compiler.cjs");
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS typescript_field_uses (
 owner INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
 field INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
 path TEXT NOT NULL,start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,data TEXT NOT NULL,
 PRIMARY KEY(owner,field,path,start_byte,end_byte));";

#[derive(Debug, Serialize)]
pub struct TypeScriptReport {
    #[serde(flatten)]
    pub analysis: crate::SemanticReport,
    pub compiler: String,
    pub projects: Vec<String>,
    pub inferred_files: usize,
    pub diagnostic_count: usize,
    pub diagnostics: Vec<serde_json::Value>,
    pub field_uses: usize,
}

#[derive(Deserialize, Serialize)]
struct Input {
    path: PathBuf,
    hash: Option<String>,
    #[serde(default)]
    directory: bool,
    #[serde(default)]
    entries: Option<Vec<String>>,
}
#[derive(Deserialize)]
struct Export {
    compiler: String,
    node: String,
    projects: Vec<String>,
    inferred_files: usize,
    diagnostics: Vec<serde_json::Value>,
    diagnostic_count: usize,
    documents: usize,
    mapped_definitions: usize,
    skipped_occurrences: usize,
    ambiguous_symbols: usize,
    edges: Vec<Edge>,
    field_uses: Vec<Field>,
    inputs: Vec<Input>,
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
    access: String,
    context: Option<[usize; 2]>,
    associations: Vec<Association>,
}
#[derive(Deserialize, Serialize)]
struct Association {
    target: i64,
    relation: String,
    start: usize,
    end: usize,
}

fn hash(path: &Path) -> Result<Option<String>> {
    #[cfg(unix)]
    {
        use std::{
            os::unix::fs::MetadataExt,
            sync::{Mutex, OnceLock},
        };
        type Stamp = (u64, u64, u64, i64, i64, i64, i64);
        static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, (Stamp, String)>>> = OnceLock::new();
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if metadata.is_dir() {
            return Ok(None);
        }
        let stamp = |m: &fs::Metadata| {
            (
                m.dev(),
                m.ino(),
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        };
        let identity = stamp(&metadata);
        // ctime also detects equal-length writes followed by restored mtime.
        if metadata.ctime_nsec() != 0 || metadata.mtime_nsec() != 0 {
            let cache = CACHE.get_or_init(|| Mutex::new(BTreeMap::new()));
            if let Some((old, hash)) = cache
                .lock()
                .map_err(|_| anyhow::anyhow!("analysis cache poisoned"))?
                .get(path)
                && *old == identity
            {
                return Ok(Some(hash.clone()));
            }
            let hash = format!("{:x}", Sha256::digest(fs::read(path)?));
            ensure!(
                stamp(&fs::metadata(path)?) == identity,
                "analysis input changed while hashing"
            );
            let mut entries = cache
                .lock()
                .map_err(|_| anyhow::anyhow!("analysis cache poisoned"))?;
            if entries.len() > 16384 {
                entries.clear();
            }
            entries.insert(path.into(), (identity, hash.clone()));
            return Ok(Some(hash));
        }
    }
    match fs::read(path) {
        Ok(bytes) => Ok(Some(format!("{:x}", Sha256::digest(bytes)))),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => {
            Err(error).with_context(|| format!("reading analysis input {}", path.display()))
        }
    }
}

fn input_current(input: &Input) -> Result<bool> {
    if let Some(entries) = &input.entries {
        let directory = match fs::read_dir(&input.path) {
            Ok(directory) => directory,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let mut current = Vec::new();
        for entry in directory {
            let entry = entry?;
            if entry.path().is_dir() {
                current.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        // Match JavaScript's UTF-16 string ordering for non-ASCII directory names.
        current.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        Ok(&current == entries)
    } else if input.directory {
        Ok(!input.path.is_dir())
    } else {
        Ok(hash(&input.path)? == input.hash)
    }
}

pub(crate) fn coverage_notice(conn: &Connection) -> Result<Option<crate::Notice>> {
    let Some(manifest) = store::metadata(conn, "typescript_semantic_manifest")? else {
        return Ok(None);
    };
    let data: serde_json::Value = serde_json::from_str(&manifest)?;
    let inferred = data["inferred_files"].as_u64().unwrap_or(0);
    let diagnostics = data["diagnostic_count"].as_u64().unwrap_or(0);
    Ok(Some(crate::Notice::new(
        "typescript_analysis_coverage",
        format!(
            "TypeScript relationships use the recorded compiler configuration: {inferred} files used inferred options and {diagnostics} compiler diagnostics were reported. Unresolved, ambiguous, and external targets are omitted; static calls do not prove runtime dispatch."
        ),
    )))
}

pub(crate) fn current(conn: &Connection, snapshot: &str, fresh: bool) -> Result<bool> {
    if !fresh
        || store::metadata(conn, "typescript_semantic_snapshot")?.as_deref() != Some(snapshot)
        || store::metadata(conn, "typescript_semantic_policy")?.as_deref() != Some(POLICY)
    {
        return Ok(false);
    }
    let Some(manifest) = store::metadata(conn, "typescript_semantic_manifest")? else {
        return Ok(false);
    };
    if serde_json::from_str::<serde_json::Value>(&manifest)?["script"]
        != blake3::hash(SCRIPT.as_bytes()).to_hex().as_str()
    {
        return Ok(false);
    }
    let Some(inputs) = store::metadata(conn, "typescript_semantic_inputs")? else {
        return Ok(false);
    };
    for input in serde_json::from_str::<Vec<Input>>(&inputs)? {
        if !input_current(&input)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn resolve(
    db: &Path,
    node: &Path,
    compiler: Option<&Path>,
    project: Option<&Path>,
) -> Result<TypeScriptReport> {
    let started = Instant::now();
    let conn = store::open(db, false)?;
    let root = store::metadata(&conn, "root")?.context("index sources first")?;
    let snapshot = store::metadata(&conn, "snapshot")?.context("index sources first")?;
    let files = workspace::scan_index(&conn, Path::new(&root))?;
    ensure!(
        workspace::snapshot(&files) == snapshot,
        "sources changed; run sonar index before resolve"
    );
    let typed: Vec<_> = files
        .iter()
        .filter(|f| crate::source_language(&f.path) == Some("typescript"))
        .collect();
    ensure!(!typed.is_empty(), "no indexed TypeScript sources");
    let mut statement=conn.prepare("SELECT l.id,f.path,l.kind,l.start_byte,l.end_byte,l.name_start,l.name_end FROM locations l JOIN files f ON f.id=l.file_id")?;
    let locations=statement.query_map([],|r|Ok(serde_json::json!({"id":r.get::<_,i64>(0)?,"path":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"start":r.get::<_,i64>(3)?,"end":r.get::<_,i64>(4)?,"name_start":r.get::<_,i64>(5)?,"name_end":r.get::<_,i64>(6)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let compiler = compiler
        .map(fs::canonicalize)
        .transpose()
        .context("TypeScript compiler path is unavailable")?;
    let project = project
        .map(fs::canonicalize)
        .transpose()
        .context("TypeScript project config is unavailable")?;
    let request = serde_json::to_vec(
        &serde_json::json!({"root":root,"compiler":compiler,"project":project,
        "files":typed.iter().map(|f|serde_json::json!({"path":f.path,"source":f.source})).collect::<Vec<_>>(),"locations":locations}),
    )?;
    let mut child = Command::new(node)
        .args(["-e", SCRIPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting Node.js for TypeScript analysis")?;
    child
        .stdin
        .take()
        .context("compiler stdin missing")?
        .write_all(&request)?;
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "TypeScript analysis failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let export: Export =
        serde_json::from_slice(&output.stdout).context("invalid TypeScript compiler response")?;
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
            .context("compiler occurrence outside indexed TypeScript")?;
        ensure!(
            matches!(edge.kind.as_str(), "calls" | "references")
                && edge.start < edge.end
                && edge.end <= source.len()
                && source.is_char_boundary(edge.start)
                && source.is_char_boundary(edge.end)
                && source[edge.start..edge.end] == edge.text,
            "invalid compiler occurrence span"
        );
        let owner = ids
            .get(&edge.from)
            .context("compiler owner is not indexed")?;
        let target = ids
            .get(&edge.to)
            .context("compiler target is not indexed")?;
        ensure!(
            owner["path"] == edge.path
                && owner["start"].as_u64().unwrap() as usize <= edge.start
                && owner["end"].as_u64().unwrap() as usize >= edge.end
                && crate::source_language(target["path"].as_str().unwrap()) == Some("typescript"),
            "invalid compiler binding"
        );
        ensure!(
            edge.kind != "calls" || target["kind"] == "function",
            "call target is not callable"
        );
        ensure!(
            source[..edge.start].bytes().filter(|b| *b == b'\n').count() + 1 == edge.line,
            "invalid compiler line"
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
            recorded.contains(&(e.from, e.to, e.path.as_str(), e.start, e.end, "references")),
            "field use has no validated reference"
        );
        let source = sources[e.path.as_str()];
        ensure!(
            ids[&e.to]["kind"] == "field"
                && matches!(
                    field.access.as_str(),
                    "read" | "write" | "read_write" | "initialize" | "pattern" | "receiver"
                ),
            "invalid field classification"
        );
        if let Some([a, b]) = field.context {
            ensure!(
                a <= e.start
                    && e.end <= b
                    && b <= source.len()
                    && source.is_char_boundary(a)
                    && source.is_char_boundary(b)
                    && b - a <= 4096
                    && source[a..b].lines().count() <= 16,
                "invalid field context"
            );
        }
        for a in &field.associations {
            let kind = match a.relation.as_str() {
                "call" => "calls",
                "same_expression" => "references",
                _ => anyhow::bail!("invalid field association relation"),
            };
            ensure!(
                field
                    .context
                    .is_some_and(|[start, end]| start <= a.start && a.end <= end)
                    && recorded.contains(&(
                        e.from,
                        a.target,
                        e.path.as_str(),
                        a.start,
                        a.end,
                        kind
                    )),
                "field association has no validated evidence"
            );
        }
    }
    for input in &export.inputs {
        ensure!(
            input_current(input)?,
            "compiler inputs changed during analysis"
        )
    }
    ensure!(
        workspace::snapshot(&workspace::scan_index(&conn, Path::new(&root))?) == snapshot,
        "sources changed during analysis"
    );
    let manifest = serde_json::to_string(
        &serde_json::json!({"root":root,"snapshot":snapshot,"compiler":export.compiler,"node":export.node,"projects":export.projects,
        "inferred_files":export.inferred_files,"diagnostic_count":export.diagnostic_count,"diagnostics":export.diagnostics,"inputs":export.inputs,"policy":POLICY,"script":blake3::hash(SCRIPT.as_bytes()).to_hex().to_string()}),
    )?;
    let profile = blake3::hash(manifest.as_bytes()).to_hex().to_string();
    let mut writer = store::open(db, true)?;
    let tx = writer.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    ensure!(
        store::metadata(&tx, "snapshot")?.as_deref() == Some(&snapshot),
        "index changed during analysis"
    );
    tx.execute_batch(SCHEMA)?;
    tx.execute("DELETE FROM semantic_edges WHERE from_id IN (SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path GLOB '*.ts' OR f.path GLOB '*.tsx' OR f.path GLOB '*.mts' OR f.path GLOB '*.cts')",[])?;
    tx.execute("DELETE FROM typescript_field_uses", [])?;
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
            "INSERT OR IGNORE INTO typescript_field_uses VALUES (?1,?2,?3,?4,?5,?6)",
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
        ("typescript_semantic_profile", profile.clone()),
        ("typescript_semantic_snapshot", snapshot.clone()),
        ("typescript_semantic_policy", POLICY.into()),
        (
            "typescript_semantic_inputs",
            serde_json::to_string(&export.inputs)?,
        ),
        ("typescript_semantic_manifest", manifest),
    ] {
        tx.execute(
            "INSERT OR REPLACE INTO metadata VALUES (?1,?2)",
            [key, value.as_str()],
        )?;
    }
    ensure!(
        workspace::snapshot(&workspace::scan_index(&tx, Path::new(&root))?) == snapshot,
        "sources changed during import"
    );
    for input in &export.inputs {
        ensure!(
            input_current(input)?,
            "compiler inputs changed during import"
        );
    }
    tx.commit()?;
    Ok(TypeScriptReport {
        analysis: crate::SemanticReport {
            snapshot,
            profile,
            documents: export.documents,
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
        projects: export.projects,
        inferred_files: export.inferred_files,
        diagnostic_count: export.diagnostic_count,
        diagnostics: export.diagnostics,
        field_uses: export.field_uses.len(),
    })
}

pub(crate) fn enrich(
    s: &crate::explore::Snapshot<'_>,
    owner: i64,
    field: i64,
    path: &str,
    span_bytes: [usize; 2],
    locations: &mut BTreeSet<i64>,
    use_: &mut crate::FieldUse,
) -> Result<()> {
    let [start, end] = span_bytes;
    let json:String=s.conn.query_row("SELECT data FROM typescript_field_uses WHERE owner=?1 AND field=?2 AND path=?3 AND start_byte=?4 AND end_byte=?5",params![owner,field,path,start as i64,end as i64],|r|r.get(0))?;
    let record: Field = serde_json::from_str(&json)?;
    use_.access = serde_json::from_value(serde_json::Value::String(record.access))?;
    let source = &s.sources[path].source;
    let span = |a: usize, b: usize| crate::SourceSpan {
        start_byte: a,
        end_byte: b,
        start_line: source.as_bytes()[..a]
            .iter()
            .filter(|c| **c == b'\n')
            .count()
            + 1,
        end_line: source.as_bytes()[..b.saturating_sub(1)]
            .iter()
            .filter(|c| **c == b'\n')
            .count()
            + 1,
    };
    use_.context = record.context.map(|[a, b]| span(a, b));
    for a in record.associations {
        locations.insert(a.target);
        use_.associations.push(crate::FieldAssociation {
            target: crate::explore::handle(s.conn, &s.generation, a.target)?,
            relation: a.relation,
            evidence: crate::FieldEvidence {
                path: path.into(),
                span: span(a.start, a.end),
            },
            binding: None,
        });
    }
    use_.limitations
        .push("syntax_associations_do_not_prove_data_flow".into());
    Ok(())
}
