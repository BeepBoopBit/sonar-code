//! TypeScript/TSX structural navigation, without executing code or guessing references.
use super::{LanguageAdapter, LanguageCapabilities};
use crate::extract::ParsedFile;

mod extract;
pub(crate) mod semantic;
pub(crate) mod syntax;
use anyhow::Result;
use tree_sitter::Node;

pub(crate) struct TypeScript;

impl LanguageAdapter for TypeScript {
    fn id(&self) -> &'static str {
        "typescript"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "mts", "cts"]
    }
    fn grammar(&self, path: &str) -> tree_sitter::Language {
        if path.ends_with(".tsx") {
            tree_sitter_typescript::LANGUAGE_TSX.into()
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
    }
    fn capabilities(&self) -> LanguageCapabilities {
        LanguageCapabilities {
            declarations: true,
            syntax_focus: true,
            contextual_focus: false,
            resolved_references: true,
            field_uses: true,
            source_navigation: false,
        }
    }
    fn extract(&self, path: &str, source: &str, _chunking: crate::Chunking) -> Result<ParsedFile> {
        extract::parse(path, source)
    }
    fn focus_bonus(&self, kind: &str) -> Option<f64> {
        Some(match kind {
            "if_statement" | "switch_case" | "switch_default" | "for_statement"
            | "for_in_statement" | "while_statement" | "do_statement" | "catch_clause"
            | "try_statement" => 0.6,
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition" => 0.25,
            "lexical_declaration"
            | "variable_declaration"
            | "expression_statement"
            | "return_statement"
            | "throw_statement" => 0.15,
            "call_expression"
            | "new_expression"
            | "await_expression"
            | "jsx_element"
            | "jsx_self_closing_element" => 0.0,
            _ => return None,
        })
    }
    fn anchor_priority(&self, kind: &str) -> Option<usize> {
        Some(match kind {
            "array" => 0,
            "if_statement" | "switch_case" | "switch_default" | "for_statement"
            | "for_in_statement" | "while_statement" | "do_statement" | "catch_clause" => 1,
            "lexical_declaration"
            | "variable_declaration"
            | "expression_statement"
            | "return_statement"
            | "throw_statement"
            | "public_field_definition"
            | "property_signature" => 2,
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "class_declaration"
            | "interface_declaration"
            | "type_alias_declaration" => 3,
            _ => return None,
        })
    }
    fn is_function(&self, kind: &str) -> bool {
        callable(kind)
            || matches!(
                kind,
                "function_declaration" | "generator_function_declaration" | "method_definition"
            )
    }
    fn is_comment(&self, kind: &str) -> bool {
        kind == "comment"
    }
    fn is_statement(&self, kind: &str) -> bool {
        matches!(
            kind,
            "lexical_declaration"
                | "variable_declaration"
                | "expression_statement"
                | "return_statement"
                | "throw_statement"
        )
    }
    fn is_call(&self, kind: &str) -> bool {
        matches!(kind, "call_expression" | "new_expression")
    }
    fn standalone_call(&self, node: Node<'_>) -> bool {
        !self.is_call(node.kind())
            || node.parent().is_none_or(|p| {
                matches!(
                    p.kind(),
                    "expression_statement"
                        | "return_statement"
                        | "throw_statement"
                        | "await_expression"
                        | "variable_declarator"
                )
            })
    }
    fn focus_root<'a>(&self, node: Node<'a>) -> Node<'a> {
        let declaration = unwrapped_declaration(node);
        let declaration = if matches!(
            declaration.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            let mut cursor = declaration.walk();
            let mut bindings = declaration
                .named_children(&mut cursor)
                .filter(|n| n.kind() == "variable_declarator");
            match (bindings.next(), bindings.next()) {
                (Some(binding), None) => binding,
                _ => declaration,
            }
        } else {
            declaration
        };
        declaration
            .child_by_field_name("value")
            .map(unwrapped_value)
            .filter(|n| callable(n.kind()))
            .unwrap_or(declaration)
    }
    fn semantic_profile(&self, conn: &rusqlite::Connection) -> Result<Option<String>> {
        crate::store::metadata(conn, "typescript_semantic_profile")
    }
    fn semantic_current(
        &self,
        conn: &rusqlite::Connection,
        _root: &std::path::Path,
        snapshot: &str,
        fresh: bool,
    ) -> Result<bool> {
        semantic::current(conn, snapshot, fresh)
    }
}

fn test_file(path: &str) -> bool {
    path.split('/')
        .any(|p| matches!(p, "test" | "tests" | "__tests__"))
        || std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.ends_with(".test") || s.ends_with(".spec"))
}

fn callable(kind: &str) -> bool {
    matches!(
        kind,
        "arrow_function" | "function_expression" | "generator_function"
    )
}

fn unwrapped_value(mut node: Node<'_>) -> Node<'_> {
    while matches!(
        node.kind(),
        "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression"
            | "type_assertion"
    ) {
        // A type assertion places the type before its expression.
        let child = if node.kind() == "type_assertion" {
            node.named_child(node.named_child_count().saturating_sub(1))
        } else {
            node.named_child(0)
        };
        let Some(child) = child else { break };
        node = child;
    }
    node
}

fn unwrapped_declaration(mut node: Node<'_>) -> Node<'_> {
    while matches!(node.kind(), "export_statement" | "ambient_declaration") {
        let child = node
            .child_by_field_name("declaration")
            .or_else(|| node.named_child(0));
        let Some(child) = child else { break };
        node = child;
    }
    node
}
