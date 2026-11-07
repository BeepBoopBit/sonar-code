//! Change-driven review context over the current indexed working tree.
mod baseline;
mod cache;
mod cache_inputs;
mod comparison;
mod git;
mod matching;
mod model;
use super::{
    DependencyCoverage, DependencyEvidence, ReviewContextOptions, ReviewSeed, graph, review,
};
use crate::{NavigationLocation, Notice, explore, search, store, workspace};
use anyhow::{Result, ensure};
pub use model::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

const POLICY: &str = "impact-v2";
const DECLARATION_LIMIT: usize = 100;
const CONSUMER_LIMIT: usize = 20;
const HUNK_LIMIT: usize = 100;

struct Consumer {
    from: i64,
    to: i64,
    kind: String,
    from_path: String,
    evidence: DependencyEvidence,
    occurrences: usize,
}

fn consumers(s: &explore::Snapshot<'_>, ids: &[i64]) -> Result<Vec<Consumer>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let mut stmt=s.conn.prepare("SELECT e.from_id,e.to_id,e.kind,a.path,e.path,e.line,e.start_byte,e.end_byte FROM semantic_edges e
        JOIN locations l ON l.id=e.from_id JOIN files a ON a.id=l.file_id
        WHERE e.to_id IN (SELECT value FROM json_each(?1)) AND e.kind IN ('calls','references','implements')
        ORDER BY e.kind,a.path,e.from_id,e.to_id,e.path,e.start_byte,e.end_byte")?;
    let mut grouped: BTreeMap<(String, String, i64, i64, String), Consumer> = BTreeMap::new();
    for row in stmt.query_map([serde_json::to_string(ids)?], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, u32>(5)?,
            r.get::<_, u32>(6)?,
            r.get::<_, u32>(7)?,
        ))
    })? {
        let (from, to, kind, from_path, path, line, start, end) = row?;
        let language = crate::source_language(&from_path).unwrap_or("unknown");
        let profile = s
            .index
            .languages
            .iter()
            .find(|l| l.language == language)
            .and_then(|l| l.semantic_profile.as_deref())
            .unwrap_or("compiler");
        let consumer = grouped
            .entry((kind.clone(), from_path.clone(), from, to, path.clone()))
            .or_insert(Consumer {
                from,
                to,
                kind,
                from_path,
                evidence: DependencyEvidence {
                    path,
                    line: line as usize,
                    bytes: [start as usize, end as usize],
                    provider: format!("{language}:{profile}"),
                },
                occurrences: 0,
            });
        consumer.occurrences += 1;
    }
    Ok(grouped.into_values().collect())
}

