mod cli_help;

use std::{
    io::{BufRead, Write},
    path::PathBuf,
};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use sonar::onboarding::{self, Client};
use sonar::{BriefOptions, RetrievalMode, SearchEngine};

#[derive(Parser)]
#[command(
    name = "sonar",
    version,
    about = "Find relevant code in your repository locally",
    long_about = "Find relevant code in your repository locally.\n\nReturn relevant files, symbols, and source line ranges as JSON.\nSupports Rust, Python, TypeScript/TSX, and Markdown.\nWorks from your terminal or through an AI agent using MCP.",
    after_help = cli_help::CLI
)]
struct Cli {
    /// Local index database [default: saved project database, or .sonar/index.sqlite].
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize or refresh this repository, navigation, and ignore rules.
    #[command(display_order = 0, after_help = cli_help::INIT)]
    Init(InitArgs),
    /// Initialize a repository and connect Codex or Claude Code through MCP.
    #[command(display_order = 0, after_help = cli_help::SETUP)]
    Setup {
        #[arg(value_enum)]
        client: Client,
        #[command(flatten)]
        init: InitArgs,
        /// Register for your user instead of only this project.
        #[arg(long)]
        global: bool,
        /// Custom MCP server name.
        #[arg(long)]
        name: Option<String>,
        /// Replace a same-named registration with different settings.
        #[arg(long)]
        replace: bool,
    },
    /// Report project initialization, source freshness, and preparation as JSON.
    #[command(display_order = 5, after_help = cli_help::STATUS)]
    Status,
    /// Serve code navigation and repository initialization over MCP.
    #[command(display_order = 9, after_help = cli_help::MCP)]
    Mcp {
        /// Repository root to bind before initialization [default: current project].
        #[arg(long)]
        root: Option<PathBuf>,
        /// Hybrid discovery model [default: saved project model, or lexical search].
        #[arg(long)]
        model: Option<PathBuf>,
    },
    /// Find code entry points or expand a selected location with source evidence.
    #[command(display_order = 2, after_help = cli_help::INVESTIGATE)]
    Investigate {
        /// Code question or symbol to investigate.
        query: String,
        /// Expand this handle into callers, callees, references, test code and literal document links.
        #[arg(long)]
        from: Option<String>,
        /// Restrict starting locations to a repository-relative file or directory.
        #[arg(long)]
        scope: Option<String>,
        /// Omit a literal repository-relative file or directory; repeat for multiple paths.
        #[arg(long = "exclude", value_name = "PATH")]
        exclusions: Vec<String>,
        /// Hybrid discovery model [default: saved project model, or lexical search].
        #[arg(long)]
        model: Option<PathBuf>,
    },
    /// Inspect compiler-resolved field uses and their source evidence.
    #[command(display_order = 8, after_help = cli_help::FIELD_USES)]
    FieldUses {
        /// Full field or function handle returned by outline or locate.
        handle: String,
        /// Restrict use-site files; declarations can be outside this scope.
        #[arg(long)]
        scope: Option<String>,
        /// Maximum number of use sites per page.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Continue the same handle and options using the returned next_cursor.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Search around a known location through recorded calls and references.
    #[command(display_order = 7, after_help = cli_help::FOLLOW)]
    Follow {
        /// Full location handle from locate, outline, investigate, or brief --handles.
        handle: String,
        /// Question or search terms to match within the connected locations.
        query: String,
        /// Recorded relationship to follow; semantic includes calls and references.
        #[arg(long, value_enum, default_value = "semantic")]
        relation: sonar::FollowRelation,
        /// Follow incoming connections, outgoing connections, or both.
        #[arg(long, value_enum, default_value = "both")]
        direction: sonar::Direction,
        /// Restrict returned matches to a repository-relative file or directory.
        #[arg(long)]
        scope: Option<String>,
        /// Only return endpoints of this indexed kind (for example, function).
        #[arg(long)]
        kind: Option<String>,
        /// Maximum relationship hops to explore.
        #[arg(long, default_value_t = 2)]
        hops: usize,
        /// Maximum number of matching locations to return.
        #[arg(long, default_value_t = 5)]
        limit: usize,
        /// Match using lexical terms, embeddings, or both (hybrid).
        #[arg(long, value_enum, default_value = "lexical")]
        retrieval: RetrievalMode,
        /// Local model directory, required for embedding and hybrid retrieval.
        #[arg(long)]
        model: Option<PathBuf>,
        /// Policy for choosing suggested source ranges.
        #[arg(long, value_enum, default_value = "window")]
        focus: sonar::FocusPolicy,
        /// Maximum lines per bounded source focus (1-64).
        #[arg(long, default_value_t = 16)]
        focus_lines: usize,
    },
    /// Find the indexed declaration at a source line and optional byte column.
    #[command(display_order = 5, after_help = cli_help::LOCATE)]
    Locate {
        /// File path relative to the indexed repository.
        path: String,
        /// One-based source line number.
        #[arg(long)]
        line: usize,
        /// Optional one-based UTF-8 byte column.
        #[arg(long)]
        column: Option<usize>,
    },
    /// List a file's declarations, navigation handles, and parent relationships.
    #[command(display_order = 4, after_help = cli_help::OUTLINE)]
    Outline {
        /// Indexed file path relative to the repository.
        path: String,
        /// Maximum number of declarations per page.
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Continue the same file and limit using the returned next_cursor.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// List recorded relationships and source evidence around a known location.
    #[command(display_order = 6, after_help = cli_help::NEIGHBORS)]
    Neighbors {
        /// Full location handle from locate, outline, investigate, or brief --handles.
        handle: String,
        /// Recorded relationship to inspect.
        #[arg(long, value_enum, default_value = "all")]
        relation: sonar::Relation,
        /// Inspect incoming connections, outgoing connections, or both.
        #[arg(long, value_enum, default_value = "both")]
        direction: sonar::Direction,
        /// Restrict returned locations to a repository-relative file or directory.
        #[arg(long)]
        scope: Option<String>,
        /// Maximum number of related locations per page.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Continue the same handle and options using the returned next_cursor.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Prepare optional document/resource links and module navigation.
    #[command(display_order = 11, after_help = cli_help::INDEX_NAVIGATION)]
    IndexNavigation,
    /// Prepare optional exact integer quantity evidence from Rust sources.
    #[command(display_order = 14, after_help = cli_help::INDEX_QUANTITIES)]
    IndexQuantities,
    /// Export structural features as JSON for offline ranking research.
    #[command(display_order = 18, after_help = cli_help::STRUCTURE)]
    Structure {
        /// Restrict observations to a repository-relative file or directory.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Prepare Rust or TypeScript compiler relationships for calls and references.
    #[command(display_order = 10, after_help = cli_help::RESOLVE)]
    Resolve {
        /// Rust analyzer executable; requires the repository's toolchain and dependencies.
        #[arg(long, default_value = "rust-analyzer")]
        analyzer: PathBuf,
        /// JSON file containing Rust analyzer settings.
        #[arg(long)]
        config: Option<PathBuf>,
        /// TypeScript compiler module directory/file; selects TypeScript analysis.
        #[arg(long)]
        typescript: Option<PathBuf>,
        /// Node.js executable used for TypeScript resolution.
        #[arg(long, default_value = "node")]
        node: PathBuf,
        /// TypeScript tsconfig; defaults to the repository config and its references.
        #[arg(long, requires = "typescript")]
        project: Option<PathBuf>,
    },
    /// Import a compiler export with its matching source-bound Sonar manifest.
    #[command(display_order = 15, after_help = cli_help::IMPORT_SCIP)]
    ImportScip {
        /// SCIP export file for the indexed repository snapshot.
        scip: PathBuf,
        /// Matching Sonar semantic manifest JSON file.
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Index or refresh supported source files and Markdown documentation.
    #[command(display_order = 1, after_help = cli_help::INDEX)]
    Index {
        /// Repository directory to index; use . for the current directory.
        root: PathBuf,
        /// Chunking [default: saved policy, or lines]; ast reparses implementation regions.
        #[arg(long, value_enum)]
        chunking: Option<sonar::Chunking>,
        /// YAML text resources [default: saved policy, or none].
        #[arg(long, value_enum)]
        resources: Option<sonar::ResourcePolicy>,
    },
    /// Build optional search embeddings using a model already stored locally.
    #[command(display_order = 12, after_help = cli_help::EMBED)]
    Embed {
        /// Existing local embedding model directory; no automatic downloads.
        #[arg(long)]
        model: PathBuf,
        /// Representation used to encode indexed locations.
        #[arg(long, value_enum, default_value = "separate")]
        representation: sonar::Representation,
        /// Experimental static encoder policy; full preserves all supplied tokens.
        #[arg(long, value_enum, default_value = "legacy")]
        encoding: sonar::EncodingPolicy,
    },
    /// Build optional caller embeddings after source embeddings and Rust resolution.
    #[command(display_order = 13, after_help = cli_help::EMBED_USAGE)]
    EmbedUsage {
        /// Same local model directory used by embed.
        #[arg(long)]
        model: PathBuf,
    },
    /// Find ranked files, symbols, and source line ranges as JSON.
    #[command(display_order = 3, after_help = cli_help::BRIEF)]
    Brief {
        /// Code question, symbol, or search terms.
        query: String,
        #[command(flatten)]
        options: QueryArgs,
    },
    /// Inspect search candidates, ranking, and retrieval-channel scores.
    #[command(display_order = 16, after_help = cli_help::DIAGNOSE)]
    Diagnose {
        /// Question or search terms to investigate.
        query: String,
        #[command(flatten)]
        options: QueryArgs,
    },
    /// Run JSON-string queries from stdin and write one JSON response per line.
    #[command(display_order = 17, after_help = cli_help::BATCH)]
    Batch {
        /// Return diagnostics instead of briefs, while retaining the loaded model.
        #[arg(long)]
        diagnostics: bool,
        #[command(flatten)]
        options: QueryArgs,
    },
}
#[derive(Args)]
struct InitArgs {
    /// Repository directory [default: current project].
    root: Option<PathBuf>,
    /// YAML resources [default: saved policy, or none]; remembered by init.
    #[arg(long, value_enum)]
    resources: Option<sonar::ResourcePolicy>,
    /// Chunking [default: saved policy, or lines]; remembered by init.
    #[arg(long, value_enum)]
    chunking: Option<sonar::Chunking>,
    /// Also resolve Rust calls and references with rust-analyzer.
    #[arg(long)]
    resolve: bool,
    /// Also resolve TypeScript using this local compiler module.
    #[arg(long)]
    typescript: Option<PathBuf>,
    /// Also prepare embeddings using this local model; no automatic downloads.
    #[arg(long)]
    model: Option<PathBuf>,
}

#[derive(Args)]
struct QueryArgs {
    /// Restrict starts to implementation, test, or documentation before retrieval limits.
    #[arg(long = "role", value_enum)]
    source_role: Option<sonar::SourceRole>,
    /// Restrict starting locations to an indexed kind (for example, function or documentation).
    #[arg(long)]
    kind: Option<String>,
    /// Include snapshot-bound location handles for explicit follow-up navigation.
    #[arg(long)]
    handles: bool,
    /// Experimental embedded repository names; requires --selection ranked.
    #[arg(long)]
    anchors: bool,
    /// Experimental navigation: uses requires resolve; other modes use index-navigation.
    #[arg(long, value_enum, default_value = "off")]
    navigation: sonar::NavigationPolicy,
    /// Maximum number of ranked starting locations.
    #[arg(long, default_value_t = 5)]
    limit: usize,
    /// Maximum number of supporting locations across the selected starting locations.
    #[arg(long, default_value_t = 3)]
    related: usize,
    /// Restrict results to a repository-relative directory or exact file.
    #[arg(long)]
    scope: Option<String>,
    /// Omit a literal repository-relative file or directory; repeat for multiple paths.
    #[arg(long = "exclude", value_name = "PATH")]
    exclusions: Vec<String>,
    /// Retrieval mode [default: hybrid with a saved model, otherwise lexical].
    #[arg(long, value_enum)]
    retrieval: Option<RetrievalMode>,
    /// Local model directory [default: saved project model]; no downloads.
    #[arg(long)]
    model: Option<PathBuf>,
    /// Expand and rank through a bounded neighborhood of current resolved references.
    #[arg(long)]
    graph: bool,
    /// Retrieve through verified callers using optional vectors built by embed-usage.
    #[arg(long)]
    usage: bool,
    /// Add exact byte/time quantity evidence; requires index-quantities.
    #[arg(long)]
    quantities: bool,
    /// Keep original wording, replace quantities, or expand with a normalized variant.
    #[arg(long, value_enum, default_value = "original", requires = "quantities")]
    quantity_query: sonar::QuantityQueryPolicy,
    /// Experimental quantity scoring; bounded-local adds local evidence to unified scores.
    #[arg(long, value_enum, default_value = "legacy", requires = "quantities")]
    quantity_ranking: sonar::QuantityRankingPolicy,
    /// Calibrate each query variant separately; requires unified or local quantity scoring.
    #[arg(long, value_enum, default_value = "joint", requires = "quantities")]
    variant_scoring: sonar::VariantScoring,
    /// Experimental strength-based candidate scoring; legacy preserves existing results.
    #[arg(long, value_enum, default_value = "legacy")]
    scorer: sonar::ScoringPolicy,
    /// Preserve each channel's leading candidate in hybrid briefs when the start budget permits.
    #[arg(long)]
    channel_coverage: bool,
    /// Experimental set selection; baseline retains the original file-repetition penalty.
    #[arg(long, value_enum, default_value = "baseline")]
    selection: sonar::SelectionPolicy,
    /// Graph ranking method, used only with --graph.
    #[arg(long, value_enum, default_value = "one-hop")]
    graph_method: sonar::GraphMethod,
    /// Add file/module discovery without restricting direct symbol retrieval.
    #[arg(long)]
    coarse: bool,
    /// Hybrid channel fusion; score uses normalized per-query evidence.
    #[arg(long, value_enum, default_value = "rrf")]
    fusion: sonar::FusionMethod,
    /// Lexical share of hybrid fusion, between 0 and 1.
    #[arg(long, default_value_t = 0.5)]
    lexical_weight: f64,
    /// Rank supporting locations by structure alone or structure plus query relevance.
    #[arg(long, value_enum, default_value = "structural")]
    context: sonar::ContextPolicy,
    /// Experimental lexical policy for files without separately indexed declarations.
    #[arg(long, value_enum, default_value = "legacy")]
    file_ranking: sonar::FileRankingPolicy,
    /// Reading-range selection, independent of retrieval and start ranking.
    #[arg(long, value_enum, default_value = "legacy")]
    focus: sonar::FocusPolicy,
    /// Maximum lines per bounded focus (1-64; legacy unchanged).
    #[arg(long, default_value_t = 16)]
    focus_lines: usize,
}

impl QueryArgs {
    fn quantity_options(&self) -> sonar::QuantityOptions {
        sonar::QuantityOptions {
            query: self.quantity_query,
            ranking: self.quantity_ranking,
        }
    }
    fn focus_options(&self) -> sonar::FocusOptions {
        sonar::FocusOptions {
            policy: self.focus,
            max_lines: self.focus_lines,
        }
    }

    fn ranking(&self) -> sonar::RankingOptions {
        sonar::RankingOptions {
            selection: self.selection,
            graph_method: self.graph_method,
            coarse: self.coarse,
            fusion: self.fusion,
            lexical_weight: self.lexical_weight,
            context: self.context,
            file_ranking: self.file_ranking,
        }
    }

    fn brief_options(&self) -> BriefOptions {
        BriefOptions {
            limit: self.limit,
            related: self.related,
            scope: self.scope.clone(),
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let root = match &cli.command {
        Command::Init(init) | Command::Setup { init, .. } => init.root.as_deref(),
        Command::Mcp { root, .. } => root.as_deref(),
        _ => None,
    };
    let mut configuration = onboarding::configuration(root, cli.db.as_deref())?;
    if let Command::Init(init) | Command::Setup { init, .. } = &cli.command {
        if let Some(resources) = init.resources {
            configuration.resources = resources;
        }
        if let Some(chunking) = init.chunking {
            configuration.chunking = chunking;
        }
        configuration.rust |= init.resolve;
        if let Some(compiler) = &init.typescript {
            configuration.typescript = Some(std::path::absolute(compiler)?);
        }
        if let Some(model) = &init.model {
            configuration.model = Some(std::path::absolute(model)?);
        }
    }
    let db = configuration.db.clone();
    if !matches!(
        cli.command,
        Command::Init(_)
            | Command::Setup { .. }
            | Command::Mcp { .. }
            | Command::Index { .. }
            | Command::Status
    ) {
        onboarding::require_initialized(&db)?;
    }
    let output = match cli.command {
        Command::Init(_) => serde_json::to_string_pretty(&onboarding::initialize(&configuration)?)?,
        Command::Setup {
            client,
            global,
            name,
            replace,
            ..
        } => serde_json::to_string_pretty(&onboarding::setup(
            &configuration,
            client,
            global,
            name.as_deref(),
            replace,
        )?)?,
        Command::Status => serde_json::to_string_pretty(&sonar::project_status(
            &db,
            configuration.model.as_deref(),
            Some(&configuration.root),
        )?)?,
        Command::Mcp { model, .. } => {
            return sonar::serve_mcp_project(
                &db,
                model.as_deref().or(configuration.model.as_deref()),
                Some(&configuration.root),
            );
        }
        Command::Investigate {
            query,
            from,
            scope,
            exclusions,
            model,
        } => {
            let navigator = sonar::Investigator::open(
                &db,
                model.as_deref().or(configuration.model.as_deref()),
            )?;
            serde_json::to_string_pretty(&navigator.investigate(
                &query,
                &sonar::InvestigationOptions {
                    from,
                    scope,
                    exclusions,
                },
            )?)?
        }
        Command::FieldUses {
            handle,
            scope,
            limit,
            cursor,
        } => serde_json::to_string_pretty(&sonar::field_uses(
            &db,
            &handle,
            &sonar::FieldUseOptions {
                scope,
                limit,
                cursor,
            },
        )?)?,
        Command::Follow {
            handle,
            query,
            relation,
            direction,
            scope,
            kind,
            hops,
            limit,
            retrieval,
            model,
            focus,
            focus_lines,
        } => {
            let engine = SearchEngine::open(&db, retrieval, model.as_deref())?;
            serde_json::to_string_pretty(&engine.follow(
                &handle,
                &query,
                &sonar::FollowOptions {
                    relation,
                    direction,
                    scope,
                    kind,
                    hops,
                    limit,
                    focus: sonar::FocusOptions {
                        policy: focus,
                        max_lines: focus_lines,
                    },
                },
            )?)?
        }
        Command::Locate { path, line, column } => {
            serde_json::to_string_pretty(&sonar::locate(&db, &path, line, column)?)?
        }
        Command::Outline {
            path,
            limit,
            cursor,
        } => serde_json::to_string_pretty(&sonar::outline(&db, &path, limit, cursor.as_deref())?)?,
        Command::Neighbors {
            handle,
            relation,
            direction,
            scope,
            limit,
            cursor,
        } => serde_json::to_string_pretty(&sonar::neighbors(
            &db,
            &handle,
            &sonar::NeighborOptions {
                relation,
                direction,
                scope,
                limit,
                cursor,
            },
        )?)?,
        Command::IndexNavigation => serde_json::to_string_pretty(&sonar::index_navigation(&db)?)?,
        Command::IndexQuantities => serde_json::to_string_pretty(&sonar::index_quantities(&db)?)?,
        Command::Structure { scope } => {
            serde_json::to_string_pretty(&sonar::structure(&db, scope.as_deref())?)?
        }
        Command::Resolve {
            analyzer,
            config,
            typescript,
            node,
            project,
        } => {
            if let Some(compiler) = typescript {
                serde_json::to_string_pretty(&sonar::resolve_typescript(
                    &db,
                    &node,
                    Some(&compiler),
                    project.as_deref(),
                )?)?
            } else {
                serde_json::to_string_pretty(&sonar::resolve(&db, &analyzer, config.as_deref())?)?
            }
        }
        Command::ImportScip { scip, manifest } => {
            serde_json::to_string_pretty(&sonar::import_scip(&db, &scip, &manifest)?)?
        }
        Command::Index {
            root,
            chunking,
            resources,
        } => serde_json::to_string_pretty(&sonar::index_with_resources(
            &root,
            &db,
            chunking.unwrap_or(configuration.chunking),
            resources.unwrap_or(configuration.resources),
        )?)?,
        Command::Embed {
            model,
            representation,
            encoding,
        } => {
            let mut last = None;
            let report = sonar::embed_index_with_options(
                &db,
                &model,
                representation,
                encoding,
                |progress| {
                    if last
                        .is_none_or(|instant: std::time::Instant| instant.elapsed().as_secs() >= 2)
                        || progress.encoded + progress.reused == progress.total
                    {
                        eprintln!(
                            "sonar: encoded {}, reused {}, total {}",
                            progress.encoded, progress.reused, progress.total
                        );
                        last = Some(std::time::Instant::now());
                    }
                },
            )?;
            serde_json::to_string_pretty(&report)?
        }
        Command::EmbedUsage { model } => {
            serde_json::to_string_pretty(&sonar::embed_usage(&db, &model)?)?
        }
        Command::Brief { query, options } => {
            let engine = SearchEngine::open(
                &db,
                options
                    .retrieval
                    .unwrap_or(if configuration.model.is_some() {
                        RetrievalMode::Hybrid
                    } else {
                        RetrievalMode::Lexical
                    }),
                options.model.as_deref().or(configuration.model.as_deref()),
            )?
            .with_handles(options.handles)
            .with_kind(options.kind.clone())
            .with_source_role(options.source_role)
            .with_exclusions(options.exclusions.clone())
            .with_graph(options.graph)
            .with_anchors(options.anchors)
            .with_navigation(options.navigation)
            .with_usage(options.usage)
            .with_quantities(options.quantities)
            .with_quantity_options(options.quantity_options())
            .with_variant_scoring(options.variant_scoring)
            .with_scorer(options.scorer)
            .with_channel_coverage(options.channel_coverage)
            .with_ranking(options.ranking())
            .with_focus(options.focus_options());
            serde_json::to_string_pretty(&engine.brief(&query, &options.brief_options())?)?
        }
        Command::Diagnose { query, options } => {
            let engine = SearchEngine::open(
                &db,
                options
                    .retrieval
                    .unwrap_or(if configuration.model.is_some() {
                        RetrievalMode::Hybrid
                    } else {
                        RetrievalMode::Lexical
                    }),
                options.model.as_deref().or(configuration.model.as_deref()),
            )?
            .with_handles(options.handles)
            .with_kind(options.kind.clone())
            .with_source_role(options.source_role)
            .with_exclusions(options.exclusions.clone())
            .with_graph(options.graph)
            .with_anchors(options.anchors)
            .with_navigation(options.navigation)
            .with_usage(options.usage)
            .with_quantities(options.quantities)
            .with_quantity_options(options.quantity_options())
            .with_variant_scoring(options.variant_scoring)
            .with_scorer(options.scorer)
            .with_channel_coverage(options.channel_coverage)
            .with_ranking(options.ranking())
            .with_focus(options.focus_options());
            serde_json::to_string_pretty(&engine.diagnose(&query, &options.brief_options())?)?
        }
        Command::Batch {
            options,
            diagnostics,
        } => {
            let engine = SearchEngine::open(
                &db,
                options
                    .retrieval
                    .unwrap_or(if configuration.model.is_some() {
                        RetrievalMode::Hybrid
                    } else {
                        RetrievalMode::Lexical
                    }),
                options.model.as_deref().or(configuration.model.as_deref()),
            )?
            .with_handles(options.handles)
            .with_kind(options.kind.clone())
            .with_source_role(options.source_role)
            .with_exclusions(options.exclusions.clone())
            .with_graph(options.graph)
            .with_anchors(options.anchors)
            .with_navigation(options.navigation)
            .with_usage(options.usage)
            .with_quantities(options.quantities)
            .with_quantity_options(options.quantity_options())
            .with_variant_scoring(options.variant_scoring)
            .with_scorer(options.scorer)
            .with_channel_coverage(options.channel_coverage)
            .with_ranking(options.ranking())
            .with_focus(options.focus_options());
            let mut stdout = std::io::stdout().lock();
            for line in std::io::stdin().lock().lines() {
                let query: String = serde_json::from_str(&line?)?;
                if diagnostics {
                    serde_json::to_writer(
                        &mut stdout,
                        &engine.diagnose(&query, &options.brief_options())?,
                    )?;
                } else {
                    serde_json::to_writer(
                        &mut stdout,
                        &engine.brief(&query, &options.brief_options())?,
                    )?;
                }
                writeln!(stdout)?;
                stdout.flush()?;
            }
            return Ok(());
        }
    };
    println!("{output}");
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sonar: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
