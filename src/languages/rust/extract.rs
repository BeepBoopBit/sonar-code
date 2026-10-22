use crate::extract::{ParsedFile, ParsedLocation, Region, regions};
use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

pub(crate) fn parse(source: &str, chunking: crate::Chunking) -> Result<ParsedFile> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(source, None)
        .context("Rust parser did not produce a tree")?;
    let mut locations = vec![ParsedLocation {
        symbol: None,
        kind: "file",
        definition: None,
        parent: None,
        docs: String::new(),
        docs_start: 1,
        signature: String::new(),
        start_byte: 0,
        end_byte: source.len(),
        name_start: 0,
        name_end: 0,
        is_test: false,
        regions: regions(source, 1),
    }];
    walk(tree.root_node(), source, "", 0, &mut locations, chunking);
    Ok(ParsedFile {
        locations,
        has_errors: tree.root_node().has_error(),
    })
}

fn walk(
    node: Node<'_>,
    source: &str,
    scope: &str,
    parent: usize,
    locations: &mut Vec<ParsedLocation>,
    chunking: crate::Chunking,
) {
    let kind = match node.kind() {
        "function_item" | "function_signature_item" => Some("function"),
        "struct_item" => Some("struct"),
        "enum_item" => Some("enum"),
        "enum_variant" => Some("variant"),
        "trait_item" => Some("trait"),
        "mod_item" => Some("module"),
        "type_item" | "associated_type" => Some("type"),
        "const_item" => Some("constant"),
        "static_item" => Some("static"),
        "field_declaration" => Some("field"),
        "macro_definition" => Some("macro"),
        "impl_item" => Some("impl"),
        _ => None,
    };
    let mut child_scope = scope.to_owned();
    let mut child_parent = parent;
    if let Some(kind) = kind {
        let name = if kind == "impl" {
            node.child_by_field_name("type").map(|ty| {
                let ty = node_text(ty, source);
                node.child_by_field_name("trait")
                    .map(|tr| format!("<{ty} as {}>", node_text(tr, source)))
                    .unwrap_or_else(|| ty.to_owned())
            })
        } else {
            node.child_by_field_name("name")
                .map(|name| node_text(name, source).to_owned())
        };
        if let Some(name) = name {
            child_scope = if scope.is_empty() {
                name
            } else {
                format!("{scope}::{name}")
            };
            let symbol = if kind == "impl" {
                format!("impl {child_scope}")
            } else {
                child_scope.clone()
            };
            let start = node.start_position().row + 1;
            // Container bodies are indexed through their children, not as giant declarations.
            let body_end = if matches!(
                kind,
                "impl" | "module" | "trait" | "struct" | "enum" | "variant"
            ) {
                node.child_by_field_name("body")
                    .map(|body| body.start_byte())
                    .unwrap_or(node.end_byte())
            } else {
                node.end_byte()
            };
            child_parent = locations.len();
            let (docs, docs_start, test_attribute) = preceding_comments(node, source);
            let name_node = node.child_by_field_name("name").unwrap_or(node);
            let signature_end = node
                .child_by_field_name("body")
                .map_or(node.end_byte(), |body| body.start_byte());
            locations.push(ParsedLocation {
                symbol: Some(symbol),
                kind,
                definition: Some(start),
                parent: Some(parent),
                docs,
                docs_start,
                signature: source[node.start_byte()..signature_end].to_owned(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                name_start: name_node.start_byte(),
                name_end: name_node.end_byte(),
                is_test: test_attribute || locations[parent].is_test,
                regions: if chunking == crate::Chunking::Ast {
                    ast_regions(node, source, body_end)
                } else {
                    regions(&source[node.start_byte()..body_end], start)
                },
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(
            child,
            source,
            &child_scope,
            child_parent,
            locations,
            chunking,
        );
    }
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn preceding_comments(node: Node<'_>, source: &str) -> (String, usize, bool) {
    let mut comment_start = None;
    let mut start = node.start_position().row + 1;
    let mut is_test = false;
    let mut previous = node.prev_named_sibling();
    while let Some(item) = previous {
        match item.kind() {
            "line_comment" | "block_comment" => {
                comment_start = Some(item.start_byte());
                start = item.start_position().row + 1;
            }
            "attribute_item" => {
                let attribute = node_text(item, source).replace(' ', "");
                is_test |= attribute == "#[test]"
                    || attribute.contains("::test")
                    || attribute == "#[cfg(test)]";
            }
            _ => break,
        }
        previous = item.prev_named_sibling();
    }
    let docs = comment_start.map_or_else(String::new, |begin| {
        source[begin..node.start_byte()].trim_end().to_owned()
    });
    (docs, start, is_test)
}

fn ast_regions(node: Node<'_>, source: &str, end: usize) -> Vec<Region> {
    fn fits(s: &str) -> bool {
        s.chars().filter(|c| !c.is_whitespace()).count() <= 768
            && s.bytes().filter(|b| *b == b'\n').count() < 32
    }
    fn fallback(source: &str, begin: usize, end: usize, spans: &mut Vec<(usize, usize)>) {
        let mut start = begin;
        let (mut size, mut lines) = (0, 0);
        for (offset, ch) in source[begin..end].char_indices() {
            if size >= 768 || lines >= 32 {
                spans.push((start, begin + offset));
                start = begin + offset;
                size = 0;
                lines = 0;
            }
            size += usize::from(!ch.is_whitespace());
            lines += usize::from(ch == '\n');
        }
        if start < end {
            spans.push((start, end));
        }
    }
    fn split(
        node: Node<'_>,
        source: &str,
        begin: usize,
        end: usize,
        spans: &mut Vec<(usize, usize)>,
    ) {
        if begin >= end {
            return;
        }
        if fits(&source[begin..end]) {
            spans.push((begin, end));
            return;
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|c| c.start_byte() >= begin && c.end_byte() <= end)
            .collect();
        if children.is_empty() {
            fallback(source, begin, end, spans);
            return;
        }
        let mut previous = begin;
        for child in children {
            if previous < child.start_byte() {
                fallback(source, previous, child.start_byte(), spans);
            }
            split(child, source, child.start_byte(), child.end_byte(), spans);
            previous = child.end_byte();
        }
        if previous < end {
            fallback(source, previous, end, spans);
        }
    }
    let mut pieces = Vec::new();
    split(node, source, node.start_byte(), end, &mut pieces);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in pieces {
        if let Some(last) = merged
            .last_mut()
            .filter(|last| last.1 == start && fits(&source[last.0..end]))
        {
            last.1 = end;
        } else {
            merged.push((start, end));
        }
    }
    if merged.is_empty() {
        return regions(
            &source[node.start_byte()..end],
            node.start_position().row + 1,
        );
    }
    let newlines: Vec<_> = source
        .bytes()
        .enumerate()
        .filter(|(_, b)| *b == b'\n')
        .map(|(i, _)| i)
        .collect();
    merged
        .into_iter()
        .map(|(begin, end)| Region {
            start: newlines.partition_point(|i| *i < begin) + 1,
            end: newlines.partition_point(|i| *i < end.saturating_sub(1)) + 1,
            source: source[begin..end].to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ast_chunks_preserve_source_including_unicode_and_large_single_line_expressions() {
        let source = format!(
            "fn assemble() {{\n{}\nlet value=\"{}\";\n}}",
            "if ready { step(); }\n".repeat(70),
            "界".repeat(3000)
        );
        let parsed = parse(&source, crate::Chunking::Ast).unwrap();
        let function = &parsed.locations[1];
        assert!(function.regions.len() > 4);
        assert_eq!(
            function
                .regions
                .iter()
                .map(|r| r.source.as_str())
                .collect::<String>(),
            source
        );
        for region in &function.regions {
            assert!(region.source.chars().filter(|c| !c.is_whitespace()).count() <= 768);
            assert!(region.start <= region.end);
        }
    }
}
