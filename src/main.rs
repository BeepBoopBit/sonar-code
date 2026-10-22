use std::{
    io::{BufRead, Write},
    path::PathBuf,
};

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use sonar::{BriefOptions, RetrievalMode, SearchEngine};

#[derive(Parser)]
#[command(
    name = "sonar",
    version,
    about = "Local navigation briefs for coding agents"
)]
struct Cli {
    /// Local index database. Relative paths are resolved from the current directory.
    #[arg(long, global = true, default_value = ".sonar/index.sqlite")]
    db: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve read-only agent navigation over MCP stdio; keep an optional local model loaded.
    Mcp {
        /// Local model for hybrid discovery. Omit for model-free lexical navigation.
        #[arg(long)]
        model: Option<PathBuf>,
    },
    /// Discover primary/function/documentation leads, or expand a chosen handle into evidenced context.
    Investigate {
        query: String,
        /// Expand this handle into callers, callees, references, test code and literal document links.
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long = "exclude", value_name = "PATH")]
        exclusions: Vec<String>,
        /// Use hybrid discovery with this local model; otherwise lexical. No downloads.
        #[arg(long)]
        model: Option<PathBuf>,
    },
    /// Inspect compiler-resolved field uses and bounded source associations from a field or function.
    FieldUses {
        handle: String,
        /// Restrict use-site files; declarations can be outside this scope.
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Search a bounded neighborhood of a known location and return an evidenced trail to each match.
    Follow {
        handle: String,
        query: String,
        #[arg(long, value_enum, default_value = "semantic")]
        relation: sonar::FollowRelation,
        #[arg(long, value_enum, default_value = "both")]
        direction: sonar::Direction,
        #[arg(long)]
        scope: Option<String>,
        /// Only return endpoints of this indexed kind (for example, function).
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 2)]
        hops: usize,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long, value_enum, default_value = "lexical")]
        retrieval: RetrievalMode,
        #[arg(long)]
        model: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "window")]
        focus: sonar::FocusPolicy,
        #[arg(long, default_value_t = 16)]
        focus_lines: usize,
    },
    /// Locate the smallest indexed declaration at a source line, optionally a UTF-8 byte column.
    Locate {
        path: String,
        #[arg(long)]
        line: usize,
        #[arg(long)]
        column: Option<usize>,
    },
    /// List a file's declarations in source order, with handles and parent relationships.
    Outline {
        path: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Follow evidenced connections from a handle returned by locate, outline, or brief --handles.
    Neighbors {
        handle: String,
        #[arg(long, value_enum, default_value = "all")]
        relation: sonar::Relation,
        #[arg(long, value_enum, default_value = "both")]
        direction: sonar::Direction,
        #[arg(long)]
        scope: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Build optional source-bound resource, documentation and module navigation facts.
    IndexNavigation,
    /// Build optional exact integer quantity evidence from current Rust sources.
    IndexQuantities,
    /// Export source-backed structural features for offline ranking research (read-only).
    Structure {
        /// Restrict observations to a repository-relative file or directory.
        #[arg(long)]
        scope: Option<String>,
    },
    /// Resolve Rust with rust-analyzer, or TypeScript with an explicit local compiler.
    Resolve {
        #[arg(long, default_value = "rust-analyzer")]
        analyzer: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        /// TypeScript compiler module directory/file; selects TypeScript analysis.
        #[arg(long)]
        typescript: Option<PathBuf>,
        #[arg(long, default_value = "node")]
        node: PathBuf,
        /// TypeScript tsconfig; defaults to the repository config and its references.
        #[arg(long, requires = "typescript")]
        project: Option<PathBuf>,
    },
    /// Import a SCIP export paired with its source-bound manifest.
    ImportScip {
        scip: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
    },
    /// Index supported source languages and Markdown; reuse unchanged files and remove deleted entries.
    Index {
        root: PathBuf,
        /// Experimental AST-aligned implementation regions; switching reparses the index.
        #[arg(long, value_enum, default_value = "lines")]
        chunking: sonar::Chunking,
        /// Optional bounded YAML text resources; persisted for later freshness checks.
        #[arg(long, value_enum, default_value = "none")]
        resources: sonar::ResourcePolicy,
    },
    /// Build or update vectors with an explicitly supplied local encoder directory.
    Embed {
        #[arg(long)]
        model: PathBuf,
        #[arg(long, value_enum, default_value = "separate")]
        representation: sonar::Representation,
        /// Experimental static encoder policy; full preserves all supplied tokens.
        #[arg(long, value_enum, default_value = "legacy")]
        encoding: sonar::EncodingPolicy,
    },
    /// Build optional caller vectors after source embeddings and current Rust analysis.
    EmbedUsage {
        #[arg(long)]
        model: PathBuf,
    },
    /// Return ranked locations as JSON, without source excerpts.
    Brief {
        query: String,
        #[command(flatten)]
        options: QueryArgs,
    },
    /// Inspect candidate ranks and retrieval-channel scores; separate from the brief contract.
    Diagnose {
        query: String,
        #[command(flatten)]
        options: QueryArgs,
    },
    /// Read JSON-string queries on stdin and write one brief per line, reusing the loaded model.
    Batch {
        /// Return diagnostics instead of briefs, while retaining the loaded model.
        #[arg(long)]
        diagnostics: bool,
        #[command(flatten)]
        options: QueryArgs,
    },
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
    #[arg(long, default_value_t = 5)]
    limit: usize,
    #[arg(long, default_value_t = 3)]
    related: usize,
    /// Restrict results to a repository-relative directory or exact file.
    #[arg(long)]
    scope: Option<String>,
    /// Omit a literal repository-relative file or directory; repeat for multiple paths.
    #[arg(long = "exclude", value_name = "PATH")]
    exclusions: Vec<String>,
    #[arg(long, value_enum, default_value = "lexical")]
    retrieval: RetrievalMode,
    /// Local model directory, required for embedding and hybrid modes.
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
    let output = match cli.command {
        Command::Mcp { model } => return sonar::serve_mcp(&cli.db, model.as_deref()),
        Command::Investigate {
            query,
            from,
            scope,
            exclusions,
            model,
        } => {
            let navigator = sonar::Investigator::open(&cli.db, model.as_deref())?;
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
            &cli.db,
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
            let engine = SearchEngine::open(&cli.db, retrieval, model.as_deref())?;
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
            serde_json::to_string_pretty(&sonar::locate(&cli.db, &path, line, column)?)?
        }
        Command::Outline {
            path,
            limit,
            cursor,
        } => serde_json::to_string_pretty(&sonar::outline(
            &cli.db,
            &path,
            limit,
            cursor.as_deref(),
        )?)?,
        Command::Neighbors {
            handle,
            relation,
            direction,
            scope,
            limit,
            cursor,
        } => serde_json::to_string_pretty(&sonar::neighbors(
            &cli.db,
            &handle,
            &sonar::NeighborOptions {
                relation,
                direction,
                scope,
                limit,
                cursor,
            },
        )?)?,
        Command::IndexNavigation => {
            serde_json::to_string_pretty(&sonar::index_navigation(&cli.db)?)?
        }
        Command::IndexQuantities => {
            serde_json::to_string_pretty(&sonar::index_quantities(&cli.db)?)?
        }
        Command::Structure { scope } => {
            serde_json::to_string_pretty(&sonar::structure(&cli.db, scope.as_deref())?)?
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
                    &cli.db,
                    &node,
                    Some(&compiler),
                    project.as_deref(),
                )?)?
            } else {
                serde_json::to_string_pretty(&sonar::resolve(
                    &cli.db,
                    &analyzer,
                    config.as_deref(),
                )?)?
            }
        }
        Command::ImportScip { scip, manifest } => {
            serde_json::to_string_pretty(&sonar::import_scip(&cli.db, &scip, &manifest)?)?
        }
        Command::Index {
            root,
            chunking,
            resources,
        } => serde_json::to_string_pretty(&sonar::index_with_resources(
            &root, &cli.db, chunking, resources,
        )?)?,
        Command::Embed {
            model,
            representation,
            encoding,
        } => {
            let mut last = None;
            let report = sonar::embed_index_with_options(
                &cli.db,
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
            serde_json::to_string_pretty(&sonar::embed_usage(&cli.db, &model)?)?
        }
        Command::Brief { query, options } => {
            let engine = SearchEngine::open(&cli.db, options.retrieval, options.model.as_deref())?
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
            let engine = SearchEngine::open(&cli.db, options.retrieval, options.model.as_deref())?
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
            let engine = SearchEngine::open(&cli.db, options.retrieval, options.model.as_deref())?
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
