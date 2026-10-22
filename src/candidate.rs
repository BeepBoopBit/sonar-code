use crate::RetrievalHit;
use std::collections::BTreeMap;

#[derive(Clone)]
pub(crate) struct Candidate {
    pub(crate) id: i64,
    pub(crate) path: String,
    pub(crate) symbol: Option<String>,
    pub(crate) kind: String,
    pub(crate) is_test: bool,
    pub(crate) exact_symbol_match: bool,
    pub(crate) definition: Option<usize>,
    pub(crate) parent: Option<i64>,
    pub(crate) start: usize,
    pub(crate) score: f64,
    pub(crate) lexical_score: Option<f64>,
    pub(crate) lexical_normalized: Option<f64>,
    pub(crate) embedding_normalized: Option<f64>,
    pub(crate) lexical_rank: Option<usize>,
    pub(crate) embedding_rank: Option<usize>,
    pub(crate) view_ranks: BTreeMap<String, usize>,
    pub(crate) view_similarities: BTreeMap<String, f64>,
    pub(crate) graph_seed_count: usize,
    pub(crate) usage: Option<crate::UsageEvidence>,
    pub(crate) ranking: Option<crate::RankingEvidence>,
    pub(crate) quantity: Option<crate::QuantityEvidence>,
    pub(crate) measurements: Option<crate::DiscoveryMeasurements>,
    pub(crate) hit: RetrievalHit,
    pub(crate) hits: Vec<RetrievalHit>,
    pub(crate) exact_path_match: bool,
    pub(crate) coarse_rank: Option<usize>,
    pub(crate) graph_score: f64,
    pub(crate) cluster: Option<usize>,
}

impl Candidate {
    pub(crate) fn exact(&self) -> bool {
        self.exact_symbol_match || self.exact_path_match
    }
    pub(crate) fn role(&self) -> &'static str {
        if self.is_test {
            "test"
        } else if self.kind == "documentation" {
            "documentation"
        } else {
            "implementation"
        }
    }
    pub(crate) fn use_hit(&mut self, hit: RetrievalHit) {
        self.start = hit.start;
        self.hit = hit;
    }
}

pub(crate) fn sort(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| {
        b.exact()
            .cmp(&a.exact())
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start.cmp(&b.start))
            .then_with(|| a.id.cmp(&b.id))
    });
}
