//! Language-neutral, source-bound dependency facts. Targets are indexed file paths.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    DeclaresModule,
    Imports,
    ReExports,
    Calls,
    References,
    Implements,
}

impl DependencyKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::DeclaresModule => "declares_module",
            Self::Imports => "imports",
            Self::ReExports => "re_exports",
            Self::Calls => "calls",
            Self::References => "references",
            Self::Implements => "implements",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DependencyEvidence {
    pub path: String,
    pub line: usize,
    pub bytes: [usize; 2],
    pub provider: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DependencyFact {
    pub from: String,
    pub to: String,
    pub kind: DependencyKind,
    pub evidence: DependencyEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedDependency {
    pub from: String,
    pub target: String,
    pub kind: DependencyKind,
    pub reason: String,
    pub evidence: DependencyEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisCoverage {
    pub language: String,
    pub provider: String,
    pub analyzed_files: Vec<String>,
    pub skipped_files: Vec<String>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisBatch {
    pub coverage: AnalysisCoverage,
    pub relationships: Vec<DependencyFact>,
    pub unresolved: Vec<UnresolvedDependency>,
}

/// Repository-wide provider summary; successful file names are omitted to keep queries compact.
#[derive(Debug, Serialize)]
pub struct DependencyCoverage {
    pub language: String,
    pub provider: String,
    pub analyzed_files: usize,
    pub skipped_files: Vec<String>,
    pub skipped_total: usize,
    pub limitations: Vec<String>,
}

/// Counts distinct source occurrences, with bounded examples for each relation kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyKindSummary {
    pub kind: DependencyKind,
    pub occurrences: usize,
    pub evidence: Vec<DependencyEvidence>,
    pub evidence_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DependencyRelationship {
    pub from: String,
    pub to: String,
    pub kinds: Vec<DependencyKindSummary>,
}
