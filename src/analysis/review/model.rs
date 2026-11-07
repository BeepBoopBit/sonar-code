use super::super::{
    DependencyCoverage, DependencyEvidence, DependencyKind, DependencyRelationship,
};
use crate::{IndexState, Notice};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct ReviewContextOptions {
    pub max_files: usize,
    pub scope: Option<String>,
}
impl Default for ReviewContextOptions {
    fn default() -> Self {
        Self {
            max_files: 12,
            scope: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReviewAreasOptions {
    pub max_files: usize,
    pub scope: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
}
impl Default for ReviewAreasOptions {
    fn default() -> Self {
        Self {
            max_files: 12,
            scope: None,
            limit: 5,
            cursor: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewSeed {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<[usize; 2]>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewConnection {
    /// Original dependency direction, which may oppose reading order.
    pub from: String,
    pub to: String,
    pub kind: DependencyKind,
    pub occurrences: usize,
    pub evidence: DependencyEvidence,
}

#[derive(Debug, Serialize)]
pub struct ReviewReadingStep {
    pub file: String,
    pub source_role: String,
    /// Selected file from which this reading step was discovered.
    pub parent_file: Option<String>,
    pub via: Option<ReviewConnection>,
    pub ranges: Vec<[usize; 2]>,
}

#[derive(Debug, Serialize)]
pub struct ReviewBoundary {
    pub relationship: DependencyRelationship,
    pub outside_file: String,
    pub classification: String,
    pub other_area: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ReviewTest {
    pub path: String,
    pub selected: bool,
    pub basis: String,
    pub connection: ReviewConnection,
}

#[derive(Debug, Serialize)]
pub struct ReviewCluster {
    pub id: String,
    /// Structural label, not an inferred feature name or risk assessment.
    pub title: String,
    pub seed: ReviewSeed,
    pub max_files: usize,
    pub files: Vec<String>,
    pub reading_path: Vec<ReviewReadingStep>,
    pub related_tests: Vec<ReviewTest>,
    pub related_tests_total: usize,
    pub boundary: Vec<ReviewBoundary>,
    pub boundary_total: usize,
    pub omitted_files_total: usize,
    pub boundary_truncated: bool,
    pub unresolved_total: usize,
}

#[derive(Debug, Serialize)]
pub struct ReviewContextResponse {
    pub version: u32,
    pub policy: String,
    pub cluster: ReviewCluster,
    pub index: IndexState,
    pub analysis_coverage: Vec<DependencyCoverage>,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewUnassigned {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ReviewAreaCoverage {
    pub indexed_code_files: usize,
    pub assigned_files: usize,
    pub shared_files_total: usize,
    pub shared_files: Vec<String>,
    pub unassigned_total: usize,
    pub unassigned: Vec<ReviewUnassigned>,
    pub excluded_non_code_files: usize,
}

#[derive(Debug, Serialize)]
pub struct ReviewAreasResponse {
    pub version: u32,
    pub policy: String,
    pub areas: Vec<ReviewCluster>,
    pub areas_total: usize,
    pub coverage: ReviewAreaCoverage,
    pub index: IndexState,
    pub analysis_coverage: Vec<DependencyCoverage>,
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub notices: Vec<Notice>,
}
