use anyhow::Result;
use tree_sitter::Node;

use crate::extract::{ParsedFile, ParsedLocation, regions};

pub(super) fn parse(path: &str, source: &str) -> Result<ParsedFile> {
    let tree = crate::languages::parse_tree(path, source)?;
    let mut parsed = crate::extract::resource(source);
    parsed.has_errors = tree.root_node().has_error();
    parsed.locations[0].is_test = path.ends_with("_test.go");
    let mut collector = Collector {
        source,
        locations: &mut parsed.locations,
    };
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        match node.kind() {
            "package_clause" => {
                let (docs, start) = collector.comments(node);
                collector.locations[0].docs = docs;
                collector.locations[0].docs_start = start;
                collector.locations[0].signature = source[node.byte_range()].to_owned();
            }
            "function_declaration" | "method_declaration" => collector.function(node),
            "type_declaration" | "const_declaration" | "var_declaration" => {
                collector.declarations(node, node);
            }
            _ => {}
        }
    }
    Ok(parsed)
}

struct Collector<'a> {
    source: &'a str,
    locations: &'a mut Vec<ParsedLocation>,
}

impl Collector<'_> {
    /// Only adjacent leading comments are documentation; trailing and detached comments are not.
    fn comments(&self, node: Node<'_>) -> (String, usize) {
        let mut begin = node.start_byte();
        let mut line = node.start_position().row + 1;
        let mut previous = node.prev_named_sibling();
        while let Some(comment) = previous.filter(|n| n.kind() == "comment") {
            let gap = &self.source[comment.end_byte()..begin];
            let line_start = self.source[..comment.start_byte()]
                .rfind('\n')
                .map_or(0, |at| at + 1);
            if !gap.trim().is_empty()
                || gap.bytes().filter(|b| *b == b'\n').count() > 1
                || !self.source[line_start..comment.start_byte()]
                    .trim()
                    .is_empty()
            {
                break;
            }
            begin = comment.start_byte();
            line = comment.start_position().row + 1;
            previous = comment.prev_named_sibling();
        }
        (
            self.source[begin..node.start_byte()].trim_end().to_owned(),
            line,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        node: Node<'_>,
        name: Node<'_>,
        symbol: String,
        kind: &'static str,
        parent: usize,
        header_end: usize,
        container: bool,
        docs: (String, usize),
    ) -> usize {
        let index = self.locations.len();
        self.locations.push(ParsedLocation {
            symbol: Some(symbol),
            kind,
            definition: Some(node.start_position().row + 1),
            parent: Some(parent),
            docs: docs.0,
            docs_start: docs.1,
            signature: self.source[node.start_byte()..header_end]
                .trim_end()
                .to_owned(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            name_start: name.start_byte(),
            name_end: name.end_byte(),
            is_test: self.locations[parent].is_test,
            regions: regions(
                &self.source[node.start_byte()..if container {
                    header_end
                } else {
                    node.end_byte()
                }],
                node.start_position().row + 1,
            ),
        });
        index
    }

    fn function(&mut self, node: Node<'_>) {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let bare = &self.source[name.byte_range()];
        if bare.is_empty() || bare == "_" {
            return;
        }
        let symbol = if node.kind() == "method_declaration" {
            let receiver = node.child_by_field_name("receiver");
            let ty = receiver.and_then(|r| {
                let mut cursor = r.walk();
                r.named_children(&mut cursor)
                    .find(|p| p.kind() == "parameter_declaration")
                    .and_then(|p| p.child_by_field_name("type"))
            });
            let Some(receiver) = ty.and_then(type_name) else {
                return;
            };
            format!("{}.{bare}", &self.source[receiver.byte_range()])
        } else {
            bare.to_owned()
        };
        let header_end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        // A receiver qualifies a name; it is not syntactic containment in a type declaration.
        self.push(
            node,
            name,
            symbol,
            "function",
            0,
            header_end,
            false,
            self.comments(node),
        );
    }

    fn declarations(&mut self, node: Node<'_>, declaration: Node<'_>) {
        if matches!(
            node.kind(),
            "type_spec" | "type_alias" | "const_spec" | "var_spec"
        ) {
            let mut cursor = declaration.walk();
            // A single spec owns its keyword. Grouped declarations retain individual extents.
            let span = if declaration.children(&mut cursor).any(|n| n.kind() == "(") {
                node
            } else {
                declaration
            };
            let mut docs = self.comments(node);
            if docs.0.is_empty() {
                docs = self.comments(declaration);
            }
            match node.kind() {
                "type_spec" | "type_alias" => self.ty(node, span, docs),
                _ => {
                    let mut cursor = node.walk();
                    for name in node
                        .children_by_field_name("name", &mut cursor)
                        .filter(|n| n.is_named())
                    {
                        let symbol = self.source[name.byte_range()].to_owned();
                        if symbol != "_" {
                            self.push(
                                span,
                                name,
                                symbol,
                                if node.kind() == "const_spec" {
                                    "constant"
                                } else {
                                    "variable"
                                },
                                0,
                                span.end_byte(),
                                false,
                                docs.clone(),
                            );
                        }
                    }
                }
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if matches!(
                child.kind(),
                "type_spec" | "type_alias" | "const_spec" | "var_spec" | "var_spec_list"
            ) {
                self.declarations(child, declaration);
            }
        }
    }

    fn ty(&mut self, node: Node<'_>, span: Node<'_>, docs: (String, usize)) {
        let (Some(name), Some(ty)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("type"),
        ) else {
            return;
        };
        let symbol = self.source[name.byte_range()].to_owned();
        if symbol.is_empty() || symbol == "_" {
            return;
        }
        let kind = match (node.kind(), ty.kind()) {
            ("type_spec", "struct_type") => "struct",
            ("type_spec", "interface_type") => "interface",
            _ => "type",
        };
        let mut cursor = ty.walk();
        let body = ty
            .children(&mut cursor)
            .find(|n| matches!(n.kind(), "{" | "field_declaration_list"));
        let end = body.map_or(span.end_byte(), |b| b.start_byte());
        let parent = self.push(
            span,
            name,
            symbol.clone(),
            kind,
            0,
            end,
            body.is_some(),
            docs,
        );
        let members = if ty.kind() == "struct_type" {
            body
        } else if ty.kind() == "interface_type" {
            Some(ty)
        } else {
            None
        };
        if let Some(members) = members {
            let mut cursor = members.walk();
            for member in members.named_children(&mut cursor) {
                let mut cursor = member.walk();
                let mut names: Vec<_> = member
                    .children_by_field_name("name", &mut cursor)
                    .filter(|n| n.is_named())
                    .collect();
                let member_kind = match member.kind() {
                    "field_declaration" => {
                        if names.is_empty() {
                            names.extend(member.child_by_field_name("type").and_then(type_name));
                        }
                        "field"
                    }
                    "method_elem" => "function",
                    _ => continue,
                };
                for name in names {
                    let bare = &self.source[name.byte_range()];
                    if bare != "_" {
                        self.push(
                            member,
                            name,
                            format!("{symbol}.{bare}"),
                            member_kind,
                            parent,
                            member.end_byte(),
                            false,
                            self.comments(member),
                        );
                    }
                }
            }
        }
    }
}

/// Normalize pointer/generic receivers and embedded fields to their declared base name.
fn type_name(node: Node<'_>) -> Option<Node<'_>> {
    match node.kind() {
        "type_identifier" => Some(node),
        "generic_type" => type_name(node.child_by_field_name("type")?),
        "qualified_type" => node.child_by_field_name("name"),
        "pointer_type" | "parenthesized_type" => {
            let mut cursor = node.walk();
            type_name(
                node.named_children(&mut cursor)
                    .find(|n| n.kind() != "comment")?,
            )
        }
        _ => None,
    }
}
