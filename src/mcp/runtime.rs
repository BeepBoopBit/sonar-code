use std::{
    path::{Path, PathBuf},
    thread,
};

use anyhow::{Context, Result, ensure};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::params;
use crate::{Investigator, store, workspace};

pub(super) const QUEUE_CAPACITY: usize = 8;

pub(super) enum Operation {
    Status,
    Brief(params::Brief),
    Investigate(params::Investigate),
    Locate(params::Locate),
    Outline(params::Outline),
    Neighbors(params::Neighbors),
    Follow(params::Follow),
    FieldUses(params::FieldUses),
}

struct Job {
    operation: Operation,
    response: oneshot::Sender<Result<Value, String>>,
}

#[derive(Clone)]
pub(super) struct Worker {
    sender: mpsc::Sender<Job>,
}

pub(super) fn tool_error(code: &str, message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(json!({"error": {"code": code, "message": message.into()}}))
}

impl Worker {
    pub(super) fn start(db: &Path, model: Option<&Path>) -> Result<(Self, thread::JoinHandle<()>)> {
        let db = db.to_owned();
        let model = model.map(Path::to_owned);
        let (sender, mut receiver) = mpsc::channel::<Job>(QUEUE_CAPACITY);
        let (ready, startup) = std::sync::mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("sonar-navigation".into())
            .spawn(move || {
                // All model and SQLite work stays off the protocol executor. A cancelled
                // caller drops its receiver; queued work is skipped and active work's
                // result is discarded. The synchronous engine itself is not preempted.
                let runtime = match Runtime::open(&db, model.as_deref()) {
                    Ok(runtime) => {
                        let _ = ready.send(Ok(()));
                        runtime
                    }
                    Err(error) => {
                        let _ = ready.send(Err(format!("{error:#}")));
                        return;
                    }
                };
                while let Some(job) = receiver.blocking_recv() {
                    if job.response.is_closed() {
                        continue;
                    }
                    let result = runtime.execute(job.operation).map_err(|e| format!("{e:#}"));
                    let _ = job.response.send(result);
                }
            })?;
        match startup
            .recv()
            .context("navigation worker stopped during startup")?
        {
            Ok(()) => Ok((Self { sender }, thread)),
            Err(message) => {
                let _ = thread.join();
                anyhow::bail!(message)
            }
        }
    }

    pub(super) async fn call(&self, operation: Operation) -> CallToolResult {
        let (response, received) = oneshot::channel();
        if let Err(error) = self.sender.try_send(Job {
            operation,
            response,
        }) {
            return match error {
                mpsc::error::TrySendError::Full(_) => tool_error(
                    "busy",
                    "Sonar's navigation queue is full (one active request and eight waiting). Retry after an outstanding request completes.",
                ),
                mpsc::error::TrySendError::Closed(_) => tool_error(
                    "worker_unavailable",
                    "Navigation worker stopped; restart the Sonar MCP process.",
                ),
            };
        }
        match received.await {
            Ok(Ok(value)) => CallToolResult::structured(value),
            Ok(Err(message)) => tool_error("navigation_error", message),
            Err(_) => tool_error(
                "worker_unavailable",
                "Navigation worker stopped; restart the Sonar MCP process.",
            ),
        }
    }
}

struct Runtime {
    db: PathBuf,
    root: String,
    investigator: Investigator,
}

impl Runtime {
    fn open(db: &Path, model: Option<&Path>) -> Result<Self> {
        let conn = store::open(db, false)?;
        let root = store::metadata(&conn, "root")?
            .context("index sources first with `sonar index <repository>`")?;
        let db = db.canonicalize()?;
        Ok(Self {
            investigator: Investigator::open(&db, model)?,
            db,
            root,
        })
    }

    fn execute(&self, operation: Operation) -> Result<Value> {
        let conn = store::open(&self.db, false)?;
        ensure!(
            store::metadata(&conn, "root")?.as_deref() == Some(&self.root),
            "the configured database now belongs to a different repository; restart Sonar with the intended --db"
        );
        drop(conn);
        let engine = self.investigator.engine();
        Ok(match operation {
            Operation::Status => self.status()?,
            Operation::Brief(p) => serde_json::to_value(
                engine
                    .inspect_filtered(
                        &p.query,
                        &crate::BriefOptions {
                            limit: p.limit.unwrap_or(5),
                            related: p.related.unwrap_or(3),
                            scope: p.scope,
                        },
                        p.kind.as_deref(),
                        p.role,
                        &p.exclude,
                    )?
                    .0,
            )?,
            Operation::Investigate(p) => serde_json::to_value(self.investigator.investigate(
                &p.query,
                &crate::InvestigationOptions {
                    from: p.from,
                    scope: p.scope,
                    exclusions: p.exclude,
                },
            )?)?,
            Operation::Locate(p) => {
                serde_json::to_value(crate::locate(&self.db, &p.path, p.line, p.column)?)?
            }
            Operation::Outline(p) => serde_json::to_value(crate::outline(
                &self.db,
                &p.path,
                p.limit.unwrap_or(50),
                p.cursor.as_deref(),
            )?)?,
            Operation::Neighbors(p) => serde_json::to_value(crate::neighbors(
                &self.db,
                &p.handle,
                &crate::NeighborOptions {
                    relation: p.relation.into(),
                    direction: p.direction.into(),
                    scope: p.scope,
                    limit: p.limit.unwrap_or(20),
                    cursor: p.cursor,
                },
            )?)?,
            Operation::Follow(p) => serde_json::to_value(engine.follow(
                &p.handle,
                &p.query,
                &crate::FollowOptions {
                    relation: p.relation.into(),
                    direction: p.direction.into(),
                    scope: p.scope,
                    kind: p.kind,
                    hops: p.hops.unwrap_or(2),
                    limit: p.limit.unwrap_or(5),
                    focus: engine.navigation_setup().1,
                },
            )?)?,
            Operation::FieldUses(p) => serde_json::to_value(crate::field_uses(
                &self.db,
                &p.handle,
                &crate::FieldUseOptions {
                    scope: p.scope,
                    limit: p.limit.unwrap_or(20),
                    cursor: p.cursor,
                },
            )?)?,
        })
    }

