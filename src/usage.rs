//! Optional, source-bound caller representations. Never mix caller coordinates with callee focus.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::{
    Embedder, UsageEvidence, candidate::Candidate, embeddings::dot_bytes, store, workspace,
};

const POLICY: &str = "usage-v1|calls-only|4-callers|test-doc-path-order|name192-path192-doc512-site640|best-vote|rrf60-weight0.5-degree-sqrt";
const MAX_CALLERS: usize = 4;
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS usage_contexts (
    target_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    caller_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    path TEXT NOT NULL, line INTEGER NOT NULL, start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
    caller_symbol TEXT NOT NULL, caller_is_test INTEGER NOT NULL,
    incoming INTEGER NOT NULL, outgoing INTEGER NOT NULL,
    input TEXT NOT NULL, input_hash TEXT NOT NULL, vector BLOB NOT NULL,
    PRIMARY KEY(target_id,caller_id));";

#[derive(Debug, Serialize)]
pub struct UsageReport {
    pub snapshot: String,
    pub semantic_profile: String,
    pub model_fingerprint: String,
    pub targets: usize,
    pub contexts: usize,
    pub encoded: usize,
    pub reused: usize,
    pub vector_bytes: usize,
    pub elapsed_ms: u128,
}

struct Input {
    target: i64,
    caller: i64,
    path: String,
    line: usize,
    start: usize,
    end: usize,
    symbol: String,
    is_test: bool,
    incoming: usize,
    outgoing: usize,
    text: String,
}

fn clipped(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn inputs(conn: &Connection, files: &[workspace::SourceFile]) -> Result<Vec<Input>> {
    let sources: BTreeMap<_, _> = files
        .iter()
        .map(|f| (f.path.as_str(), f.source.as_str()))
        .collect();
    // Deduplicate call occurrences before limiting callers. Degree is measured before the cap.
    let mut stmt = conn.prepare("WITH calls AS (
        SELECT e.*, row_number() OVER (PARTITION BY e.to_id,e.from_id ORDER BY e.start_byte) AS occurrence
        FROM semantic_edges e JOIN locations c ON c.id=e.from_id JOIN locations t ON t.id=e.to_id
        JOIN files cf ON cf.id=c.file_id
        WHERE e.kind='calls' AND cf.path GLOB '*.rs' AND e.from_id!=e.to_id AND c.kind='function' AND t.kind='function'),
        pairs AS (SELECT * FROM calls WHERE occurrence=1),
        incoming AS (SELECT to_id,count(*) AS n FROM pairs GROUP BY to_id),
        outgoing AS (SELECT from_id,count(*) AS n FROM pairs GROUP BY from_id)
        SELECT e.to_id,e.from_id,e.path,e.line,e.start_byte,e.end_byte,c.symbol,c.is_test,
        i.n,o.n,c.start_byte,c.end_byte,
        coalesce((SELECT group_concat(source, char(10)) FROM (SELECT source FROM regions WHERE location_id=c.id AND view='description' ORDER BY start_line LIMIT 2)), '') AS docs
        FROM pairs e JOIN locations c ON c.id=e.from_id JOIN incoming i ON i.to_id=e.to_id
        JOIN outgoing o ON o.from_id=e.from_id
        ORDER BY e.to_id,c.is_test DESC,(docs!='') DESC,e.path,c.definition,e.from_id")?;
    let mut rows = stmt.query([])?;
    let mut counts = BTreeMap::new();
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let target: i64 = row.get(0)?;
        let count = counts.entry(target).or_insert(0);
        if *count >= MAX_CALLERS {
            continue;
        }
        let path: String = row.get(2)?;
        let source = sources
            .get(path.as_str())
            .context("usage source is missing")?;
        let line = row.get::<_, u32>(3)? as usize;
        let start = row.get::<_, u32>(4)? as usize;
        let end = row.get::<_, u32>(5)? as usize;
        let owner_start = row.get::<_, u32>(10)? as usize;
        let owner_end = row.get::<_, u32>(11)? as usize;
        ensure!(
            owner_start <= start
                && start < end
                && end <= owner_end
                && owner_end <= source.len()
                && source.is_char_boundary(owner_start)
                && source.is_char_boundary(owner_end)
                && source.is_char_boundary(start)
                && source.is_char_boundary(end),
            "invalid usage coordinates"
        );
        ensure!(
            source[..start].bytes().filter(|b| *b == b'\n').count() + 1 == line,
            "invalid usage line"
        );
        let symbol: String = row.get(6)?;
        let docs: String = row.get(12)?;
        // Keep only a small local occurrence window, bounded by the caller declaration.
        let before = source[owner_start..start]
            .rmatch_indices('\n')
            .nth(2)
            .map_or(owner_start, |(i, _)| owner_start + i + 1);
        let after = source[end..owner_end]
            .match_indices('\n')
            .nth(2)
            .map_or(owner_end, |(i, _)| end + i);
        // Center the character budget on the call, even on an unusually long line.
        let prefix: String = source[before..start]
            .chars()
            .rev()
            .take(256)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let site = format!("{prefix}{}", clipped(&source[start..after], 384));
        let text = format!(
            "{}\n{}\n{}\n{}",
            clipped(&symbol, 192),
            clipped(&path, 192),
            clipped(&docs, 512),
            site
        );
        result.push(Input {
            target,
            caller: row.get(1)?,
            path,
            line,
            start,
            end,
            symbol,
            is_test: row.get(7)?,
            incoming: row.get::<_, u32>(8)? as usize,
            outgoing: row.get::<_, u32>(9)? as usize,
            text,
        });
        *count += 1;
    }
    Ok(result)
}

