//! Dispatch analysis through language adapters; the legacy SCIP import is Rust's provider.
pub(crate) mod inputs;
pub(crate) use crate::languages::rust::semantic::callee_name;
pub use crate::languages::rust::semantic::{SemanticManifest, SemanticReport, import_scip};
use anyhow::{Result, ensure};
use rusqlite::Connection;
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
        "no resolved-analysis provider for the requested Rust languages; use resolve --typescript <compiler> for TypeScript or resolve --go for Go"
    );
    provider.unwrap().resolve(db, analyzer, config)
}

pub(crate) fn profile(conn: &Connection) -> Result<Option<String>> {
    let mut profile: Option<String> = None;
    for adapter in crate::languages::ADAPTERS {
        if let Some(next) = adapter.semantic_profile(conn)? {
            profile = Some(match profile {
                Some(previous) => blake3::hash(format!("{previous}:{next}").as_bytes())
                    .to_hex()
                    .to_string(),
                None => next,
            });
        }
    }
    Ok(profile)
}

pub(crate) fn setup(states: &[crate::LanguageState]) -> String {
    let mut commands = Vec::new();
    for state in states.iter().filter(|s| s.files > 0) {
        match state.language.as_str() {
            "rust" => commands.push("`sonar resolve` for Rust"),
            "typescript" => {
                commands.push("`sonar resolve --typescript <compiler module>` for TypeScript")
            }
            "go" => commands.push("`sonar resolve --go` for Go"),
            _ => {}
        }
    }
    format!(
        "Run {} after indexing for compiler-resolved calls and references.",
        commands.join(" and ")
    )
}

pub(crate) fn provider_notice(
    conn: &Connection,
    language: Option<&str>,
) -> Result<Option<crate::Notice>> {
    match crate::languages::ADAPTERS
        .iter()
        .find(|a| Some(a.id()) == language)
    {
        Some(adapter) => adapter.coverage_notice(conn),
        None => Ok(None),
    }
}

pub(crate) fn coverage_notices(
    conn: &Connection,
    states: &[crate::LanguageState],
) -> Result<Vec<crate::Notice>> {
    let mut notices = Vec::new();
    for state in states.iter().filter(|s| s.semantic_freshness == "current") {
        if let Some(notice) = provider_notice(conn, Some(&state.language))? {
            notices.push(notice);
        }
    }
    Ok(notices)
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