    fn status(&self) -> Result<Value> {
        let mut conn = store::open(&self.db, false)?;
        let tx = conn.transaction()?;
        let snapshot =
            store::metadata(&tx, "snapshot")?.context("index is missing its snapshot")?;
        let sources = workspace::scan_index(&tx, Path::new(&self.root))?;
        let fresh = workspace::snapshot(&sources) == snapshot;
        let indexed = store::file_hashes(&tx)?;
        let current: std::collections::BTreeMap<_, _> =
            sources.iter().map(|s| (&s.path, &s.hash)).collect();
        let changed = indexed
            .iter()
            .filter(|(p, h)| current.get(p).copied() != Some(h))
            .count();
        let added = current
            .keys()
            .filter(|p| !indexed.contains_key(p.as_str()))
            .count();
        let profile = crate::semantic::profile(&tx)?;
        let languages = crate::languages::states(&tx, &sources, &snapshot, fresh)?;
        let semantic = languages.iter().any(|l| l.semantic_freshness == "current");
        let navigation = crate::navigation::current(&tx, fresh, &snapshot)?;
        let embedding_model = store::metadata(&tx, "embedding_model")?;
        let embedding_snapshot = store::metadata(&tx, "embedding_snapshot")?;
        let representation = store::metadata(&tx, "embedding_representation")?;
        let vectors: i64 = tx.query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))?;
        let expected: i64 = tx.query_row("SELECT count(*) FROM regions WHERE (?1='combined' AND view='combined') OR (?1 IN ('separate','context') AND view!='combined')", [representation.as_deref().unwrap_or("")], |r| r.get(0))?;
        let (retrieval, focus, configured_model) = self.investigator.engine().navigation_setup();
        let mut notices = crate::languages::coverage_notices(&languages);
        if languages
            .iter()
            .any(|l| l.language == "typescript" && l.semantic_freshness == "current")
            && let Some(notice) = crate::languages::typescript::semantic::coverage_notice(&tx)?
        {
            notices.push(notice);
        }
        if !fresh {
            notices.push(crate::Notice::new("stale_index", "Run `sonar index <repository>` with the same indexing options. Changed/deleted files are omitted by briefs; explicit navigation requires a current snapshot."));
        }
        if !semantic && languages.iter().any(|l| l.capabilities.resolved_references) {
            notices.push(crate::Notice::new(
                "graph_unavailable",
                crate::semantic::setup(&languages),
            ));
        }
        for language in &languages {
            if language.files > 0 && !language.capabilities.resolved_references {
                notices.push(crate::Notice::new("language_analysis_unavailable", format!("{} supports structural navigation; resolved calls, references, and field-use analysis are not available for this language.", language.language)));
            }
        }
        if !navigation {
            notices.push(crate::Notice::new("navigation_unavailable", "Run `sonar index-navigation` after indexing for literal documentation/resource links."));
        }
        let embeddings_current = fresh
            && embedding_snapshot.as_deref() == Some(&snapshot)
            && expected > 0
            && vectors == expected;
        if configured_model.is_some()
            && (!embeddings_current || configured_model != embedding_model.as_deref())
        {
            notices.push(crate::Notice::new("embeddings_unavailable", "Hybrid discovery needs matching complete embeddings: run `sonar embed --model <configured-model>`. If switching models/encoding, restart this server with the new configuration."));
        }
        Ok(json!({
            "version": 1, "root": self.root, "database": self.db,
            "index": {"snapshot": snapshot, "syntax_freshness": if fresh {"current"} else {"stale"},
                "semantic_profile": profile, "semantic_freshness": crate::languages::aggregate_semantic(&languages, semantic, profile.is_some()), "languages": languages},
            "files": indexed.len(), "changed_or_deleted_files": changed, "added_files": added,
            "locations": tx.query_row("SELECT count(*) FROM locations", [], |r| r.get::<_,i64>(0))?,
            "parse_error_files": tx.query_row("SELECT count(*) FROM files WHERE parse_errors != 0", [], |r| r.get::<_,i64>(0))?,
            "literal_navigation_current": navigation,
            "retrieval": retrieval, "focus_policy": focus.policy,
            "configured_model_fingerprint": configured_model,
            "embeddings": {"model_fingerprint": embedding_model, "representation": representation,
                "vectors": vectors, "expected_vectors": expected, "current": embeddings_current,
                "matches_configured_model": configured_model.map(|m| Some(m) == embedding_model.as_deref())},
            "read_only": true, "queue_capacity": QUEUE_CAPACITY, "notices": notices
        }))
    }
}
