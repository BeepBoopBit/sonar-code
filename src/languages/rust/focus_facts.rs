//! Bounded source facts shared by contextual focus and its research exporter.
//! This module is an implementation detail, not the stable navigation response API.
use crate::SourceSpan;
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Tree};

pub struct Owner<'a> {
    pub location_id: i64,
    pub source_hash: &'a str,
    pub symbol: Option<&'a str>,
    pub source_role: &'a str,
    pub span: Option<&'a SourceSpan>,
    pub status: &'a str,
    pub evidence_truncated: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub operation: String,
    pub span: SourceSpan,
    pub source: String,
    pub role: usize,
    pub origin: SourceSpan,
}
#[derive(Debug, Serialize)]
pub struct Views {
    pub location_id: i64,
    pub source_hash: String,
    pub status: String,
    pub observations: Vec<Observation>,
    pub facts: Vec<Fact>,
    pub reservoir: Vec<ReservoirFact>,
    pub oversized_observations: usize,
    pub capped_observations: usize,
    pub redundant_observations: usize,
    pub input_evidence_truncated: bool,
    pub visited_nodes: usize,
}
#[derive(Debug, Serialize)]
pub struct ReservoirFact {
    pub observation: Observation,
    pub roles: Vec<usize>,
    pub branch: usize,
    pub buckets: Vec<usize>,
    pub priority: bool,
}
#[derive(Debug, Serialize)]
pub struct Fragment {
    pub relation: String,
    pub span: SourceSpan,
    pub source: String,
}
#[derive(Debug, Serialize)]
pub struct Fact {
    pub observation: Observation,
    pub roles: Vec<usize>,
    pub context: Vec<Fragment>,
    pub plain: String,
    pub contextual: String,
}
fn span(n: Node<'_>) -> SourceSpan {
    SourceSpan {
        start_byte: n.start_byte(),
        end_byte: n.end_byte(),
        start_line: n.start_position().row + 1,
        end_line: n.end_position().row + 1,
    }
}
fn children(n: Node<'_>) -> Vec<Node<'_>> {
    let mut c = n.walk();
    n.named_children(&mut c).collect()
}
fn boundary(n: Node<'_>) -> bool {
    matches!(
        n.kind(),
        "closure_expression"
            | "function_item"
            | "function_signature_item"
            | "async_block"
            | "gen_block"
            | "const_block"
            | "impl_item"
            | "trait_item"
            | "mod_item"
            | "const_item"
            | "static_item"
            | "macro_definition"
    )
}
// Reject incomplete inspections rather than selecting a misleading early prefix.
fn inspect(body: Node<'_>) -> Option<Vec<Node<'_>>> {
    let mut pending = vec![(body, 0)];
    let mut result = Vec::new();
    while let Some((n, depth)) = pending.pop() {
        if result.len() == 20_000 || depth > 128 {
            return None;
        }
        result.push(n);
        if boundary(n) || n.kind() == "macro_invocation" {
            continue;
        }
        pending.extend(children(n).into_iter().rev().map(|c| (c, depth + 1)));
    }
    Some(result)
}
fn tail(block: Node<'_>) -> Option<Node<'_>> {
    let n = children(block).into_iter().rev().find(|n| {
        !matches!(
            n.kind(),
            "line_comment" | "block_comment" | "attribute_item"
        )
    })?;
    if n.kind() == "expression_statement" {
        // An expression_statement may represent an un-terminated if/match/block.
        let mut c = n.walk();
        if n.children(&mut c).any(|c| c.kind() == ";") {
            None
        } else {
            n.named_child(0)
        }
    } else if n.kind().ends_with("_declaration") || n.kind().ends_with("_item") {
        None
    } else {
        Some(n)
    }
}
struct Collector<'a> {
    pub source: &'a str,
    pub atoms: Vec<Observation>,
    pub oversized: usize,
}
impl Collector<'_> {
    fn atom(&mut self, n: Node<'_>, role: usize, operation: &str, origin: Node<'_>) -> bool {
        if n.end_byte() - n.start_byte() > 128 {
            self.oversized += 1;
            return false;
        }
        self.atoms.push(Observation {
            operation: operation.into(),
            span: span(n),
            source: self.source[n.byte_range()].into(),
            role,
            origin: span(origin),
        });
        true
    }
    // Only whole syntax nodes survive. Long literals, macros and deferred bodies are opaque.
    fn value(&mut self, n: Node<'_>, role: usize, operation: &str, origin: Node<'_>) {
        if boundary(n) {
            return;
        }
        match n.kind() {
            "block" => {
                if let Some(t) = tail(n) {
                    self.value(t, role, operation, origin);
                }
                return;
            }
            "if_expression" => {
                for name in ["consequence", "alternative"] {
                    if let Some(c) = n.child_by_field_name(name) {
                        self.value(c, role, operation, origin);
                    }
                }
                return;
            }
            "else_clause" | "match_block" => {
                for c in children(n) {
                    self.value(c, role, operation, origin);
                }
                return;
            }
            "match_expression" => {
                if let Some(b) = n.child_by_field_name("body") {
                    self.value(b, role, operation, origin);
                }
                return;
            }
            "match_arm" => {
                if let Some(v) = n.child_by_field_name("value") {
                    self.value(v, role, operation, origin);
                }
                return;
            }
            "return_expression" => {
                if let Some(v) = n.named_child(0) {
                    self.value(v, role, operation, origin);
                }
                return;
            }
            _ => {}
        }
        if self.atom(n, role, operation, origin) {
            return;
        }
        if n.kind() == "call_expression"
            && let Some(callee) = n.child_by_field_name("function")
        {
            self.atom(callee, role, "callee of oversized call", origin);
        }
        if matches!(
            n.kind(),
            "macro_invocation" | "string_literal" | "raw_string_literal" | "char_literal"
        ) {
            return;
        }
        for c in children(n) {
            if matches!(c.kind(), "arguments" | "field_initializer_list") {
                for v in children(c) {
                    self.value(v, role, operation, origin);
                }
            } else if c.kind().ends_with("_expression")
                || matches!(
                    c.kind(),
                    "block" | "field_initializer" | "shorthand_field_initializer"
                )
            {
                self.value(c, role, operation, origin);
            }
        }
    }
}
pub fn build(location: &Owner<'_>, source: &str, tree: &Tree) -> Result<Views> {
    build_for_query(location, source, tree, &[])
}

/// Query terms select within the bounded syntax reservoir before encoding sixteen facts.
/// This is lexical evidence selection, not macro expansion or control-flow analysis.
pub(crate) fn build_for_query(
    location: &Owner<'_>,
    source: &str,
    tree: &Tree,
    terms: &[String],
) -> Result<Views> {
    let mut out = Views {
        location_id: location.location_id,
        source_hash: location.source_hash.to_owned(),
        status: location.status.to_owned(),
        observations: vec![],
        facts: Vec::new(),
        reservoir: Vec::new(),
        oversized_observations: 0,
        capped_observations: 0,
        redundant_observations: 0,
        input_evidence_truncated: false,
        visited_nodes: 0,
    };
    let Some(owner) = location.span.filter(|_| location.status == "current") else {
        return Ok(out);
    };
    out.input_evidence_truncated = location.evidence_truncated;
    let function = tree
        .root_node()
        .descendant_for_byte_range(owner.start_byte, owner.end_byte)
        .context("missing owner")?;
    ensure!(
        function.kind() == "function_item"
            && function.start_byte() == owner.start_byte
            && function.end_byte() == owner.end_byte,
        "owner mismatch"
    );
    let Some(body) = function.child_by_field_name("body") else {
        return Ok(out);
    };
    if owner.end_byte - owner.start_byte > 256 * 1024 {
        out.status = "inspection_byte_limit".into();
        return Ok(out);
    }
    let Some(nodes) = inspect(body) else {
        out.status = "inspection_node_or_depth_limit".into();
        return Ok(out);
    };
    out.visited_nodes = nodes.len();
    let mut c = Collector {
        source,
        atoms: vec![],
        oversized: 0,
    };
    let mut returns = Vec::new();
    if let Some(t) = tail(body) {
        returns.push(t);
    }
    returns.extend(
        nodes
            .iter()
            .filter(|n| n.kind() == "return_expression")
            .filter_map(|n| n.named_child(0)),
    );
    for n in &returns {
        c.value(*n, 0, "returned expression", *n);
    }
    // One-hop syntactic association only. No reaching-definition, alias or mutation proof.
    let mut bindings = BTreeMap::<&str, usize>::new();
    for n in &nodes {
        if let Some(p) = n.child_by_field_name("pattern") {
            let mut pending = vec![p];
            while let Some(id) = pending.pop() {
                if id.kind() == "identifier" {
                    *bindings.entry(&source[id.byte_range()]).or_default() += 1;
                }
                pending.extend(children(id));
            }
        }
    }
    for n in children(body)
        .into_iter()
        .filter(|n| n.kind() == "let_declaration")
    {
        let (Some(p), Some(v)) = (
            n.child_by_field_name("pattern"),
            n.child_by_field_name("value"),
        ) else {
            continue;
        };
        let name = &source[p.byte_range()];
        let mutable = children(n).iter().any(|n| n.kind() == "mutable_specifier");
        if p.kind() == "identifier"
            && !mutable
            && bindings.get(name) == Some(&1)
            && nodes.iter().any(|id| {
                id.kind() == "identifier"
                    && id.start_byte() >= n.end_byte()
                    && &source[id.byte_range()] == name
                    && returns
                        .iter()
                        .any(|r| r.start_byte() <= id.start_byte() && id.end_byte() <= r.end_byte())
            })
        {
            c.value(v, 0, "returned local initializer", n);
        }
    }
    for n in &nodes {
        if boundary(*n) {
            continue;
        }
        match n.kind() {
            "assignment_expression" | "compound_assignment_expr" => {
                if !c.atom(*n, 1, "assignment", *n)
                    && let Some(v) = n.child_by_field_name("right")
                {
                    c.value(v, 1, "assignment right side", *n);
                }
            }
            "let_declaration" => {
                if let Some(v) = n.child_by_field_name("value") {
                    c.value(v, 1, "local initializer", *n);
                }
            }
            "if_expression" | "while_expression" => {
                if let Some(v) = n.child_by_field_name("condition") {
                    c.value(v, 2, "condition", *n);
                }
            }
            "match_expression" => {
                if let Some(v) = n.child_by_field_name("value") {
                    c.value(v, 2, "matched expression", *n);
                }
            }
            "binary_expression" => {
                if n.child_by_field_name("operator").is_some_and(|op| {
                    matches!(
                        &source[op.byte_range()],
                        "==" | "!=" | "<" | ">" | "<=" | ">="
                    )
                }) {
                    c.value(*n, 2, "comparison", *n);
                }
            }
            "call_expression" => {
                if !c.atom(*n, 3, "call", *n)
                    && let Some(callee) = n.child_by_field_name("function")
                {
                    c.atom(callee, 3, "callee of oversized call", *n);
                }
            }
            "macro_invocation" => {
                // Assertions often contain a multiline expected value. Keep their complete
                // source when bounded, but never inspect arbitrary macro token contents.
                let assertion = n.child_by_field_name("macro").is_some_and(|name| {
                    matches!(
                        source[name.byte_range()].rsplit("::").next(),
                        Some(
                            "assert"
                                | "assert_eq"
                                | "assert_ne"
                                | "debug_assert"
                                | "debug_assert_eq"
                                | "debug_assert_ne"
                        )
                    )
                });
                if assertion && n.end_byte() - n.start_byte() <= 512 {
                    c.atoms.push(Observation {
                        operation: "assertion macro invocation".into(),
                        span: span(*n),
                        source: source[n.byte_range()].into(),
                        role: 3,
                        origin: span(*n),
                    });
                } else {
                    c.atom(*n, 3, "opaque macro invocation", *n);
                }
            }
            _ => {}
        }
    }
    out.oversized_observations = c.oversized;
    let prefix = format!(
        "{}function {}",
        if location.source_role == "test" {
            "test "
        } else {
            ""
        },
        location.symbol.unwrap_or("")
    );
    if prefix.len() > 256 {
        out.status = "oversized_identity".into();
        return Ok(out);
    }
    // Equal source spans share roles, so a full return quota cannot erase an initializer.
    let mut unique: Vec<(Observation, BTreeSet<usize>)> = Vec::new();
    let mut index = BTreeMap::new();
    for a in c.atoms {
        let key = (a.span.start_byte, a.span.end_byte);
        if let Some(&i) = index.get(&key) {
            let (old, roles): &mut (Observation, BTreeSet<usize>) = &mut unique[i];
            roles.insert(a.role);
            if matches!(
                a.operation.as_str(),
                "callee of oversized call" | "assertion macro invocation"
            ) {
                old.operation = a.operation;
            }
            out.redundant_observations += 1;
        } else {
            index.insert(key, unique.len());
            let role = a.role;
            unique.push((a, BTreeSet::from([role])));
        }
    }
    if unique.len() > 2048 {
        out.status = "inspection_fact_limit".into();
        return Ok(out);
    }
    for (observation, roles) in &unique {
        let node = tree
            .root_node()
            .descendant_for_byte_range(observation.span.start_byte, observation.span.end_byte)
            .context("missing reservoir node")?;
        let origin = tree
            .root_node()
            .descendant_for_byte_range(observation.origin.start_byte, observation.origin.end_byte)
            .context("missing origin")?;
        let is_write = matches!(
            node.kind(),
            "assignment_expression" | "compound_assignment_expr"
        ) || matches!(
            origin.kind(),
            "assignment_expression" | "compound_assignment_expr"
        );
        let mut buckets = Vec::new();
        if is_write {
            buckets.push(0);
        }
        if roles.contains(&0) {
            buckets.push(1);
        }
        if roles.contains(&2) {
            buckets.push(2);
        }
        if roles.contains(&1) && !is_write {
            buckets.push(3);
        }
        if roles.contains(&3) {
            buckets.push(4);
        }
        if observation.operation == "assertion macro invocation" {
            buckets.push(5);
        }
        out.reservoir.push(ReservoirFact {
            observation: observation.clone(),
            roles: roles.iter().copied().collect(),
            branch: branch_of(node, body),
            buckets,
            priority: observation.operation == "callee of oversized call"
                || (node.kind() == "binary_expression"
                    && node.child_by_field_name("operator").is_some_and(|op| {
                        matches!(
                            &source[op.byte_range()],
                            "==" | "!=" | "<" | ">" | "<=" | ">="
                        )
                    })),
        });
    }
    let chosen = select_facts(&out.reservoir, terms);
    out.capped_observations = out
        .reservoir
        .iter()
        .filter(|f| {
            !chosen.iter().any(|&i| {
                let s = &out.reservoir[i].observation.span;
                s.start_byte <= f.observation.span.start_byte
                    && f.observation.span.end_byte <= s.end_byte
            })
        })
        .count();
    ensure!(chosen.len() <= 16, "fact budget");
    for i in chosen {
        let (a, roles) = &unique[i];
        let context = context_for(a, &nodes, body, source, tree, &prefix)?;
        let plain = format!("{}\n{}", prefix, a.source);
        let contextual = plain.clone()
            + &context
                .iter()
                .map(|c| format!("\n{}: {}", c.relation, c.source))
                .collect::<String>();
        ensure!(contextual.len() <= 1024, "context budget");
        out.observations.push(a.clone());
        out.facts.push(Fact {
            observation: a.clone(),
            roles: roles.iter().copied().collect(),
            context,
            plain,
            contextual,
        });
    }

    Ok(out)
}

// Nearest syntactic branch identity, never a claim about execution or effects.
fn branch_of(mut node: Node<'_>, body: Node<'_>) -> usize {
    while let Some(parent) = node.parent() {
        if node.id() == body.id() {
            break;
        }
        if node.kind() == "match_arm"
            || (matches!(node.kind(), "block" | "else_clause")
                && matches!(
                    parent.kind(),
                    "if_expression"
                        | "else_clause"
                        | "while_expression"
                        | "for_expression"
                        | "loop_expression"
                ))
        {
            return node.start_byte();
        }
        node = parent;
    }
    body.start_byte()
}

fn select_facts(facts: &[ReservoirFact], terms: &[String]) -> Vec<usize> {
    let mut chosen: Vec<usize> = Vec::new();
    let mut branches: [BTreeSet<usize>; 6] = std::array::from_fn(|_| BTreeSet::new());
    let mut operations: [BTreeSet<&str>; 6] = std::array::from_fn(|_| BTreeSet::new());
    let coverage: Vec<_> = facts
        .iter()
        .map(|f| {
            let tokens = crate::text::query_terms(&f.observation.source);
            terms.iter().filter(|t| tokens.contains(t)).count()
        })
        .collect();
    while chosen.len() < 16 {
        let before = chosen.len();
        for bucket in [0, 1, 2, 5, 3, 4] {
            if chosen.len() == 16 {
                break;
            }
            let next = facts
                .iter()
                .enumerate()
                .filter(|(_, f)| {
                    f.buckets.contains(&bucket)
                        && !chosen.iter().any(|&j| {
                            let s = &facts[j].observation.span;
                            s.start_byte <= f.observation.span.start_byte
                                && f.observation.span.end_byte <= s.end_byte
                        })
                })
                .min_by_key(|(i, f)| {
                    (
                        std::cmp::Reverse(coverage[*i]),
                        branches[bucket].contains(&f.branch),
                        operations[bucket].contains(f.observation.operation.as_str()),
                        !f.priority,
                        f.observation.span.start_byte,
                        f.observation.span.end_byte,
                    )
                })
                .map(|(i, _)| i);
            if let Some(i) = next {
                chosen.push(i);
                for &b in &facts[i].buckets {
                    branches[b].insert(facts[i].branch);
                    operations[b].insert(&facts[i].observation.operation);
                }
            }
        }
        if chosen.len() == before {
            break;
        }
    }
    chosen
}

fn context_for(
    a: &Observation,
    nodes: &[Node<'_>],
    body: Node<'_>,
    source: &str,
    tree: &Tree,
    prefix: &str,
) -> Result<Vec<Fragment>> {
    let node = tree
        .root_node()
        .descendant_for_byte_range(a.span.start_byte, a.span.end_byte)
        .context("missing fact node")?;
    ensure!(
        node.start_byte() == a.span.start_byte && node.end_byte() == a.span.end_byte,
        "fact node mismatch"
    );
    let mut proposals: Vec<(&str, Node<'_>)> = Vec::new();
    let mut current = node.parent();
    let mut statement = false;
    while let Some(n) = current {
        if n.id() == body.id() {
            break;
        }
        if boundary(n) {
            break;
        }
        if !statement
            && matches!(
                n.kind(),
                "let_declaration"
                    | "expression_statement"
                    | "return_expression"
                    | "assignment_expression"
                    | "compound_assignment_expr"
            )
        {
            proposals.push(("statement", n));
            statement = true;
            if let Some(p) = n.child_by_field_name("pattern") {
                proposals.push(("binding", p));
            }
        }
        if matches!(n.kind(), "if_expression" | "while_expression")
            && let Some(condition) = n.child_by_field_name("condition")
        {
            let is_else = n.child_by_field_name("alternative").is_some_and(|b| {
                b.start_byte() <= a.span.start_byte && a.span.end_byte <= b.end_byte()
            });
            proposals.push((if is_else { "else guard" } else { "guard" }, condition));
        }
        if n.kind() == "match_expression"
            && let Some(value) = n.child_by_field_name("value")
        {
            proposals.push(("matched value", value));
        }
        if n.kind() == "match_arm"
            && let Some(pattern) = n.child_by_field_name("pattern")
        {
            proposals.push(("match pattern", pattern));
        }
        current = n.parent();
    }
    // A single unshadowed direct-body immutable binding, with an earlier declaration.
    let mut binding_counts = BTreeMap::<&str, usize>::new();
    for n in nodes {
        if let Some(p) = n.child_by_field_name("pattern") {
            let mut pending = vec![p];
            while let Some(id) = pending.pop() {
                if id.kind() == "identifier" {
                    *binding_counts.entry(&source[id.byte_range()]).or_default() += 1;
                }
                pending.extend(children(id));
            }
        }
    }
    for n in children(body)
        .into_iter()
        .filter(|n| n.kind() == "let_declaration" && n.end_byte() <= a.span.start_byte)
    {
        let (Some(p), Some(_)) = (
            n.child_by_field_name("pattern"),
            n.child_by_field_name("value"),
        ) else {
            continue;
        };
        let name = &source[p.byte_range()];
        if p.kind() == "identifier"
            && !children(n).iter().any(|n| n.kind() == "mutable_specifier")
            && binding_counts.get(name) == Some(&1)
            && nodes.iter().any(|id| {
                id.kind() == "identifier"
                    && a.span.start_byte <= id.start_byte()
                    && id.end_byte() <= a.span.end_byte
                    && &source[id.byte_range()] == name
            })
        {
            proposals.push(("local binding", n));
            break;
        }
    }
    let mut selected: Vec<Fragment> = Vec::new();
    let mut bytes = prefix.len() + 1 + a.source.len();
    for (relation, n) in proposals {
        if selected.len() == 4 {
            break;
        }
        if n.end_byte() - n.start_byte() > 256
            || (a.span.start_byte <= n.start_byte() && n.end_byte() <= a.span.end_byte)
            || selected
                .iter()
                .any(|f| f.span.start_byte <= n.start_byte() && n.end_byte() <= f.span.end_byte)
        {
            continue;
        }
        let text = &source[n.byte_range()];
        let addition = relation.len() + 3 + text.len();
        if bytes + addition > 1024 {
            continue;
        }
        ensure!(
            body.start_byte() <= n.start_byte() && n.end_byte() <= body.end_byte(),
            "context outside function body"
        );
        selected.push(Fragment {
            relation: relation.into(),
            span: span(n),
            source: text.into(),
        });
        bytes += addition;
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(source: &str, query: &str) -> Views {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let function = tree.root_node().named_child(0).unwrap();
        let owner = span(function);
        build_for_query(
            &Owner {
                location_id: 1,
                source_hash: "fixture",
                symbol: Some("scenario"),
                source_role: "test",
                span: Some(&owner),
                status: "current",
                evidence_truncated: false,
            },
            source,
            &tree,
            &crate::text::query_terms(query),
        )
        .unwrap()
    }

    #[test]
    fn late_assertions_and_branches_survive_setup_with_a_fixed_encoding_budget() {
        let setup = (0..40)
            .map(|i| format!("let value_{i} = prepare_{i}();\n"))
            .collect::<String>();
        let source = format!(
            "fn scenario() {{\n{setup}\nif expired {{ release_lease(); }}\nassert_eq!(recovered_bytes, retained_bytes);\n}}"
        );
        let result = facts(&source, "release lease recovered retained bytes");
        assert_eq!(result.facts.len(), 16);
        assert!(
            result
                .facts
                .iter()
                .any(|f| f.observation.source == "release_lease()")
        );
        assert!(
            result
                .facts
                .iter()
                .any(|f| f.observation.source == "assert_eq!(recovered_bytes, retained_bytes)")
        );
        assert!(result.facts.iter().all(|f| f.contextual.len() <= 1024));
    }

    #[test]
    fn multiline_assertion_is_whole_source_and_macro_tokens_stay_opaque() {
        let source = format!(
            "fn scenario() {{\nassert_eq!(\n observed_rows.collect::<Vec<_>>(),\n expected_rows.collect::<Vec<_>>(),\n \"{}\"\n);\n}}",
            "π".repeat(60)
        );
        let result = facts(&source, "observed expected rows");
        let assertion = result
            .facts
            .iter()
            .find(|f| f.observation.operation == "assertion macro invocation")
            .unwrap();
        let a = &assertion.observation;
        assert!(a.source.len() > 128 && a.source.len() <= 512);
        assert_eq!(&source[a.span.start_byte..a.span.end_byte], a.source);
        assert!(a.source.starts_with("assert_eq!("));
        assert!(a.source.ends_with(')'));
        assert!(
            !result
                .reservoir
                .iter()
                .any(|f| f.observation.operation == "call")
        );
    }

    #[test]
    fn oversized_assertions_and_deferred_bodies_are_not_partially_inspected() {
        let source = format!(
            "fn scenario() {{\nassert!(\"{}\", hidden_call());\nlet later = || {{ assert!(hidden_closure()); }};\ncustom! {{ assert!(hidden_macro()); }};\n}}",
            "x".repeat(600)
        );
        let result = facts(&source, "hidden assertion");
        assert!(
            !result
                .reservoir
                .iter()
                .any(|f| f.observation.operation == "assertion macro invocation")
        );
        assert!(
            !result
                .reservoir
                .iter()
                .any(|f| f.observation.operation == "call")
        );
        assert!(result.oversized_observations > 0);
    }
}
