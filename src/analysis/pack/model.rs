use super::super::{
    BaselineInfo, DeclarationChange, DependencyCoverage, DiffHunk, RelationshipChange,
    ReviewAreaCoverage, ReviewBoundary, ReviewConnection, ReviewTest,
};
use crate::{IndexState, Notice};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct ReviewPackOptions {
    pub seed: Option<String>,
    /// Some(base) selects change review; None selects audit areas or a seed.
    pub base: Option<String>,
    pub baseline_resolve: bool,
    pub max_files: usize,
    /// Conservative token units: one compact JSON UTF-8 byte per unit.
    pub max_tokens: usize,
    pub scope: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
}
impl Default for ReviewPackOptions {
    fn default() -> Self {
        Self {
            seed: None,
            base: None,
            baseline_resolve: false,
            max_files: 12,
            max_tokens: 16000,
            scope: None,
            limit: 5,
            cursor: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ReviewExcerpt {
    pub range: [usize; 2],
    /// Exact source text, including original line terminators.
    pub text: String,
}
#[derive(Debug, Serialize)]
pub struct ReviewPackFile {
    pub path: String,
    pub snapshot: String,
    pub source_role: String,
    pub parent_file: Option<String>,
    pub via: Option<ReviewConnection>,
    pub requested_ranges: Vec<[usize; 2]>,
    pub excerpts: Vec<ReviewExcerpt>,
    pub requested_lines: usize,
    pub included_lines: usize,
}
#[derive(Debug, Serialize)]
pub struct ReviewPackBoundary {
    pub snapshot: String,
    pub detail: ReviewBoundary,
}
#[derive(Debug, Serialize)]
pub struct ReviewPackTest {
    pub snapshot: String,
    pub detail: ReviewTest,
}
#[derive(Debug, Default, Serialize)]
pub struct ReviewPackTotals {
    /// Source versions count separately against max_files.
    pub files: usize,
    pub boundary: usize,
    pub related_tests: usize,
    pub declaration_changes: usize,
    pub relationship_changes: usize,
    pub hunks: usize,
    pub unresolved: usize,
    pub unavailable_sources: usize,
    pub gaps: usize,
    pub outside_files: usize,
}
#[derive(Debug, Serialize)]
pub struct ReviewPack {
    pub id: String,
    pub fingerprint: String,
    pub mode: String,
    pub current_snapshot: String,
    pub base_commit: Option<String>,
    pub title: String,
    pub change_path: Option<String>,
    pub change_status: Option<String>,
    pub max_files: usize,
    pub max_tokens: usize,
    pub budget_policy: String,
    /// Includes this field and the entire compact serialized pack, not the response envelope.
    pub estimated_tokens: usize,
    pub reading_path: Vec<ReviewPackFile>,
    pub hunks: Vec<DiffHunk>,
    pub declaration_changes: Vec<DeclarationChange>,
    pub relationship_changes: Vec<RelationshipChange>,
    pub boundary: Vec<ReviewPackBoundary>,
    pub related_tests: Vec<ReviewPackTest>,
    pub gaps: Vec<String>,
    pub totals: ReviewPackTotals,
    pub truncated: bool,
}
#[derive(Debug, Serialize)]
pub struct ReviewPackResponse {
    pub version: u32,
    pub policy: String,
    pub fingerprint: String,
    pub packs: Vec<ReviewPack>,
    pub packs_total: usize,
    pub index: IndexState,
    pub analysis_coverage: Vec<DependencyCoverage>,
    pub area_coverage: Option<ReviewAreaCoverage>,
    pub baseline: Option<BaselineInfo>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub notices: Vec<Notice>,
}
