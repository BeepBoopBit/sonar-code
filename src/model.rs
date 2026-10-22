use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    #[default]
    Lexical,
    Embedding,
    Hybrid,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum VariantScoring {
    #[default]
    Joint,
    Calibrated,
}

#[derive(Debug, Serialize)]
pub struct RetrievalDiagnostic {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_role: Option<crate::SourceRole>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclusions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchors: Option<crate::AnchorDiagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub navigation: Option<crate::NavigationDiagnostic>,
    pub mode: RetrievalMode,
    pub snapshot: String,
    pub model_fingerprint: Option<String>,
    pub representation: Option<String>,
    pub graph_enabled: bool,
    pub usage_status: String,
    pub scorer: ScoringPolicy,
    pub scorer_bypassed_for_exact: bool,
    pub channel_coverage: bool,
    pub selection: SelectionPolicy,
    pub graph_method: Option<GraphMethod>,
    pub coarse_enabled: bool,
    pub fusion: FusionMethod,
    pub lexical_weight: f64,
    pub context: ContextPolicy,
    pub file_ranking: FileRankingPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity_query: Option<String>,
    /// Present only when a distinct normalized query was added to original retrieval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_variants: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity_ranking: Option<crate::QuantityRankingPolicy>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_scoring: Option<VariantScoring>,
    pub related_candidates: Vec<ContextDiagnostic>,
    pub focus_policy: FocusPolicy,
    pub focus_max_lines: usize,
    pub focus: Vec<FocusDiagnostic>,
    pub candidates: Vec<DiagnosticCandidate>,
}

#[derive(Debug, Serialize)]
pub struct DiagnosticCandidate {
    pub rank: usize,
    pub path: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub source_role: String,
    pub exact_symbol_match: bool,
    pub exact_path_match: bool,
    pub winning_hit: RetrievalHit,
    pub retrieval_hits: Vec<RetrievalHit>,
    pub coarse_rank: Option<usize>,
    pub graph_score: f64,
    pub cluster: Option<usize>,
    pub definition: Option<usize>,
    pub region_start: usize,
    pub location_id: i64,
    pub lexical_rank: Option<usize>,
    pub embedding_rank: Option<usize>,
    pub score: f64,
    pub lexical_score: Option<f64>,
    pub lexical_normalized: Option<f64>,
    pub embedding_normalized: Option<f64>,
    pub view_ranks: std::collections::BTreeMap<String, usize>,
    pub view_similarities: std::collections::BTreeMap<String, f64>,
    pub graph_seed_count: usize,
    pub usage: Option<UsageEvidence>,
    pub ranking: Option<RankingEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<crate::QuantityEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovery: Option<DiscoveryProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurements: Option<DiscoveryMeasurements>,
    pub selected_rank: Option<usize>,
    pub selected_for_channel_coverage: bool,
}

/// Membership in direct discovery pools, independent of subsequent ranking.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DiscoveryProvenance {
    pub original_lexical: bool,
    pub original_embedding: bool,
    pub normalized_lexical: bool,
    pub normalized_embedding: bool,
    pub quantity: bool,
}