/// Atomically build optional usage vectors after indexing, embedding, and resolving Rust calls.
pub fn embed_usage(db: &Path, model_path: &Path) -> Result<UsageReport> {
    let started = Instant::now();
    let mut conn = store::open(db, true)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let model =
        Embedder::load_with_encoding(model_path, crate::EncodingPolicy::from_metadata(&tx)?)?;
    let root = store::metadata(&tx, "root")?.context("index sources first")?;
    let snapshot = store::metadata(&tx, "snapshot")?.context("index sources first")?;
    let files = workspace::scan_index(&tx, Path::new(&root))?;
    ensure!(
        crate::semantic::current(
            &tx,
            Path::new(&root),
            &snapshot,
            workspace::snapshot(&files) == snapshot
        )?,
        "usage embeddings require current sources and resolved calls; run sonar index, embed, and resolve first"
    );
    ensure!(
        store::metadata(&tx, "embedding_model")?.as_deref() == Some(model.fingerprint())
            && store::metadata(&tx, "embedding_snapshot")?.as_deref() == Some(&snapshot),
        "build source embeddings with the same model first"
    );
    let profile = store::metadata(&tx, "semantic_profile")?.context("missing semantic profile")?;
    let inputs = inputs(&tx, &files)?;
    tx.execute_batch(SCHEMA)?;
    let mut cached = BTreeMap::new();
    if store::metadata(&tx, "usage_model")?.as_deref() == Some(model.fingerprint()) {
        let mut stmt = tx.prepare("SELECT input_hash,vector FROM usage_contexts")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(1)?;
            if dot_bytes(&vec![0.0; model.dimensions()], &bytes).is_ok() {
                cached.insert(row.get::<_, String>(0)?, bytes);
            }
        }
    }
    tx.execute("DELETE FROM usage_contexts", [])?;
    let mut encoded = 0;
    let mut reused = 0;
    for batch in inputs.chunks(128) {
        let hashes: Vec<_> = batch
            .iter()
            .map(|i| blake3::hash(i.text.as_bytes()).to_hex().to_string())
            .collect();
        let missing: Vec<_> = batch
            .iter()
            .zip(&hashes)
            .filter(|(_, h)| !cached.contains_key(*h))
            .map(|(i, _)| i.text.clone())
            .collect();
        let mut vectors = model.encode(&missing)?.into_iter();
        for (input, hash) in batch.iter().zip(hashes) {
            let bytes = if let Some(bytes) = cached.get(&hash) {
                reused += 1;
                bytes.clone()
            } else {
                encoded += 1;
                vectors
                    .next()
                    .context("missing usage vector")?
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect()
            };
            tx.execute(
                "INSERT INTO usage_contexts VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                params![
                    input.target,
                    input.caller,
                    input.path,
                    input.line as i64,
                    input.start as i64,
                    input.end as i64,
                    input.symbol,
                    input.is_test,
                    input.incoming as i64,
                    input.outgoing as i64,
                    input.text,
                    hash,
                    bytes
                ],
            )?;
        }
    }
    for (key, value) in [
        ("usage_model", model.fingerprint()),
        ("usage_snapshot", &snapshot),
        ("usage_profile", &profile),
        ("usage_policy", POLICY),
        ("usage_count", &inputs.len().to_string()),
    ] {
        tx.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,value])?;
    }
    // A long encode must not publish vectors for sources/build inputs changed during the work.
    ensure!(
        crate::semantic::current(
            &tx,
            Path::new(&root),
            &snapshot,
            workspace::snapshot(&workspace::scan_index(&tx, Path::new(&root))?) == snapshot
        )?,
        "sources changed while embedding usage; retry after refresh"
    );
    tx.commit()?;
    Ok(UsageReport {
        snapshot,
        semantic_profile: profile,
        model_fingerprint: model.fingerprint().into(),
        targets: inputs
            .iter()
            .map(|i| i.target)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        contexts: inputs.len(),
        encoded,
        reused,
        vector_bytes: inputs.len() * model.dimensions() * 4,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

pub(crate) fn status(
    conn: &Connection,
    snapshot: &str,
    model: &Embedder,
    semantic: bool,
) -> Result<&'static str> {
    if !semantic {
        return Ok("stale_semantics");
    }
    if store::metadata(conn, "usage_policy")?.as_deref() != Some(POLICY) {
        return Ok("missing");
    }
    if store::metadata(conn, "usage_snapshot")?.as_deref() != Some(snapshot)
        || store::metadata(conn, "usage_profile")? != store::metadata(conn, "semantic_profile")?
    {
        return Ok("stale");
    }
    if store::metadata(conn, "usage_model")?.as_deref() != Some(model.fingerprint()) {
        return Ok("model_mismatch");
    }
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='usage_contexts' AND type='table')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok("missing");
    }
    let count: u32 = conn.query_row("SELECT count(*) FROM usage_contexts", [], |r| r.get(0))?;
    if store::metadata(conn, "usage_count")?.as_deref() != Some(&count.to_string()) {
        return Ok("incomplete");
    }
    Ok("current")
}

