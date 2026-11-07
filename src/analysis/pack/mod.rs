//! Model-independent review inputs with explicit source provenance and bounded payloads.
use super::{
    ImpactChange, ImpactOptions, ReviewAreasOptions, ReviewCluster, ReviewContextOptions, graph,
    impact, review_areas, review_context,
};
use crate::{explore, search, store};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, path::Path};

const POLICY: &str = "review-pack-v1";

mod model;
pub use model::*;

fn size(pack: &mut ReviewPack) -> Result<usize> {
    // The decimal size field contributes to its own size; converge before testing admission.
    loop {
        let bytes = serde_json::to_vec(pack)?.len();
        if bytes == pack.estimated_tokens {
            return Ok(bytes);
        }
        pack.estimated_tokens = bytes;
    }
}
fn state(s: &explore::Snapshot<'_>, options: &ReviewPackOptions) -> Result<String> {
    let graph = graph::load(s)?;
    let diff = options
        .base
        .as_deref()
        .map(|base| impact::pack_diff_fingerprint(s, base, options.scope.as_deref()))
        .transpose()?;
    explore::request((
        POLICY,
        &s.generation,
        &s.index,
        s.navigation_current,
        &graph.relationships,
        &graph.unresolved,
        &graph.coverage,
        diff,
    ))
}