/// Common measurements over all indexed regions of every merged candidate.
/// Zero is a measured absence of matching evidence; None means no semantic model.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DiscoveryMeasurements {
    pub lexical: f64,
    pub lexical_normalized: f64,
    pub semantic: Option<f64>,
    pub quantity: f64,
    pub binding: f64,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variants: Option<Vec<VariantMeasurements>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct VariantMeasurements {
    pub lexical: f64,
    pub lexical_normalized: f64,
    pub semantic: Option<f64>,
    pub weight: f64,
    pub score: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Brief {
    pub version: u32,
    pub starts: Vec<String>,
    pub files: Vec<BriefFile>,
    pub links: Vec<Link>,
    pub index: IndexState,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BriefFile {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub locations: Vec<Location>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Location {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<[usize; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub evidence: Evidence,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub path: String,
    pub line: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexState {
    pub snapshot: String,
    pub semantic_profile: Option<String>,
    pub syntax_freshness: String,
    pub semantic_freshness: String,
    /// Capabilities and freshness of indexed or newly detected programming languages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<crate::LanguageState>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Notice {
    pub code: String,
    pub message: String,
}

impl Notice {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BriefOptions {
    pub limit: usize,
    pub related: usize,
    /// Repository-relative directory or exact file path; no glob syntax.
    pub scope: Option<String>,
}

impl Default for BriefOptions {
    fn default() -> Self {
        Self {
            limit: 5,
            related: 3,
            scope: None,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub root: String,
    pub snapshot: String,
    pub files: usize,
    pub locations: usize,
    pub regions: usize,
    pub changed: usize,
    pub removed: usize,
    pub parse_error_files: usize,
    pub elapsed_ms: u128,
}

/// Source-bound evidence used by retrieval, exposed only by diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalHit {
    pub region_id: i64,
    pub start: usize,
    pub end: usize,
    pub view: String,
    pub channel: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SelectionPolicy {
    #[default]
    Baseline,
    Ranked,
    Mmr,
    Coverage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum GraphMethod {
    #[default]
    OneHop,
    Propagation,
    PageRank,
}

/// Experimental methods are explicit and independently selectable.
#[derive(Debug, Clone)]
pub struct RankingOptions {
    pub selection: SelectionPolicy,
    pub graph_method: GraphMethod,
    pub coarse: bool,
    pub fusion: FusionMethod,
    /// Lexical share in hybrid fusion, in [0, 1]. Not a confidence estimate.
    pub lexical_weight: f64,
    pub context: ContextPolicy,
    pub file_ranking: FileRankingPolicy,
}

impl Default for RankingOptions {
    fn default() -> Self {
        Self {
            selection: SelectionPolicy::default(),
            graph_method: GraphMethod::default(),
            coarse: false,
            fusion: FusionMethod::default(),
            lexical_weight: 0.5,
            context: ContextPolicy::default(),
            file_ranking: FileRankingPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum FileRankingPolicy {
    #[default]
    Legacy,
    /// Discount file containers only when their declarations are indexed separately.
    Standalone,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum FusionMethod {
    #[default]
    Rrf,
    Score,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ContextPolicy {
    #[default]
    Structural,
    Query,
}

#[derive(Debug, Serialize)]
pub struct ContextDiagnostic {
    pub location_id: i64,
    pub path: String,
    pub symbol: Option<String>,
    pub lexical_score: Option<f64>,
    pub semantic_similarity: Option<f64>,
    pub relevance: f64,
    pub winning_hit: RetrievalHit,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Chunking {
    #[default]
    Lines,
    Ast,
}

/// Optional text resources. Values are indexed as text, never evaluated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ResourcePolicy {
    #[default]
    None,
    /// Repository YAML files up to 256 KiB each, honoring the source ignore boundary.
    Yaml,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum FocusPolicy {
    /// Compatibility behavior: lexical windows or the winning semantic region.
    #[default]
    Legacy,
    /// A bounded line-window control, using the same anchors as syntax selection.
    Window,
    /// Complete language-specific units near the source-bound retrieval anchor, when they fit.
    Syntax,
    /// Reconsider source windows throughout the location using local evidence.
    Evidence,
    /// Complete nearby syntax evidence while preserving visible source matches.
    Anchored,
    /// Select bounded source facts with the loaded static encoder and complete nearby context.
    Contextual,
}

#[derive(Debug, Clone, Copy)]
pub struct FocusOptions {
    pub policy: FocusPolicy,
    /// Per-location line budget for bounded policies; legacy is unchanged.
    pub max_lines: usize,
}

impl Default for FocusOptions {
    fn default() -> Self {
        Self {
            policy: FocusPolicy::Legacy,
            max_lines: 16,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FocusDiagnostic {
    pub location_id: i64,
    pub path: String,
    pub symbol: Option<String>,
    pub anchor: Option<[usize; 2]>,
    pub range: Option<[usize; 2]>,
    pub reason: String,
    pub syntax_kind: Option<String>,
    pub lines: usize,
    pub bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protected: Option<FocusProtection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contextual: Option<ContextualFocusDiagnostic>,
}

#[derive(Debug, Serialize)]
pub struct ContextualFocusDiagnostic {
    pub facts: usize,
    pub operation: Option<String>,
    pub occurrence: Option<crate::SourceSpan>,
    pub context: Vec<FocusUnit>,
}

/// Source coordinates only; no generated explanation or source text.
#[derive(Debug, Serialize)]
pub struct FocusProtection {
    pub window: Option<[usize; 2]>,
    pub lines: Vec<usize>,
    pub units: Vec<FocusUnit>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct FocusUnit {
    pub range: [usize; 2],
    pub kind: String,
}

/// Caller evidence for an optional usage match; coordinates belong to the caller, not the target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEvidence {
    pub caller_id: i64,
    pub caller_symbol: String,
    pub caller_is_test: bool,
    pub path: String,
    pub line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub similarity: f64,
    pub adjusted_similarity: f64,
    pub incoming_callers: usize,
    pub outgoing_targets: usize,
    pub rank: usize,
    pub contribution: f64,
}

/// Optional candidate scoring; legacy preserves the original pipeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum ScoringPolicy {
    #[default]
    Legacy,
    Evidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankingEvidence {
    pub base_score: f64,
    pub base_strength: f64,
    pub lexical_strength: f64,
    pub semantic_strength: f64,
    pub usage_strength: f64,
    pub semantic_median: f64,
    pub semantic_peak: f64,
    /// Score before optional graph expansion and set selection.
    pub score: f64,
}
