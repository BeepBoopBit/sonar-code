use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};

use crate::{
    Brief, BriefFile, BriefOptions, DiagnosticCandidate, Embedder, IndexState, Location, Notice,
    RetrievalDiagnostic, RetrievalHit, RetrievalMode, store, text, workspace,
};

const CANDIDATE_LIMIT: usize = 512;

use crate::candidate::{Candidate, sort as sort_candidates};

/// Reuse the loaded local model across queries. Each query reads a coherent database generation.
pub struct SearchEngine {
    db: PathBuf,
    mode: RetrievalMode,
    model: Option<Embedder>,
    graph: bool,
    usage: bool,
    scorer: crate::ScoringPolicy,
    channel_coverage: bool,
    ranking: crate::RankingOptions,
    focus: crate::FocusOptions,
    quantities: bool,
    quantity_options: crate::QuantityOptions,
    variant_scoring: crate::VariantScoring,
    anchors: bool,
    navigation: crate::NavigationPolicy,
    handles: bool,
    kind: Option<String>,
    source_role: Option<crate::SourceRole>,
    exclusions: Vec<String>,
}

impl SearchEngine {
    pub(crate) fn navigation_setup(&self) -> (RetrievalMode, crate::FocusOptions, Option<&str>) {
        (
            self.mode,
            self.focus,
            self.model.as_ref().map(|m| m.fingerprint.as_str()),
        )
    }

    /// A reproducible navigation setup: lexical without a model, hybrid with one.
    /// Keeps the existing ranked/anchor/uses policy and bounded reading ranges.
    pub fn open_navigation(db: &Path, model: Option<&Path>) -> Result<Self> {
        let mode = if model.is_some() {
            RetrievalMode::Hybrid
        } else {
            RetrievalMode::Lexical
        };
        let engine = Self::open(db, mode, model)?;
        let focus = if engine
            .model
            .as_ref()
            .is_some_and(Embedder::supports_contextual_focus)
        {
            crate::FocusPolicy::Contextual
        } else {
            crate::FocusPolicy::Window
        };
        Ok(engine
            .with_handles(true)
            .with_anchors(true)
            .with_navigation(crate::NavigationPolicy::Uses)
            .with_ranking(crate::RankingOptions {
                selection: crate::SelectionPolicy::Ranked,
                context: crate::ContextPolicy::Query,
                file_ranking: crate::FileRankingPolicy::Standalone,
                ..Default::default()
            })
            .with_focus(crate::FocusOptions {
                policy: focus,
                max_lines: 16,
            }))
    }

    pub fn open(db: &Path, mode: RetrievalMode, model: Option<&Path>) -> Result<Self> {
        let model = if mode == RetrievalMode::Lexical {
            None
        } else {
            let conn = store::open(db, false)?;
            let encoding = crate::EncodingPolicy::from_metadata(&conn)?;
            Some(Embedder::load_with_encoding(
                model
                    .context("embedding and hybrid retrieval require --model <local-directory>")?,
                encoding,
            )?)
        };
        Ok(Self {
            db: db.into(),
            mode,
            model,
            graph: false,
            usage: false,
            scorer: crate::ScoringPolicy::Legacy,
            channel_coverage: false,
            ranking: crate::RankingOptions::default(),
            focus: crate::FocusOptions::default(),
            quantities: false,
            quantity_options: crate::QuantityOptions::default(),
            variant_scoring: crate::VariantScoring::Joint,
            anchors: false,
            navigation: crate::NavigationPolicy::Off,
            handles: false,
            kind: None,
            source_role: None,
            exclusions: Vec::new(),
        })
    }

    /// Add exact quantity observations as an independent discovery channel.
    pub fn with_quantities(mut self, enabled: bool) -> Self {
        self.quantities = enabled;
        self
    }

    /// Opt in to bounded, index-grounded names embedded in ordinary queries.
    pub fn with_anchors(mut self, enabled: bool) -> Self {
        self.anchors = enabled;
        self
    }

    pub fn with_navigation(mut self, policy: crate::NavigationPolicy) -> Self {
        self.navigation = policy;
        self
    }

    /// Omit literal repository-relative files or directories from query results.
    /// Each path is validated when a brief or diagnostic is requested.
    pub fn with_exclusions(mut self, paths: Vec<String>) -> Self {
        self.exclusions = paths;
        self
    }

    /// Include snapshot-bound handles for explicit follow-up navigation.
    pub fn with_handles(mut self, enabled: bool) -> Self {
        self.handles = enabled;
        self
    }

    /// Restrict starting locations by indexed kind before direct retrieval pool limits.
    /// Supporting locations can retain other kinds to explain their connections.
    pub fn with_kind(mut self, kind: Option<String>) -> Self {
        self.kind = kind;
        self
    }

    /// Restrict starts by source role before direct retrieval caps; supporting links retain their roles.
    pub fn with_source_role(mut self, role: Option<crate::SourceRole>) -> Self {
        self.source_role = role;
        self
    }

    pub fn with_quantity_options(mut self, options: crate::QuantityOptions) -> Self {
        self.quantity_options = options;
        self
    }

    pub fn with_variant_scoring(mut self, policy: crate::VariantScoring) -> Self {
        self.variant_scoring = policy;
        self
    }

    /// Add source-bound caller embeddings as an independent discovery channel.
    pub fn with_usage(mut self, enabled: bool) -> Self {
        self.usage = enabled;
        self
    }

    pub fn with_scorer(mut self, scorer: crate::ScoringPolicy) -> Self {
        self.scorer = scorer;
        self
    }

    pub fn with_graph(mut self, enabled: bool) -> Self {
        self.graph = enabled;
        self
    }

    /// Reserve room for each retrieval channel's leader in hybrid briefs of at least two starts.
    pub fn with_channel_coverage(mut self, enabled: bool) -> Self {
        self.channel_coverage = enabled;
        self
    }

    pub fn with_ranking(mut self, ranking: crate::RankingOptions) -> Self {
        self.ranking = ranking;
        self
    }

    pub fn with_focus(mut self, options: crate::FocusOptions) -> Self {
        self.focus = options;
        self
    }

    pub fn brief(&self, query: &str, options: &BriefOptions) -> Result<Brief> {
        Ok(self.inspect(query, options, self.kind.as_deref())?.0)
    }

    pub fn diagnose(&self, query: &str, options: &BriefOptions) -> Result<RetrievalDiagnostic> {
        Ok(self.inspect(query, options, self.kind.as_deref())?.1)
    }

    pub(crate) fn inspect(
        &self,
        query: &str,
        options: &BriefOptions,
        kind: Option<&str>,
    ) -> Result<(Brief, RetrievalDiagnostic)> {
        self.inspect_filtered(query, options, kind, self.source_role, &self.exclusions)
    }

    pub(crate) fn inspect_filtered(
        &self,
        query: &str,
        options: &BriefOptions,
        kind: Option<&str>,
        source_role: Option<crate::SourceRole>,
        exclusions: &[String],
    ) -> Result<(Brief, RetrievalDiagnostic)> {
        run_query(
            &self.db,
            query,
            options,
            self.mode,
            self.model.as_ref(),
            self.graph,
            self.usage,
            self.scorer,
            self.channel_coverage,
            &self.ranking,
            &self.focus,
            self.quantities,
            self.quantity_options,
            self.variant_scoring,
            self.anchors,
            self.navigation,
            self.handles,
            kind,
            source_role,
            exclusions,
        )
    }

    /// Search recorded connections from a known location, returning the route to each match.
    /// Uses this engine's retrieval mode and model; follow options control traversal and focus.
    pub fn follow(
        &self,
        anchor: &str,
        query: &str,
        options: &crate::FollowOptions,
    ) -> Result<crate::FollowResponse> {
        crate::follow::run(
            &self.db,
            anchor,
            query,
            options,
            self.mode,
            self.model.as_ref(),
        )
    }
}

