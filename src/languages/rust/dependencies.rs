//! Conservative syntax dependencies; no build configuration or name binding is inferred.
use crate::analysis::{
    AnalysisBatch, AnalysisCoverage, DependencyEvidence, DependencyFact, DependencyKind,
    UnresolvedDependency,
    provider::{AnalysisInput, AnalysisProvider},
};
use anyhow::Result;
use std::path::Path;
use tree_sitter::Node;

const PROVIDER: &str = "rust-dependencies-v2";
pub(crate) struct RustDependencies;

impl AnalysisProvider for RustDependencies {
    fn id(&self) -> &'static str {
        PROVIDER
    }
    fn resolve(
        &self,
        dependency: &UnresolvedDependency,
        bindings: &[DependencyFact],
    ) -> Option<DependencyFact> {
        if !matches!(
            dependency.kind,
            DependencyKind::Imports | DependencyKind::ReExports
        ) || dependency.reason != "name_resolution_required"
        {
            return None;
        }
        let targets: std::collections::BTreeSet<_> = bindings
            .iter()
            .filter(|b| b.from == dependency.from && b.evidence.bytes == dependency.evidence.bytes)
            .map(|b| b.to.as_str())
            .collect();
        if targets.len() != 1 {
            return None;
        }
        let binding = bindings.iter().find(|b| {
            b.from == dependency.from
                && b.evidence.path == dependency.evidence.path
                && b.evidence.bytes == dependency.evidence.bytes
        })?;
        Some(DependencyFact {
            from: dependency.from.clone(),
            to: (*targets.first()?).into(),
            kind: dependency.kind,
            evidence: binding.evidence.clone(),
        })
    }
    fn analyze(&self, input: &AnalysisInput<'_>) -> Result<AnalysisBatch> {
        let mut batch = AnalysisBatch {
            coverage: AnalysisCoverage {
                language: "rust".into(), provider: PROVIDER.into(), analyzed_files: vec![], skipped_files: vec![],
                limitations: vec![
                    "Module links describe source layout, not active cfg/build targets.".into(),
                    "Import leaves require exact current compiler bindings to indexed targets; globs and missing bindings remain unresolved.".into(),
                    "Macro-generated modules and dependencies are not expanded.".into(),
                ],
            }, relationships: vec![], unresolved: vec![],
        };
        for file in input
            .files
            .values()
            .filter(|f| crate::source_language(&f.path) == Some("rust"))
        {
            let tree = crate::languages::parse_tree(&file.path, &file.source)?;
            if tree.root_node().has_error() {
                batch.coverage.skipped_files.push(file.path.clone());
                continue;
            }
            batch.coverage.analyzed_files.push(file.path.clone());
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
            visit(
                tree.root_node(),
                &file.source,
                &file.path,
                &directory,
                input,
                &mut batch,
            );
        }
        Ok(batch)
    }
}

fn evidence(node: Node<'_>, path: &str) -> DependencyEvidence {
    DependencyEvidence {
        path: path.into(),
        line: node.start_position().row + 1,
        bytes: [node.start_byte(), node.end_byte()],
        provider: PROVIDER.into(),
    }
}

fn unresolved(
    batch: &mut AnalysisBatch,
    node: Node<'_>,
    path: &str,
    target: &str,
    kind: DependencyKind,
    reason: &str,
) {
    batch.unresolved.push(UnresolvedDependency {
        from: path.into(),
        target: target.into(),
        kind,
        reason: reason.into(),
        evidence: evidence(node, path),
    });
}

fn import_leaves(
    node: Node<'_>,
    path: &str,
    kind: DependencyKind,
    statement: &str,
    batch: &mut AnalysisBatch,
) {
    match node.kind() {
        "scoped_use_list" => {
            if let Some(list) = node.child_by_field_name("list") {
                import_leaves(list, path, kind, statement, batch);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if !matches!(child.kind(), "line_comment" | "block_comment") {
                    import_leaves(child, path, kind, statement, batch);
                }
            }
        }
        "use_as_clause" => {
            if let Some(original) = node.child_by_field_name("path") {
                import_leaves(original, path, kind, statement, batch);
            }
        }
        "scoped_identifier" => {
            if let Some(name) = node.child_by_field_name("name") {
                import_leaves(name, path, kind, statement, batch);
            }
        }
        "use_wildcard" => unresolved(
            batch,
            node,
            path,
            statement,
            kind,
            "glob_expansion_unavailable",
        ),
        _ => unresolved(
            batch,
            node,
            path,
            statement,
            kind,
            "name_resolution_required",
        ),
    }
}

fn visit(
    node: Node<'_>,
    source: &str,
    path: &str,
    directory: &Path,
    input: &AnalysisInput<'_>,
    batch: &mut AnalysisBatch,
) {
    if matches!(node.kind(), "macro_definition" | "macro_invocation") {
        return;
    }
    if node.kind() == "use_declaration" {
        let kind = if node.child_by_field_name("visibility").is_some() || {
            let mut c = node.walk();
            node.named_children(&mut c)
                .any(|n| n.kind() == "visibility_modifier")
        } {
            DependencyKind::ReExports
        } else {
            DependencyKind::Imports
        };
        if let Some(argument) = node.child_by_field_name("argument") {
            import_leaves(argument, path, kind, &source[node.byte_range()], batch);
        }
        return;
    }
    if node.kind() == "mod_item" {
        let Some(name) = node.child_by_field_name("name") else {
            return;
        };
        let name = source[name.byte_range()].trim_start_matches("r#");
        // Attribute-driven layout needs Rust's full attribute semantics. Keep it explicit.
        let mut sibling = node.prev_named_sibling();
        let mut custom = false;
        while let Some(prev) = sibling {
            match prev.kind() {
                "attribute_item" => custom |= source[prev.byte_range()].contains("path"),
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            sibling = prev.prev_named_sibling();
        }
        if custom {
            unresolved(
                batch,
                node,
                path,
                name,
                DependencyKind::DeclaresModule,
                "attribute_driven_module_layout_unsupported",
            );
            return;
        }
        if let Some(body) = node.child_by_field_name("body") {
            let next = directory.join(name);
            let mut c = body.walk();
            for child in body.named_children(&mut c) {
                visit(child, source, path, &next, input, batch);
            }
        } else {
            let candidates: Vec<_> = [format!("{name}.rs"), format!("{name}/mod.rs")]
                .iter()
                .filter_map(|n| crate::navigation::relative(directory, n))
                .filter(|p| input.files.contains_key(p.as_str()))
                .collect();
            if candidates.len() == 1 {
                batch.relationships.push(DependencyFact {
                    from: path.into(),
                    to: candidates[0].clone(),
                    kind: DependencyKind::DeclaresModule,
                    evidence: evidence(node, path),
                });
            } else {
                unresolved(
                    batch,
                    node,
                    path,
                    name,
                    DependencyKind::DeclaresModule,
                    if candidates.is_empty() {
                        "target_not_indexed"
                    } else {
                        "ambiguous_module_target"
                    },
                );
            }
        }
        return;
    }
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        visit(child, source, path, directory, input, batch);
    }
}
