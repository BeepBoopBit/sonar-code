use super::{LanguageAdapter, LanguageCapabilities};
use anyhow::Result;
use tree_sitter::Node;

pub(crate) mod dependencies;
pub(crate) mod extract;
pub(crate) mod field_uses;
mod focus;
pub mod focus_facts;
pub(crate) mod inputs;
mod navigation;
pub(crate) mod quantity;
pub(crate) mod quantity_structure;
pub(crate) mod semantic;
pub(crate) mod structure;
pub(crate) mod value_use;

pub(crate) struct Rust;
impl LanguageAdapter for Rust {
    fn id(&self) -> &'static str {
        "rust"
    }
    fn dependency_provider(&self) -> Option<&dyn crate::analysis::provider::AnalysisProvider> {
        Some(&dependencies::RustDependencies)
    }
    fn resolve_baseline(
        &self,
        db: &std::path::Path,
        current: &rusqlite::Connection,
        scratch: &std::path::Path,
    ) -> Result<bool> {
        let config = crate::store::metadata(current, "semantic_manifest")?
            .map(|text| serde_json::from_str::<serde_json::Value>(&text))
            .transpose()?
            .and_then(|v| v.get("config").cloned())
            .unwrap_or_else(|| serde_json::json!({}));
        let path = scratch.join("rust-baseline-config.json");
        std::fs::write(&path, serde_json::to_vec(&config)?)?;
        let analyzer = crate::store::metadata(current, "rust_baseline_analyzer")?
            .unwrap_or_else(|| "rust-analyzer".into());
        semantic::resolve(db, std::path::Path::new(&analyzer), Some(&path))?;
        Ok(true)
    }
    fn baseline_cache_profile(
        &self,
        current: &rusqlite::Connection,
        source: &std::path::Path,
    ) -> Result<Option<super::BaselineCacheProfile>> {
        let mut config = crate::store::metadata(current, "semantic_manifest")?
            .map(|text| serde_json::from_str::<serde_json::Value>(&text))
            .transpose()?
            .and_then(|v| v.get("config").cloned())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(cargo) = config
            .get_mut("cargo")
            .and_then(serde_json::Value::as_object_mut)
        {
            cargo.remove("targetDir");
        }
        let mut paths = std::collections::BTreeSet::new();
        for parent in source.ancestors().skip(1) {
            for name in [".cargo/config", ".cargo/config.toml"] {
                let path = parent.join(name);
                if path.is_file() {
                    paths.insert(path);
                }
            }
        }
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|p| std::path::PathBuf::from(p).join(".cargo"))
            });
        if let Some(home) = cargo_home {
            for name in ["config", "config.toml"] {
                let path = home.join(name);
                if path.is_file() {
                    paths.insert(path);
                }
            }
        }
        let external_configs = paths
            .into_iter()
            .map(|path| Ok((path.clone(), crate::semantic::inputs::hash(&path)?)))
            .collect::<Result<Vec<_>>>()?;
        let analyzer = crate::store::metadata(current, "rust_baseline_analyzer")?
            .unwrap_or_else(|| "rust-analyzer".into());
        Ok(Some(super::BaselineCacheProfile {
            settings: serde_json::json!({"config":config,"external_configs":external_configs}),
            tools: vec![
                (analyzer.into(), vec!["--version".into()]),
                ("rustc".into(), vec!["-vV".into()]),
                ("cargo".into(), vec!["--version".into()]),
            ],
        }))
    }
    fn baseline_cache_dependencies(&self, db: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
        let conn = crate::store::open(db, false)?;
        let root = crate::store::metadata(&conn, "root")?.unwrap();
        let output = std::process::Command::new("cargo")
            .args([
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--offline",
                "--all-features",
            ])
            .current_dir(&root)
            .env("CARGO_NET_OFFLINE", "true")
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "cannot validate baseline Cargo dependencies: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let root = std::path::Path::new(&root).canonicalize()?;
        let mut dependencies = std::collections::BTreeSet::new();
        for package in metadata["packages"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Cargo package metadata missing"))?
        {
            let manifest = std::path::Path::new(
                package["manifest_path"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Cargo manifest path missing"))?,
            );
            let directory = manifest.parent().unwrap().canonicalize()?;
            if !directory.starts_with(&root) {
                dependencies.insert(directory);
            }
        }
        Ok(dependencies.into_iter().collect())
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }
    fn grammar(&self, _path: &str) -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
    }
    fn capabilities(&self) -> LanguageCapabilities {
        LanguageCapabilities {
            declarations: true,
            syntax_focus: true,
            contextual_focus: true,
            resolved_references: true,
            field_uses: true,
            source_navigation: true,
        }
    }
    fn extract(
        &self,
        _path: &str,
        source: &str,
        chunking: crate::Chunking,
    ) -> Result<crate::extract::ParsedFile> {
        extract::parse(source, chunking)
    }
    fn focus_bonus(&self, kind: &str) -> Option<f64> {
        Some(match kind {
            "if_expression" | "match_expression" | "match_arm" | "for_expression"
            | "while_expression" | "loop_expression" => 0.6,
            "function_item" | "closure_expression" => 0.25,
            "let_declaration" | "expression_statement" | "return_expression" => 0.15,
            "macro_invocation" | "call_expression" => 0.0,
            _ => return None,
        })
    }
    fn anchor_priority(&self, kind: &str) -> Option<usize> {
        Some(match kind {
            "tuple_expression" => 0,
            "if_expression" | "match_arm" | "for_expression" | "while_expression"
            | "loop_expression" => 1,
            "let_declaration"
            | "expression_statement"
            | "field_declaration"
            | "return_expression" => 2,
            "function_item" | "struct_item" | "const_item" | "static_item" => 3,
            _ => return None,
        })
    }
    fn is_function(&self, kind: &str) -> bool {
        kind == "function_item"
    }
    fn is_comment(&self, kind: &str) -> bool {
        matches!(kind, "line_comment" | "block_comment")
    }
    fn is_statement(&self, kind: &str) -> bool {
        matches!(kind, "let_declaration" | "expression_statement")
    }
    fn is_call(&self, kind: &str) -> bool {
        kind == "call_expression"
    }
    fn standalone_call(&self, node: Node<'_>) -> bool {
        !self.is_call(node.kind())
            || node.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    "block"
                        | "match_arm"
                        | "expression_statement"
                        | "return_expression"
                        | "let_declaration"
                )
            })
    }
    fn contextual_focus(
        &self,
        resolver: &mut crate::focus::Resolver<'_>,
        conn: &rusqlite::Connection,
        candidate: &crate::candidate::Candidate,
        query: crate::focus::ContextualQuery<'_>,
        site: Option<usize>,
        diagnostic: &mut crate::FocusDiagnostic,
    ) -> Result<()> {
        resolver.rust_contextual(conn, candidate, query, site, diagnostic)
    }
    fn navigation(
        &self,
        builder: &mut crate::navigation::Builder<'_>,
        file: &crate::workspace::SourceFile,
    ) -> Result<()> {
        navigation::build(builder, file)
    }
    fn semantic_profile(&self, conn: &rusqlite::Connection) -> Result<Option<String>> {
        crate::store::metadata(conn, "semantic_profile")
    }
    fn semantic_current(
        &self,
        conn: &rusqlite::Connection,
        root: &std::path::Path,
        snapshot: &str,
        fresh: bool,
    ) -> Result<bool> {
        semantic::current(conn, root, snapshot, fresh)
    }
    fn resolve(
        &self,
        db: &std::path::Path,
        analyzer: &std::path::Path,
        config: Option<&std::path::Path>,
    ) -> Result<crate::SemanticReport> {
        semantic::resolve(db, analyzer, config)
    }
}
