//! Indexed Python module layout, without executing imports or guessing symbol bindings.
use crate::analysis::{
    AnalysisBatch, AnalysisCoverage, DependencyEvidence, DependencyFact, DependencyKind,
    UnresolvedDependency,
    provider::{AnalysisInput, AnalysisProvider},
};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Tree};

const PROVIDER: &str = "python-dependencies-v1";
pub(crate) struct PythonDependencies;

struct Modules<'a> {
    input: &'a AnalysisInput<'a>,
    directories: BTreeSet<String>,
    trees: BTreeMap<String, Tree>,
}
struct Module {
    root: String,
    parts: Vec<String>,
    links: Vec<String>,
    package: bool,
}

impl AnalysisProvider for PythonDependencies {
    fn id(&self) -> &'static str {
        PROVIDER
    }
    fn analyze(&self, input: &AnalysisInput<'_>) -> Result<AnalysisBatch> {
        let mut modules = Modules {
            input,
            directories: BTreeSet::new(),
            trees: BTreeMap::new(),
        };
        for file in input
            .files
            .values()
            .filter(|f| crate::source_language(&f.path) == Some("python"))
        {
            let mut parent = file.path.as_str();
            while let Some((directory, _)) = parent.rsplit_once('/') {
                modules.directories.insert(directory.into());
                parent = directory;
            }
            modules.trees.insert(
                file.path.clone(),
                crate::languages::parse_tree(&file.path, &file.source)?,
            );
        }
        let mut batch = AnalysisBatch {
            coverage: AnalysisCoverage {
                language: "python".into(), provider: PROVIDER.into(), analyzed_files: vec![], skipped_files: vec![],
                limitations: vec![
                    "Syntax module-layout leads use the repository root and conventional src/ as candidate absolute import roots; Python path configuration is not evaluated.".into(),
                    "Regular-package relative imports are supported. Namespace packages, stub-only modules, ambiguous roots/layouts, and uncertain package members remain unresolved.".into(),
                    "Imports do not prove runtime execution or symbol binding. Conditions, sys.path/__path__ mutation, import hooks, built-in/frozen module precedence, and rebindings are not evaluated.".into(),
                    "Wildcard expansion and recognizable dynamic import calls remain unresolved; arbitrary aliases or computed loader calls may be missed.".into(),
                ],
            }, relationships: vec![], unresolved: vec![],
        };
        for (path, tree) in &modules.trees {
            if tree.root_node().has_error() {
                batch.coverage.skipped_files.push(path.clone());
                continue;
            }
            batch.coverage.analyzed_files.push(path.clone());
            let source = &input.files[path.as_str()].source;
            let mut dynamic = BTreeSet::from([
                "__import__".to_owned(),
                "importlib.import_module".to_owned(),
            ]);
            collect_dynamic_aliases(tree.root_node(), source, &mut dynamic);
            visit(
                tree.root_node(),
                source,
                path,
                &modules,
                &dynamic,
                &mut batch,
            );
        }
        batch.relationships.sort();
        batch.relationships.dedup();
        Ok(batch)
    }
}

