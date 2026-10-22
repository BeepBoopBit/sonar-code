//! Python structural navigation. No imports or calls are guessed from names.
use super::{LanguageAdapter, LanguageCapabilities};
use crate::extract::{ParsedFile, ParsedLocation, regions};
use anyhow::Result;
use tree_sitter::Node;

pub(crate) struct Python;
impl LanguageAdapter for Python {
    fn id(&self) -> &'static str {
        "python"
    }
    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }
    fn grammar(&self, _path: &str) -> tree_sitter::Language {
        tree_sitter_python::LANGUAGE.into()
    }
    fn capabilities(&self) -> LanguageCapabilities {
        LanguageCapabilities {
            declarations: true,
            syntax_focus: true,
            contextual_focus: false,
            resolved_references: false,
            field_uses: false,
            source_navigation: false,
        }
    }
    fn extract(&self, path: &str, source: &str, _chunking: crate::Chunking) -> Result<ParsedFile> {
        let tree = super::parse_tree(path, source)?;
        let test_file = path.split('/').any(|p| p == "tests")
            || std::path::Path::new(path)
                .file_stem()
                .and_then(|p| p.to_str())
                .is_some_and(|p| p.starts_with("test_") || p.ends_with("_test"));
        let mut parsed = crate::extract::resource(source);
        parsed.has_errors = tree.root_node().has_error();
        parsed.locations[0].is_test = test_file;
        let (docs, line) = docstring(tree.root_node(), source);
        parsed.locations[0].docs = docs;
        parsed.locations[0].docs_start = line;
        walk(tree.root_node(), source, "", 0, &mut parsed.locations);
        Ok(parsed)
    }
    fn focus_bonus(&self, kind: &str) -> Option<f64> {
        Some(match kind {
            "if_statement" | "elif_clause" | "else_clause" | "for_statement"
            | "while_statement" | "try_statement" | "except_clause" | "with_statement"
            | "case_clause" => 0.6,
            "function_definition" | "class_definition" => 0.25,
            "expression_statement"
            | "return_statement"
            | "raise_statement"
            | "assert_statement" => 0.15,
            "call" => 0.0,
            _ => return None,
        })
    }
    fn anchor_priority(&self, kind: &str) -> Option<usize> {
        Some(match kind {
            "tuple" | "list" | "dictionary" => 0,
            "if_statement" | "elif_clause" | "else_clause" | "for_statement"
            | "while_statement" | "except_clause" | "with_statement" | "case_clause" => 1,
            "expression_statement"
            | "return_statement"
            | "raise_statement"
            | "assert_statement" => 2,
            "function_definition" | "class_definition" => 3,
            _ => return None,
        })
    }
    fn is_function(&self, kind: &str) -> bool {
        kind == "function_definition"
    }
    fn is_comment(&self, kind: &str) -> bool {
        kind == "comment"
    }
    fn is_statement(&self, kind: &str) -> bool {
        matches!(
            kind,
            "expression_statement" | "return_statement" | "raise_statement" | "assert_statement"
        )
    }
    fn is_call(&self, kind: &str) -> bool {
        kind == "call"
    }
    fn standalone_call(&self, node: Node<'_>) -> bool {
        !self.is_call(node.kind())
            || node.parent().is_none_or(|p| {
                matches!(
                    p.kind(),
                    "expression_statement"
                        | "return_statement"
                        | "raise_statement"
                        | "assert_statement"
                )
            })
    }
}

fn docstring(body: Node<'_>, source: &str) -> (String, usize) {
    let mut cursor = body.walk();
    let Some(first) = body
        .named_children(&mut cursor)
        .find(|n| n.kind() != "comment")
    else {
        return (String::new(), 1);
    };
    if first.kind() == "expression_statement"
        && let Some(value) = first.named_child(0)
        && matches!(value.kind(), "string" | "concatenated_string")
    {
        let text = &source[value.byte_range()];
        // Retain exact source spelling/line count, but exclude bytes and f-strings.
        let prefix = text
            .split(['\'', '"'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !prefix.contains(['b', 'f']) {
            return (text.to_owned(), value.start_position().row + 1);
        }
    }
    (String::new(), 1)
}

fn walk(
    node: Node<'_>,
    source: &str,
    scope: &str,
    parent: usize,
    locations: &mut Vec<ParsedLocation>,
) {
    let declaration = if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition")
    } else {
        Some(node)
    };
    let Some(declaration) = declaration else {
        return;
    };
    let kind = match declaration.kind() {
        "function_definition" => Some("function"),
        "class_definition" => Some("class"),
        "assignment" if matches!(locations[parent].kind, "file" | "class") => declaration
            .child_by_field_name("left")
            .filter(|n| n.kind() == "identifier")
            .map(|n| {
                if locations[parent].kind == "class" {
                    "field"
                } else if source[n.byte_range()]
                    .chars()
                    .all(|c| !c.is_alphabetic() || c.is_uppercase())
                {
                    "constant"
                } else {
                    "variable"
                }
            }),
        _ => None,
    };
    if let Some(kind) = kind
        && let Some(name) = declaration.child_by_field_name(if declaration.kind() == "assignment" {
            "left"
        } else {
            "name"
        })
    {
        let bare = &source[name.byte_range()];
        let symbol = if scope.is_empty() {
            bare.to_owned()
        } else {
            format!("{scope}.{bare}")
        };
        let body = declaration.child_by_field_name("body");
        let signature_end = body.map_or(declaration.end_byte(), |b| b.start_byte());
        let (docs, docs_start) = body.map_or((String::new(), 1), |b| docstring(b, source));
        let index = locations.len();
        let inherited = locations[parent].is_test;
        locations.push(ParsedLocation {
            symbol: Some(symbol.clone()),
            kind,
            definition: Some(declaration.start_position().row + 1),
            parent: Some(parent),
            docs,
            docs_start,
            signature: source[node.start_byte()..signature_end]
                .trim_end()
                .to_owned(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            name_start: name.start_byte(),
            name_end: name.end_byte(),
            is_test: inherited || (kind == "function" && bare.starts_with("test_")),
            regions: regions(
                &source[node.start_byte()..if kind == "class" {
                    signature_end
                } else {
                    node.end_byte()
                }],
                node.start_position().row + 1,
            ),
        });
        if let Some(body) = body {
            let mut cursor = body.walk();
            for child in body.named_children(&mut cursor) {
                walk(child, source, &symbol, index, locations);
            }
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, source, scope, parent, locations);
    }
}
