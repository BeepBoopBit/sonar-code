//! Query expansion and rank-independent measurements for the opt-in discovery experiment.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::{DiscoveryMeasurements, FileRankingPolicy, candidate::Candidate, text};

pub(crate) fn rank_calibrated(
    conn: &Connection,
    candidates: &mut [Candidate],
    variants: &[(&[String], Option<&[f32]>)],
    semantic_available: bool,
    file_ranking: FileRankingPolicy,
) -> Result<()> {
    let mut components: BTreeMap<i64, Vec<crate::VariantMeasurements>> = BTreeMap::new();
    for (i, (terms, vector)) in variants.iter().enumerate() {
        let mut copy = candidates.to_vec();
        let vectors: Vec<_> = vector.iter().copied().collect();
        rank(
            conn,
            &mut copy,
            &[terms],
            &vectors,
            semantic_available,
            file_ranking,
        )?;
        let weight = if variants.len() == 1 {
            1.0
        } else if i == 0 {
            0.75
        } else {
            0.25
        };
        for c in copy {
            let m = c.measurements.unwrap();
            components
                .entry(c.id)
                .or_default()
                .push(crate::VariantMeasurements {
                    lexical: m.lexical,
                    lexical_normalized: m.lexical_normalized,
                    semantic: m.semantic,
                    weight,
                    score: m.score,
                });
        }
    }
    for c in candidates.iter_mut() {
        let variants = components.remove(&c.id).unwrap();
        let first = &variants[0];
        let q = c.quantity.as_ref();
        let m = DiscoveryMeasurements {
            lexical: first.lexical,
            lexical_normalized: first.lexical_normalized,
            semantic: first.semantic,
            quantity: q.map_or(0.0, |q| if q.unit == "unknown" { 0.5 } else { 1.0 }),
            binding: q
                .and_then(|q| q.structure.as_ref())
                .map_or(0.0, |s| s.strength),
            score: variants.iter().map(|v| v.weight * v.score).sum(),
            variants: Some(variants),
        };
        c.score = m.score;
        c.measurements = Some(m);
    }
    crate::candidate::sort(candidates);
    Ok(())
}

/// Each location receives one contribution per channel, at its best variant rank.
/// Preserve the union, including locations beyond either individual top-k boundary.
pub(crate) fn merge_channel(
    original: Vec<Candidate>,
    normalized: Vec<Candidate>,
    lexical: bool,
) -> Vec<Candidate> {
    let mut best: BTreeMap<i64, (usize, Candidate)> = BTreeMap::new();
    for channel in [original, normalized] {
        for (rank, mut candidate) in channel.into_iter().enumerate() {
            if let Some((old_rank, old)) = best.get_mut(&candidate.id) {
                if rank < *old_rank {
                    std::mem::swap(old, &mut candidate);
                    *old_rank = rank;
                }
                for hit in candidate.hits {
                    if !old
                        .hits
                        .iter()
                        .any(|h| h.region_id == hit.region_id && h.channel == hit.channel)
                    {
                        old.hits.push(hit);
                    }
                }
                if let Some(score) = candidate.lexical_score {
                    old.lexical_score = Some(old.lexical_score.unwrap_or(0.0).max(score));
                }
                for (view, similarity) in candidate.view_similarities {
                    old.view_similarities
                        .entry(view)
                        .and_modify(|v| *v = v.max(similarity))
                        .or_insert(similarity);
                }
                for (view, rank) in candidate.view_ranks {
                    old.view_ranks
                        .entry(view)
                        .and_modify(|v| *v = (*v).min(rank))
                        .or_insert(rank);
                }
            } else {
                best.insert(candidate.id, (rank, candidate));
            }
        }
    }
    let mut best: Vec<_> = best.into_values().collect();
    best.sort_by(|(ar, a), (br, b)| {
        ar.cmp(br)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start.cmp(&b.start))
            .then_with(|| a.id.cmp(&b.id))
    });
    let mut candidates: Vec<_> = best.into_iter().map(|(_, c)| c).collect();
    for (rank, c) in candidates.iter_mut().enumerate() {
        if lexical {
            c.lexical_rank = Some(rank + 1);
        } else {
            c.embedding_rank = Some(rank + 1);
        }
    }
    if !lexical {
        crate::fusion::normalize_dense(&mut candidates);
    }
    candidates
}