fn join(root: &str, path: &str) -> String {
    if root.is_empty() {
        path.into()
    } else {
        format!("{root}/{path}")
    }
}
fn names(node: Node<'_>, source: &str) -> Vec<String> {
    if node.kind() == "identifier" {
        return vec![source[node.byte_range()].into()];
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .flat_map(|child| names(child, source))
        .collect()
}
fn original(node: Node<'_>) -> Node<'_> {
    node.child_by_field_name("name")
        .filter(|_| node.kind() == "aliased_import")
        .unwrap_or(node)
}
fn evidence(node: Node<'_>, path: &str) -> DependencyEvidence {
    DependencyEvidence {
        path: path.into(),
        line: node.start_position().row + 1,
        bytes: [node.start_byte(), node.end_byte()],
        provider: PROVIDER.into(),
    }
}
fn unresolved(batch: &mut AnalysisBatch, node: Node<'_>, path: &str, target: &str, reason: &str) {
    batch.unresolved.push(UnresolvedDependency {
        from: path.into(),
        target: target.into(),
        kind: DependencyKind::Imports,
        reason: reason.into(),
        evidence: evidence(node, path),
    });
}
fn links(batch: &mut AnalysisBatch, module: &Module, node: Node<'_>, path: &str, omit: &[String]) {
    for target in &module.links {
        if target != path && !omit.contains(target) {
            batch.relationships.push(DependencyFact {
                from: path.into(),
                to: target.clone(),
                kind: DependencyKind::Imports,
                evidence: evidence(node, path),
            });
        }
    }
}

impl Modules<'_> {
    fn exists(&self, path: &str) -> bool {
        self.input.files.contains_key(path)
    }
    fn observed(&self, root: &str, first: &str) -> bool {
        let stem = join(root, first);
        self.exists(&format!("{stem}.py"))
            || self.exists(&format!("{stem}.pyi"))
            || self.directories.contains(&stem)
    }
    fn lookup(&self, root: &str, parts: &[String]) -> std::result::Result<Module, &'static str> {
        let mut result = Module {
            root: root.into(),
            parts: parts.into(),
            links: vec![],
            package: false,
        };
        for i in 0..parts.len() {
            let stem = join(root, &parts[..=i].join("/"));
            let file = format!("{stem}.py");
            let init = format!("{stem}/__init__.py");
            let (has_file, has_init) = (self.exists(&file), self.exists(&init));
            if has_file && has_init {
                return Err("ambiguous_module_target");
            }
            if has_init {
                result.links.push(init);
                result.package = i + 1 == parts.len();
            } else if has_file {
                if i + 1 != parts.len() {
                    return Err("parent_module_is_not_package");
                }
                result.links.push(file);
            } else if self.exists(&format!("{stem}.pyi"))
                || self.exists(&format!("{stem}/__init__.pyi"))
            {
                return Err("stub_only_target");
            } else if self.directories.contains(&stem) {
                return Err("namespace_package_layout_unsupported");
            } else {
                return Err("external_or_not_indexed");
            }
        }
        if result.links.is_empty() {
            return Err("external_or_not_indexed");
        }
        Ok(result)
    }
    fn absolute(&self, parts: &[String]) -> std::result::Result<Module, &'static str> {
        let Some(first) = parts.first() else {
            return Err("external_or_not_indexed");
        };
        let candidates = ["", "src"]
            .into_iter()
            .filter(|root| self.observed(root, first))
            .map(|root| self.lookup(root, parts))
            .collect::<Vec<_>>();
        // Do not silently choose one search root when another can shadow its package.
        if candidates.len() > 1 {
            return Err("ambiguous_import_roots");
        }
        candidates
            .into_iter()
            .next()
            .unwrap_or(Err("external_or_not_indexed"))
    }
    fn relative(
        &self,
        path: &str,
        level: usize,
        suffix: &[String],
    ) -> std::result::Result<Module, &'static str> {
        let mut directory = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        let mut package = vec![];
        while !directory.is_empty() && self.exists(&join(directory, "__init__.py")) {
            let (parent, name) = directory.rsplit_once('/').unwrap_or(("", directory));
            package.push(name.to_owned());
            directory = parent;
        }
        if package.is_empty() {
            return Err("relative_package_context_unavailable");
        }
        if level > package.len() {
            return Err("relative_import_beyond_package");
        }
        package.reverse();
        package.truncate(package.len() + 1 - level);
        package.extend_from_slice(suffix);
        self.lookup(directory, &package)
    }
    fn transparent_package(&self, module: &Module) -> bool {
        let Some(path) = module.links.last() else {
            return false;
        };
        let tree = &self.trees[path];
        if tree.root_node().has_error() {
            return false;
        }
        let source = &self.input.files[path.as_str()].source;
        let mut cursor = tree.root_node().walk();
        tree.root_node().named_children(&mut cursor).all(|node| {
            matches!(node.kind(), "comment" | "pass_statement")
                || (node.kind() == "expression_statement"
                    && node.named_child(0).is_some_and(|value| {
                        value.kind() == "string"
                            && !source[value.byte_range()]
                                .split(['\'', '"'])
                                .next()
                                .unwrap_or("")
                                .to_ascii_lowercase()
                                .contains('f')
                    }))
        })
    }
}

