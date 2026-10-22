//! Language-specific syntax and analysis feed the shared navigation model.
use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser, Tree};

pub(crate) mod python;
pub(crate) mod rust;
pub(crate) mod typescript;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageCapabilities {
    pub declarations: bool,
    pub syntax_focus: bool,
    pub contextual_focus: bool,
    pub resolved_references: bool,
    pub field_uses: bool,
    pub source_navigation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageState {
    pub language: String,
    /// Files in the persisted index, including any that are now stale or deleted.
    pub files: usize,
    /// Currently visible source files under the repository's ignore policy.
    #[serde(default)]
    pub detected_files: usize,
    /// Detected paths absent from the persisted index; queries never index them implicitly.
    #[serde(default)]
    pub unindexed_files: usize,
    pub syntax_freshness: String,
    pub semantic_freshness: String,
    pub semantic_profile: Option<String>,
    pub capabilities: LanguageCapabilities,
}

pub(crate) trait LanguageAdapter: Sync {
    fn id(&self) -> &'static str;
    fn extensions(&self) -> &'static [&'static str];
    fn grammar(&self, path: &str) -> tree_sitter::Language;
    fn capabilities(&self) -> LanguageCapabilities;
    fn extract(
        &self,
        path: &str,
        source: &str,
        chunking: crate::Chunking,
    ) -> Result<crate::extract::ParsedFile>;
    fn focus_bonus(&self, kind: &str) -> Option<f64>;
    fn anchor_priority(&self, kind: &str) -> Option<usize>;
    fn is_function(&self, kind: &str) -> bool;
    fn is_comment(&self, kind: &str) -> bool;
    fn is_statement(&self, kind: &str) -> bool;
    fn is_call(&self, kind: &str) -> bool;
    fn standalone_call(&self, node: Node<'_>) -> bool;
    /// Strip declaration wrappers when the indexed location owns a callable value.
    fn focus_root<'a>(&self, node: Node<'a>) -> Node<'a> {
        node
    }
    fn contextual_focus(
        &self,
        _resolver: &mut crate::focus::Resolver<'_>,
        _conn: &Connection,
        _candidate: &crate::candidate::Candidate,
        _query: crate::focus::ContextualQuery<'_>,
        _site: Option<usize>,
        _diagnostic: &mut crate::FocusDiagnostic,
    ) -> Result<()> {
        Ok(())
    }
    fn navigation(
        &self,
        _builder: &mut crate::navigation::Builder<'_>,
        _file: &crate::workspace::SourceFile,
    ) -> Result<()> {
        Ok(())
    }
    fn semantic_profile(&self, _conn: &Connection) -> Result<Option<String>> {
        Ok(None)
    }
    fn semantic_current(
        &self,
        _conn: &Connection,
        _root: &Path,
        _snapshot: &str,
        _fresh: bool,
    ) -> Result<bool> {
        Ok(false)
    }
    fn resolve(
        &self,
        _db: &Path,
        _analyzer: &Path,
        _config: Option<&Path>,
    ) -> Result<crate::SemanticReport> {
        anyhow::bail!("resolved analysis is not available for {}", self.id())
    }
}

static RUST: rust::Rust = rust::Rust;
static PYTHON: python::Python = python::Python;
static TYPESCRIPT: typescript::TypeScript = typescript::TypeScript;
pub(crate) static ADAPTERS: [&'static dyn LanguageAdapter; 3] = [&RUST, &PYTHON, &TYPESCRIPT];

pub(crate) fn for_path(path: &str) -> Option<&'static dyn LanguageAdapter> {
    let extension = Path::new(path).extension()?.to_str()?;
    ADAPTERS
        .iter()
        .copied()
        .find(|a| a.extensions().contains(&extension))
}

pub(crate) fn parse_tree(path: &str, source: &str) -> Result<Tree> {
    let adapter = for_path(path).context("no language adapter for source")?;
    let mut parser = Parser::new();
    parser.set_language(&adapter.grammar(path))?;
    parser
        .parse(source, None)
        .context("language parser returned no tree")
}

pub(crate) fn extract(
    path: &str,
    source: &str,
    chunking: crate::Chunking,
) -> Result<crate::extract::ParsedFile> {
    if let Some(adapter) = for_path(path) {
        adapter.extract(path, source, chunking)
    } else if path.ends_with(".md") {
        Ok(crate::extract::markdown(source))
    } else {
        Ok(crate::extract::resource(source))
    }
}

pub(crate) fn states(
    conn: &Connection,
    sources: &[crate::workspace::SourceFile],
    snapshot: &str,
    syntax_current: bool,
) -> Result<Vec<LanguageState>> {
    let indexed = crate::store::file_hashes(conn)?;
    let current: BTreeMap<_, _> = sources
        .iter()
        .map(|f| (f.path.as_str(), f.hash.as_str()))
        .collect();
    let root = crate::store::metadata(conn, "root")?.context("index root is missing")?;
    let uncovered = crate::workspace::uncovered_language_paths(conn, Path::new(&root))?;
    let detected: Vec<_> = sources
        .iter()
        .map(|s| s.path.as_str())
        .chain(uncovered.iter().map(String::as_str))
        .collect();
    let mut result = Vec::new();
    for adapter in ADAPTERS {
        let paths: Vec<_> = indexed
            .iter()
            .filter(|(p, _)| for_path(p).is_some_and(|a| a.id() == adapter.id()))
            .collect();
        let detected: Vec<_> = detected
            .iter()
            .copied()
            .filter(|p| for_path(p).is_some_and(|a| a.id() == adapter.id()))
            .collect();
        if paths.is_empty() && detected.is_empty() {
            continue;
        }
        let fresh = paths
            .iter()
            .all(|(p, h)| current.get(p.as_str()).copied() == Some(h.as_str()))
            && detected.iter().all(|p| indexed.contains_key(*p));
        let capabilities = adapter.capabilities();
        let semantic_current =
            adapter.semantic_current(conn, Path::new(&root), snapshot, syntax_current)?;
        let profile = adapter.semantic_profile(conn)?;
        result.push(LanguageState {
            language: adapter.id().into(),
            files: paths.len(),
            detected_files: detected.len(),
            unindexed_files: detected
                .iter()
                .filter(|p| !indexed.contains_key(**p))
                .count(),
            syntax_freshness: if paths.is_empty() {
                "unindexed"
            } else if fresh {
                "current"
            } else {
                "stale"
            }
            .into(),
            semantic_freshness: if !capabilities.resolved_references {
                "unsupported"
            } else if semantic_current {
                "current"
            } else if profile.is_some() {
                "stale"
            } else {
                "unavailable"
            }
            .into(),
            semantic_profile: profile,
            capabilities,
        });
    }
    Ok(result)
}

pub(crate) fn aggregate_semantic(
    states: &[LanguageState],
    current: bool,
    profile: bool,
) -> &'static str {
    if current {
        if states
            .iter()
            .any(|s| s.files > 0 && s.semantic_freshness != "current")
        {
            "partial"
        } else {
            "current"
        }
    } else if profile {
        "stale"
    } else {
        "unavailable"
    }
}

pub(crate) fn coverage_notices(states: &[LanguageState]) -> Vec<crate::Notice> {
    states.iter().filter(|s| s.unindexed_files > 0).map(|s| crate::Notice::new(
        if s.files == 0 { "unindexed_language" } else { "unindexed_sources" },
        format!("Detected {} {} source files absent from this index. Run `sonar index <repository>` with the same resource options to include them; queries do not index files automatically.", s.unindexed_files, s.language),
    )).collect()
}