struct Input {
    title: String,
    clusters: Vec<(&'static str, ReviewCluster)>,
    change: Option<ImpactChange>,
}

fn build(
    s: &explore::Snapshot<'_>,
    input: Input,
    options: &ReviewPackOptions,
    fingerprint: &str,
    base: Option<&str>,
) -> Result<ReviewPack> {
    let identity = explore::request((
        fingerprint,
        &input.title,
        input
            .clusters
            .iter()
            .map(|(snapshot, c)| (*snapshot, &c.id))
            .collect::<Vec<_>>(),
        &input.change,
    ))?;
    let mut pack = ReviewPack {
        id: format!("rp1:{identity}"),
        fingerprint: fingerprint.into(),
        mode: if options.base.is_some() {
            "changes"
        } else {
            "audit"
        }
        .into(),
        current_snapshot: s.index.snapshot.clone(),
        base_commit: base.map(str::to_owned),
        title: input.title,
        change_path: input.change.as_ref().map(|c| c.path.clone()),
        change_status: input.change.as_ref().map(|c| c.status.clone()),
        max_files: options.max_files,
        max_tokens: options.max_tokens,
        budget_policy: "compact-json-utf8-bytes-v1".into(),
        estimated_tokens: 0,
        reading_path: vec![],
        hunks: vec![],
        declaration_changes: vec![],
        relationship_changes: vec![],
        boundary: vec![],
        related_tests: vec![],
        gaps: vec![],
        totals: ReviewPackTotals::default(),
        truncated: false,
    };
    let mut files = vec![];
    let mut boundaries = vec![];
    let mut tests = vec![];
    for (snapshot, cluster) in input.clusters {
        pack.totals.files += cluster.files.len();
        pack.totals.boundary += cluster.boundary_total;
        pack.totals.related_tests += cluster.related_tests_total;
        pack.totals.unresolved += cluster.unresolved_total;
        pack.totals.outside_files += cluster.omitted_files_total;
        files.extend(
            cluster
                .reading_path
                .into_iter()
                .map(|step| (snapshot, step)),
        );
        boundaries.extend(
            cluster
                .boundary
                .into_iter()
                .map(|detail| ReviewPackBoundary {
                    snapshot: snapshot.into(),
                    detail,
                }),
        );
        tests.extend(
            cluster
                .related_tests
                .into_iter()
                .map(|detail| ReviewPackTest {
                    snapshot: snapshot.into(),
                    detail,
                }),
        );
        if cluster.boundary_truncated {
            pack.truncated = true;
        }
    }
    // Prioritize the changed file in both snapshots before expanding into consumers.
    files.sort_by_key(|(snapshot, step)| {
        let position = if Some(&step.file) == pack.change_path.as_ref() {
            0
        } else {
            1
        };
        (position, if *snapshot == "current" { 0 } else { 1 })
    });
    for (snapshot, step) in files {
        if pack.reading_path.len() >= options.max_files {
            break;
        }
        let mut ranges = step.ranges;
        ranges.sort();
        let mut merged: Vec<[usize; 2]> = vec![];
        for range in ranges {
            if let Some(last) = merged.last_mut()
                && range[0] <= last[1].saturating_add(1)
            {
                last[1] = last[1].max(range[1]);
            } else {
                merged.push(range);
            }
        }
        let ranges = merged;
        let requested_lines = ranges.iter().map(|r| r[1].saturating_sub(r[0]) + 1).sum();
        pack.reading_path.push(ReviewPackFile {
            path: step.file,
            snapshot: snapshot.into(),
            source_role: step.source_role,
            parent_file: step.parent_file,
            via: step.via,
            requested_ranges: ranges,
            excerpts: vec![],
            requested_lines,
            included_lines: 0,
        });
        if size(&mut pack)? > options.max_tokens * 3 / 4 {
            pack.reading_path.pop();
            pack.truncated = true;
        }
    }
    if let Some(change) = input.change {
        pack.totals.hunks = change.hunks_total;
        pack.totals.declaration_changes = change.declaration_changes_total;
        pack.totals.relationship_changes = change.relationship_changes_total;
        pack.truncated |= change.details_truncated;
        pack.totals.gaps = change.gaps.len();
        // Reserve at least a quarter of the pack budget for source excerpts.
        for gap in change.gaps {
            pack.gaps.push(gap);
            if size(&mut pack)? > options.max_tokens * 3 / 4 {
                pack.gaps.pop();
                pack.truncated = true;
            }
        }
        for hunk in change.hunks {
            pack.hunks.push(hunk);
            if size(&mut pack)? > options.max_tokens * 3 / 4 {
                pack.hunks.pop();
                pack.truncated = true;
            }
        }
        for change in change.declaration_changes {
            pack.declaration_changes.push(change);
            if size(&mut pack)? > options.max_tokens * 3 / 4 {
                pack.declaration_changes.pop();
                pack.truncated = true;
            }
        }
        for change in change.relationship_changes {
            pack.relationship_changes.push(change);
            if size(&mut pack)? > options.max_tokens * 3 / 4 {
                pack.relationship_changes.pop();
                pack.truncated = true;
            }
        }
    }
    for boundary in boundaries {
        pack.boundary.push(boundary);
        if size(&mut pack)? > options.max_tokens * 3 / 4 {
            pack.boundary.pop();
            pack.truncated = true;
        }
    }
    for test in tests {
        pack.related_tests.push(test);
        if size(&mut pack)? > options.max_tokens * 3 / 4 {
            pack.related_tests.pop();
            pack.truncated = true;
        }
    }
    ensure!(
        size(&mut pack)? <= options.max_tokens,
        "review pack identity exceeds budget; increase max_tokens"
    );
    let root = store::metadata(s.conn, "root")?.context("missing index root")?;
    let mut candidates = Vec::new();
    for file in &pack.reading_path {
        let source = if file.snapshot == "current" {
            s.sources.get(&file.path).map(|f| f.source.clone())
        } else {
            impact::pack_baseline_source(
                Path::new(&root),
                base.context("missing baseline commit")?,
                &file.path,
            )?
        };
        let Some(source) = source else {
            candidates.push(vec![]);
            pack.totals.unavailable_sources += 1;
            continue;
        };
        let mut lines = BTreeMap::new();
        for (index, line) in source.split_inclusive('\n').enumerate() {
            let n = index + 1;
            if file.requested_ranges.iter().any(|r| r[0] <= n && n <= r[1]) {
                lines.insert(n, line.to_owned());
            }
            // Source excerpts are intentionally bounded even when a declaration spans thousands of lines.
            if lines.len() >= 120 {
                break;
            }
        }
        candidates.push(lines.into_iter().collect::<Vec<_>>());
    }
    // Round-robin admission gives each selected file source context before expanding any one file.
    for row in 0..120 {
        for (i, lines) in candidates.iter().enumerate() {
            let Some((line, text)) = lines.get(row) else {
                continue;
            };
            let file = &mut pack.reading_path[i];
            let merged = file
                .excerpts
                .last()
                .is_some_and(|e| e.range[1] + 1 == *line);
            if merged {
                let last = file.excerpts.last_mut().unwrap();
                last.range[1] = *line;
                last.text.push_str(text);
            } else {
                file.excerpts.push(ReviewExcerpt {
                    range: [*line, *line],
                    text: text.clone(),
                });
            }
            file.included_lines += 1;
            if size(&mut pack)? > options.max_tokens {
                let file = &mut pack.reading_path[i];
                file.included_lines -= 1;
                if merged {
                    let last = file.excerpts.last_mut().unwrap();
                    last.range[1] -= 1;
                    last.text.truncate(last.text.len() - text.len());
                } else {
                    file.excerpts.pop();
                }
                pack.truncated = true;
            }
        }
    }
    pack.truncated |= pack.reading_path.len() < pack.totals.files
        || pack
            .reading_path
            .iter()
            .any(|f| f.included_lines < f.requested_lines)
        || pack.boundary.len() < pack.totals.boundary
        || pack.related_tests.len() < pack.totals.related_tests
        || pack.declaration_changes.len() < pack.totals.declaration_changes
        || pack.relationship_changes.len() < pack.totals.relationship_changes
        || pack.hunks.len() < pack.totals.hunks
        || pack.gaps.len() < pack.totals.gaps;
    ensure!(
        size(&mut pack)? <= options.max_tokens,
        "review pack exceeded budget"
    );
    Ok(pack)
}

/// Build review inputs without invoking an LLM or interpreting code as instructions.
pub fn review_pack(db: &Path, options: &ReviewPackOptions) -> Result<ReviewPackResponse> {
    ensure!(
        (1..=50).contains(&options.max_files),
        "max_files must be between 1 and 50"
    );
    ensure!(
        (2048..=65536).contains(&options.max_tokens),
        "max_tokens must be between 2048 and 65536"
    );
    ensure!(
        (1..=20).contains(&options.limit),
        "limit must be between 1 and 20"
    );
    ensure!(
        options.seed.is_none() || options.base.is_none(),
        "seed and base are mutually exclusive"
    );
    ensure!(
        !options.baseline_resolve || options.base.is_some(),
        "baseline_resolve requires base"
    );
    ensure!(
        options.seed.is_none() || options.cursor.is_none(),
        "seed packs have no continuation"
    );
    let scope = search::normalize_scope(options.scope.as_deref())?;
    let query = explore::request((
        POLICY,
        &options.seed,
        &options.base,
        options.baseline_resolve,
        options.max_files,
        options.max_tokens,
        &scope,
        options.limit,
    ))?;
    let cursor = options
        .cursor
        .as_deref()
        .map(|cursor| {
            ensure!(cursor.len() <= 256, "invalid review pack cursor");
            let prefix = format!("rp1:{query}:");
            Ok(cursor
                .strip_prefix(&prefix)
                .context("review pack cursor belongs to different options; restart the query")?
                .to_owned())
        })
        .transpose()?;
    let before = explore::read(db, |s| state(s, options))?;
    let (inputs, total, index, coverage, area_coverage, baseline, next, truncated, notices) =
        if let Some(base) = &options.base {
            let response = impact(
                db,
                &ImpactOptions {
                    base: base.clone(),
                    max_files: options.max_files,
                    scope: options.scope.clone(),
                    limit: options.limit,
                    cursor,
                    baseline_resolve: options.baseline_resolve,
                },
            )?;
            let inputs = response
                .changes
                .into_iter()
                .map(|mut change| {
                    let mut clusters = vec![];
                    if let Some(c) = change.cluster.take() {
                        clusters.push(("current", c));
                    }
                    if let Some(c) = change.baseline_cluster.take() {
                        clusters.push(("baseline", c));
                    }
                    Input {
                        title: format!("Review changes in {}", change.path),
                        clusters,
                        change: Some(change),
                    }
                })
                .collect::<Vec<_>>();
            (
                inputs,
                response.changes_total,
                response.index,
                response.analysis_coverage,
                None,
                Some(response.baseline),
                response.next_cursor,
                response.truncated,
                response.notices,
            )
        } else if let Some(seed) = &options.seed {
            let response = review_context(
                db,
                seed,
                &ReviewContextOptions {
                    max_files: options.max_files,
                    scope: options.scope.clone(),
                },
            )?;
            (
                vec![Input {
                    title: response.cluster.title.clone(),
                    clusters: vec![("current", response.cluster)],
                    change: None,
                }],
                1,
                response.index,
                response.analysis_coverage,
                None,
                None,
                None,
                false,
                response.notices,
            )
        } else {
            let response = review_areas(
                db,
                &ReviewAreasOptions {
                    max_files: options.max_files,
                    scope: options.scope.clone(),
                    limit: options.limit,
                    cursor,
                },
            )?;
            let inputs = response
                .areas
                .into_iter()
                .map(|c| Input {
                    title: c.title.clone(),
                    clusters: vec![("current", c)],
                    change: None,
                })
                .collect();
            (
                inputs,
                response.areas_total,
                response.index,
                response.analysis_coverage,
                Some(response.coverage),
                None,
                response.next_cursor,
                response.truncated,
                response.notices,
            )
        };
    let fingerprint = explore::request((&query, &before))?;
    explore::read(db, |s| {
        ensure!(
            state(s, options)? == before,
            "review inputs changed during pack construction; retry the query"
        );
        let packs = inputs
            .into_iter()
            .map(|input| {
                build(
                    s,
                    input,
                    options,
                    &fingerprint,
                    baseline.as_ref().map(|b| b.commit.as_str()),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        // Recheck sources and Git after reading baseline excerpts as well.
        ensure!(
            state(s, options)? == before,
            "review inputs changed during pack construction; retry the query"
        );
        let root = store::metadata(s.conn, "root")?.context("missing index root")?;
        ensure!(
            crate::workspace::snapshot(&crate::workspace::scan_index(s.conn, Path::new(&root))?)
                == s.index.snapshot,
            "sources changed during pack construction; reindex and retry"
        );
        Ok(ReviewPackResponse {
            version: 1,
            policy: POLICY.into(),
            fingerprint,
            packs,
            packs_total: total,
            index,
            analysis_coverage: coverage,
            area_coverage,
            baseline,
            next_cursor: next.map(|c| format!("rp1:{query}:{c}")),
            truncated,
            notices,
        })
    })
}