fn change_report(
    s: &explore::Snapshot<'_>,
    data: &graph::GraphData,
    change: &git::Change,
    options: &ImpactOptions,
    comparison: &comparison::Comparison<'_, '_>,
) -> Result<ImpactChange> {
    let compared = comparison.compare(s, data, change, options)?;
    let mut gaps = compared.gaps;
    let indexed = s.sources.contains_key(&change.path);
    let code = crate::source_language(&change.path).is_some();
    if !code {
        gaps.push("non_code_change_not_analyzed".into());
    }
    if !indexed && change.status != "D" {
        gaps.push("file_not_indexed".into());
    }
    if change.hunks.is_empty() {
        gaps.push("no_text_hunks".into());
    }
    let parse_errors: bool = if indexed {
        s.conn.query_row(
            "SELECT parse_errors!=0 FROM files WHERE path=?1",
            [&change.path],
            |r| r.get(0),
        )?
    } else {
        false
    };
    if parse_errors {
        gaps.push("parse_errors".into());
    }
    let ids = if code && indexed && !parse_errors {
        compared.after_ids.iter().copied().collect()
    } else {
        vec![]
    };
    if code && indexed && ids.is_empty() {
        gaps.push("no_overlapping_current_declaration".into());
    }
    if let Some(language) = crate::source_language(&change.path)
        && !s
            .index
            .languages
            .iter()
            .any(|l| l.language == language && l.semantic_freshness == "current")
    {
        gaps.push("current_compiler_consumers_unavailable".into());
    }
    let incoming = consumers(s, &ids)?;
    let mut preferred = incoming
        .iter()
        .map(|c| c.from_path.clone())
        .collect::<BTreeSet<_>>();
    preferred.extend(compared.preferred);
    let consumers_total = incoming.len();
    let mut result_consumers = Vec::new();
    for consumer in incoming.into_iter().take(CONSUMER_LIMIT) {
        result_consumers.push(ImpactConsumer {
            location: s.location(consumer.from)?,
            target: s.location(consumer.to)?.handle,
            kind: consumer.kind,
            occurrences: consumer.occurrences,
            evidence: consumer.evidence,
        });
    }
    let declarations = ids
        .iter()
        .take(DECLARATION_LIMIT)
        .map(|id| s.location(*id))
        .collect::<Result<Vec<NavigationLocation>>>()?;
    let cluster = if code && indexed && !parse_errors {
        let range = change
            .hunks
            .iter()
            .find(|h| h.new_lines > 0)
            .map(|h| [h.new_start, h.new_start.saturating_add(h.new_lines - 1)]);
        Some(
            review::context_cluster(
                s,
                data,
                ReviewSeed {
                    path: change.path.clone(),
                    symbol: None,
                    range,
                },
                &ReviewContextOptions {
                    max_files: options.max_files,
                    scope: options.scope.clone(),
                },
                &preferred,
            )?
            .0,
        )
    } else {
        None
    };
    Ok(ImpactChange {
        path: change.path.clone(),
        status: change.status.clone(),
        hunks: change.hunks.iter().take(HUNK_LIMIT).cloned().collect(),
        hunks_total: change.hunks.len(),
        declarations,
        declarations_total: ids.len(),
        consumers: result_consumers,
        consumers_total,
        cluster,
        gaps,
        details_truncated: ids.len() > DECLARATION_LIMIT
            || consumers_total > CONSUMER_LIMIT
            || change.hunks.len() > HUNK_LIMIT
            || compared.before_ids.len() > DECLARATION_LIMIT
            || compared.changes.len() > DECLARATION_LIMIT
            || compared.consumers.len() > CONSUMER_LIMIT
            || compared.relationships.len() > CONSUMER_LIMIT,
        baseline_declarations_total: compared.before_ids.len(),
        baseline_declarations: compared
            .before_ids
            .iter()
            .take(DECLARATION_LIMIT)
            .map(|id| comparison.before.declarations[id].location.clone())
            .collect(),
        declaration_changes_total: compared.changes.len(),
        declaration_changes: compared
            .changes
            .into_iter()
            .take(DECLARATION_LIMIT)
            .collect(),
        baseline_consumers_total: compared.consumers.len(),
        baseline_consumers: compared
            .consumers
            .into_iter()
            .take(CONSUMER_LIMIT)
            .collect(),
        baseline_cluster: compared.cluster,
        relationship_changes_total: compared.relationships.len(),
        relationship_changes: compared
            .relationships
            .into_iter()
            .take(CONSUMER_LIMIT)
            .collect(),
    })
}