/// Read a coherent index generation, verify source freshness, and return navigation metadata.
pub fn brief(db: &Path, query: &str, options: &BriefOptions) -> Result<Brief> {
    Ok(run_query(
        db,
        query,
        options,
        RetrievalMode::Lexical,
        None,
        false,
        false,
        crate::ScoringPolicy::Legacy,
        false,
        &crate::RankingOptions::default(),
        &crate::FocusOptions::default(),
        false,
        crate::QuantityOptions::default(),
        crate::VariantScoring::Joint,
        false,
        crate::NavigationPolicy::Off,
        false,
        None,
        None,
        &[],
    )?
    .0)
}

#[allow(clippy::too_many_arguments)]
fn run_query(
    db: &Path,
    query: &str,
    options: &BriefOptions,
    mode: RetrievalMode,
    model: Option<&Embedder>,
    graph: bool,
    usage: bool,
    scorer: crate::ScoringPolicy,
    channel_coverage: bool,
    ranking: &crate::RankingOptions,
    focus_options: &crate::FocusOptions,
    quantities: bool,
    quantity_options: crate::QuantityOptions,
    variant_scoring: crate::VariantScoring,
    anchors: bool,
    navigation: crate::NavigationPolicy,
    handles: bool,
    kind: Option<&str>,
    source_role: Option<crate::SourceRole>,
    exclusions: &[String],
) -> Result<(Brief, RetrievalDiagnostic)> {
    validate_kind(kind)?;
    let kind = kind.unwrap_or("");
    ensure!(!query.trim().is_empty(), "query must not be empty");
    ensure!(
        focus_options.policy != crate::FocusPolicy::Contextual
            || model.is_some_and(crate::Embedder::supports_contextual_focus),
        "contextual focus requires embedding or hybrid retrieval with a static model and legacy encoding"
    );
    ensure!(
        (!anchors && navigation == crate::NavigationPolicy::Off)
            || ranking.selection == crate::SelectionPolicy::Ranked
                && scorer == crate::ScoringPolicy::Legacy
                && ranking.fusion == crate::FusionMethod::Rrf
                && !graph
                && !usage
                && !ranking.coarse
                && !channel_coverage
                && quantity_options.query == crate::QuantityQueryPolicy::Original
                && variant_scoring == crate::VariantScoring::Joint,
        "anchors require ranked selection, legacy scoring, RRF, original/joint quantities, and graph/usage/coarse/channel-coverage disabled"
    );
    ensure!(
        quantity_options.ranking != crate::QuantityRankingPolicy::BoundedLocal
            || quantities
                && quantity_options.query == crate::QuantityQueryPolicy::Original
                && variant_scoring == crate::VariantScoring::Joint,
        "bounded-local scoring requires --quantities, --quantity-query original and --variant-scoring joint"
    );
    ensure!(
        variant_scoring == crate::VariantScoring::Joint
            || quantities
                && matches!(
                    quantity_options.ranking,
                    crate::QuantityRankingPolicy::Unified | crate::QuantityRankingPolicy::Local
                ),
        "calibrated variant scoring requires --quantities and --quantity-ranking unified or local"
    );
    ensure!(query.len() <= 16_384, "query exceeds the 16 KiB limit");
    ensure!(
        navigation != crate::NavigationPolicy::Uses || !quantities,
        "navigation uses requires quantities disabled; its contribution uses direct retrieval scores"
    );
    ensure!(
        (1..=20).contains(&options.limit),
        "limit must be between 1 and 20"
    );
    ensure!(options.related <= 10, "related must be between 0 and 10");
    ensure!(
        ranking.lexical_weight.is_finite() && (0.0..=1.0).contains(&ranking.lexical_weight),
        "lexical weight must be finite and between 0 and 1"
    );
    ensure!(
        (1..=64).contains(&focus_options.max_lines),
        "focus lines must be between 1 and 64"
    );
    ensure!(
        !usage || model.is_some(),
        "usage retrieval requires embedding or hybrid mode and a local model"
    );
    let scope = normalize_scope(options.scope.as_deref())?;
    let exclusions = crate::exclusions::normalize(exclusions)?;
    let mut conn = store::open(db, false)?;
    let tx = conn.transaction()?;
    let root = store::metadata(&tx, "root")?
        .context("index is empty; run `sonar index <repository>` first")?;
    let snapshot = store::metadata(&tx, "snapshot")?.context("index is missing its snapshot")?;
    let indexed = store::file_hashes(&tx)?;
    let current = workspace::scan_index(&tx, Path::new(&root))?;
    let current_hashes: BTreeMap<_, _> = current
        .iter()
        .map(|file| (file.path.as_str(), file.hash.as_str()))
        .collect();
    let stale_files: BTreeSet<_> = indexed
        .iter()
        .filter(|(path, hash)| current_hashes.get(path.as_str()) != Some(&hash.as_str()))
        .map(|(path, _)| path.clone())
        .collect();
    let excluded_files: BTreeSet<_> = indexed
        .keys()
        .filter(|path| exclusions.iter().any(|scope| in_scope(path, scope)))
        .cloned()
        .collect();
    // Filtering before each direct channel's cap lets eligible results fill the
    // pool instead of letting excluded files consume the shortlist.
    let omitted_files: BTreeSet<_> = stale_files.union(&excluded_files).cloned().collect();
    let fresh = workspace::snapshot(&current) == snapshot;
    let role_filter = crate::source_role::Filter::new(&tx, source_role, fresh, &snapshot)?;
    let semantic_profile = crate::semantic::profile(&tx)?;
    let language_states = crate::languages::states(&tx, &current, &snapshot, fresh)?;
    crate::semantic::prepare(&tx, &language_states)?;
    let semantic_current = language_states
        .iter()
        .any(|l| l.semantic_freshness == "current");
    let mut notices = crate::languages::coverage_notices(&language_states);
    if language_states
        .iter()
        .any(|l| l.language == "typescript" && l.semantic_freshness == "current")
        && let Some(notice) = crate::languages::typescript::semantic::coverage_notice(&tx)?
    {
        notices.push(notice);
    }
    if !exclusions.is_empty() {
        notices.push(Notice::new("path_exclusions", format!(
            "{} indexed files are omitted by explicit path exclusions. Exclusions do not change index freshness or prove feature absence.", excluded_files.len())));
    }
    if semantic_current {
        notices.push(Notice::new("semantic_coverage", if language_states.iter().any(|l| l.language == "typescript" && l.files > 0) {
            "Relationships use the recorded compiler analysis profiles. Unmapped/external occurrences and ambiguous definitions are omitted; static calls do not prove runtime dispatch."
        } else {
            "Relationships use the recorded Rust analysis profile. Unmapped/external occurrences and ambiguous definitions are omitted; static calls do not prove runtime dispatch. Syntax candidates include inactive cfg branches."
        }));
    } else if language_states
        .iter()
        .any(|l| l.capabilities.resolved_references)
    {
        let message = if language_states
            .iter()
            .any(|l| l.language == "typescript" && l.files > 0)
        {
            format!(
                "Current compiler-resolved relationships are unavailable. {}",
                crate::semantic::setup(&language_states)
            )
        } else {
            "Current compiler-resolved relationships are unavailable. Run `sonar resolve` after indexing; cfg branches are unfiltered and macro-generated locations may be absent.".into()
        };
        notices.push(Notice::new(
            if semantic_profile.is_some() {
                "stale_semantics"
            } else {
                "syntax_only"
            },
            message,
        ));
    }
    for language in &language_states {
        if language.files > 0 && !language.capabilities.resolved_references {
            notices.push(Notice::new("language_analysis_unavailable", format!("{} supports structural navigation; resolved calls, references, and field-use analysis are not available for this language.", language.language)));
        }
    }
    if graph && !semantic_current {
        notices.push(Notice::new("graph_unavailable", "Reference expansion was requested but current semantic data is unavailable; results use direct retrieval only."));
    }
    if !fresh {
        notices.push(Notice::new("stale_index", "Indexed sources or Markdown have changed. Changed or deleted files are omitted; added files are not indexed. Run `sonar index` again."));
    }
    let parse_errors: usize = tx.query_row(
        "SELECT count(*) FROM files WHERE parse_errors != 0",
        [],
        |row| row.get::<_, u32>(0).map(|n| n as usize),
    )?;
    if parse_errors > 0 {
        notices.push(Notice::new("parse_errors", format!("{parse_errors} indexed source files contain syntax errors; extraction may be incomplete.")));
    }
    let mut canonical_query = None;
    if quantities
        && quantity_options.query != crate::QuantityQueryPolicy::Original
        && crate::quantity::current(
            &tx,
            quantity_options.ranking != crate::QuantityRankingPolicy::Legacy,
        )?
        && (!quantity_options.ranking.needs_local() || crate::quantity::local_current(&tx)?)
    {
        // Explicit identities keep the entire original retrieval behavior.
        let mut exact = Vec::new();
        exact_lookup(
            &tx,
            &mut exact,
            query,
            &scope,
            &omitted_files,
            kind,
            &role_filter,
        )?;
        if exact.is_empty() {
            canonical_query = crate::quantity::canonical_query(query);
        }
    }
    let expansion = quantity_options.query == crate::QuantityQueryPolicy::Expand;
    if expansion && canonical_query.as_deref() == Some(query) {
        canonical_query = None;
    }
    let retrieval_query = if expansion {
        query
    } else {
        canonical_query.as_deref().unwrap_or(query)
    };
    let mut terms = text::query_terms(retrieval_query);
    if terms.len() > 64 {
        terms.truncate(64);
        notices.push(Notice::new(
            "query_truncated",
            "Lexical matching and focus selection use only the first 64 distinct searchable query terms.",
        ));
    }
    let mut lexical = if terms.is_empty() || mode == RetrievalMode::Embedding {
        Vec::new()
    } else {
        retrieve(
            &tx,
            &terms,
            &scope,
            &omitted_files,
            false,
            ranking.file_ranking,
            kind,
            &role_filter,
        )?
    };
    let lexical_pool_full = lexical.len() == CANDIDATE_LIMIT;
    let mut lexical_seen = BTreeSet::new();
    lexical.retain(|candidate| lexical_seen.insert(candidate.id));
    for (i, candidate) in lexical.iter_mut().enumerate() {
        candidate.lexical_rank = Some(i + 1);
    }
    let DenseRetrieval {
        candidates: mut dense,
        coarse: coarse_dense,
        query_vector,
    } = if let Some(model) = model {
        retrieve_embeddings(
            &tx,
            retrieval_query,
            &scope,
            &snapshot,
            model,
            &omitted_files,
            ranking.coarse,
            kind,
            &role_filter,
        )?
    } else {
        DenseRetrieval::default()
    };
    let mut pool_full = lexical_pool_full || dense.len() == CANDIDATE_LIMIT;
    let lexical_ids: BTreeSet<_> = lexical.iter().map(|c| c.id).collect();
    let embedding_ids: BTreeSet<_> = dense.iter().map(|c| c.id).collect();
    let (original_lexical, original_embedding, mut normalized_lexical, mut normalized_embedding) =
        if retrieval_query == query {
            (lexical_ids, embedding_ids, BTreeSet::new(), BTreeSet::new())
        } else {
            (BTreeSet::new(), BTreeSet::new(), lexical_ids, embedding_ids)
        };
    let mut normalized_terms = None;
    let mut normalized_vector = None;
    if expansion && let Some(normalized) = &canonical_query {
        let expanded_terms: Vec<_> = text::query_terms(normalized).into_iter().take(64).collect();
        let extra_lexical = if mode != RetrievalMode::Embedding && !expanded_terms.is_empty() {
            retrieve(
                &tx,
                &expanded_terms,
                &scope,
                &omitted_files,
                false,
                ranking.file_ranking,
                kind,
                &role_filter,
            )?
        } else {
            Vec::new()
        };
        // Encode separately: legacy static pooling can depend on batch padding.
        let extra_dense = if let Some(model) = model {
            retrieve_embeddings(
                &tx,
                normalized,
                &scope,
                &snapshot,
                model,
                &omitted_files,
                false,
                kind,
                &role_filter,
            )?
        } else {
            DenseRetrieval::default()
        };
        pool_full |= extra_lexical.len() == CANDIDATE_LIMIT
            || extra_dense.candidates.len() == CANDIDATE_LIMIT;
        normalized_lexical.extend(extra_lexical.iter().map(|c| c.id));
        normalized_embedding.extend(extra_dense.candidates.iter().map(|c| c.id));
        lexical = crate::discovery::merge_channel(lexical, extra_lexical, true);
        dense = crate::discovery::merge_channel(dense, extra_dense.candidates, false);
        normalized_vector = extra_dense.query_vector;
        normalized_terms = Some(expanded_terms);
    }
    let mut candidates = match mode {
        RetrievalMode::Lexical => lexical,
        RetrievalMode::Embedding => dense,
        RetrievalMode::Hybrid => {
            crate::fusion::fuse(lexical, dense, ranking.fusion, ranking.lexical_weight)
        }
    };
    candidates.retain(|candidate| !omitted_files.contains(&candidate.path));
    // The best region represents each location; repeated matches in long functions cannot add votes.
    let mut seen = BTreeSet::new();
    candidates.retain(|candidate| seen.insert(candidate.id));
    if ranking.coarse {
        let coarse_lexical = if mode != RetrievalMode::Embedding && !terms.is_empty() {
            retrieve(
                &tx,
                &terms,
                &scope,
                &omitted_files,
                true,
                ranking.file_ranking,
                kind,
                &role_filter,
            )?
        } else {
            Vec::new()
        };
        add_coarse(&mut candidates, coarse_lexical, coarse_dense);
    }
    // Preserve the complete legacy no-usage ordering for explicit identity queries,
    // including collisions between same-named functions and fields.
    let evidence_scoring = scorer == crate::ScoringPolicy::Evidence;
    if evidence_scoring {
        exact_lookup(
            &tx,
            &mut candidates,
            query,
            &scope,
            &omitted_files,
            kind,
            &role_filter,
        )?;
    }
    let scorer_bypassed_for_exact = evidence_scoring && candidates.iter().any(Candidate::exact);
    let usage_status = if usage {
        let rust_current = language_states
            .iter()
            .any(|l| l.language == "rust" && l.semantic_freshness == "current");
        crate::usage::status(&tx, &snapshot, model.unwrap(), rust_current)?
    } else {
        "disabled"
    };
    if usage_status == "current" {
        if !scorer_bypassed_for_exact && let Some(vector) = query_vector.as_deref() {
            crate::usage::retrieve(
                &tx,
                vector,
                &scope,
                &excluded_files,
                &mut candidates,
                !evidence_scoring,
            )?;
        }
    } else if usage {
        notices.push(Notice::new("usage_unavailable", format!("Usage retrieval is {usage_status}; using the existing retrieval pipeline. Refresh index, source embeddings, resolve, then run sonar embed-usage with this model.")));
    }
    if evidence_scoring && !scorer_bypassed_for_exact {
        crate::scorer::rank(&mut candidates);
    }
    if !evidence_scoring {
        exact_lookup(
            &tx,
            &mut candidates,
            query,
            &scope,
            &omitted_files,
            kind,
            &role_filter,
        )?;
    }
    crate::languages::typescript::syntax::rank(&tx, &current, &mut candidates, &terms)?;
    if graph && semantic_current {
        if ranking.graph_method == crate::GraphMethod::OneHop {
            expand_graph(&tx, &mut candidates, &scope, &excluded_files)?;
        } else {
            crate::graph::expand(
                &tx,
                &mut candidates,
                &scope,
                &excluded_files,
                ranking.graph_method,
            )?;
        }
        mark_exact_symbols(&mut candidates, query);
    }
    let quantity_status = if quantities {
        let status = crate::quantity::apply(
            &tx,
            query,
            &scope,
            &omitted_files,
            &mut candidates,
            quantity_options.ranking,
        )?;
        if status == "unavailable" {
            notices.push(Notice::new("quantities_unavailable", "Current quantity evidence is unavailable; run sonar index-quantities after indexing."));
        }
        Some(status.to_owned())
    } else {
        None
    };
    let common_scoring = matches!(
        quantity_options.ranking,
        crate::QuantityRankingPolicy::Unified
            | crate::QuantityRankingPolicy::Local
            | crate::QuantityRankingPolicy::BoundedLocal
    ) && quantity_status.as_deref() == Some("current");
    if common_scoring {
        let mut variants = vec![terms.as_slice()];
        if let Some(extra) = normalized_terms.as_deref() {
            variants.push(extra);
        }
        let vectors: Vec<_> = [query_vector.as_deref(), normalized_vector.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if variant_scoring == crate::VariantScoring::Calibrated {
            let mut variants = vec![(terms.as_slice(), query_vector.as_deref())];
            if let Some(extra) = normalized_terms.as_deref() {
                variants.push((extra, normalized_vector.as_deref()));
            }
            crate::discovery::rank_calibrated(
                &tx,
                &mut candidates,
                &variants,
                model.is_some(),
                ranking.file_ranking,
            )?;
        } else {
            crate::discovery::rank(
                &tx,
                &mut candidates,
                &variants,
                &vectors,
                model.is_some(),
                ranking.file_ranking,
            )?;
        }
        if quantity_options.ranking == crate::QuantityRankingPolicy::Local {
            crate::value_use::rank(&tx, &mut candidates, query, model)?;
        } else if quantity_options.ranking == crate::QuantityRankingPolicy::BoundedLocal {
            crate::value_use::rank_bounded(&tx, &mut candidates, query, model)?;
        }
    }
    let discovery_active = common_scoring || expansion && canonical_query.is_some();
    let anchor_diagnostic = if anchors {
        Some(crate::anchors::apply(
            &tx,
            query,
            &scope,
            &omitted_files,
            &current,
            &mut candidates,
        )?)
    } else {
        None
    };
    let navigation_current = if navigation == crate::NavigationPolicy::Uses {
        semantic_current
    } else {
        navigation != crate::NavigationPolicy::Off
            && crate::navigation::current(&tx, fresh, &snapshot)?
    };
    let navigation_diagnostic = if navigation != crate::NavigationPolicy::Off {
        if !navigation_current {
            notices.push(Notice::new("navigation_unavailable", if navigation == crate::NavigationPolicy::Uses {
                if language_states.iter().any(|l| l.language == "typescript" && l.files > 0) {
                    format!("Current compiler-resolved references are unavailable. {} Retrieval is unchanged.", crate::semantic::setup(&language_states))
                } else {
                    "Current compiler-resolved references are unavailable; run sonar resolve after indexing. Retrieval is unchanged.".into()
                }
            } else {
                "Current navigation facts are unavailable; run sonar index-navigation after indexing. Retrieval is unchanged.".into()
            }));
        }
        Some(if navigation == crate::NavigationPolicy::Uses {
            crate::navigation_uses::apply(&tx, &mut candidates, navigation_current)?
        } else {
            crate::navigation::apply(
                &tx,
                &mut candidates,
                query,
                &scope,
                navigation,
                &excluded_files,
                navigation_current,
                semantic_current,
                crate::assembly::Query {
                    terms: &terms,
                    vector: query_vector.as_deref(),
                },
            )?
        })
    } else {
        None
    };
    if navigation == crate::NavigationPolicy::Uses
        && navigation_diagnostic.as_ref().is_some_and(|d| d.truncated)
    {
        notices.push(Notice::new(
            "navigation_truncated",
            "Declaration-use ranking skipped fields or variants with more than 64 recorded function users. Direct retrieval remains available.",
        ));
    }
    // Optional discovery/graph channels can add new candidates after direct retrieval.
    // The requested kind constrains starts, while supporting routes keep their endpoints.
    if !kind.is_empty() {
        candidates.retain(|candidate| candidate.kind == kind);
    }
    candidates.retain(|candidate| {
        !excluded_files.contains(&candidate.path) && !role_filter.excluded.contains(&candidate.id)
    });
    role_filter.correct_roles(&mut candidates);
    let channel_coverage = channel_coverage && mode == RetrievalMode::Hybrid && options.limit >= 2;
    if matches!(
        ranking.selection,
        crate::SelectionPolicy::Mmr | crate::SelectionPolicy::Coverage
    ) {
        crate::selector::annotate_clusters(&tx, &mut candidates, semantic_current)?;
    }
    let mut diagnostics = RetrievalDiagnostic {
        source_role,
        exclusions,
        kind: (!kind.is_empty()).then(|| kind.to_owned()),
        anchors: anchor_diagnostic,
        navigation: navigation_diagnostic,
        mode,
        snapshot: snapshot.clone(),
        model_fingerprint: model.map(|model| model.fingerprint().to_owned()),
        representation: if model.is_some() {
            store::metadata(&tx, "embedding_representation")?
        } else {
            None
        },
        graph_enabled: graph && semantic_current,
        usage_status: usage_status.into(),
        scorer,
        scorer_bypassed_for_exact,
        channel_coverage,
        selection: ranking.selection,
        graph_method: (graph && semantic_current).then_some(ranking.graph_method),
        coarse_enabled: ranking.coarse,
        fusion: ranking.fusion,
        lexical_weight: ranking.lexical_weight,
        context: ranking.context,
        file_ranking: ranking.file_ranking,
        quantity_status,
        quantity_query: if expansion {
            None
        } else {
            canonical_query.clone()
        },
        query_variants: (expansion && canonical_query.is_some())
            .then(|| vec![query.to_owned(), canonical_query.clone().unwrap()]),
        quantity_ranking: (quantities
            && quantity_options.ranking != crate::QuantityRankingPolicy::Legacy)
            .then_some(quantity_options.ranking),
        variant_scoring: (variant_scoring != crate::VariantScoring::Joint)
            .then_some(variant_scoring),
        related_candidates: Vec::new(),
        focus_policy: focus_options.policy,
        focus_max_lines: focus_options.max_lines,
        focus: Vec::new(),
        candidates: candidates
            .iter()
            .enumerate()
            .map(|(i, candidate)| DiagnosticCandidate {
                rank: i + 1,
                path: candidate.path.clone(),
                symbol: candidate.symbol.clone(),
                kind: candidate.kind.clone(),
                exact_symbol_match: candidate.exact_symbol_match,
                exact_path_match: candidate.exact_path_match,
                winning_hit: candidate.hit.clone(),
                retrieval_hits: candidate.hits.clone(),
                coarse_rank: candidate.coarse_rank,
                graph_score: candidate.graph_score,
                cluster: candidate.cluster,
                source_role: candidate.role().into(),
                definition: candidate.definition,
                region_start: candidate.start,
                location_id: candidate.id,
                lexical_rank: candidate.lexical_rank,
                embedding_rank: candidate.embedding_rank,
                score: candidate.score,
                lexical_score: candidate.lexical_score,
                lexical_normalized: candidate.lexical_normalized,
                embedding_normalized: candidate.embedding_normalized,
                view_ranks: candidate.view_ranks.clone(),
                view_similarities: candidate.view_similarities.clone(),
                graph_seed_count: candidate.graph_seed_count,
                usage: candidate.usage.clone(),
                ranking: candidate.ranking.clone(),
                quantity: candidate.quantity.clone(),
                discovery: discovery_active.then(|| crate::DiscoveryProvenance {
                    original_lexical: original_lexical.contains(&candidate.id),
                    original_embedding: original_embedding.contains(&candidate.id),
                    normalized_lexical: normalized_lexical.contains(&candidate.id),
                    normalized_embedding: normalized_embedding.contains(&candidate.id),
                    quantity: candidate.quantity.is_some(),
                }),
                measurements: candidate.measurements.clone(),
                selected_rank: None,
                selected_for_channel_coverage: false,
            })
            .collect(),
    };
    let more = candidates.len() > options.limit;
    let (mut selected, coverage_selections) = crate::selector::select_starts(
        &tx,
        candidates,
        options.limit,
        channel_coverage,
        ranking.selection,
        semantic_current,
    )?;
    for diagnostic in &mut diagnostics.candidates {
        diagnostic.selected_for_channel_coverage =
            coverage_selections.contains(&diagnostic.location_id);
    }
    if more || pool_full {
        notices.push(Notice::new(
            "results_truncated",
            "The brief includes a bounded selection of matching locations.",
        ));
    }
    if selected.is_empty() {
        notices.push(Notice::new("no_matches", "No current indexed locations matched the searchable terms and scope. This does not establish that the requested feature is absent."));
    }

    for row in &mut diagnostics.candidates {
        row.selected_rank = selected
            .iter()
            .position(|c| c.id == row.location_id)
            .map(|i| i + 1);
    }
    let mut ids: BTreeMap<i64, String> = selected
        .iter()
        .enumerate()
        .map(|(i, candidate)| (candidate.id, format!("s{}", i + 1)))
        .collect();
    let starts = selected
        .iter()
        .map(|candidate| ids[&candidate.id].clone())
        .collect();
    let extra = if navigation_current && navigation == crate::NavigationPolicy::Uses {
        diagnostics.navigation.as_ref().unwrap().edges.clone()
    } else if navigation_current
        && navigation == crate::NavigationPolicy::Owners
        && diagnostics
            .navigation
            .as_ref()
            .is_some_and(|d| d.status == "active" || d.status == "inactive")
    {
        crate::navigation::edges(
            &tx,
            &selected.iter().map(|c| c.id).collect::<Vec<_>>(),
            &scope,
            semantic_current,
        )?
        .0
    } else {
        Vec::new()
    };
    let extra = if excluded_files.is_empty() {
        extra
    } else {
        let mut allowed = Vec::new();
        for edge in extra {
            if !excluded_files.contains(&edge.path)
                && !excluded_files.contains(&load_parent(&tx, edge.from)?.path)
                && !excluded_files.contains(&load_parent(&tx, edge.to)?.path)
            {
                allowed.push(edge);
            }
        }
        allowed
    };
    if let Some(d) = &mut diagnostics.navigation {
        for e in &extra {
            if !d
                .edges
                .iter()
                .any(|old| old.from == e.from && old.to == e.to && old.kind == e.kind)
            {
                d.edges.push(e.clone());
            }
        }
    }
    let extra_graph: Vec<_> = extra.iter().map(crate::NavigationEdge::graph).collect();
    let (links, focus_sites, related_diagnostics) = crate::assembly::related(
        &tx,
        &mut selected,
        &mut ids,
        options.related,
        &scope,
        &excluded_files,
        graph && semantic_current,
        (ranking.context == crate::ContextPolicy::Query).then_some(crate::assembly::Query {
            terms: &terms,
            vector: query_vector.as_deref(),
        }),
        &extra_graph,
    )?;
    if navigation_current && navigation != crate::NavigationPolicy::Uses {
        let corrected = crate::navigation::roles(&tx, &mut selected)?;
        if let Some(d) = &mut diagnostics.navigation {
            d.role_corrections.extend(corrected);
            d.role_corrections.sort();
            d.role_corrections.dedup();
        }
    }
    role_filter.correct_roles(&mut selected);
    diagnostics.related_candidates = related_diagnostics;
    let mut files: Vec<BriefFile> = Vec::new();
    let handle_generation = handles
        .then(|| crate::explore::generation(&tx))
        .transpose()?;
    let exact_focus = diagnostics
        .candidates
        .iter()
        .any(|c| c.exact_symbol_match || c.exact_path_match);
    let mut focuser = crate::focus::Resolver::new(&current, *focus_options)?;
    for candidate in selected {
        let legacy_focus = location_focus(
            &tx,
            &candidate,
            &terms,
            focus_sites.get(&candidate.id).copied(),
        )?;
        let mut focused = focuser.resolve(
            &tx,
            &candidate,
            &terms,
            focus_sites.get(&candidate.id).copied(),
            legacy_focus,
        )?;
        if focus_options.policy == crate::FocusPolicy::Evidence {
            focuser.evidence(
                &tx,
                &candidate,
                &terms,
                query_vector.as_deref(),
                focus_sites.get(&candidate.id).copied(),
                &mut focused,
            )?;
        }
        if focus_options.policy == crate::FocusPolicy::Contextual
            && !exact_focus
            && ids[&candidate.id].starts_with('s')
            && let Some(vector) = query_vector.as_deref()
        {
            focuser.contextual(
                &tx,
                &candidate,
                crate::focus::ContextualQuery {
                    model: model.unwrap(),
                    vector,
                    terms: &terms,
                },
                focus_sites.get(&candidate.id).copied(),
                &mut focused,
            )?;
        }
        let focus = focused.range;
        diagnostics.focus.push(focused);
        let title = if candidate.kind == "documentation" {
            markdown_title(&tx, candidate.id)?
        } else {
            None
        };
        let source_role = Some(candidate.role().to_owned());
        let location = Location {
            id: ids[&candidate.id].clone(),
            handle: handle_generation
                .as_deref()
                .map(|g| crate::explore::handle(&tx, g, candidate.id))
                .transpose()?,
            source_role,
            title,
            symbol: candidate.symbol,
            kind: candidate.kind,
            definition: candidate.definition,
            focus,
        };
        if let Some(file) = files.iter_mut().find(|file| file.path == candidate.path) {
            file.locations.push(location);
        } else {
            files.push(BriefFile {
                language: crate::source_language(&candidate.path).map(str::to_owned),
                path: candidate.path,
                locations: vec![location],
            });
        }
    }
    Ok((
        Brief {
            version: 1,
            starts,
            files,
            links,
            index: IndexState {
                snapshot,
                semantic_profile: semantic_profile.clone(),
                syntax_freshness: if fresh { "current" } else { "stale" }.into(),
                semantic_freshness: crate::languages::aggregate_semantic(
                    &language_states,
                    semantic_current,
                    semantic_profile.is_some(),
                )
                .into(),
                languages: language_states,
            },
            notices,
        },
        diagnostics,
    ))
}

#[derive(Default)]
struct DenseRetrieval {
    candidates: Vec<Candidate>,
    coarse: Vec<Candidate>,
    query_vector: Option<Vec<f32>>,
}

#[allow(clippy::too_many_arguments)]
fn retrieve_embeddings(
    conn: &Connection,
    query: &str,
    scope: &str,
    snapshot: &str,
    model: &Embedder,
    stale_files: &BTreeSet<String>,
    coarse: bool,
    kind: &str,
    role_filter: &crate::source_role::Filter,
) -> Result<DenseRetrieval> {
    let representation = validate_embeddings(conn, snapshot, model)?;
    let vector = model.encode(&[query.to_owned()])?.remove(0);
    if vector.iter().all(|value| *value == 0.0) {
        return Ok(DenseRetrieval::default());
    }
    retrieve_embedding_vector(
        conn,
        vector,
        &representation,
        scope,
        stale_files,
        coarse,
        kind,
        role_filter,
    )
}

pub(crate) fn validate_embeddings(
    conn: &Connection,
    snapshot: &str,
    model: &Embedder,
) -> Result<String> {
    ensure!(
        store::metadata(conn, "embedding_model")?.as_deref() == Some(model.fingerprint()),
        "index model does not match these assets; run `sonar embed --model <directory>`"
    );
    ensure!(
        store::metadata(conn, "embedding_snapshot")?.as_deref() == Some(snapshot),
        "embeddings do not cover this index generation; run `sonar embed --model <directory>`"
    );
    let representation = store::metadata(conn, "embedding_representation")?
        .context("missing embedding representation; rerun sonar embed")?;
    ensure!(
        matches!(representation.as_str(), "combined" | "separate" | "context"),
        "unsupported embedding representation"
    );
    let missing: u32 = conn.query_row("SELECT count(*) FROM regions r LEFT JOIN embeddings e ON e.region_id = r.id WHERE e.region_id IS NULL AND ((?1='combined' AND r.view='combined') OR (?1 IN ('separate','context') AND r.view!='combined'))", [&representation], |row| row.get(0))?;
    ensure!(
        missing == 0,
        "embedding index is incomplete; run `sonar embed --model <directory>`"
    );
    Ok(representation)
}

#[allow(clippy::too_many_arguments)]
fn retrieve_embedding_vector(
    conn: &Connection,
    vector: Vec<f32>,
    representation: &str,
    scope: &str,
    stale_files: &BTreeSet<String>,
    coarse: bool,
    kind: &str,
    role_filter: &crate::source_role::Filter,
) -> Result<DenseRetrieval> {
    let mut stmt = conn.prepare(
        "SELECT l.id, f.path, l.symbol, l.kind, l.definition, l.parent_id, r.start_line, '', 0.0, e.vector, r.view, l.is_test, r.id AS region_id, r.end_line
         FROM embeddings e JOIN regions r ON r.id = e.region_id JOIN locations l ON l.id = r.location_id
         JOIN files f ON f.id = l.file_id WHERE (?1 = '' OR f.path = ?1 OR substr(f.path, 1, length(?1) + 1) = ?1 || '/') AND (?2='' OR l.kind=?2)")?;
    let mut rows = stmt.query([scope, kind])?;
    let mut best: BTreeMap<(String, i64), Candidate> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let mut candidate = candidate_from_row(row)?;
        if stale_files.contains(&candidate.path) || role_filter.excluded.contains(&candidate.id) {
            continue;
        }
        candidate.score = crate::embeddings::dot_bytes(&vector, row.get_ref(9)?.as_blob()?)?;
        let view: String = row.get(10)?;
        candidate.hit.channel = "embedding".into();
        candidate.hits = vec![candidate.hit.clone()];
        let key = (view, candidate.id);
        if best.get(&key).is_none_or(|old| {
            candidate.score > old.score
                || (candidate.score == old.score && candidate.start < old.start)
        }) {
            best.insert(key, candidate);
        }
    }
    let coarse_candidates = if coarse {
        coarse_embeddings(conn, &best)?
    } else {
        Vec::new()
    };
    let mut channels: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for ((view, _), candidate) in best {
        channels.entry(view).or_default().push(candidate);
    }
    let mut merged: BTreeMap<i64, Candidate> = BTreeMap::new();
    for (view, mut channel) in channels {
        sort_candidates(&mut channel);
        channel.truncate(CANDIDATE_LIMIT);
        for (i, mut candidate) in channel.into_iter().enumerate() {
            let similarity = candidate.score;
            candidate.view_ranks.insert(view.clone(), i + 1);
            candidate.view_similarities.insert(view.clone(), similarity);
            if let Some(existing) = merged.get_mut(&candidate.id) {
                // Focus follows the highest-ranked view, not incomparable raw similarities.
                if i + 1 < *existing.view_ranks.values().min().unwrap_or(&usize::MAX) {
                    existing.use_hit(candidate.hit.clone());
                }
                existing.hits.extend(candidate.hits);
                existing.view_ranks.insert(view.clone(), i + 1);
                existing.view_similarities.insert(view.clone(), similarity);
            } else {
                merged.insert(candidate.id, candidate);
            }
        }
    }
    let mut candidates: Vec<_> = merged.into_values().collect();
    for candidate in &mut candidates {
        if matches!(representation, "separate" | "context") {
            let mut ranks: Vec<_> = candidate.view_ranks.values().copied().collect();
            ranks.sort_unstable();
            candidate.score = ranks
                .iter()
                .zip([1.0, 0.25, 0.125])
                .map(|(rank, weight)| weight / (60.0 + *rank as f64))
                .sum();
        }
    }
    sort_candidates(&mut candidates);
    candidates.truncate(CANDIDATE_LIMIT);
    for (i, candidate) in candidates.iter_mut().enumerate() {
        candidate.embedding_rank = Some(i + 1);
    }
    crate::fusion::normalize_dense(&mut candidates);
    Ok(DenseRetrieval {
        candidates,
        coarse: coarse_candidates,
        query_vector: Some(vector),
    })
}

fn mark_exact_symbols(candidates: &mut [Candidate], query: &str) {
    // A query consisting of a symbol path is stronger identity evidence than semantic similarity.
    // This does not treat identifiers embedded in an ordinary sentence as an exact lookup.
    let normalize = |value: &str| {
        value
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let query = normalize(query);
    if query.is_empty() {
        return;
    }
    for candidate in candidates.iter_mut() {
        candidate.exact_symbol_match = candidate
            .symbol
            .as_deref()
            .is_some_and(|symbol| normalize(symbol) == query);
    }
    sort_candidates(candidates);
}

#[derive(Clone)]
pub(crate) struct GraphEdge {
    pub(crate) from: i64,
    pub(crate) to: i64,
    pub(crate) kind: String,
    pub(crate) path: String,
    pub(crate) line: usize,
}

pub(crate) fn neighbors(conn: &Connection, id: i64) -> Result<Vec<GraphEdge>> {
    // Group occurrence duplicates before limiting neighbors; one long function gets no extra votes.
    let mut stmt = conn.prepare(
        "SELECT from_id,to_id,kind,path,min(line) FROM semantic_edges
        WHERE from_id=?1 OR to_id=?1 GROUP BY from_id,to_id,kind,path
        ORDER BY CASE kind WHEN 'calls' THEN 0 ELSE 1 END,path,min(line),from_id,to_id LIMIT 16",
    )?;
    Ok(stmt
        .query_map([id], |row| {
            Ok(GraphEdge {
                from: row.get(0)?,
                to: row.get(1)?,
                kind: row.get(2)?,
                path: row.get(3)?,
                line: row.get::<_, u32>(4)? as usize,
            })
        })?
        .collect::<Result<_, _>>()?)
}

pub(crate) fn in_scope(path: &str, scope: &str) -> bool {
    scope.is_empty()
        || path == scope
        || path
            .strip_prefix(scope)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn expand_graph(
    conn: &Connection,
    candidates: &mut Vec<Candidate>,
    scope: &str,
    excluded: &BTreeSet<String>,
) -> Result<()> {
    let mut support: BTreeMap<i64, BTreeMap<i64, f64>> = BTreeMap::new();
    let mut loaded = BTreeMap::new();
    // Exactly one hop from direct retrieval. Newly added neighbors cannot propagate another vote.
    for (rank, seed) in candidates.iter().take(32).enumerate() {
        for edge in neighbors(conn, seed.id)? {
            let id = if edge.from == seed.id {
                edge.to
            } else {
                edge.from
            };
            if id == seed.id {
                continue;
            }
            if let std::collections::btree_map::Entry::Vacant(entry) = loaded.entry(id) {
                entry.insert(load_parent(conn, id)?);
            }
            if !in_scope(&loaded[&id].path, scope)
                || excluded.contains(&loaded[&id].path)
                || excluded.contains(&edge.path)
            {
                continue;
            }
            let degree: f64 = conn.query_row("SELECT (SELECT count(DISTINCT from_id) FROM semantic_edges WHERE to_id=?1) + (SELECT count(DISTINCT to_id) FROM semantic_edges WHERE from_id=?1)", [id], |r| r.get(0))?;
            let relation_weight = if edge.kind == "calls" { 1.0 } else { 0.5 };
            let vote = relation_weight / ((60.0 + (rank + 1) as f64) * degree.max(1.0).sqrt());
            support
                .entry(id)
                .or_default()
                .entry(seed.id)
                .and_modify(|v| *v = v.max(vote))
                .or_insert(vote);
        }
    }
    let mut merged: BTreeMap<_, _> = candidates
        .drain(..)
        .enumerate()
        .map(|(rank, mut candidate)| {
            candidate.score = 1.0 / (60.0 + (rank + 1) as f64);
            (candidate.id, candidate)
        })
        .collect();
    for (id, votes) in support {
        let candidate = merged.entry(id).or_insert_with(|| {
            let mut candidate = loaded.remove(&id).unwrap();
            candidate.score = 0.0;
            candidate.hit.channel = "graph".into();
            candidate.hits = vec![candidate.hit.clone()];
            candidate
        });
        candidate.graph_seed_count = votes.len();
        candidate.graph_score = votes.values().sum::<f64>();
        candidate.score += (candidate.graph_score * 0.5).min(0.8 / 61.0);
    }
    *candidates = merged.into_values().collect();
    sort_candidates(candidates);
    Ok(())
}

pub(crate) fn normalize_scope(scope: Option<&str>) -> Result<String> {
    let Some(scope) = scope else {
        return Ok(String::new());
    };
    ensure!(
        !scope.contains('\\'),
        "scope must use repository-relative forward slashes"
    );
    let path = Path::new(scope);
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
        "scope must be a relative path without '..'"
    );
    Ok(path
        .components()
        .filter_map(|part| match part {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

#[allow(clippy::too_many_arguments)]
fn retrieve(
    conn: &Connection,
    terms: &[String],
    scope: &str,
    stale_files: &BTreeSet<String>,
    coarse: bool,
    file_ranking: crate::FileRankingPolicy,
    kind: &str,
    role_filter: &crate::source_role::Filter,
) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "WITH matched AS MATERIALIZED (
         SELECT l.id, r.id AS region_id, r.start_line,
            -bm25(search, 8.0, 1.0, 3.0, 1.0) * CASE WHEN l.kind = 'file'
                AND (?6=0 OR EXISTS (SELECT 1 FROM locations child WHERE child.parent_id=l.id))
                THEN 0.35 ELSE 1.0 END AS relevance
         FROM search JOIN regions r ON r.id = search.rowid
         JOIN locations l ON l.id = r.location_id JOIN files f ON f.id = l.file_id
         WHERE search MATCH ?1 AND (?2 = '' OR f.path = ?2 OR substr(f.path, 1, length(?2) + 1) = ?2 || '/')
         AND (?5=0 OR l.kind IN ('file','module','documentation'))
         AND (?7='' OR l.kind=?7)
         AND f.path NOT IN (SELECT value FROM json_each(?4))
         AND l.id NOT IN (SELECT value FROM json_each(?8))
         ), distinct_locations AS (
             SELECT *, row_number() OVER (PARTITION BY id ORDER BY relevance DESC, start_line, region_id) AS location_rank FROM matched
         ), limited AS MATERIALIZED (
             SELECT m.* FROM distinct_locations m JOIN locations l ON l.id=m.id JOIN files f ON f.id=l.file_id
             WHERE m.location_rank=1 ORDER BY m.relevance DESC,f.path,m.start_line,m.id LIMIT ?3
         ) SELECT l.id, f.path, l.symbol, l.kind, l.definition, l.parent_id, r.start_line, r.source,
             m.relevance, search.name || ' ' || search.path || ' ' || search.docs || ' ' || search.body,
             l.is_test, r.id AS region_id, r.end_line, r.view
         FROM limited m JOIN regions r ON r.id=m.region_id JOIN locations l ON l.id=m.id
         JOIN files f ON f.id=l.file_id JOIN search ON search.rowid=r.id
         ORDER BY m.relevance DESC,f.path,r.start_line,l.id"
    )?;
    let mut candidates: Vec<Candidate> = stmt
        .query_map(
            params![
                text::match_expression(terms),
                scope,
                if coarse { 64 } else { CANDIDATE_LIMIT as i64 },
                serde_json::to_string(stale_files)?,
                coarse,
                file_ranking == crate::FileRankingPolicy::Standalone,
                kind,
                &role_filter.excluded_json
            ],
            |row| {
                let mut candidate = candidate_from_row(row)?;
                let document: String = row.get(9)?;
                let tokens: BTreeSet<_> = document.split_whitespace().collect();
                let coverage = terms
                    .iter()
                    .filter(|term| tokens.contains(term.as_str()))
                    .count() as f64
                    / terms.len() as f64;
                candidate.score *= 0.2 + coverage * coverage;
                candidate.lexical_score = Some(candidate.score);
                Ok(candidate)
            },
        )?
        .collect::<Result<_, _>>()?;
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start.cmp(&b.start))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(candidates)
}

/// Select focus independently of BM25 length normalization: a short tail region must not
/// win just because every region repeats the enclosing symbol's name.
fn lexical_focus(
    conn: &Connection,
    id: i64,
    preferred_start: usize,
    terms: &[String],
) -> Result<Option<[usize; 2]>> {
    let mut stmt = conn.prepare(
        "SELECT start_line, source FROM regions WHERE location_id = ?1 AND view IN ('combined', 'description') ORDER BY start_line, id",
    )?;
    let regions = stmt.query_map([id], |row| {
        Ok((row.get::<_, u32>(0)? as usize, row.get::<_, String>(1)?))
    })?;
    let mut best = None;
    let mut retrieved_region = None;
    for region in regions {
        let (start, source) = region?;
        if source.is_empty() {
            continue;
        }
        if start == preferred_start {
            retrieved_region = Some([start, start + source.lines().count().saturating_sub(1)]);
        }
        let candidate = text::focus(&source, start, terms);
        if best.as_ref().is_none_or(|(_, count)| candidate.1 > *count) {
            best = Some(candidate);
        }
    }
    Ok(best.and_then(|(range, matches)| {
        if matches > 0 {
            Some(range)
        } else {
            retrieved_region.or(Some(range))
        }
    }))
}

pub(crate) fn load_parent(conn: &Connection, id: i64) -> Result<Candidate> {
    Ok(conn.query_row(
        "SELECT l.id, f.path, l.symbol, l.kind, l.definition, l.parent_id, r.start_line, r.source, 0.0, l.is_test, r.id AS region_id, r.end_line, r.view
         FROM locations l JOIN files f ON f.id = l.file_id JOIN regions r ON r.location_id = l.id
         WHERE l.id = ?1 AND r.view='combined' ORDER BY r.start_line LIMIT 1", [id], candidate_from_row
    )?)
}

pub(crate) fn candidate_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Candidate> {
    let hit = RetrievalHit {
        region_id: row.get("region_id")?,
        start: row.get::<_, u32>(6)? as usize,
        end: row.get::<_, u32>("end_line")? as usize,
        view: row.get("view")?,
        channel: "lexical".into(),
    };
    Ok(Candidate {
        hit: hit.clone(),
        hits: vec![hit],
        exact_path_match: false,
        coarse_rank: None,
        graph_score: 0.0,
        cluster: None,
        id: row.get(0)?,
        path: row.get(1)?,
        symbol: row.get(2)?,
        kind: row.get(3)?,
        is_test: row.get("is_test")?,
        exact_symbol_match: false,
        definition: row.get::<_, Option<u32>>(4)?.map(|line| line as usize),
        parent: row.get(5)?,
        start: row.get::<_, u32>(6)? as usize,
        score: row.get(8)?,
        lexical_score: None,
        lexical_normalized: None,
        embedding_normalized: None,
        lexical_rank: None,
        embedding_rank: None,
        view_ranks: BTreeMap::new(),
        view_similarities: BTreeMap::new(),
        graph_seed_count: 0,
        usage: None,
        ranking: None,
        quantity: None,
        measurements: None,
    })
}

pub(crate) fn location_focus(
    conn: &Connection,
    candidate: &Candidate,
    terms: &[String],
    site: Option<usize>,
) -> Result<Option<[usize; 2]>> {
    if let Some(line) = site {
        // A related caller should open at the recorded occurrence, not at an unrelated word match.
        return Ok(Some([line.saturating_sub(2).max(1), line]));
    }
    if let Some(quantity) = &candidate.quantity {
        return Ok(Some([quantity.line, quantity.end_line]));
    }
    if matches!(
        candidate.hit.channel.as_str(),
        "embedding" | "coarse_embedding"
    ) && candidate.hit.view != "identity"
    {
        let source: String = conn.query_row(
            "SELECT source FROM regions WHERE id=?1",
            [candidate.hit.region_id],
            |r| r.get(0),
        )?;
        let (range, matches) = text::focus(&source, candidate.hit.start, terms);
        return Ok(Some(if matches > 0 {
            range
        } else {
            [candidate.hit.start, candidate.hit.end]
        }));
    }
    lexical_focus(conn, candidate.id, candidate.start, terms)
}

pub(crate) fn markdown_title(conn: &Connection, id: i64) -> Result<Option<String>> {
    let source: String = conn.query_row("SELECT source FROM regions WHERE location_id=?1 AND view='combined' ORDER BY start_line,id LIMIT 1", [id], |r| r.get(0))?;
    let line = source.lines().next().unwrap_or("").trim_start();
    let hashes = line.chars().take_while(|c| *c == '#').count();
    Ok(
        if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
            // Preserve the actual source heading, including inline Markdown, rather than generating a label.
            Some(line[hashes..].trim().to_owned())
        } else {
            None
        },
    )
}

fn exact_lookup(
    conn: &Connection,
    candidates: &mut Vec<Candidate>,
    query: &str,
    scope: &str,
    stale: &BTreeSet<String>,
    kind_filter: &str,
    role_filter: &crate::source_role::Filter,
) -> Result<()> {
    let normalize = |value: &str| {
        value
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let identity = normalize(query);
    if identity.is_empty() {
        return Ok(());
    }
    let path_query = query
        .trim()
        .trim_matches('`')
        .strip_prefix("./")
        .unwrap_or(query.trim().trim_matches('`'));
    let mut stmt = conn.prepare("SELECT l.id,l.symbol,l.kind,f.path FROM locations l JOIN files f ON f.id=l.file_id WHERE ?1='' OR f.path=?1 OR substr(f.path,1,length(?1)+1)=?1||'/' ORDER BY f.path,l.id")?;
    let rows = stmt.query_map([scope], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut matches = 0;
    for row in rows {
        let (id, symbol, kind, path) = row?;
        if !kind_filter.is_empty() && kind != kind_filter {
            continue;
        }
        if stale.contains(&path) || role_filter.excluded.contains(&id) {
            continue;
        }
        let symbol_match = symbol.as_deref().is_some_and(|s| normalize(s) == identity);
        let path_match = path == path_query && (kind == "file" || kind == "documentation");
        if !symbol_match && !path_match {
            continue;
        }
        if let Some(existing) = candidates.iter_mut().find(|c| c.id == id) {
            existing.exact_symbol_match = symbol_match;
            existing.exact_path_match = path_match;
        } else {
            let mut candidate = load_parent(conn, id)?;
            candidate.exact_symbol_match = symbol_match;
            candidate.exact_path_match = path_match;
            candidate.hit.channel = "exact".into();
            candidate.hits = vec![candidate.hit.clone()];
            candidates.push(candidate);
        }
        matches += 1;
        if matches >= CANDIDATE_LIMIT {
            break;
        }
    }
    sort_candidates(candidates);
    Ok(())
}

pub(crate) fn validate_kind(kind: Option<&str>) -> Result<()> {
    if let Some(kind) = kind {
        ensure!(
            !kind.is_empty()
                && kind.len() <= 64
                && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "kind must be a lowercase indexed kind, such as function, struct, field or documentation"
        );
    }
    Ok(())
}

fn coarse_embeddings(
    conn: &Connection,
    best: &BTreeMap<(String, i64), Candidate>,
) -> Result<Vec<Candidate>> {
    let mut groups: BTreeMap<(&str, &str), Vec<&Candidate>> = BTreeMap::new();
    for ((view, _), candidate) in best {
        if candidate.kind != "file" && candidate.kind != "documentation" {
            groups
                .entry((&candidate.path, view))
                .or_default()
                .push(candidate);
        }
    }
    let mut channels: BTreeMap<&str, Vec<Candidate>> = BTreeMap::new();
    for ((path, view), mut members) in groups {
        members.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
        // At most three independent symbols contribute. Repeated regions never add votes.
        let score =
            members.iter().take(3).map(|c| c.score).sum::<f64>() / members.len().min(3) as f64;
        let id: i64 = conn.query_row("SELECT l.id FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path=?1 AND l.kind='file' LIMIT 1",[path],|r| r.get(0))?;
        let mut candidate = load_parent(conn, id)?;
        candidate.use_hit(members[0].hit.clone());
        candidate.hit.channel = "coarse_embedding".into();
        candidate.hits = vec![candidate.hit.clone()];
        candidate.score = score;
        channels.entry(view).or_default().push(candidate);
    }
    let mut merged: BTreeMap<i64, Candidate> = BTreeMap::new();
    for (_, mut channel) in channels {
        sort_candidates(&mut channel);
        for (rank, mut candidate) in channel.into_iter().take(64).enumerate() {
            candidate.score = 1.0 / (60.0 + (rank + 1) as f64);
            if let Some(previous) = merged.get_mut(&candidate.id) {
                if candidate.score > previous.score {
                    *previous = candidate;
                }
            } else {
                merged.insert(candidate.id, candidate);
            }
        }
    }
    let mut result: Vec<_> = merged.into_values().collect();
    sort_candidates(&mut result);
    result.truncate(64);
    Ok(result)
}

fn add_coarse(candidates: &mut Vec<Candidate>, lexical: Vec<Candidate>, dense: Vec<Candidate>) {
    let mut coarse = crate::fusion::fuse(lexical, dense, crate::FusionMethod::Rrf, 0.5);
    coarse.truncate(64);
    for (rank, candidate) in candidates.iter_mut().enumerate() {
        candidate.score = 1.0 / (60.0 + (rank + 1) as f64);
    }
    for (rank, mut candidate) in coarse.into_iter().enumerate() {
        let boost = 0.35 / (60.0 + (rank + 1) as f64);
        if let Some(previous) = candidates.iter_mut().find(|c| c.id == candidate.id) {
            previous.score += boost;
            previous.coarse_rank = Some(rank + 1);
            previous.hits.extend(candidate.hits);
        } else {
            candidate.score = boost;
            candidate.coarse_rank = Some(rank + 1);
            candidates.push(candidate);
        }
    }
    sort_candidates(candidates);
}
