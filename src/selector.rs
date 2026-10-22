use crate::candidate::Candidate;
use crate::{SelectionPolicy, candidate::sort};
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

fn baseline(
    mut candidates: Vec<Candidate>,
    limit: usize,
    channel_coverage: bool,
    file_penalty: bool,
) -> (Vec<Candidate>, BTreeSet<i64>) {
    let mut pending: BTreeSet<_> = candidates
        .iter()
        .filter(|c| {
            channel_coverage
                && limit >= 2
                && (c.lexical_rank == Some(1) || c.embedding_rank == Some(1))
        })
        .map(|c| c.id)
        .collect();
    let mut selected = Vec::new();
    let mut coverage_selections = BTreeSet::new();
    let mut per_file: BTreeMap<String, usize> = BTreeMap::new();
    while !candidates.is_empty() && selected.len() < limit {
        // Exact identities always take precedence. Otherwise only reserve the slots still
        // needed for missing channel leaders; earlier positions retain the existing ranking.
        let reserve = !pending.is_empty()
            && pending.len() == limit - selected.len()
            && !candidates.iter().any(|c| c.exact());
        let preferred = |only_leaders: bool| {
            candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| !only_leaders || pending.contains(&c.id))
                .max_by(|(_, a), (_, b)| {
                    let a_score = penalize_repetition(
                        a.score,
                        if file_penalty
                            && !(a.kind == "function"
                                && crate::source_language(&a.path) == Some("typescript"))
                        {
                            *per_file.get(&a.path).unwrap_or(&0)
                        } else {
                            0
                        },
                    );
                    let b_score = penalize_repetition(
                        b.score,
                        if file_penalty
                            && !(b.kind == "function"
                                && crate::source_language(&b.path) == Some("typescript"))
                        {
                            *per_file.get(&b.path).unwrap_or(&0)
                        } else {
                            0
                        },
                    );
                    a.exact()
                        .cmp(&b.exact())
                        .then_with(|| a_score.total_cmp(&b_score))
                        .then_with(|| b.path.cmp(&a.path))
                        .then_with(|| b.start.cmp(&a.start))
                        .then_with(|| b.id.cmp(&a.id))
                })
                .map(|(i, _)| i)
                .unwrap()
        };
        let ordinary = preferred(false);
        let best = if reserve { preferred(true) } else { ordinary };
        let candidate = candidates.remove(best);
        if best != ordinary {
            coverage_selections.insert(candidate.id);
        }
        pending.remove(&candidate.id);
        *per_file.entry(candidate.path.clone()).or_default() += 1;
        selected.push(candidate);
    }
    (selected, coverage_selections)
}

fn penalize_repetition(score: f64, count: usize) -> f64 {
    let penalty = 1.0 + 0.35 * count as f64;
    if score < 0.0 {
        score * penalty
    } else {
        score / penalty
    }
}

const SET_POOL: usize = 96;

struct Features {
    vector: Vec<f32>,
    targets: BTreeSet<i64>,
    span: (i64, i64),
}

fn features(conn: &Connection, candidates: &[Candidate], semantic: bool) -> Result<Vec<Features>> {
    let mut result = Vec::new();
    for c in candidates.iter().take(SET_POOL) {
        let mut stmt = conn.prepare("SELECT e.vector FROM regions r JOIN embeddings e ON e.region_id=r.id WHERE r.location_id=?1 ORDER BY CASE WHEN r.id=?2 THEN 0 WHEN r.view='identity' THEN 1 ELSE 2 END,r.id LIMIT 1")?;
        let mut rows = stmt.query(rusqlite::params![c.id, c.hit.region_id])?;
        let vector = if let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0)?;
            anyhow::ensure!(
                bytes.len().is_multiple_of(4),
                "invalid stored vector byte length"
            );
            let values: Vec<_> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect();
            anyhow::ensure!(
                values.iter().all(|v| v.is_finite()),
                "stored embedding contains non-finite values"
            );
            values
        } else {
            Vec::new()
        };
        let mut targets = BTreeSet::new();
        if semantic {
            let mut stmt = conn.prepare("SELECT DISTINCT to_id FROM semantic_edges WHERE from_id=?1 AND kind='calls' ORDER BY to_id LIMIT 128")?;
            targets = stmt
                .query_map([c.id], |r| r.get(0))?
                .collect::<Result<_, _>>()?;
        }
        let span = conn.query_row(
            "SELECT start_byte,end_byte FROM locations WHERE id=?1",
            [c.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        result.push(Features {
            vector,
            targets,
            span,
        });
    }
    Ok(result)
}

