//! Providers observe sources; storage, freshness, and traversal belong to the engine.
use anyhow::Result;
use std::collections::BTreeMap;

use super::{AnalysisBatch, DependencyFact, UnresolvedDependency};
use crate::workspace::SourceFile;

pub(crate) struct AnalysisInput<'a> {
    pub files: BTreeMap<&'a str, &'a SourceFile>,
}

pub(crate) trait AnalysisProvider {
    /// Versioned identity; change when extraction semantics change.
    fn id(&self) -> &'static str;
    fn analyze(&self, input: &AnalysisInput<'_>) -> Result<AnalysisBatch>;
    /// Refine one syntax lead using compiler bindings at its exact source span.
    /// Providers remain database-free; absent or ambiguous evidence stays unresolved.
    fn resolve(
        &self,
        _dependency: &UnresolvedDependency,
        _bindings: &[DependencyFact],
    ) -> Option<DependencyFact> {
        None
    }
}
