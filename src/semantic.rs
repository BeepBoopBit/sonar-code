//! Dispatch analysis through language adapters; the legacy SCIP import is Rust's provider.
pub(crate) use crate::languages::rust::semantic::callee_name;
pub use crate::languages::rust::semantic::{SemanticManifest, SemanticReport, import_scip};
use anyhow::{Result, ensure};
use std::path::Path;

pub fn resolve(db: &Path, analyzer: &Path, config: Option<&Path>) -> Result<SemanticReport> {
    let conn = crate::store::open(db, false)?;
    let paths = crate::store::file_hashes(&conn)?;
    let provider = crate::languages::ADAPTERS.iter().find(|a| {
        a.id() == "rust"
            && a.capabilities().resolved_references
            && paths
                .keys()
                .any(|p| crate::languages::for_path(p).is_some_and(|lang| lang.id() == a.id()))
    });
    ensure!(
        provider.is_some(),
        "no resolved-analysis provider for the requested Rust languages; use resolve --typescript <compiler> for TypeScript"
    );
    provider.unwrap().resolve(db, analyzer, config)
}

pub(crate) fn profile(conn: &rusqlite::Connection) -> Result<Option<String>> {
    let rust = crate::store::metadata(conn, "semantic_profile")?;
    let typescript = crate::store::metadata(conn, "typescript_semantic_profile")?;
    Ok(match (rust, typescript) {
        (Some(rust), Some(typescript)) => Some(
            blake3::hash(format!("{rust}:{typescript}").as_bytes())
                .to_hex()
                .to_string(),
        ),
        (rust, typescript) => rust.or(typescript),
    })
}

pub(crate) fn setup(states: &[crate::LanguageState]) -> &'static str {
    let rust = states.iter().any(|s| s.language == "rust" && s.files > 0);
    let typescript = states
        .iter()
        .any(|s| s.language == "typescript" && s.files > 0);
    match (rust, typescript) {
        (true, true) => {
            "Run `sonar resolve` for Rust and `sonar resolve --typescript <compiler module>` for TypeScript after indexing."
        }
        (_, true) => {
            "Run `sonar resolve --typescript <compiler module>` after indexing for compiler-resolved TypeScript calls and references."
        }
        _ => "Run `sonar resolve` after indexing for compiler-resolved calls and references.",
    }
}

/// Read-only TEMP view prevents one current provider from exposing another's stale edges.
pub(crate) fn prepare(conn: &rusqlite::Connection, states: &[crate::LanguageState]) -> Result<()> {
    let mut clauses = Vec::new();
    for state in states.iter().filter(|s| s.semantic_freshness == "current") {
        if let Some(adapter) = crate::languages::ADAPTERS
            .iter()
            .find(|a| a.id() == state.language)
        {
            for ext in adapter.extensions() {
                clauses.push(format!("f.path GLOB '*.{ext}'"));
            }
        }
    }
    let filter = if clauses.is_empty() {
        "0".into()
    } else {
        clauses.join(" OR ")
    };
    conn.execute_batch(&format!("CREATE TEMP VIEW semantic_edges AS SELECT e.* FROM main.semantic_edges e JOIN locations l ON l.id=e.from_id JOIN files f ON f.id=l.file_id WHERE {filter}"))?;
    Ok(())
}

pub(crate) fn current(
    conn: &rusqlite::Connection,
    root: &Path,
    snapshot: &str,
    fresh: bool,
) -> Result<bool> {
    // Legacy Rust-only structure/usage extensions require their own provider.
    // Shared navigation uses per-language states and the filtered graph view.
    crate::languages::rust::semantic::current(conn, root, snapshot, fresh)
}
