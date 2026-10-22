//! Local source indexing and navigational retrieval. No model or network is required.

mod embeddings;
mod exclusions;
mod explore;
mod investigation;
pub use explore::{
    Direction, NavigationLink, NavigationLocation, NavigationResponse, NeighborOptions, Relation,
    locate, neighbors, outline,
};
pub use investigation::{
    Investigation, InvestigationLead, InvestigationLink, InvestigationLocation,
    InvestigationOptions, InvestigationPanel, InvestigationTraversal, Investigator, TermOverlap,
};
mod extract;
mod languages;
pub use languages::{LanguageCapabilities, LanguageState};

/// Registered programming languages; document/resource formats are indexed separately.
pub fn supported_languages() -> Vec<&'static str> {
    languages::ADAPTERS.iter().map(|a| a.id()).collect()
}

/// Programming-language adapter selected for a source path, if supported.
pub fn source_language(path: &str) -> Option<&'static str> {
    languages::for_path(path).map(|adapter| adapter.id())
}
pub use field_uses::{
    FieldAccess, FieldAssociation, FieldEvidence, FieldUse, FieldUseOptions, FieldUseResponse,
    field_uses,
};
use languages::rust::field_uses;
mod follow;
pub use follow::{FollowMatch, FollowOptions, FollowRelation, FollowResponse, FollowTraversal};
mod mcp;
mod model;
pub use mcp::serve_mcp;
mod search;
mod semantic;
mod store;
mod text;
mod workspace;

pub use embeddings::{
    Embedder, EmbeddingProgress, EmbeddingReport, EncodingPolicy, Representation, embed_index,
    embed_index_with, embed_index_with_options, embed_index_with_progress,
};
pub use languages::typescript::semantic::{TypeScriptReport, resolve as resolve_typescript};
pub use model::*;
pub use search::{SearchEngine, brief};
pub use semantic::{SemanticManifest, SemanticReport, import_scip, resolve};
pub use store::{index, index_with, index_with_resources};
mod contextual;

mod anchors;
mod navigation;
mod navigation_uses;
pub use navigation::{
    ModuleProof, NavigationContribution, NavigationDiagnostic, NavigationEdge, NavigationPolicy,
    NavigationReport, ReferenceSupport, index_navigation,
};
mod assembly;
pub use anchors::{
    AnchorContribution, AnchorDiagnostic, AnchorEvidence, AnchorOwner, EmbeddedAnchor,
};
mod candidate;
mod discovery;
mod focus;
#[doc(hidden)]
pub use languages::rust::focus_facts;
mod fusion;
mod graph;
mod selector;
mod source_role;
pub use source_role::SourceRole;

mod usage;
pub use usage::{UsageReport, embed_usage};

mod scorer;

use languages::rust::quantity;
use languages::rust::quantity_structure;
use languages::rust::structure;
use languages::rust::value_use;
pub use quantity::{
    QuantityEvidence, QuantityOptions, QuantityQueryPolicy, QuantityRankingPolicy, QuantityReport,
    index_quantities,
};
pub use quantity_structure::QuantityStructure;
pub use structure::{
    FunctionStructure, ResolvedStructureCall, STRUCTURE_POLICY, SelfFieldPath, SourceSpan,
    StructureEvidence, StructureLocation, StructureReport, structure,
};
pub use value_use::{LocalContribution, ValueUse, ValueUseMatch, ValueUseStep};
