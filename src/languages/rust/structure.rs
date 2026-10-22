//! Read-only, source-bound observations for ranking research, not inferred program semantics.
use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tree_sitter::{Node, Parser};

use crate::{store, workspace};

pub const STRUCTURE_POLICY: &str = "rust-structure-v2";
const EVIDENCE_LIMIT: usize = 64;
const COUNTERS: &[&str] = &[
    "calls",
    "assignments",
    "self_field_assignments",
    "self_field_path_assignments",
    "field_accesses",
    "branches",
    "loops",
    "awaits",
    "tries",
    "returns",
    "macros",
    "closures",
    "nested_functions",
    "deferred_blocks",
];
const SHAPES: &[&str] = &[
    "field_return_body",
    "parameter_field_setter_body",
    "field_path_return_body",
    "parameter_field_path_setter_body",
    "single_call_body",
    "single_struct_literal_body",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
}

impl SourceSpan {
    fn of(n: Node<'_>) -> Self {
        Self {
            start_byte: n.start_byte(),
            end_byte: n.end_byte(),
            start_line: n.start_position().row + 1,
            end_line: n.end_position().row + 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfFieldPath {
    pub span: SourceSpan,
    /// Number of field selectors, including tuple members, after the self root.
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructureEvidence {
    pub feature: String,
    pub span: SourceSpan,
    /// Terminal callee name, distinct from its receiver, arguments and type parameters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee: Option<SourceSpan>,
    /// A syntactic self-rooted field chain; no alias or type resolution is inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_path: Option<SelfFieldPath>,
    /// Current compiler bindings whose occurrence is within this callee range.
    pub targets: Vec<ResolvedStructureCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedStructureCall {
    pub location_id: i64,
    pub path: String,
    pub symbol: Option<String>,
    pub occurrence: SourceSpan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionStructure {
    pub counts: BTreeMap<String, usize>,
    pub shapes: BTreeMap<String, bool>,
    pub evidence: Vec<StructureEvidence>,
    pub evidence_truncated: bool,
}

impl FunctionStructure {
    fn new() -> Self {
        Self {
            counts: COUNTERS.iter().map(|k| (k.to_string(), 0)).collect(),
            shapes: SHAPES.iter().map(|k| (k.to_string(), false)).collect(),
            evidence: Vec::new(),
            evidence_truncated: false,
        }
    }

    fn observation(&mut self, feature: &str, node: Node<'_>, callee: Option<Node<'_>>) {
        self.observe(feature, node, callee, None);
    }

    fn observe(
        &mut self,
        feature: &str,
        node: Node<'_>,
        callee: Option<Node<'_>>,
        field_path: Option<SelfFieldPath>,
    ) {
        if self.evidence.len() == EVIDENCE_LIMIT {
            self.evidence_truncated = true;
            return;
        }
        self.evidence.push(StructureEvidence {
            feature: feature.into(),
            span: SourceSpan::of(node),
            callee: callee.map(SourceSpan::of),
            field_path,
            targets: Vec::new(),
        });
    }

    fn count(&mut self, feature: &str, node: Node<'_>) {
        *self.counts.get_mut(feature).unwrap() += 1;
        self.observation(feature, node, None);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructureLocation {
    pub location_id: i64,
    pub path: String,
    pub symbol: Option<String>,
    pub kind: String,
    pub definition: Option<usize>,
    pub source_role: String,
    pub source_hash: String,
    pub status: String,
    pub span: Option<SourceSpan>,
    pub facts: Option<FunctionStructure>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StructureReport {
    pub policy: String,
    pub snapshot: String,
    pub semantic_profile: Option<String>,
    pub semantic_current: bool,
    pub locations: Vec<StructureLocation>,
}

/// Export observations without modifying the index, embeddings, candidates, or briefs.
/// Stale source is rejected, and stale semantic bindings are omitted.
pub fn structure(db: &Path, scope: Option<&str>) -> Result<StructureReport> {
    let scope = crate::search::normalize_scope(scope)?;
    let mut conn = store::open(db, false)?;
    let tx = conn.transaction()?;
    let root = store::metadata(&tx, "root")?.context("index is empty")?;
    let snapshot = store::metadata(&tx, "snapshot")?.context("index has no snapshot")?;
    let current = workspace::scan_index(&tx, Path::new(&root))?;
    ensure!(
        workspace::snapshot(&current) == snapshot,
        "stale source; run sonar index before exporting structure"
    );
    let source: BTreeMap<_, _> = current.iter().map(|f| (f.path.as_str(), f)).collect();
    let semantic_current = crate::semantic::current(&tx, Path::new(&root), &snapshot, true)?;
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let mut parsed = BTreeMap::new();
    for f in &current {
        if f.path.ends_with(".rs") && crate::search::in_scope(&f.path, &scope) {
            let tree = parser
                .parse(&f.source, None)
                .context("Rust parser returned no tree")?;
            let mut nodes = BTreeMap::new();
            collect(tree.root_node(), &f.source, &mut nodes);
            parsed.insert(f.path.as_str(), nodes);
        }
    }
    let mut stmt = tx.prepare("SELECT l.id,f.path,l.symbol,l.kind,l.definition,l.start_byte,l.end_byte,l.is_test FROM locations l JOIN files f ON f.id=l.file_id ORDER BY f.path,l.start_byte,l.id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<u32>>(4)?.map(|v| v as usize),
            r.get::<_, u32>(5)? as usize,
            r.get::<_, u32>(6)? as usize,
            r.get::<_, bool>(7)?,
        ))
    })?;
    let mut locations = Vec::new();
    for row in rows {
        let (id, path, symbol, kind, definition, start, end, is_test) = row?;
        if !crate::search::in_scope(&path, &scope) {
            continue;
        }
        let file = source
            .get(path.as_str())
            .context("indexed source is missing")?;
        let role = if is_test {
            "test"
        } else if kind == "documentation" {
            "documentation"
        } else {
            "implementation"
        };
        let mut loc = StructureLocation {
            location_id: id,
            path: path.clone(),
            symbol,
            kind: kind.clone(),
            definition,
            source_role: role.into(),
            source_hash: file.hash.clone(),
            status: if kind == "function" && crate::source_language(&path) != Some("rust") {
                "unsupported_language"
            } else {
                "not_function"
            }
            .into(),
            span: None,
            facts: None,
        };
        if kind == "function" && crate::source_language(&path) == Some("rust") {
            let (status, span, facts) = parsed
                .get(path.as_str())
                .and_then(|m| m.get(&(start, end)))
                .context(
                    "function span does not match syntax; reindex with the current extractor",
                )?;
            loc.status = status.clone();
            loc.span = Some(span.clone());
            loc.facts = facts.clone();
            if semantic_current && let Some(facts) = &mut loc.facts {
                let mut edges = tx.prepare("SELECT e.to_id,f.path,l.symbol,e.start_byte,e.end_byte,e.line FROM semantic_edges e JOIN locations l ON l.id=e.to_id JOIN files f ON f.id=l.file_id WHERE e.from_id=? AND e.kind='calls' AND e.path=? ORDER BY e.start_byte,e.to_id")?;
                let mut edges = edges.query(rusqlite::params![id, path])?;
                while let Some(edge) = edges.next()? {
                    let a = edge.get::<_, u32>(3)? as usize;
                    let b = edge.get::<_, u32>(4)? as usize;
                    ensure!(
                        start <= a
                            && a < b
                            && b <= end
                            && file.source.is_char_boundary(a)
                            && file.source.is_char_boundary(b),
                        "invalid compiler call coordinates"
                    );
                    for observation in &mut facts.evidence {
                        if observation
                            .callee
                            .as_ref()
                            .is_some_and(|c| c.start_byte <= a && b <= c.end_byte)
                        {
                            let line = edge.get::<_, u32>(5)? as usize;
                            ensure!(
                                file.source[..a].bytes().filter(|b| *b == b'\n').count() + 1
                                    == line,
                                "invalid compiler call line"
                            );
                            observation.targets.push(ResolvedStructureCall {
                                location_id: edge.get(0)?,
                                path: edge.get(1)?,
                                symbol: edge.get(2)?,
                                occurrence: SourceSpan {
                                    start_byte: a,
                                    end_byte: b,
                                    start_line: line,
                                    end_line: file.source[..b]
                                        .bytes()
                                        .filter(|b| *b == b'\n')
                                        .count()
                                        + 1,
                                },
                            });
                        }
                    }
                }
            }
        }
        locations.push(loc);
    }
    ensure!(
        workspace::snapshot(&workspace::scan_index(&tx, Path::new(&root))?) == snapshot,
        "source changed during structure export; retry after indexing"
    );
    ensure!(
        !semantic_current || crate::semantic::current(&tx, Path::new(&root), &snapshot, true)?,
        "compiler inputs changed during structure export; retry after resolving"
    );
    Ok(StructureReport {
        policy: STRUCTURE_POLICY.into(),
        snapshot,
        semantic_profile: store::metadata(&tx, "semantic_profile")?,
        semantic_current,
        locations,
    })
}

type ParsedStructures = BTreeMap<(usize, usize), (String, SourceSpan, Option<FunctionStructure>)>;

fn collect(node: Node<'_>, source: &str, output: &mut ParsedStructures) {
    if matches!(node.kind(), "function_item" | "function_signature_item") {
        let (status, facts) = if node.has_error() {
            ("parse_error", None)
        } else if let Some(body) = node.child_by_field_name("body") {
            ("current", Some(analyze(node, body, source)))
        } else {
            ("declaration", None)
        };
        output.insert(
            (node.start_byte(), node.end_byte()),
            (status.into(), SourceSpan::of(node), facts),
        );
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, source, output);
    }
}

fn meaningful(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| !matches!(n.kind(), "line_comment" | "block_comment"))
        .collect()
}

fn unbox(mut node: Node<'_>) -> Node<'_> {
    while matches!(
        node.kind(),
        "expression_statement" | "return_expression" | "parenthesized_expression"
    ) {
        let children = meaningful(node);
        if children.len() != 1 {
            break;
        }
        node = children[0];
    }
    node
}

fn self_field(node: Node<'_>) -> bool {
    node.kind() == "field_expression"
        && node
            .child_by_field_name("value")
            .is_some_and(|n| n.kind() == "self")
}

/// Accept field chains and grouping parentheses only. Indexing, explicit dereferences,
/// calls, aliases, casts and references do not prove the same syntactic root.
fn self_field_path(mut node: Node<'_>) -> Option<SelfFieldPath> {
    let span = SourceSpan::of(node);
    let mut depth = 0;
    loop {
        match node.kind() {
            "field_expression" => {
                depth += 1;
                node = node.child_by_field_name("value")?;
            }
            "parenthesized_expression" => {
                let children = meaningful(node);
                if children.len() != 1 {
                    return None;
                }
                node = children[0];
            }
            "self" if depth > 0 => return Some(SelfFieldPath { span, depth }),
            _ => return None,
        }
    }
}

fn returns_value(node: Node<'_>) -> bool {
    if node.kind() != "expression_statement" {
        return true;
    }
    let children = meaningful(node);
    children.len() == 1 && children[0].kind() == "return_expression"
}

fn analyze(function: Node<'_>, body: Node<'_>, source: &str) -> FunctionStructure {
    let mut facts = FunctionStructure::new();
    let statements = meaningful(body);
    if statements.len() == 1 {
        let mut expression = unbox(statements[0]);
        if expression.kind() == "reference_expression" {
            expression = expression.child_by_field_name("value").unwrap();
        }
        if self_field(expression) && returns_value(statements[0]) {
            facts.shapes.insert("field_return_body".into(), true);
            facts.observation("field_return_body", expression, None);
        }
        if returns_value(statements[0])
            && let Some(path) = self_field_path(expression)
        {
            facts.shapes.insert("field_path_return_body".into(), true);
            facts.observe("field_path_return_body", expression, None, Some(path));
        }
        let mut expression = unbox(statements[0]);
        while matches!(expression.kind(), "try_expression" | "await_expression") {
            expression = unbox(expression.named_child(0).unwrap());
        }
        if expression.kind() == "call_expression" {
            facts.shapes.insert("single_call_body".into(), true);
            facts.observation("single_call_body", expression, None);
        }
        if expression.kind() == "struct_expression" && returns_value(statements[0]) {
            facts
                .shapes
                .insert("single_struct_literal_body".into(), true);
            facts.observation("single_struct_literal_body", expression, None);
        }
    }
    if (1..=2).contains(&statements.len()) {
        let assignment = unbox(statements[0]);
        let self_tail = statements.len() == 1 || unbox(statements[1]).kind() == "self";
        if assignment.kind() == "assignment_expression" && self_tail {
            let left = assignment.child_by_field_name("left").unwrap();
            let right = assignment.child_by_field_name("right").unwrap();
            let parameter = function
                .child_by_field_name("parameters")
                .is_some_and(|params| {
                    meaningful(params).iter().any(|p| {
                        p.kind() == "parameter"
                            && p.child_by_field_name("pattern").is_some_and(|p| {
                                p.kind() == "identifier"
                                    && source[p.byte_range()] == source[right.byte_range()]
                            })
                    })
                });
            if self_field(left) && right.kind() == "identifier" && parameter {
                facts
                    .shapes
                    .insert("parameter_field_setter_body".into(), true);
                facts.observation("parameter_field_setter_body", assignment, None);
            }
            if right.kind() == "identifier"
                && parameter
                && let Some(path) = self_field_path(left)
            {
                facts
                    .shapes
                    .insert("parameter_field_path_setter_body".into(), true);
                facts.observe(
                    "parameter_field_path_setter_body",
                    assignment,
                    None,
                    Some(path),
                );
            }
        }
    }
    visit(body, &mut facts);
    // An outer call expression with calls in its arguments is not a simple forwarding body.
    if facts.counts["calls"] != 1
        || facts.counts["macros"] != 0
        || facts.counts["closures"] != 0
        || facts.counts["deferred_blocks"] != 0
        || facts.counts["assignments"] != 0
        || facts.counts["branches"] != 0
        || facts.counts["loops"] != 0
    {
        facts.shapes.insert("single_call_body".into(), false);
        facts.evidence.retain(|e| e.feature != "single_call_body");
    }
    facts
}

fn visit(node: Node<'_>, facts: &mut FunctionStructure) {
    match node.kind() {
        "closure_expression" => {
            facts.count("closures", node);
            return;
        }
        "function_item" | "function_signature_item" => {
            facts.count("nested_functions", node);
            return;
        }
        "async_block" | "gen_block" | "const_block" => {
            facts.count("deferred_blocks", node);
            return;
        }
        "macro_invocation" => {
            facts.count("macros", node);
            return;
        }
        // These are nested declarations, not operations performed by this function body.
        "impl_item" | "trait_item" | "mod_item" | "const_item" | "static_item"
        | "macro_definition" => return,
        "call_expression" => {
            *facts.counts.get_mut("calls").unwrap() += 1;
            facts.observation(
                "calls",
                node,
                node.child_by_field_name("function")
                    .and_then(crate::semantic::callee_name),
            );
        }
        "assignment_expression" | "compound_assignment_expr" => {
            facts.count("assignments", node);
            if node.child_by_field_name("left").is_some_and(self_field) {
                facts.count("self_field_assignments", node);
            }
            if let Some(path) = node.child_by_field_name("left").and_then(self_field_path) {
                *facts.counts.get_mut("self_field_path_assignments").unwrap() += 1;
                facts.observe("self_field_path_assignments", node, None, Some(path));
            }
        }
        "field_expression" => {
            let selector = node
                .parent()
                .filter(|p| p.kind() == "generic_function")
                .unwrap_or(node);
            let method_selector = selector.parent().is_some_and(|p| {
                p.kind() == "call_expression" && p.child_by_field_name("function") == Some(selector)
            });
            if !method_selector {
                facts.count("field_accesses", node);
            }
        }
        "if_expression" | "match_expression" => facts.count("branches", node),
        "for_expression" | "while_expression" | "loop_expression" => facts.count("loops", node),
        "await_expression" => facts.count("awaits", node),
        "try_expression" => facts.count("tries", node),
        "return_expression" => facts.count("returns", node),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, facts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn parsed(source: &str) -> ParsedStructures {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut output = BTreeMap::new();
        collect(tree.root_node(), source, &mut output);
        output
    }

    #[test]
    fn rooted_paths_cover_nested_and_tuple_fields_without_redefining_direct_features() {
        for (source, depth) in [
            ("fn f(&mut self, n: u8) { self.config.limit = n; }", 2),
            (
                "fn f(mut self, n: u8) -> Self { (self.config).limit = n; self }",
                2,
            ),
            ("fn f(&mut self, n: u8) { self.config.flags.0 = n; }", 3),
            ("// π🙂\r\nfn f(&mut self, n: u8) { self.配置.值 = n; }", 2),
        ] {
            let output = parsed(source);
            let facts = output.values().next().unwrap().2.as_ref().unwrap();
            assert!(facts.shapes["parameter_field_path_setter_body"], "{source}");
            assert!(!facts.shapes["parameter_field_setter_body"], "{source}");
            assert_eq!(facts.counts["self_field_assignments"], 0);
            assert_eq!(facts.counts["self_field_path_assignments"], 1);
            let paths: Vec<_> = facts
                .evidence
                .iter()
                .filter_map(|e| e.field_path.as_ref())
                .collect();
            assert_eq!(paths.len(), 2);
            for path in paths {
                assert_eq!(path.depth, depth);
                let text = &source[path.span.start_byte..path.span.end_byte];
                assert!(text.contains("self") && !text.contains('='));
                assert_eq!(
                    path.span.start_line,
                    source[..path.span.start_byte]
                        .bytes()
                        .filter(|b| *b == b'\n')
                        .count()
                        + 1
                );
            }
        }
        let old = parsed("fn f(&mut self, n: u8) { self.value = n; }");
        let facts = old.values().next().unwrap().2.as_ref().unwrap();
        assert!(facts.shapes["parameter_field_setter_body"]);
        assert!(facts.shapes["parameter_field_path_setter_body"]);
        assert_eq!(facts.counts["self_field_assignments"], 1);
        assert_eq!(facts.counts["self_field_path_assignments"], 1);
    }

    #[test]
    fn field_path_returns_require_a_returned_value_and_a_syntactic_self_root() {
        for source in [
            "fn f(&self) -> u8 { self.config.limit }",
            "fn f(&self) -> &u8 { &self.config.limit }",
            "fn f(&mut self) -> &mut u8 { return &mut (self.config.limit); }",
            "fn f(&self) -> u8 { (self.config).pair.0 }",
        ] {
            let output = parsed(source);
            let facts = output.values().next().unwrap().2.as_ref().unwrap();
            assert!(facts.shapes["field_path_return_body"], "{source}");
            assert!(!facts.shapes["field_return_body"], "{source}");
        }
        for source in [
            "fn f(&self) { self.config.limit; }",
            "fn f(&self) { &self.config.limit; }",
            "fn f(&self) -> u8 { self.config.limit.clone() }",
            "fn f(&self) -> u8 { self.config[0].limit }",
            "fn f(&self) -> u8 { (*self).config.limit }",
            "fn f(&self) -> u8 { self.config().limit }",
            "fn f(other: &Config) -> u8 { other.inner.limit }",
            "fn f(&self) -> u8 { let x = &self.config; x.limit }",
        ] {
            let output = parsed(source);
            let facts = output.values().next().unwrap().2.as_ref().unwrap();
            assert!(!facts.shapes["field_path_return_body"], "{source}");
        }
    }

    #[test]
    fn operations_and_unproven_roots_do_not_become_parameter_setters() {
        for (source, assignments) in [
            ("fn f(&mut self, n: u8) { self.config.limit += n; }", 1),
            (
                "fn f(&mut self, n: u8) { self.config.limit = n.max(1); }",
                1,
            ),
            (
                "fn f(&mut self, n: u8) { self.config.limit = n; validate(); }",
                1,
            ),
            (
                "fn f(&mut self, n: u8) { if n > 0 { self.config.limit = n; } }",
                1,
            ),
            (
                "fn f(&mut self, n: u8) { let n = normalize(n); self.config.limit = n; }",
                1,
            ),
            ("fn f(&mut self, n: u8) { self.config[0].limit = n; }", 0),
            ("fn f(&mut self, n: u8) { (*self).config.limit = n; }", 0),
            ("fn f(&mut self, n: u8) { self.config().limit = n; }", 0),
            (
                "fn f(other: &mut Config, n: u8) { other.inner.limit = n; }",
                0,
            ),
            (
                "fn f(&mut self, n: u8) { let alias = &mut self.config; alias.limit = n; }",
                0,
            ),
            (
                "fn f(&mut self, n: u8) { let later = || { self.config.limit = n; }; }",
                0,
            ),
            (
                "fn f(&mut self, n: u8) { opaque!(self.config.limit = n); }",
                0,
            ),
        ] {
            let output = parsed(source);
            let facts = output.values().next().unwrap().2.as_ref().unwrap();
            assert!(
                !facts.shapes["parameter_field_path_setter_body"],
                "{source}"
            );
            assert_eq!(
                facts.counts["self_field_path_assignments"], assignments,
                "{source}"
            );
        }
    }

    #[test]
    fn path_evidence_stays_bounded_and_nested_identifier_renaming_preserves_features() {
        let source = format!(
            "fn f(&mut self, n: u8) {{ {} }}",
            "self.config.limit = n; ".repeat(100)
        );
        let output = parsed(&source);
        let facts = output.values().next().unwrap().2.as_ref().unwrap();
        assert_eq!(facts.counts["self_field_path_assignments"], 100);
        assert_eq!(facts.evidence.len(), EVIDENCE_LIMIT);
        assert!(facts.evidence_truncated);
        for e in &facts.evidence {
            if let Some(path) = &e.field_path {
                assert_eq!(
                    &source[path.span.start_byte..path.span.end_byte],
                    "self.config.limit"
                );
                assert!(
                    e.span.start_byte <= path.span.start_byte
                        && path.span.end_byte <= e.span.end_byte
                );
            }
        }
        let a = parsed("fn configure(&mut self, amount: u8) { self.settings.limit = amount; }");
        let b = parsed("fn opaque(&mut self, unnamed: u8) { self.foo.bar = unnamed; }");
        let a = a.values().next().unwrap().2.as_ref().unwrap();
        let b = b.values().next().unwrap().2.as_ref().unwrap();
        assert_eq!(a.counts, b.counts);
        assert_eq!(a.shapes, b.shapes);
        let legacy: StructureEvidence = serde_json::from_str(r#"{"feature":"calls","span":{"start_byte":0,"end_byte":3,"start_line":1,"end_line":1},"targets":[]}"#).unwrap();
        assert!(legacy.field_path.is_none());
    }

    #[test]
    fn patterns_distinguish_setters_getters_wrappers_and_operations_without_names() {
        let cases = [
            (
                "fn opaque(&mut self, yes: bool) -> &mut Self { self.flag = yes; self }",
                "parameter_field_setter_body",
            ),
            (
                "fn opaque(&self) -> &bool { &self.flag }",
                "field_return_body",
            ),
            (
                "async fn opaque(x: u8) -> Result<(), E> { target(x).await? }",
                "single_call_body",
            ),
            (
                "fn opaque(kind: K) -> Self { Self { kind } }",
                "single_struct_literal_body",
            ),
        ];
        for (source, shape) in cases {
            let output = parsed(source);
            let facts = output.values().next().unwrap().2.as_ref().unwrap();
            assert!(facts.shapes[shape], "{source}");
            for e in &facts.evidence {
                assert!(!source[e.span.start_byte..e.span.end_byte].is_empty());
            }
        }
        for source in [
            "fn f(&mut self, x: u8) { let x = transform(x); self.x = x; }",
            "fn f(&mut self, x: u8) { self.x = transform(x); }",
            "fn f(x: u8) { forward(transform(x)); }",
            "fn f(x: u8) { forward(if x > 0 { 1 } else { 2 }); }",
        ] {
            let output = parsed(source);
            let f = output.values().next().unwrap().2.as_ref().unwrap();
            assert!(!f.shapes["parameter_field_setter_body"] && !f.shapes["single_call_body"]);
        }
    }

    #[test]
    fn deferred_nested_and_macro_bodies_do_not_become_outer_operations() {
        let source = "fn outer() { let f = || { hidden(); }; fn inner() { nested(); } let f = async { later().await; }; opaque!(fake()); visible(); }";
        let output = parsed(source);
        let outer = output.values().next().unwrap().2.as_ref().unwrap();
        assert_eq!(outer.counts["calls"], 1);
        assert_eq!(outer.counts["closures"], 1);
        assert_eq!(outer.counts["nested_functions"], 1);
        assert_eq!(outer.counts["deferred_blocks"], 1);
        assert_eq!(outer.counts["macros"], 1);
        assert_eq!(outer.counts["awaits"], 0);
        assert_eq!(output.len(), 2);
        let trait_method = parsed("trait T { fn f(&self); }");
        assert_eq!(trait_method.values().next().unwrap().0, "declaration");
        assert!(trait_method.values().next().unwrap().2.is_none());
        let malformed = parsed("fn f() { let a = ; }");
        assert_eq!(malformed.values().next().unwrap().0, "parse_error");
    }

    #[test]
    fn discarded_values_are_not_returns_and_names_do_not_change_observations() {
        for source in ["fn f(&self) { self.x; }", "fn f() { (S { x: 1 }); }"] {
            let output = parsed(source);
            let f = output.values().next().unwrap().2.as_ref().unwrap();
            assert!(!f.shapes["field_return_body"]);
            assert!(!f.shapes["single_struct_literal_body"]);
        }
        let explicit = parsed("fn f(&self) -> u8 { return self.x; }");
        assert!(explicit.values().next().unwrap().2.as_ref().unwrap().shapes["field_return_body"]);
        let a = parsed("fn run(&mut self, value: u8) { self.state = value; }");
        let b = parsed("fn xyz(&mut self, opaque: u8) { self.abc = opaque; }");
        let a = a.values().next().unwrap().2.as_ref().unwrap();
        let b = b.values().next().unwrap().2.as_ref().unwrap();
        assert_eq!(a.counts, b.counts);
        assert_eq!(a.shapes, b.shapes);
    }

    #[test]
    fn evidence_is_byte_accurate_bounded_and_does_not_truncate_counts() {
        let source = format!("// é🙂\r\nfn f() {{ {} }}", "call(); ".repeat(100));
        let output = parsed(&source);
        let f = output.values().next().unwrap().2.as_ref().unwrap();
        assert_eq!(f.counts["calls"], 100);
        assert_eq!(f.evidence.len(), EVIDENCE_LIMIT);
        assert!(f.evidence_truncated);
        for e in &f.evidence {
            assert_eq!(&source[e.span.start_byte..e.span.end_byte], "call()");
            assert_eq!((e.span.start_line, e.span.end_line), (2, 2));
        }
    }

    #[test]
    fn call_names_exclude_nested_receivers_arguments_and_type_parameters() {
        let source = "fn f() { factory().method::<T>(argument()); }";
        let output = parsed(source);
        let f = output.values().next().unwrap().2.as_ref().unwrap();
        let names: Vec<_> = f
            .evidence
            .iter()
            .filter_map(|e| e.callee.as_ref())
            .map(|s| &source[s.start_byte..s.end_byte])
            .collect();
        assert_eq!(names, ["method", "factory", "argument"]);
        assert_eq!(f.counts["calls"], 3);
        assert_eq!(f.counts["field_accesses"], 0);
        assert!(!f.shapes["single_call_body"]);
    }

    #[test]
    fn export_is_read_only_scoped_and_rejects_source_drift() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("a.rs"),
            "fn f() { target(); }\nstruct S { value: bool }\n",
        )
        .unwrap();
        fs::write(root.join("b.rs"), "fn other() {}\n").unwrap();
        let db = temp.path().join("index.sqlite");
        crate::index(&root, &db).unwrap();
        let before = fs::read(&db).unwrap();
        let export = structure(&db, Some("a.rs")).unwrap();
        assert!(!export.semantic_current);
        assert!(export.locations.iter().all(|l| l.path == "a.rs"));
        let f = export
            .locations
            .iter()
            .find(|l| l.symbol.as_deref() == Some("f"))
            .unwrap();
        assert_eq!(f.facts.as_ref().unwrap().counts["calls"], 1);
        assert!(
            export
                .locations
                .iter()
                .filter(|l| l.kind != "function")
                .all(|l| l.facts.is_none())
        );
        assert_eq!(before, fs::read(&db).unwrap());
        assert!(structure(&db, Some("../")).is_err());
        fs::write(root.join("a.rs"), "fn f() { changed(); }\n").unwrap();
        assert!(structure(&db, None).is_err());
        assert_eq!(before, fs::read(&db).unwrap());
    }
}
