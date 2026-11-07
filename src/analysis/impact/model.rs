use super::super::{DependencyCoverage, DependencyEvidence, ReviewCluster};
use crate::{IndexState, NavigationLocation, Notice};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct ImpactOptions {
    /// Compare this commit/ref with the tracked working tree plus nonignored untracked files.
    pub base: String,
    pub max_files: usize,
    pub scope: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
    /// Explicitly attempt baseline compiler preparation in temporary storage.
    pub baseline_resolve: bool,
}
impl Default for ImpactOptions {
    fn default() -> Self {
        Self {
            base: "HEAD".into(),
            max_files: 12,
            scope: None,
            limit: 5,
            cursor: None,
            baseline_resolve: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
}

#[derive(Debug, Serialize)]
pub struct ImpactConsumer {
    pub location: NavigationLocation,
    pub target: String,
    pub kind: String,
    pub occurrences: usize,
    pub evidence: DependencyEvidence,
}

#[derive(Debug, Serialize)]
pub struct ImpactChange {
    pub path: String,
    pub status: String,
    pub hunks: Vec<DiffHunk>,
    pub hunks_total: usize,
    /// Candidates overlap changed current lines; this is not a semantic diff.
    pub declarations: Vec<NavigationLocation>,
    pub declarations_total: usize,
    pub consumers: Vec<ImpactConsumer>,
    pub consumers_total: usize,
    pub cluster: Option<ReviewCluster>,
    pub gaps: Vec<String>,
    pub details_truncated: bool,
    pub baseline_declarations: Vec<VersionedDeclaration>,
    pub baseline_declarations_total: usize,
    pub declaration_changes: Vec<DeclarationChange>,
    pub declaration_changes_total: usize,
    pub baseline_consumers: Vec<BaselineConsumer>,
    pub baseline_consumers_total: usize,
    pub baseline_cluster: Option<ReviewCluster>,
    pub relationship_changes: Vec<RelationshipChange>,
    pub relationship_changes_total: usize,
}

#[derive(Debug, Serialize)]
pub struct ImpactResponse {
    pub version: u32,
    pub policy: String,
    pub requested_base: String,
    pub base_commit: String,
    pub comparison: String,
    pub baseline_analysis: bool,
    pub baseline: BaselineInfo,
    pub changes: Vec<ImpactChange>,
    pub changes_total: usize,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub index: IndexState,
    pub analysis_coverage: Vec<DependencyCoverage>,
    pub notices: Vec<Notice>,
}

/// Baseline locations deliberately have no current-navigation handles.
#[derive(Debug, Clone, Serialize)]
pub struct VersionedDeclaration {
    pub id: String,
    pub snapshot: String,
    pub commit: Option<String>,
    pub path: String,
    pub language: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub range: [usize; 2],
    pub signature: String,
    pub signature_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct DeclarationChange {
    pub before: Option<VersionedDeclaration>,
    pub after: Option<VersionedDeclaration>,
    pub status: String,
    pub match_basis: String,
    pub uncertain: bool,
    pub signature_changed: Option<bool>,
    pub source_changed: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct BaselineConsumer {
    pub location: VersionedDeclaration,
    pub target: String,
    pub kind: String,
    pub occurrences: usize,
    pub evidence: DependencyEvidence,
}

#[derive(Debug, Serialize)]
pub struct RelationshipChange {
    pub from: String,
    pub to: String,
    pub kind: super::super::DependencyKind,
    pub before_occurrences: usize,
    pub after_occurrences: usize,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct BaselineCompiler {
    pub language: String,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct BaselineInfo {
    pub commit: String,
    pub syntax_snapshot: String,
    pub compiler_requested: bool,
    pub compilers: Vec<BaselineCompiler>,
    pub excluded_entries: Vec<String>,
    pub excluded_entries_total: usize,
    pub parse_error_files: usize,
    pub notices: Vec<Notice>,
}