fn similarity(a: &Candidate, b: &Candidate, x: &Features, y: &Features) -> f64 {
    let semantic = if x.vector.len() == y.vector.len() && !x.vector.is_empty() {
        x.vector
            .iter()
            .zip(&y.vector)
            .map(|(a, b)| *a as f64 * *b as f64)
            .sum::<f64>()
            .clamp(0.0, 1.0)
    } else {
        0.0
    };
    let shared = if x.targets.is_empty() || y.targets.is_empty() {
        0.0
    } else {
        x.targets.intersection(&y.targets).count() as f64
            / x.targets.union(&y.targets).count() as f64
    };
    let mut value = semantic * 0.85 + shared * 0.15;
    if a.path == b.path {
        value = value.max(0.15);
        if a.parent.is_some() && a.parent == b.parent {
            value = value.max(0.25);
        }
        let overlap = (x.span.1.min(y.span.1) - x.span.0.max(y.span.0)).max(0);
        let shorter = (x.span.1 - x.span.0).min(y.span.1 - y.span.0);
        if shorter > 0 {
            value = value.max(0.95 * overlap as f64 / shorter as f64);
        }
    }
    value
}

pub(crate) fn annotate_clusters(
    conn: &Connection,
    candidates: &mut [Candidate],
    semantic: bool,
) -> Result<()> {
    let fs = features(conn, candidates, semantic)?;
    let mut representatives: Vec<usize> = Vec::new();
    // Representative assignment avoids single-link chaining unrelated ends of a component.
    for i in 0..fs.len() {
        let cluster = representatives
            .iter()
            .position(|j| similarity(&candidates[i], &candidates[*j], &fs[i], &fs[*j]) >= 0.82);
        let cluster = cluster.unwrap_or_else(|| {
            representatives.push(i);
            representatives.len() - 1
        });
        candidates[i].cluster = Some(cluster);
    }
    Ok(())
}

pub(crate) fn select_starts(
    conn: &Connection,
    candidates: Vec<Candidate>,
    limit: usize,
    channel_coverage: bool,
    policy: SelectionPolicy,
    semantic: bool,
) -> Result<(Vec<Candidate>, BTreeSet<i64>)> {
    if matches!(policy, SelectionPolicy::Baseline | SelectionPolicy::Ranked)
        || candidates.iter().any(Candidate::exact)
    {
        return Ok(baseline(
            candidates,
            limit,
            channel_coverage,
            policy != SelectionPolicy::Ranked,
        ));
    }
    let fs = features(conn, &candidates, semantic)?;
    let n = fs.len();
    let mut selected: Vec<usize> = Vec::new();
    let mut clusters = BTreeSet::new();
    let mut changed = BTreeSet::new();
    let mut leaders: BTreeSet<_> = (0..n)
        .filter(|i| {
            channel_coverage
                && (candidates[*i].lexical_rank == Some(1)
                    || candidates[*i].embedding_rank == Some(1))
        })
        .collect();
    let relevance = |i: usize| 1.0 / (1.0 + 0.035 * i as f64);
    while selected.len() < limit.min(n) {
        let reserve = !leaders.is_empty() && leaders.len() == limit - selected.len();
        let utility = |i: usize| {
            let redundancy = selected
                .iter()
                .map(|j| similarity(&candidates[i], &candidates[*j], &fs[i], &fs[*j]))
                .fold(0.0, f64::max);
            match policy {
                SelectionPolicy::Mmr => 0.8 * relevance(i) - 0.2 * redundancy,
                SelectionPolicy::Coverage => {
                    relevance(i)
                        * (1.0
                            + if candidates[i]
                                .cluster
                                .is_some_and(|c| !clusters.contains(&c))
                            {
                                0.25
                            } else {
                                0.0
                            })
                        - 0.15 * redundancy
                }
                _ => unreachable!(),
            }
        };
        let preferred = |reserved: bool| {
            (0..n)
                .filter(|i| !selected.contains(i) && (!reserved || leaders.contains(i)))
                .max_by(|a, b| utility(*a).total_cmp(&utility(*b)).then_with(|| b.cmp(a)))
                .unwrap()
        };
        let ordinary = preferred(false);
        let best = if reserve { preferred(true) } else { ordinary };
        if best != ordinary {
            changed.insert(candidates[best].id);
        }
        selected.push(best);
        if let Some(cluster) = candidates[best].cluster {
            clusters.insert(cluster);
        }
        leaders.remove(&best);
    }
    let mut result: Vec<_> = selected
        .into_iter()
        .map(|i| candidates[i].clone())
        .collect();
    // The chosen set is presented in retrieval usefulness order, not in marginal-diversity order.
    sort(&mut result);
    Ok((result, changed))
}
