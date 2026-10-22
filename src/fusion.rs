//! Comparable per-query channel scales. None of these scores are calibrated confidence.
use std::collections::BTreeMap;

use crate::{
    FusionMethod,
    candidate::{Candidate, sort},
};

#[derive(Default)]
pub(crate) struct Range {
    min: Option<f64>,
    max: Option<f64>,
}

impl Range {
    pub(crate) fn add(&mut self, value: f64) {
        if value.is_finite() {
            self.min = Some(self.min.map_or(value, |v| v.min(value)));
            self.max = Some(self.max.map_or(value, |v| v.max(value)));
        }
    }

    pub(crate) fn normalize(&self, value: f64) -> f64 {
        let (Some(min), Some(max)) = (self.min, self.max) else {
            return 0.0;
        };
        if !value.is_finite() {
            return 0.0;
        }
        // A flat channel has no discrimination. Keep a positive constant tied, not NaN.
        if max - min <= f64::EPSILON {
            return if value > 0.0 { 1.0 } else { 0.0 };
        }
        ((value - min) / (max - min)).clamp(0.0, 1.0)
    }
}

/// Normalize each dense view separately before combining strengths. Retain the existing
/// candidate pool and view-rank winner so fusion comparisons change only channel scoring.
pub(crate) fn normalize_dense(candidates: &mut [Candidate]) {
    let mut ranges: BTreeMap<String, Range> = BTreeMap::new();
    for c in candidates.iter() {
        for (view, value) in &c.view_similarities {
            ranges.entry(view.clone()).or_default().add(*value);
        }
    }
    for c in candidates {
        let mut strengths: Vec<_> = c
            .view_similarities
            .iter()
            .map(|(view, value)| ranges[view].normalize(*value))
            .collect();
        strengths.sort_by(|a, b| b.total_cmp(a));
        // Missing views contribute zero, rather than increasing another view's weight.
        let divisor: f64 = [1.0, 0.25, 0.125].into_iter().take(ranges.len()).sum();
        c.embedding_normalized = Some(
            strengths
                .iter()
                .zip([1.0, 0.25, 0.125])
                .map(|(s, w)| s * w)
                .sum::<f64>()
                / divisor.max(1.0),
        );
    }
}

pub(crate) fn fuse(
    mut lexical: Vec<Candidate>,
    dense: Vec<Candidate>,
    method: FusionMethod,
    alpha: f64,
) -> Vec<Candidate> {
    let max_lexical = lexical
        .iter()
        .filter_map(|c| c.lexical_score)
        .fold(0.0, f64::max);
    for c in &mut lexical {
        c.lexical_normalized = Some(if max_lexical > 0.0 {
            (c.lexical_score.unwrap_or(0.0) / max_lexical).clamp(0.0, 1.0)
        } else {
            0.0
        });
    }
    let contribution = |rank: usize, normalized: f64, weight: f64| match method {
        // The factor of two preserves legacy RRF scores at alpha=0.5.
        FusionMethod::Rrf => 2.0 * weight / (60.0 + rank as f64),
        // Same maximum scale as RRF for downstream coarse/selection policies.
        FusionMethod::Score => 2.0 * weight * normalized / 61.0,
    };
    let mut merged: BTreeMap<i64, Candidate> = BTreeMap::new();
    for (rank, mut c) in lexical.into_iter().enumerate() {
        c.score = contribution(rank + 1, c.lexical_normalized.unwrap_or(0.0), alpha);
        merged.insert(c.id, c);
    }
    for (rank, mut c) in dense.into_iter().enumerate() {
        let score = contribution(rank + 1, c.embedding_normalized.unwrap_or(0.0), 1.0 - alpha);
        if let Some(existing) = merged.get_mut(&c.id) {
            // RRF preserves its existing region choice; score fusion follows stronger
            // normalized channel evidence, never a comparison of BM25 against cosine.
            let prefer_dense = match method {
                FusionMethod::Rrf => existing.lexical_rank.is_some_and(|r| rank + 1 < r),
                FusionMethod::Score => score > existing.score,
            };
            existing.score += score;
            existing.embedding_rank = c.embedding_rank;
            existing.embedding_normalized = c.embedding_normalized;
            existing.hits.extend(c.hits.clone());
            existing.view_ranks = c.view_ranks;
            existing.view_similarities = c.view_similarities;
            if prefer_dense {
                existing.use_hit(c.hit);
            }
        } else {
            c.score = score;
            merged.insert(c.id, c);
        }
    }
    let mut result: Vec<_> = merged.into_values().collect();
    sort(&mut result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_handles_empty_flat_negative_and_nonfinite_values() {
        let mut range = Range::default();
        assert_eq!(range.normalize(1.0), 0.0);
        range.add(f64::NAN);
        range.add(-0.2);
        assert_eq!(range.normalize(-0.2), 0.0);
        range.add(0.6);
        assert!((range.normalize(0.2) - 0.5).abs() < 1e-10);
        assert_eq!(range.normalize(f64::NAN), 0.0);
        assert_eq!(range.normalize(9.0), 1.0);
        let mut flat = Range::default();
        flat.add(0.3);
        assert_eq!(flat.normalize(0.3), 1.0);
    }
}