/// An independent discovery channel. One best caller per target; repeated calls never add votes.
pub(crate) fn retrieve(
    conn: &Connection,
    vector: &[f32],
    scope: &str,
    excluded: &BTreeSet<String>,
    candidates: &mut Vec<Candidate>,
    legacy_scoring: bool,
) -> Result<()> {
    let mut stmt=conn.prepare("SELECT u.*,f.path AS target_path FROM usage_contexts u JOIN locations t ON t.id=u.target_id JOIN files f ON f.id=t.file_id ORDER BY u.target_id,u.caller_id")?;
    let mut rows = stmt.query([])?;
    let mut best: BTreeMap<i64, UsageEvidence> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let path: String = row.get("path")?;
        let target_path: String = row.get("target_path")?;
        if !crate::search::in_scope(&path, scope)
            || !crate::search::in_scope(&target_path, scope)
            || excluded.contains(&path)
            || excluded.contains(&target_path)
        {
            continue;
        }
        let similarity = dot_bytes(vector, row.get_ref("vector")?.as_blob()?)
            .context("reading usage vector; rerun sonar embed-usage")?;
        // Nonpositive evidence adds no support; this is not calibrated abstention.
        if similarity <= 0.0 {
            continue;
        }
        let incoming: usize = row.get::<_, u32>("incoming")? as usize;
        let outgoing: usize = row.get::<_, u32>("outgoing")? as usize;
        let adjusted = similarity / (outgoing.max(1) as f64).sqrt();
        let target: i64 = row.get("target_id")?;
        let evidence = UsageEvidence {
            caller_id: row.get("caller_id")?,
            caller_symbol: row.get("caller_symbol")?,
            caller_is_test: row.get("caller_is_test")?,
            path,
            line: row.get::<_, u32>("line")? as usize,
            start_byte: row.get::<_, u32>("start_byte")? as usize,
            end_byte: row.get::<_, u32>("end_byte")? as usize,
            similarity,
            adjusted_similarity: adjusted,
            incoming_callers: incoming,
            outgoing_targets: outgoing,
            rank: 0,
            contribution: 0.0,
        };
        if best
            .get(&target)
            .is_none_or(|e| adjusted > e.adjusted_similarity)
        {
            best.insert(target, evidence);
        }
    }
    let mut ranked: Vec<_> = best.into_iter().collect();
    ranked.sort_by(|(aid, a), (bid, b)| {
        b.adjusted_similarity
            .total_cmp(&a.adjusted_similarity)
            .then_with(|| aid.cmp(bid))
    });
    ranked.truncate(512);
    if ranked.is_empty() {
        return Ok(());
    }
    // Normalize the complete base pipeline by rank, preserving its order. Exact lookup follows.
    let mut merged: BTreeMap<_, _> = candidates
        .drain(..)
        .enumerate()
        .map(|(i, mut c)| {
            if legacy_scoring {
                c.score = 1.0 / (60.0 + (i + 1) as f64);
            }
            (c.id, c)
        })
        .collect();
    for (i, (id, mut evidence)) in ranked.into_iter().enumerate() {
        evidence.rank = i + 1;
        evidence.contribution = if legacy_scoring {
            0.5 / ((60.0 + evidence.rank as f64) * (evidence.incoming_callers.max(1) as f64).sqrt())
        } else {
            0.0
        };
        if let std::collections::btree_map::Entry::Vacant(entry) = merged.entry(id) {
            let mut c = crate::search::load_parent(conn, id)?;
            c.score = 0.0;
            c.hit.channel = "usage_target".into();
            c.hits = vec![c.hit.clone()];
            entry.insert(c);
        }
        let c = merged.get_mut(&id).unwrap();
        c.score += evidence.contribution;
        c.usage = Some(evidence);
    }
    *candidates = merged.into_values().collect();
    crate::candidate::sort(candidates);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BriefOptions, RetrievalMode, SearchEngine};
    use protobuf::Message;
    use safetensors::tensor::{Dtype, TensorView, serialize_to_file};
    use scip::types::{Document, Index, Metadata, Occurrence, PositionEncoding};
    use std::{collections::HashMap, fs, path::PathBuf};
    use tokenizers::{
        Tokenizer, models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace,
    };
    fn tiny_model(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let vocabulary = ["[UNK]", "cat", "feline", "dog", "canine"]
            .into_iter()
            .enumerate()
            .map(|(i, token)| (token.to_owned(), i as u32))
            .collect();
        let mut tokenizer = Tokenizer::new(
            WordLevel::builder()
                .vocab(vocabulary)
                .unk_token("[UNK]".into())
                .build()
                .unwrap(),
        );
        tokenizer.with_pre_tokenizer(Some(Whitespace));
        tokenizer.save(path.join("tokenizer.json"), false).unwrap();
        fs::write(path.join("config.json"), "{\"normalize\":true}").unwrap();
        let values = [0.0f32, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 1.0];
        let bytes: Vec<_> = values.into_iter().flat_map(f32::to_le_bytes).collect();
        let tensor = TensorView::new(Dtype::F32, vec![5, 2], &bytes).unwrap();
        serialize_to_file(
            HashMap::from([("embeddings", tensor)]),
            &None,
            &path.join("model.safetensors"),
        )
        .unwrap();
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        db: PathBuf,
        model: PathBuf,
        scip: PathBuf,
        manifest: PathBuf,
    }
    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        let model = temp.path().join("model");
        tiny_model(&model);
        let db = temp.path().join("index.sqlite");
        let targets = "pub fn accept() {}\npub fn spare() {}\n";
        let callers="/// cat\r\n#[test]\r\nfn scenario() { let _s=\"π\"; accept(); accept(); let alias=spare; }\r\n".to_string()
       + &(0..6).map(|i| format!("/// dog\nfn caller_{i}() {{ accept(); }}\n")).collect::<String>();
        fs::write(root.join("target.rs"), targets).unwrap();
        fs::write(root.join("caller.rs"), &callers).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        let report = crate::index(&root, &db).unwrap();
        crate::embed_index(&db, &model).unwrap();
        let conn = store::open(&db, false).unwrap();
        let mut docs = vec![];
        for (path, source) in [("target.rs", targets), ("caller.rs", callers.as_str())] {
            let mut occurrences = vec![];
            let mut stmt=conn.prepare("SELECT l.symbol,l.name_start,l.name_end FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path=?1 AND l.kind='function'").unwrap();
            let definitions: Vec<(String, usize, usize)> = stmt
                .query_map([path], |r| {
                    Ok((
                        r.get(0)?,
                        r.get::<_, u32>(1)? as usize,
                        r.get::<_, u32>(2)? as usize,
                    ))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let mut add = |symbol: &str, start: usize, end: usize, definition: bool| {
                let line = source[..start].bytes().filter(|b| *b == b'\n').count();
                let column = start - source[..start].rfind('\n').map_or(0, |i| i + 1);
                occurrences.push(Occurrence {
                    range: vec![line as i32, column as i32, (column + end - start) as i32],
                    symbol: format!("resolved {symbol}"),
                    symbol_roles: i32::from(definition),
                    ..Default::default()
                });
            };
            for (symbol, start, end) in definitions {
                add(&symbol, start, end, true);
            }
            if path == "caller.rs" {
                for word in ["accept", "spare"] {
                    for (start, _) in source.match_indices(word) {
                        add(word, start, start + word.len(), false);
                    }
                }
            }
            docs.push(Document {
                relative_path: path.into(),
                language: "rust".into(),
                position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                occurrences,
                ..Default::default()
            });
        }
        let index = Index {
            metadata: Some(Metadata {
                project_root: format!("file://{}", root.display()),
                ..Default::default()
            })
            .into(),
            documents: docs,
            ..Default::default()
        };
        let scip = temp.path().join("index.scip");
        fs::write(&scip, index.write_to_bytes().unwrap()).unwrap();
        let manifest = temp.path().join("manifest.json");
        let record = crate::SemanticManifest {
            root: root.to_str().unwrap().into(),
            snapshot: report.snapshot,
            inputs: crate::languages::rust::inputs::semantic_inputs(&root).unwrap(),
            analyzer: "fixture".into(),
            toolchain: "fixture".into(),
            config: serde_json::json!({}),
            scip_hash: blake3::hash(&fs::read(&scip).unwrap()).to_hex().to_string(),
        };
        fs::write(&manifest, serde_json::to_vec(&record).unwrap()).unwrap();
        crate::import_scip(&db, &scip, &manifest).unwrap();
        Fixture {
            _temp: temp,
            root,
            db,
            model,
            scip,
            manifest,
        }
    }

    #[test]
    fn structure_binds_only_current_call_sites_and_retains_external_scope_targets() {
        let f = fixture();
        let report = crate::structure(&f.db, Some("caller.rs")).unwrap();
        assert!(report.semantic_current);
        let caller = report
            .locations
            .iter()
            .find(|l| l.symbol.as_deref() == Some("scenario"))
            .unwrap();
        let calls: Vec<_> = caller
            .facts
            .as_ref()
            .unwrap()
            .evidence
            .iter()
            .filter(|e| e.feature == "calls")
            .collect();
        assert_eq!(calls.len(), 2);
        for call in calls {
            assert_eq!(call.targets.len(), 1);
            assert_eq!(call.targets[0].symbol.as_deref(), Some("accept"));
            assert_eq!(call.targets[0].path, "target.rs");
            assert_eq!(call.callee.as_ref().unwrap(), &call.targets[0].occurrence);
        }
        fs::write(
            f.root.join("Cargo.toml"),
            "[package]\nname='changed'\nversion='0.1.0'\n",
        )
        .unwrap();
        let stale = crate::structure(&f.db, None).unwrap();
        assert!(!stale.semantic_current);
        assert!(
            stale
                .locations
                .iter()
                .filter_map(|l| l.facts.as_ref())
                .flat_map(|f| &f.evidence)
                .all(|e| e.targets.is_empty())
        );
    }

    #[test]
    fn usage_discovers_through_verified_caller_and_keeps_coordinates_separate() {
        let f = fixture();
        let report = embed_usage(&f.db, &f.model).unwrap();
        assert_eq!((report.targets, report.contexts, report.encoded), (1, 4, 4));
        let reused = embed_usage(&f.db, &f.model).unwrap();
        assert_eq!((reused.reused, reused.encoded), (4, 0));
        let engine = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model))
            .unwrap()
            .with_usage(true);
        let d = engine.diagnose("feline", &BriefOptions::default()).unwrap();
        assert_eq!(d.usage_status, "current");
        let target = d
            .candidates
            .iter()
            .find(|c| c.symbol.as_deref() == Some("accept"))
            .unwrap();
        assert!(target.selected_rank.is_some());
        assert_eq!(target.lexical_rank, None);
        let u = target.usage.as_ref().unwrap();
        assert_eq!(u.caller_symbol, "scenario");
        assert_eq!((u.incoming_callers, u.outgoing_targets), (7, 1));
        assert_eq!((u.path.as_str(), u.line), ("caller.rs", 3));
        assert!(u.caller_is_test && u.similarity > 0.99);
        assert!(
            d.candidates
                .iter()
                .find(|c| c.symbol.as_deref() == Some("spare"))
                .unwrap()
                .usage
                .is_none()
        );
        let b = engine.brief("feline", &BriefOptions::default()).unwrap();
        assert!(
            b.links.iter().any(|l| l.kind == "calls"
                && l.evidence.path == "caller.rs"
                && l.evidence.line == 3)
        );
        let target = b
            .files
            .iter()
            .find(|f| f.path == "target.rs")
            .unwrap()
            .locations
            .iter()
            .find(|l| l.symbol.as_deref() == Some("accept"))
            .unwrap();
        assert!(target.focus.unwrap()[1] <= 2);
        assert_eq!(
            engine
                .diagnose("accept", &BriefOptions::default())
                .unwrap()
                .candidates[0]
                .symbol
                .as_deref(),
            Some("accept")
        );
        let conn = store::open(&f.db, false).unwrap();
        let rows: Vec<String> = conn
            .prepare("SELECT input FROM usage_contexts")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(rows.iter().all(|s| s.chars().count() <= 1540));
    }

    #[test]
    fn exclusions_block_callers_targets_and_graph_context_without_staling_analysis() {
        let f = fixture();
        embed_usage(&f.db, &f.model).unwrap();
        for excluded in ["caller.rs", "target.rs"] {
            for method in [
                crate::GraphMethod::OneHop,
                crate::GraphMethod::Propagation,
                crate::GraphMethod::PageRank,
            ] {
                let engine = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model))
                    .unwrap()
                    .with_usage(true)
                    .with_graph(true)
                    .with_exclusions(vec![excluded.into()])
                    .with_ranking(crate::RankingOptions {
                        graph_method: method,
                        ..Default::default()
                    });
                let d = engine
                    .diagnose("cat accept", &BriefOptions::default())
                    .unwrap();
                assert_eq!(d.usage_status, "current");
                assert!(
                    d.candidates
                        .iter()
                        .all(|c| c.path != excluded && c.usage.is_none())
                );
                assert!(d.related_candidates.iter().all(|c| c.path != excluded));
                let b = engine
                    .brief("cat accept", &BriefOptions::default())
                    .unwrap();
                assert!(!b.starts.is_empty());
                assert_eq!(b.index.semantic_freshness, "current");
                assert!(b.files.iter().all(|f| f.path != excluded));
                assert!(b.links.iter().all(|e| e.evidence.path != excluded));
            }
        }
    }

    #[test]
    fn usage_obeys_scope_budget_and_zero_vectors_without_changing_disabled_queries() {
        let f = fixture();
        let base = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model)).unwrap();
        let before =
            serde_json::to_value(base.brief("feline", &BriefOptions::default()).unwrap()).unwrap();
        embed_usage(&f.db, &f.model).unwrap();
        assert_eq!(
            before,
            serde_json::to_value(base.brief("feline", &BriefOptions::default()).unwrap()).unwrap()
        );
        let engine = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model))
            .unwrap()
            .with_usage(true);
        for scope in ["target.rs", "outside/"] {
            let opts = BriefOptions {
                scope: Some(scope.into()),
                ..Default::default()
            };
            let d = engine.diagnose("feline", &opts).unwrap();
            assert!(d.candidates.iter().all(|c| c.usage.is_none()));
            assert_eq!(
                serde_json::to_value(engine.brief("feline", &opts).unwrap()).unwrap(),
                serde_json::to_value(base.brief("feline", &opts).unwrap()).unwrap()
            );
        }
        let opts = BriefOptions {
            limit: 1,
            related: 0,
            scope: None,
        };
        let b = engine.brief("feline", &opts).unwrap();
        assert!(b.links.is_empty());
        assert_eq!(b.files.iter().map(|f| f.locations.len()).sum::<usize>(), 1);
        let d = engine
            .diagnose("unknown", &BriefOptions::default())
            .unwrap();
        assert!(d.candidates.is_empty());
        assert!(
            SearchEngine::open(&f.db, RetrievalMode::Lexical, None)
                .unwrap()
                .with_usage(true)
                .brief("cat", &opts)
                .is_err()
        );
    }

    #[test]
    fn usage_requires_fresh_complete_compatible_vectors_and_reimport_invalidates() {
        let f = fixture();
        let engine = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model))
            .unwrap()
            .with_usage(true);
        let status = || {
            engine
                .diagnose("feline", &BriefOptions::default())
                .unwrap()
                .usage_status
        };
        assert_eq!(status(), "missing");
        embed_usage(&f.db, &f.model).unwrap();
        let conn = store::open(&f.db, true).unwrap();
        conn.execute("DELETE FROM usage_contexts WHERE caller_id=(SELECT min(caller_id) FROM usage_contexts)",[]).unwrap();
        assert_eq!(status(), "incomplete");
        embed_usage(&f.db, &f.model).unwrap();
        conn.execute(
            "UPDATE metadata SET value='wrong' WHERE key='usage_model'",
            [],
        )
        .unwrap();
        assert_eq!(status(), "model_mismatch");
        embed_usage(&f.db, &f.model).unwrap();
        crate::import_scip(&f.db, &f.scip, &f.manifest).unwrap();
        assert_eq!(status(), "missing");
        embed_usage(&f.db, &f.model).unwrap();
        fs::write(f.root.join("Cargo.toml"), "changed build inputs").unwrap();
        assert_eq!(status(), "stale_semantics");
        assert!(embed_usage(&f.db, &f.model).is_err());
        assert_eq!(
            conn.query_row("SELECT count(*) FROM usage_contexts", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            4
        );
        fs::write(f.root.join("caller.rs"), "fn scenario() {}\n").unwrap();
        assert!(
            engine
                .diagnose("feline", &BriefOptions::default())
                .unwrap()
                .candidates
                .iter()
                .all(|c| c.usage.is_none())
        );
        crate::index(&f.root, &f.db).unwrap();
        assert_eq!(store::metadata(&conn, "usage_policy").unwrap(), None);
        assert_eq!(
            conn.query_row("SELECT count(*) FROM usage_contexts", [], |r| r
                .get::<_, u32>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn usage_damaged_vectors_fail_query_and_are_repaired_atomically() {
        let f = fixture();
        embed_usage(&f.db, &f.model).unwrap();
        let conn = store::open(&f.db, true).unwrap();
        conn.execute("UPDATE usage_contexts SET vector=x'00'", [])
            .unwrap();
        let engine = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model))
            .unwrap()
            .with_usage(true);
        assert!(engine.brief("feline", &BriefOptions::default()).is_err());
        assert_eq!(embed_usage(&f.db, &f.model).unwrap().encoded, 4);
        assert!(engine.brief("feline", &BriefOptions::default()).is_ok());
    }
    #[test]
    fn evidence_scoring_allows_usage_only_starts_and_rejects_weak_caller_votes() {
        let f = fixture();
        embed_usage(&f.db, &f.model).unwrap();
        let conn = store::open(&f.db, false).unwrap();
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM locations WHERE symbol LIKE 'caller_%' ORDER BY symbol")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let mut original: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let mut c = crate::search::load_parent(&conn, *id).unwrap();
                c.score = 1.0 / (61.0 + i as f64);
                c.view_similarities
                    .insert("implementation".into(), 0.2 + 0.05 * i as f64);
                c
            })
            .collect();
        let mut expanded = original.clone();
        retrieve(
            &conn,
            &[1.0, 0.0],
            "",
            &BTreeSet::new(),
            &mut expanded,
            false,
        )
        .unwrap();
        assert!(
            expanded
                .iter()
                .any(|c| c.symbol.as_deref() == Some("accept") && c.hit.channel == "usage_target")
        );
        let mut weak = expanded.clone();
        for c in &mut weak {
            if let Some(u) = &mut c.usage {
                u.similarity = 0.01;
            }
        }
        crate::scorer::rank(&mut original);
        crate::scorer::rank(&mut weak);
        assert_eq!(
            original.iter().map(|c| (c.id, c.score)).collect::<Vec<_>>(),
            weak.iter()
                .filter(|c| ids.contains(&c.id))
                .map(|c| (c.id, c.score))
                .collect::<Vec<_>>()
        );
        assert!(
            weak.iter()
                .filter_map(|c| c.usage.as_ref())
                .all(|u| u.contribution == 0.0)
        );
        crate::scorer::rank(&mut expanded);
        assert!(
            expanded
                .iter()
                .take(5)
                .any(|c| c.symbol.as_deref() == Some("accept"))
        );
        let target = expanded
            .iter()
            .find(|c| c.symbol.as_deref() == Some("accept"))
            .unwrap();
        assert_eq!(target.path, "target.rs");
        assert!(target.ranking.as_ref().unwrap().usage_strength > 0.0);
        assert_eq!(target.usage.as_ref().unwrap().path, "caller.rs");
    }

    #[test]
    fn evidence_exact_queries_preserve_the_complete_no_usage_control() {
        let f = fixture();
        embed_usage(&f.db, &f.model).unwrap();
        for graph in [false, true] {
            let baseline = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model))
                .unwrap()
                .with_graph(graph);
            let revised = SearchEngine::open(&f.db, RetrievalMode::Hybrid, Some(&f.model))
                .unwrap()
                .with_graph(graph)
                .with_usage(true)
                .with_scorer(crate::ScoringPolicy::Evidence);
            for query in ["accept", "scenario", "target.rs"] {
                let opts = BriefOptions::default();
                assert_eq!(
                    serde_json::to_value(baseline.brief(query, &opts).unwrap()).unwrap(),
                    serde_json::to_value(revised.brief(query, &opts).unwrap()).unwrap()
                );
                let d = revised.diagnose(query, &opts).unwrap();
                assert!(d.scorer_bypassed_for_exact);
                assert!(
                    d.candidates
                        .iter()
                        .all(|c| c.usage.is_none() && c.ranking.is_none())
                );
            }
        }
    }
}
