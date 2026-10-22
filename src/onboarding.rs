//! Project initialization and AI client setup.
pub use crate::project::Client;
use crate::{Chunking, ResourcePolicy, project, store};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Configuration {
    pub version: u32,
    pub root: PathBuf,
    pub db: PathBuf,
    pub chunking: Chunking,
    pub resources: ResourcePolicy,
    pub model: Option<PathBuf>,
    pub rust: bool,
    pub typescript: Option<PathBuf>,
}

fn saved(root: &Path) -> Result<Option<Configuration>> {
    let path = root.join(".sonar/project.json");
    project::read_text(&path)?
        .map(|text| {
            let config: Configuration = serde_json::from_str(&text)
                .with_context(|| format!("Invalid configuration in {}", path.display()))?;
            ensure!(
                config.version == 1,
                "Unsupported Sonar project configuration version"
            );
            ensure!(config.root == root,
                "Project settings belong to {}; this project was moved or copied. Relocate or recreate the index and update project.json before initialization.", config.root.display());
            Ok(config)
        })
        .transpose()
}

pub fn indexed_root(db: &Path) -> Result<Option<PathBuf>> {
    if !db.is_file() {
        return Ok(None);
    }
    let connection = store::open(db, false)?;
    Ok(store::metadata(&connection, "root")?
        .filter(|root| !root.is_empty())
        .map(PathBuf::from))
}

pub fn configuration(root: Option<&Path>, db: Option<&Path>) -> Result<Configuration> {
    let cwd = std::env::current_dir()?;
    let initial = match root {
        Some(root) => root
            .canonicalize()
            .with_context(|| format!("Repository does not exist: {}", root.display()))?,
        None => project::discover(&cwd, ".sonar").unwrap_or(cwd),
    };
    let mut previous = saved(&initial)?;
    let db = project::absolute(
        &db.map(Path::to_owned)
            .or_else(|| previous.as_ref().map(|config| config.db.clone()))
            .unwrap_or_else(|| initial.join(".sonar/index.sqlite")),
    )?;
    let root = if root.is_none() {
        indexed_root(&db)?.unwrap_or(initial)
    } else {
        initial
    };
    if previous.as_ref().is_none_or(|config| config.root != root) {
        previous = saved(&root)?;
    }
    Ok(previous
        .map(|mut config| {
            config.root = root.clone();
            config.db = db.clone();
            config
        })
        .unwrap_or(Configuration {
            version: 1,
            root,
            db,
            chunking: Chunking::default(),
            resources: ResourcePolicy::default(),
            model: None,
            rust: false,
            typescript: None,
        }))
}

pub fn initialize(config: &Configuration) -> Result<Value> {
    ensure!(
        config.root.is_dir(),
        "Repository directory is unavailable: {}",
        config.root.display()
    );
    project::validate_state_directory(&config.root.join(".sonar"))?;
    if let Some(root) = indexed_root(&config.db)? {
        ensure!(
            root == config.root,
            "Index belongs to {}; choose a separate --db for {}",
            root.display(),
            config.root.display()
        );
    }
    project::ignore_state(&config.root, ".sonar")?;
    project::ignore_database(&config.root, &config.db)?;
    eprintln!("Sonar: indexing {}", config.root.display());
    let index =
        crate::index_with_resources(&config.root, &config.db, config.chunking, config.resources)?;
    let navigation = crate::index_navigation(&config.db)?;
    if config.rust {
        eprintln!("Sonar: resolving Rust relationships");
        crate::resolve(&config.db, Path::new("rust-analyzer"), None)?;
    }
    if let Some(compiler) = &config.typescript {
        eprintln!("Sonar: resolving TypeScript relationships");
        crate::resolve_typescript(&config.db, Path::new("node"), Some(compiler), None)?;
    }
    if let Some(model) = &config.model {
        eprintln!("Sonar: preparing embeddings");
        crate::embed_index(&config.db, model)?;
    }
    let configuration = config.root.join(".sonar/project.json");
    project::write_atomic(
        &configuration,
        &format!("{}\n", serde_json::to_string_pretty(config)?),
    )?;
    Ok(
        json!({"initialized":true,"state":"ready","root":config.root,"database":config.db,
        "configuration":configuration,"index":index,"navigation":navigation,
        "next":"sonar investigate \"Where are incoming requests authenticated?\""}),
    )
}

pub fn uninitialized(root: &Path, db: &Path) -> Value {
    json!({"initialized":false,"state":"not_initialized","root":root,"database":db,
        "next":"Run sonar init, sonar setup codex, or call sonar_init through MCP."})
}

pub fn require_initialized(db: &Path) -> Result<()> {
    ensure!(
        indexed_root(db)?.is_some(),
        "not_initialized: No initialized Sonar index at {}. Run 'sonar init' or 'sonar setup codex' first.",
        db.display()
    );
    Ok(())
}

pub fn server_args(config: &Configuration) -> Vec<String> {
    let mut args = vec![
        "--db".into(),
        config.db.to_string_lossy().into(),
        "mcp".into(),
        "--root".into(),
        config.root.to_string_lossy().into(),
    ];
    if let Some(model) = &config.model {
        args.extend(["--model".into(), model.to_string_lossy().into()]);
    }
    args
}

pub fn setup(
    config: &Configuration,
    client: Client,
    global: bool,
    name: Option<&str>,
    replace: bool,
) -> Result<Value> {
    let args = server_args(config);
    let instructions = "Use Sonar to find relevant code before broad file reads.\nCheck sonar_status for the repository root and freshness. If not initialized, call sonar_init.\nUse sonar_investigate or sonar_brief, then read returned source ranges with your file tools.\nUse full handles for follow-up navigation, not short response IDs. Inspect notices and language coverage.\nAfter source changes, refresh with sonar init from the repository root and obtain fresh handles.\nIf MCP is unavailable, use the Sonar CLI from this project (sonar --help).";
    let registration = project::Registration::prepare(
        "sonar",
        &config.root,
        client,
        global,
        name,
        replace,
        &args,
        instructions,
    )?;
    let prepared = registration.complete(|| {
        let initialized = initialize(config)?;
        eprintln!("Sonar: verifying MCP status");
        project::probe(&args, "sonar_status", &config.root)?;
        Ok(initialized)
    })?;
    Ok(
        json!({"initialized":true,"root":config.root,"client":client,"server_name":registration.name,
        "scope":if global {"user"} else {"project"},"configuration":registration.configuration,
        "instructions":registration.instructions,"mcp_verified":true,"initialization":prepared,
        "next":"Reload the client's MCP configuration or start a new session. Accept project trust if requested, then call sonar_status."}),
    )
}
