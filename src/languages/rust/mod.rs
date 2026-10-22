use super::{LanguageAdapter, LanguageCapabilities};
use anyhow::Result;
use tree_sitter::Node;

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