pub fn impact(db: &Path, options: &ImpactOptions) -> Result<ImpactResponse> {
    ensure!(
        (1..=50).contains(&options.max_files),
        "max_files must be between 1 and 50"
    );
    ensure!(
        (1..=20).contains(&options.limit),
        "impact limit must be between 1 and 20"
    );
    ensure!(
        !options.base.is_empty() && options.base.len() <= 1024,
        "base must be a nonempty Git revision of at most 1024 bytes"
    );
    let scope = search::normalize_scope(options.scope.as_deref())?;
    explore::read(db, |s| {
        let root = store::metadata(s.conn, "root")?.expect("checked snapshot root");
        let root = Path::new(&root);
        let diff = git::read(root, &options.base, &scope, &s.sources)?;
        let data = graph::load(s)?;
        let prepared =
            baseline::Prepared::new(root, &diff.base_commit, s, options.baseline_resolve)?;
        explore::read(&prepared.db, |old| {
            let old_graph = graph::load(old)?;
            let comparison = comparison::Comparison::new(old, &old_graph, s, &diff.base_commit)?;
            let baseline_info = prepared.info(old, &diff.base_commit, options.baseline_resolve)?;
            let request = explore::request((
                POLICY,
                &s.generation,
                &s.index,
                s.navigation_current,
                &diff.fingerprint,
                &options.base,
                &scope,
                options.max_files,
                options.limit,
                &data.relationships,
                &data.unresolved,
                &data.coverage,
                (
                    options.baseline_resolve,
                    comparison.before.fingerprint()?,
                    comparison.after.fingerprint()?,
                    comparison.graph_fingerprint()?,
                    (
                        &old_graph.relationships,
                        &old_graph.unresolved,
                        &old_graph.coverage,
                    ),
                    baseline_info
                        .compilers
                        .iter()
                        .map(|c| (&c.language, &c.status))
                        .collect::<Vec<_>>(),
                ),
            ))?;
            let offset = super::pagination::offset(
                options.cursor.as_deref(),
                &request,
                options.limit,
                diff.changes.len(),
            )?;
            let next_cursor =
                super::pagination::next(&request, offset, options.limit, diff.changes.len());
            let changes = diff
                .changes
                .iter()
                .skip(offset)
                .take(options.limit)
                .map(|c| change_report(s, &data, c, options, &comparison))
                .collect::<Result<Vec<_>>>()?;
            // Git state and current sources must still match the snapshot used for all evidence.
            ensure!(
                git::read(root, &options.base, &scope, &s.sources)?.fingerprint == diff.fingerprint,
                "Git changes changed during impact analysis; refresh and retry"
            );
            ensure!(
                workspace::snapshot(&workspace::scan_index(s.conn, root)?) == s.index.snapshot,
                "sources changed during impact analysis; refresh and retry"
            );
            let analysis_coverage = data
                .coverage
                .iter()
                .map(|c| DependencyCoverage {
                    language: c.language.clone(),
                    provider: c.provider.clone(),
                    analyzed_files: c.analyzed_files,
                    skipped_files: c.skipped_files.iter().take(20).cloned().collect(),
                    skipped_total: c.skipped_total,
                    limitations: c.limitations.clone(),
                })
                .collect();
            let mut notices = data.notices;
            notices.push(Notice::new("impact_limits","This compares a base commit with the current working tree, including nonignored untracked files. Declaration candidates overlap changed lines; this is not a semantic diff or confirmed regression analysis. Baseline syntax is analyzed in isolation. Unique path/kind/symbol matches are structural identities; masked-source rename/move candidates remain uncertain. Compiler-backed baseline consumers require explicit preparation; unavailable analysis is not evidence of absent callers. Git renames are reported as deletion/addition while possible declaration matches are shown separately. Review clusters prioritize recorded current consumers within their file budget; paths are static evidence, not execution traces. Current compiler evidence covers its analyzed build contexts; empty consumer lists do not establish absence of impact. Per-file details are capped with totals."));
            Ok(ImpactResponse {
                version: 2,
                policy: POLICY.into(),
                requested_base: options.base.clone(),
                base_commit: diff.base_commit,
                comparison: "base_to_worktree_including_untracked".into(),
                baseline_analysis: true,
                baseline: baseline_info,
                changes,
                changes_total: diff.changes.len(),
                truncated: next_cursor.is_some(),
                next_cursor,
                index: s.index.clone(),
                analysis_coverage,
                notices,
            })
        })
    })
}

pub use cache::{
    BaselineCacheEntry, BaselineCacheList, BaselineCachePrune, baseline_cache_list,
    baseline_cache_prune,
};

// Narrow read-only helpers for review-pack provenance and baseline excerpts.
pub(super) fn pack_diff_fingerprint(
    s: &explore::Snapshot<'_>,
    base: &str,
    scope: Option<&str>,
) -> Result<String> {
    let root = store::metadata(s.conn, "root")?.expect("checked snapshot root");
    Ok(git::read(
        Path::new(&root),
        base,
        &search::normalize_scope(scope)?,
        &s.sources,
    )?
    .fingerprint)
}
pub(super) fn pack_baseline_source(
    root: &Path,
    commit: &str,
    path: &str,
) -> Result<Option<String>> {
    let bytes = git::output(root, &["show", &format!("{commit}:{path}")])?;
    Ok(String::from_utf8(bytes).ok())
}
