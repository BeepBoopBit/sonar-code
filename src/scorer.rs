//! Experimental strength-based scoring over the existing candidate union.
//! These query-relative signals are not calibrated confidence or an absence detector.
use crate::{RankingEvidence, candidate::Candidate};

#[derive(Clone, Copy)]
struct Contrast {
    median: f64,
    peak: f64,
}

impl Contrast {
    fn new(values: impl Iterator<Item = f64>) -> Self {
        let mut values: Vec<_> = values.filter(|v| v.is_finite()).collect();
        values.sort_by(f64::total_cmp);
        Self {
            median: values
                .get(values.len() / 2)
                .copied()
                .unwrap_or(0.0)
                .max(0.0),
            peak: values.last().copied().unwrap_or(0.0).max(0.0),
        }
    }

    fn strength(self, value: f64) -> f64 {
        if !value.is_finite() || value <= self.median {
            return 0.0;
        }
        let spread = if self.peak > self.median + 1e-9 {
            self.peak - self.median
        } else {
            // Flat source evidence has no discrimination. A stronger caller can still
            // contribute relative to the remaining cosine interval; ties add nothing.
            (1.0 - self.median).max(1e-9)
        };
        ((value - self.median) / spread).clamp(0.0, 1.0)
    }
}

fn semantic(c: &Candidate) -> Option<f64> {
    c.view_similarities
        .values()
        .copied()
        .filter(|v| v.is_finite())
        .max_by(f64::total_cmp)
}

fn combine(lexical: f64, semantic: f64, usage: f64, prior: f64) -> f64 {
    let mut values = [lexical, semantic, usage];
    values.sort_by(|a, b| b.total_cmp(a));
    (values[0] + 0.25 * values[1] + 0.125 * values[2] + 0.05 * prior) / 61.0
}

pub(crate) fn rank(candidates: &mut [Candidate]) {
    let contrast = Contrast::new(candidates.iter().filter_map(semantic));
    let max_lexical = candidates
        .iter()
        .filter_map(|c| c.lexical_score)
        .fold(0.0, f64::max);
    let max_base = candidates.iter().map(|c| c.score).fold(0.0, f64::max);
    for c in candidates.iter_mut() {
        let lexical = if max_lexical > 0.0 {
            (c.lexical_score.unwrap_or(0.0) / max_lexical).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let dense = semantic(c).map_or(0.0, |v| contrast.strength(v));
        let usage = c.usage.as_ref().map_or(0.0, |u| {
            let relevance = contrast.strength(u.similarity);
            relevance * relevance
                / (1.0 + (u.outgoing_targets.max(1) as f64).ln())
                / (1.0 + 0.25 * (u.incoming_callers.max(1) as f64).ln())
        });
        let prior = if max_base > 0.0 {
            (c.score / max_base).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let score = combine(lexical, dense, usage, prior);
        if let Some(u) = &mut c.usage {
            u.contribution = score - combine(lexical, dense, 0.0, prior);
        }
        c.ranking = Some(RankingEvidence {
            base_score: c.score,
            base_strength: prior,
            lexical_strength: lexical,
            semantic_strength: dense,
            usage_strength: usage,
            semantic_median: contrast.median,
            semantic_peak: contrast.peak,
            score,
        });
        c.score = score;
    }
    crate::candidate::sort(candidates);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contrast_rejects_weak_flat_and_nonfinite_evidence_without_confidence_claims() {
        let contrast = Contrast::new([0.1, 0.2, 0.3, 0.4, 0.5].into_iter());
        assert_eq!(contrast.strength(0.2), 0.0);
        assert_eq!(contrast.strength(0.3), 0.0);
        assert!((contrast.strength(0.4) - 0.5).abs() < 1e-9);
        assert_eq!(contrast.strength(0.5), 1.0);
        assert_eq!(contrast.strength(f64::NAN), 0.0);
        let flat = Contrast::new([0.3, 0.3].into_iter());
        assert_eq!(flat.strength(0.3), 0.0);
        assert!(flat.strength(0.8) > 0.0);
        assert_eq!(Contrast::new([].into_iter()).strength(-0.2), 0.0);
    }
}
