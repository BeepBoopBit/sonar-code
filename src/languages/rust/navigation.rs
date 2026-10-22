use crate::navigation::{Builder, ModuleProof, relative};
use anyhow::Result;
use std::path::Path;
use tree_sitter::Node;

fn literal(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('"') {
        // Rust and JSON escape grammars differ. Escaped paths need a Rust
        // literal decoder; omit them rather than treating JSON as Rust.
        if value.contains('\\') {
            return None;
        }
        return serde_json::from_str::<String>(value).ok();
    }
    let raw = value.strip_prefix('r')?;
    let hashes = raw.chars().take_while(|c| *c == '#').count();
    let begin = hashes + 1;
    let end = raw.len().checked_sub(hashes + 1)?;
    if raw.as_bytes().get(hashes) != Some(&b'"')
        || begin > end
        || raw.as_bytes().get(end) != Some(&b'"')
        || !raw[end + 1..].chars().all(|c| c == '#')
    {
        return None;
    }
    Some(raw[begin..end].into())
}

fn attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut prev = node.prev_named_sibling();
    let mut result = Vec::new();
    while let Some(n) = prev {
        match n.kind() {
            "attribute_item" => result.push(source[n.byte_range()].into()),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        prev = n.prev_named_sibling();
    }
    result
}

impl Builder<'_> {
    fn rust(
        &mut self,
        node: Node<'_>,
        source: &str,
        path: &str,
        directory: &Path,
        inline: bool,
        gated: bool,
    ) -> Result<()> {
        let attrs = attributes(node, source);
        let gated = gated
            || attrs.iter().any(|a| {
                a.chars().filter(|c| !c.is_whitespace()).collect::<String>() == "#[cfg(test)]"
            });
        if node.kind() == "mod_item" {
            let name = node
                .child_by_field_name("name")
                .map(|n| source[n.byte_range()].trim_start_matches("r#"))
                .unwrap_or("");
            let path_attrs: Vec<_> = attrs.iter().filter(|a| a.contains("path")).collect();
            let custom = if path_attrs.is_empty() {
                None
            } else if path_attrs.len() == 1 {
                let a = path_attrs[0].trim();
                a.strip_prefix("#[path")
                    .and_then(|a| a.strip_suffix(']'))
                    .and_then(|a| a.trim().strip_prefix('='))
                    .and_then(literal)
            } else {
                None
            };
            if !path_attrs.is_empty() && custom.is_none() {
                self.report.unsupported_modules += 1;
                return Ok(());
            }
            let base = if custom.is_some() && !inline {
                Path::new(path).parent().unwrap_or(Path::new(""))
            } else {
                directory
            };
            if let Some(body) = node.child_by_field_name("body") {
                let next = if let Some(custom) = custom {
                    relative(base, &custom)
                } else {
                    relative(directory, name)
                };
                if let Some(next) = next {
                    let mut cursor = body.walk();
                    for child in body.named_children(&mut cursor) {
                        self.rust(child, source, path, Path::new(&next), true, gated)?;
                    }
                } else {
                    self.report.unsupported_modules += 1;
                }
                return Ok(());
            }
            let possibilities: Vec<_> = if let Some(custom) = custom {
                relative(base, &custom).into_iter().collect()
            } else {
                [format!("{name}.rs"), format!("{name}/mod.rs")]
                    .iter()
                    .filter_map(|n| relative(directory, n))
                    .collect()
            };
            let targets: Vec<_> = possibilities
                .into_iter()
                .filter(|p| p.ends_with(".rs") && self.files.contains_key(p.as_str()))
                .collect();
            if targets.len() == 1 {
                let target = targets[0].clone();
                self.modules.push(ModuleProof {
                    path: path.into(),
                    target: target.clone(),
                    line: node.start_position().row + 1,
                    bytes: [node.start_byte(), node.end_byte()],
                    test_gate: gated,
                });
                self.edge(
                    path,
                    [node.start_byte(), node.end_byte()],
                    &target,
                    "declares_module",
                    None,
                )?;
            } else {
                self.report.unsupported_modules += 1;
            }
            return Ok(());
        }
        if node.kind() == "macro_definition" {
            return Ok(());
        }
        if node.kind() == "macro_invocation" {
            let name = node
                .child_by_field_name("macro")
                .map(|n| &source[n.byte_range()])
                .unwrap_or("");
            if matches!(
                name,
                "include_str"
                    | "include_bytes"
                    | "std::include_str"
                    | "std::include_bytes"
                    | "core::include_str"
                    | "core::include_bytes"
            ) {
                let mut cursor = node.walk();
                if let Some(body) = node
                    .named_children(&mut cursor)
                    .find(|n| n.kind() == "token_tree")
                {
                    let text = &source[body.byte_range()];
                    if let Some(value) =
                        literal(text[1..text.len() - 1].trim().trim_end_matches(','))
                    {
                        if let Some(target) =
                            relative(Path::new(path).parent().unwrap_or(Path::new("")), &value)
                        {
                            self.edge(
                                path,
                                [body.start_byte(), body.end_byte()],
                                &target,
                                "embeds",
                                None,
                            )?;
                        } else {
                            self.report.unresolved_links += 1;
                        }
                    } else {
                        self.report.unresolved_links += 1;
                    }
                }
            }
            return Ok(());
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.rust(child, source, path, directory, inline, gated)?;
        }
        Ok(())
    }
}
pub(crate) fn build(builder: &mut Builder<'_>, file: &crate::workspace::SourceFile) -> Result<()> {
    let tree = crate::languages::parse_tree(&file.path, &file.source)?;
    if tree.root_node().has_error() {
        builder.report.skipped_parse_error_files += 1;
        return Ok(());
    }
    let path = Path::new(&file.path);
    let parent = path.parent().unwrap_or(Path::new(""));
    let directory = if matches!(
        path.file_name().and_then(|s| s.to_str()),
        Some("lib.rs" | "main.rs" | "mod.rs")
    ) {
        parent.to_path_buf()
    } else {
        parent.join(path.file_stem().unwrap())
    };
    builder.rust(
        tree.root_node(),
        &file.source,
        &file.path,
        &directory,
        false,
        false,
    )
}