/// Remeasure the complete union against identical queries and corpus statistics.
/// Candidate membership/rank never stands in for lexical or semantic evidence.
pub(crate) fn rank(
    conn: &Connection,
    candidates: &mut [Candidate],
    terms: &[&[String]],
    vectors: &[&[f32]],
    semantic_available: bool,
    file_ranking: FileRankingPolicy,
) -> Result<()> {
    let ids = serde_json::to_string(&candidates.iter().map(|c| c.id).collect::<Vec<_>>())?;
    let mut measured: BTreeMap<_, _> = candidates
        .iter()
        .map(|c| {
            (
                c.id,
                DiscoveryMeasurements {
                    semantic: semantic_available.then_some(0.0),
                    ..Default::default()
                },
            )
        })
        .collect();
    // FTS uses the full indexed corpus for BM25. Restrict output, not its statistics,
    // to the union; examine every matching region rather than another top-k sample.
    let mut stmt = conn.prepare(
        "SELECT l.id, -bm25(search,8.0,1.0,3.0,1.0) * CASE WHEN l.kind='file'
         AND (?3=0 OR EXISTS (SELECT 1 FROM locations child WHERE child.parent_id=l.id))
         THEN 0.35 ELSE 1.0 END,
         search.name || ' ' || search.path || ' ' || search.docs || ' ' || search.body
         FROM search JOIN regions r ON r.id=search.rowid JOIN locations l ON l.id=r.location_id
         WHERE search MATCH ?1 AND l.id IN (SELECT value FROM json_each(?2))",
    )?;
    for terms in terms.iter().filter(|terms| !terms.is_empty()) {
        let mut rows = stmt.query(params![
            text::match_expression(terms),
            ids,
            file_ranking == FileRankingPolicy::Standalone
        ])?;
        while let Some(row) = rows.next()? {
            let document: String = row.get(2)?;
            let tokens: BTreeSet<_> = document.split_whitespace().collect();
            let coverage = terms.iter().filter(|t| tokens.contains(t.as_str())).count() as f64
                / terms.len() as f64;
            let score = row.get::<_, f64>(1)? * (0.2 + coverage * coverage);
            let m = measured.get_mut(&row.get::<_, i64>(0)?).unwrap();
            m.lexical = m.lexical.max(score);
        }
    }
    if semantic_available && !vectors.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT r.location_id,e.vector FROM embeddings e JOIN regions r ON r.id=e.region_id
             WHERE r.location_id IN (SELECT value FROM json_each(?1))",
        )?;
        let mut rows = stmt.query([ids])?;
        while let Some(row) = rows.next()? {
            let m = measured.get_mut(&row.get::<_, i64>(0)?).unwrap();
            for vector in vectors {
                let score = crate::embeddings::dot_bytes(vector, row.get_ref(1)?.as_blob()?)?;
                m.semantic = Some(m.semantic.unwrap().max(score).clamp(0.0, 1.0));
            }
        }
    }
    let lexical_max = measured.values().map(|m| m.lexical).fold(0.0, f64::max);
    for c in candidates.iter_mut() {
        let mut m = measured.remove(&c.id).unwrap();
        m.lexical_normalized = if lexical_max > 0.0 {
            (m.lexical / lexical_max).clamp(0.0, 1.0)
        } else {
            0.0
        };
        if let Some(q) = &c.quantity {
            m.quantity = if q.unit == "unknown" { 0.5 } else { 1.0 };
            m.binding = q.structure.as_ref().map_or(0.0, |s| s.strength);
        }
        // Frozen equal weights. With no model, renormalize the three available features.
        m.score = (m.lexical_normalized + m.semantic.unwrap_or(0.0) + m.quantity + m.binding)
            / if semantic_available { 4.0 } else { 3.0 }
            / 61.0;
        c.score = m.score;
        c.measurements = Some(m);
    }
    crate::candidate::sort(candidates);
    Ok(())
}