fn collect_dynamic_aliases(node: Node<'_>, source: &str, aliases: &mut BTreeSet<String>) {
    let mut cursor = node.walk();
    if node.kind() == "import_statement" {
        for item in node.children_by_field_name("name", &mut cursor) {
            if names(original(item), source) == ["importlib"] {
                let alias = item
                    .child_by_field_name("alias")
                    .map(|n| &source[n.byte_range()])
                    .unwrap_or("importlib");
                aliases.insert(format!("{alias}.import_module"));
            }
        }
    } else if node.kind() == "import_from_statement"
        && node
            .child_by_field_name("module_name")
            .is_some_and(|m| source[m.byte_range()] == *"importlib")
    {
        for item in node.children_by_field_name("name", &mut cursor) {
            if names(original(item), source) == ["import_module"] {
                let name = item
                    .child_by_field_name("alias")
                    .unwrap_or_else(|| original(item));
                aliases.insert(source[name.byte_range()].into());
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_dynamic_aliases(child, source, aliases);
    }
}

fn visit(
    node: Node<'_>,
    source: &str,
    path: &str,
    modules: &Modules<'_>,
    dynamic: &BTreeSet<String>,
    batch: &mut AnalysisBatch,
) {
    if node.kind() == "future_import_statement" {
        return;
    }
    if node.kind() == "import_statement" {
        let mut cursor = node.walk();
        for item in node.children_by_field_name("name", &mut cursor) {
            let name = original(item);
            let parts = names(name, source);
            match modules.absolute(&parts) {
                Ok(module) => links(batch, &module, name, path, &[]),
                Err(reason) => unresolved(batch, name, path, &parts.join("."), reason),
            }
        }
        return;
    }
    if node.kind() == "import_from_statement" {
        let Some(base) = node.child_by_field_name("module_name") else {
            return;
        };
        let parts = names(base, source);
        let dots = if base.kind() == "relative_import" {
            let mut cursor = base.walk();
            base.named_children(&mut cursor)
                .filter(|n| n.kind() == "import_prefix")
                .map(|n| {
                    source[n.byte_range()]
                        .bytes()
                        .filter(|b| *b == b'.')
                        .count()
                })
                .sum()
        } else {
            0
        };
        let result = if dots == 0 {
            modules.absolute(&parts)
        } else {
            modules.relative(path, dots, &parts)
        };
        match &result {
            Ok(module) => links(batch, module, base, path, &[]),
            Err(reason) => unresolved(batch, base, path, &source[base.byte_range()], reason),
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "wildcard_import" {
                unresolved(
                    batch,
                    child,
                    path,
                    &source[node.byte_range()],
                    "wildcard_expansion_unavailable",
                );
            }
        }
        if let Ok(module) = result
            && module.package
        {
            let mut cursor = node.walk();
            for member in node.children_by_field_name("name", &mut cursor) {
                let name = original(member);
                let mut parts = module.parts.clone();
                parts.extend(names(name, source));
                // Attribute lookup precedes submodule loading. Only passive __init__.py
                // files allow a unique child module lead without guessing its exports.
                match modules.lookup(&module.root, &parts) {
                    Ok(child) if modules.transparent_package(&module) => {
                        links(batch, &child, name, path, &module.links)
                    }
                    Ok(_) => unresolved(
                        batch,
                        name,
                        path,
                        &parts.join("."),
                        "package_attribute_or_submodule",
                    ),
                    Err(reason) => unresolved(
                        batch,
                        name,
                        path,
                        &parts.join("."),
                        if reason == "external_or_not_indexed" {
                            "package_member_binding_unverified"
                        } else {
                            reason
                        },
                    ),
                }
            }
        }
        return;
    }
    if node.kind() == "call"
        && let Some(function) = node.child_by_field_name("function")
    {
        let name: String = source[function.byte_range()]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if dynamic.contains(&name) {
            unresolved(batch, function, path, &name, "dynamic_import_unsupported");
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, source, path, modules, dynamic, batch);
    }
}
