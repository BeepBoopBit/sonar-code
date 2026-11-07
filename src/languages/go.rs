//! Go structural navigation. Package loading and resolved references are separate capabilities.
use super::{LanguageAdapter, LanguageCapabilities};
use crate::extract::ParsedFile;
use anyhow::Result;
use tree_sitter::Node;

mod extract;
pub(crate) mod semantic;

pub(crate) struct Go;

impl LanguageAdapter for Go {
    fn id(&self) -> &'static str {
        "go"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["go"]
    }
    fn grammar(&self, _path: &str) -> tree_sitter::Language {
        tree_sitter_go::LANGUAGE.into()
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
            "if_statement"
            | "for_statement"
            | "expression_switch_statement"
            | "type_switch_statement"
            | "expression_case"
            | "type_case"
            | "select_statement"
            | "communication_case"
            | "default_case" => 0.6,
            "function_declaration" | "method_declaration" | "func_literal" => 0.25,
            kind if statement(kind) => 0.15,
            "call_expression" => 0.0,
            _ => return None,
        })
    }
    fn anchor_priority(&self, kind: &str) -> Option<usize> {
        Some(match kind {
            "literal_element" => 0,
            "if_statement" | "for_statement" | "expression_case" | "type_case"
            | "communication_case" | "default_case" => 1,
            "field_declaration" => 2,
            kind if statement(kind) => 2,
            "function_declaration"
            | "method_declaration"
            | "type_spec"
            | "type_alias"
            | "const_spec"
            | "var_spec" => 3,
            _ => return None,
        })
    }
    fn is_function(&self, kind: &str) -> bool {
        matches!(kind, "function_declaration" | "method_declaration")
    }
    fn is_comment(&self, kind: &str) -> bool {
        kind == "comment"
    }
    fn is_statement(&self, kind: &str) -> bool {
        statement(kind)
    }
    fn is_call(&self, kind: &str) -> bool {
        kind == "call_expression"
    }
    fn standalone_call(&self, node: Node<'_>) -> bool {
        !self.is_call(node.kind())
            || node.parent().is_some_and(|p| {
                matches!(
                    p.kind(),
                    "expression_statement"
                        | "return_statement"
                        | "go_statement"
                        | "defer_statement"
                )
            })
    }
    fn resolve_baseline(
        &self,
        db: &std::path::Path,
        current: &rusqlite::Connection,
        _scratch: &std::path::Path,
    ) -> Result<bool> {
        let mut options = crate::store::metadata(current, "go_semantic_manifest")?
            .map(|text| serde_json::from_str::<serde_json::Value>(&text))
            .transpose()?
            .and_then(|v| v.get("options").cloned())
            .map(serde_json::from_value::<semantic::GoOptions>)
            .transpose()?
            .unwrap_or_default();
        if let Some(command) = crate::store::metadata(current, "go_baseline_command")? {
            options.command = command.into();
        }
        semantic::resolve(db, &options)?;
        Ok(true)
    }
    fn baseline_cache_profile(
        &self,
        current: &rusqlite::Connection,
        _source: &std::path::Path,
    ) -> Result<Option<super::BaselineCacheProfile>> {
        let options = crate::store::metadata(current, "go_semantic_manifest")?
            .map(|text| serde_json::from_str::<serde_json::Value>(&text))
            .transpose()?
            .and_then(|v| v.get("options").cloned())
            .unwrap_or_else(|| serde_json::json!({}));
        let command = crate::store::metadata(current, "go_baseline_command")?
            .or_else(|| options["command"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "go".into());
        Ok(Some(super::BaselineCacheProfile {
            settings: options,
            tools: vec![
                (command.clone().into(), vec!["version".into()]),
                (
                    command.into(),
                    [
                        "env",
                        "-json",
                        "GOOS",
                        "GOARCH",
                        "CGO_ENABLED",
                        "GOFLAGS",
                        "GOEXPERIMENT",
                        "GOTOOLCHAIN",
                        "GOMODCACHE",
                        "GOPATH",
                        "GOROOT",
                        "GOWORK",
                        "GOENV",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                ),
            ],
        }))
    }
    fn semantic_profile(&self, conn: &rusqlite::Connection) -> Result<Option<String>> {
        crate::store::metadata(conn, "go_semantic_profile")
    }
    fn coverage_notice(&self, conn: &rusqlite::Connection) -> Result<Option<crate::Notice>> {
        semantic::coverage_notice(conn)
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

fn statement(kind: &str) -> bool {
    matches!(
        kind,
        "short_var_declaration"
            | "var_declaration"
            | "const_declaration"
            | "assignment_statement"
            | "expression_statement"
            | "return_statement"
            | "go_statement"
            | "defer_statement"
            | "send_statement"
            | "receive_statement"
            | "inc_statement"
            | "dec_statement"
            | "break_statement"
            | "continue_statement"
            | "fallthrough_statement"
    )
}
