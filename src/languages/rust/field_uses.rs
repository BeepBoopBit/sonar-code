//! Occurrence-level field navigation. Compiler targets and syntax associations stay distinct.
use crate::{IndexState, NavigationLocation, Notice, SourceSpan, explore, search};
use anyhow::{Context, Result, ensure};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tree_sitter::{Node, Parser};

const POLICY: &str = "field-uses-v1";
const MAX_ASSOCIATIONS: usize = 8;
const MAX_UNIT_BYTES: usize = 4096;
const MAX_LINES: usize = 16;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
pub struct FieldUseOptions {
    /// Restricts occurrence paths, not the paths of referenced declarations.
    pub scope: Option<String>,
    pub limit: usize,
    pub cursor: Option<String>,
}
impl Default for FieldUseOptions {
    fn default() -> Self {
        Self {
            scope: None,
            limit: 20,
            cursor: None,
        }
    }
}

/// Syntactic position, not a claim about runtime effects, aliasing, or macro expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldAccess {
    Read,
    Write,
    ReadWrite,
    Initialize,
    Pattern,
    Borrow,
    BorrowMut,
    Receiver,
    Projection,
    Reference,
}

#[derive(Debug, Serialize)]
pub struct FieldEvidence {
    pub path: String,
    pub span: SourceSpan,
}

#[derive(Debug, Serialize)]
pub struct FieldAssociation {
    /// Compiler-resolved field or callee handle. The association itself is syntactic.
    pub target: String,
    /// same_expression, call, or local_binding; none proves equivalent values.
    pub relation: String,
    pub evidence: FieldEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<FieldEvidence>,
}

#[derive(Debug, Serialize)]
pub struct FieldUse {
    pub field: String,
    pub owner: String,
    pub access: FieldAccess,
    pub evidence: FieldEvidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<SourceSpan>,
    pub associations: Vec<FieldAssociation>,
    /// Opaque syntax, inspection bounds, and omitted associations are explicit.
    pub limitations: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct FieldUseResponse {
    pub version: u32,
    pub anchor: NavigationLocation,
    pub uses: Vec<FieldUse>,
    pub locations: Vec<NavigationLocation>,
    pub index: IndexState,
    pub notices: Vec<Notice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug)]
struct Occurrence {
    owner: i64,
    field: i64,
    path: String,
    start: usize,
    end: usize,
}

fn span(source: &str, start: usize, end: usize) -> SourceSpan {
    let line = |byte| {
        source.as_bytes()[..byte]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1
    };
    SourceSpan {
        start_byte: start,
        end_byte: end,
        start_line: line(start),
        end_line: line(end.saturating_sub(1).max(start)),
    }
}
fn evidence(source: &str, path: &str, start: usize, end: usize) -> FieldEvidence {
    FieldEvidence {
        path: path.into(),
        span: span(source, start, end),
    }
}
fn fits(node: Node<'_>) -> bool {
    node.end_byte() - node.start_byte() <= MAX_UNIT_BYTES
        && node.end_position().row - node.start_position().row < MAX_LINES
}
fn contains(outer: Node<'_>, inner: Node<'_>) -> bool {
    outer.start_byte() <= inner.start_byte() && inner.end_byte() <= outer.end_byte()
}
fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.named_children(&mut c).collect()
}
fn opaque(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "macro_invocation"
            | "macro_definition"
            | "closure_expression"
            | "async_block"
            | "gen_block"
            | "const_block"
            | "function_item"
            | "impl_item"
            | "mod_item"
            | "const_item"
            | "static_item"
    )
}

// Destructuring assignment can put a field place inside tuple/array/struct syntax.
// Stop at calls, indexing and dereferences: their operands are not assigned storage.
fn assigned(mut node: Node<'_>) -> Option<FieldAccess> {
    for _ in 0..64 {
        let parent = node.parent()?;
        if matches!(
            parent.kind(),
            "assignment_expression" | "compound_assignment_expr"
        ) {
            return parent
                .child_by_field_name("left")
                .filter(|n| n.id() == node.id())
                .map(|_| {
                    if parent.kind() == "assignment_expression" {
                        FieldAccess::Write
                    } else {
                        FieldAccess::ReadWrite
                    }
                });
        }
        if !matches!(
            parent.kind(),
            "parenthesized_expression"
                | "tuple_expression"
                | "array_expression"
                | "field_initializer"
                | "shorthand_field_initializer"
                | "field_initializer_list"
                | "struct_expression"
        ) {
            return None;
        }
        node = parent;
    }
    None
}

fn access(mut node: Node<'_>, source: &str) -> FieldAccess {
    let Some(parent) = node.parent() else {
        return FieldAccess::Reference;
    };
    match parent.kind() {
        "field_initializer" | "shorthand_field_initializer"
            if parent
                .child_by_field_name("field")
                .is_some_and(|f| f.id() == node.id())
                || parent.kind() == "shorthand_field_initializer" =>
        {
            return if assigned(parent).is_some() {
                FieldAccess::Pattern
            } else {
                FieldAccess::Initialize
            };
        }
        "field_pattern" | "shorthand_field_identifier" | "struct_pattern" => {
            return FieldAccess::Pattern;
        }
        "field_expression"
            if parent
                .child_by_field_name("field")
                .is_some_and(|f| f.id() == node.id()) =>
        {
            node = parent
        }
        _ => return FieldAccess::Reference,
    }
    // Parentheses preserve place identity. Indexing, dereferencing and subfield projection do not.
    while let Some(parent) = node
        .parent()
        .filter(|p| p.kind() == "parenthesized_expression")
    {
        node = parent;
    }
    if let Some(access) = assigned(node) {
        return access;
    }
    let Some(parent) = node.parent() else {
        return FieldAccess::Read;
    };
    match parent.kind() {
        "assignment_expression" | "compound_assignment_expr"
            if parent
                .child_by_field_name("left")
                .is_some_and(|n| n.id() == node.id()) =>
        {
            if parent.kind() == "assignment_expression" {
                FieldAccess::Write
            } else {
                FieldAccess::ReadWrite
            }
        }
        "reference_expression" => {
            if children(parent)
                .iter()
                .any(|n| n.kind() == "mutable_specifier")
                || source[parent.byte_range()].starts_with("&mut ")
            {
                FieldAccess::BorrowMut
            } else {
                FieldAccess::Borrow
            }
        }
        "field_expression" => {
            if parent.parent().is_some_and(|p| {
                p.kind() == "call_expression"
                    && p.child_by_field_name("function")
                        .is_some_and(|f| f.id() == parent.id())
            }) {
                FieldAccess::Receiver
            } else {
                FieldAccess::Projection
            }
        }
        "call_expression"
            if parent
                .child_by_field_name("function")
                .is_some_and(|f| f.id() == node.id()) =>
        {
            FieldAccess::Receiver
        }
        "index_expression" if parent.named_child(0).is_some_and(|n| n.id() == node.id()) => {
            FieldAccess::Projection
        }
        _ => FieldAccess::Read,
    }
}

/// Smallest enclosing statement/initializer. Never cross a deferred-body boundary.
fn unit(mut node: Node<'_>) -> Node<'_> {
    let original = node;
    for _ in 0..64 {
        if matches!(
            node.kind(),
            "field_initializer"
                | "shorthand_field_initializer"
                | "assignment_expression"
                | "compound_assignment_expr"
                | "let_declaration"
                | "return_expression"
                | "expression_statement"
                | "match_arm"
        ) {
            return node;
        }
        let Some(parent) = node.parent() else { break };
        if opaque(parent) || parent.kind() == "block" {
            break;
        }
        node = parent;
    }
    if node.end_byte() - node.start_byte() <= MAX_UNIT_BYTES {
        node
    } else {
        original
    }
}

/// Collect a complete bounded owner inspection. Macro/deferred internals never participate.
fn inspect(owner: Node<'_>) -> Option<Vec<Node<'_>>> {
    if owner.end_byte() - owner.start_byte() > 256 * 1024 {
        return None;
    }
    let mut pending = vec![(owner, 0)];
    let mut nodes = Vec::new();
    while let Some((node, depth)) = pending.pop() {
        if nodes.len() == 20_000 || depth > 128 {
            return None;
        }
        nodes.push(node);
        if node.id() != owner.id() && opaque(node) {
            continue;
        }
        pending.extend(children(node).into_iter().rev().map(|n| (n, depth + 1)));
    }
    Some(nodes)
}

/// One immutable direct-body local links source occurrences, not runtime values.
/// Reject shadowing anywhere in the inspected owner; do not chase aliases or cross closures.
fn binding_units<'a>(
    owner: Node<'a>,
    occurrence: Node<'a>,
    site: Node<'a>,
    source: &str,
    limitations: &mut Vec<String>,
) -> Option<Vec<(Node<'a>, Node<'a>)>> {
    let nodes = inspect(owner)?;
    let Some(body) = owner.child_by_field_name("body") else {
        return Some(vec![]);
    };
    let mut counts = BTreeMap::<&str, usize>::new();
    for node in &nodes {
        if let Some(pattern) = node.child_by_field_name("pattern") {
            let mut pending = vec![pattern];
            while let Some(n) = pending.pop() {
                if n.kind() == "identifier" {
                    *counts.entry(&source[n.byte_range()]).or_default() += 1;
                }
                pending.extend(children(n));
            }
        }
    }
    let mut result = Vec::new();
    for binding in children(body)
        .into_iter()
        .filter(|n| n.kind() == "let_declaration")
    {
        let (Some(pattern), Some(value)) = (
            binding.child_by_field_name("pattern"),
            binding.child_by_field_name("value"),
        ) else {
            continue;
        };
        let name = &source[pattern.byte_range()];
        if pattern.kind() != "identifier" {
            continue;
        }
        let refs: Vec<_> = nodes
            .iter()
            .filter(|n| {
                n.kind() == "identifier"
                    && n.start_byte() >= binding.end_byte()
                    && &source[n.byte_range()] == name
                    && !n.parent().is_some_and(|p| {
                        matches!(p.kind(), "scoped_identifier" | "scoped_type_identifier")
                    })
            })
            .copied()
            .collect();
        if !contains(value, occurrence) && !refs.iter().any(|n| contains(site, *n)) {
            continue;
        }
        let reason = if counts.get(name) != Some(&1) {
            Some("shadowed_local_binding")
        } else if children(binding)
            .iter()
            .any(|n| n.kind() == "mutable_specifier")
        {
            Some("mutable_local_binding")
        } else if binding.child_by_field_name("alternative").is_some() {
            Some("conditional_local_binding")
        } else if !fits(binding) {
            Some("binding_context_budget")
        } else {
            None
        };
        if let Some(reason) = reason {
            limitations.push(reason.into());
            continue;
        }
        if contains(value, occurrence) {
            for reference in refs {
                result.push((unit(reference), binding));
            }
        } else if refs.iter().any(|n| contains(site, *n)) {
            result.push((binding, binding));
        }
        if result.len() > 64 {
            return None;
        }
    }
    Some(result)
}

fn enrich(
    s: &explore::Snapshot<'_>,
    occurrence: &Occurrence,
    root: Node<'_>,
    locations: &mut BTreeSet<i64>,
    use_: &mut FieldUse,
) -> Result<()> {
    let source = &s.sources[&occurrence.path].source;
    let Some(node) = root
        .descendant_for_byte_range(occurrence.start, occurrence.end)
        .filter(|n| n.start_byte() == occurrence.start && n.end_byte() == occurrence.end)
    else {
        use_.limitations.push("unsupported_occurrence".into());
        return Ok(());
    };
    let mut parent = node.parent();
    let mut depth = 0;
    while let Some(n) = parent {
        if n.kind() == "macro_invocation" {
            if fits(n) {
                use_.context = Some(span(source, n.start_byte(), n.end_byte()));
            } else {
                use_.limitations.push("context_budget".into());
            }
            use_.limitations.push("opaque_macro".into());
            return Ok(());
        }
        if matches!(
            n.kind(),
            "closure_expression" | "async_block" | "gen_block" | "const_block"
        ) {
            use_.limitations.push("deferred_body".into());
            break;
        }
        if n.kind() == "function_item" {
            break;
        }
        depth += 1;
        if depth > 128 {
            use_.limitations.push("ancestor_limit".into());
            return Ok(());
        }
        parent = n.parent();
    }
    use_.access = access(node, source);
    let selected = unit(node);
    if fits(selected) {
        use_.context = Some(span(source, selected.start_byte(), selected.end_byte()));
    } else {
        use_.limitations.push("context_budget".into());
    }
    if use_.limitations.iter().any(|l| l == "deferred_body") {
        return Ok(());
    }
    let (start, end): (usize, usize) = s.conn.query_row(
        "SELECT start_byte,end_byte FROM locations WHERE id=?1",
        [occurrence.owner],
        |r| Ok((r.get::<_, u32>(0)? as usize, r.get::<_, u32>(1)? as usize)),
    )?;
    let owner = root
        .descendant_for_byte_range(start, end)
        .context("missing field-use owner")?;
    let mut units = vec![(selected, None)];
    match binding_units(owner, node, selected, source, &mut use_.limitations) {
        Some(bound) => units.extend(bound.into_iter().map(|(n, b)| (n, Some(b)))),
        None => use_.limitations.push("binding_inspection_limit".into()),
    }
    let mut seen = BTreeSet::new();
    for (unit, binding) in units {
        if unit.end_byte() - unit.start_byte() > MAX_UNIT_BYTES {
            use_.limitations.push("association_unit_budget".into());
            continue;
        }
        let mut statement = s.conn.prepare("SELECT e.to_id,e.kind,e.start_byte,e.end_byte FROM semantic_edges e JOIN locations l ON l.id=e.to_id
            WHERE e.from_id=?1 AND e.path=?2 AND e.start_byte>=?3 AND e.end_byte<=?4
            AND ((l.kind='field' AND e.kind='references') OR e.kind='calls')
            ORDER BY e.start_byte,e.end_byte,e.to_id LIMIT 65")?;
        let edges = statement
            .query_map(
                params![
                    occurrence.owner,
                    occurrence.path,
                    unit.start_byte() as i64,
                    unit.end_byte() as i64
                ],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, u32>(2)? as usize,
                        r.get::<_, u32>(3)? as usize,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if edges.len() == 65 {
            use_.limitations.push("association_scan_limit".into());
        }
        for (target, kind, a, b) in edges.into_iter().take(64) {
            let Some(mut syntax) = root.descendant_for_byte_range(a, b) else {
                continue;
            };
            let mut immediate = true;
            while syntax.id() != unit.id() {
                if opaque(syntax) {
                    immediate = false;
                    break;
                }
                let Some(parent) = syntax.parent() else {
                    immediate = false;
                    break;
                };
                syntax = parent;
            }
            if !immediate {
                continue;
            }
            if target == occurrence.field && a == occurrence.start && b == occurrence.end {
                continue;
            }
            let relation = if binding.is_some() {
                "local_binding"
            } else if kind == "calls" {
                "call"
            } else {
                "same_expression"
            };
            if !seen.insert((target, a, b, relation)) {
                continue;
            }
            if use_.associations.len() == MAX_ASSOCIATIONS {
                use_.limitations.push("association_limit".into());
                break;
            }
            // The source occurrence is compiler-backed; the enclosing syntax is the only association proof.
            ensure!(
                a < b
                    && b <= source.len()
                    && source.is_char_boundary(a)
                    && source.is_char_boundary(b),
                "invalid field association span"
            );
            locations.insert(target);
            use_.associations.push(FieldAssociation {
                target: explore::handle(s.conn, &s.generation, target)?,
                relation: relation.into(),
                evidence: evidence(source, &occurrence.path, a, b),
                binding: binding
                    .map(|n| evidence(source, &occurrence.path, n.start_byte(), n.end_byte())),
            });
        }
    }
    use_.limitations.sort();
    use_.limitations.dedup();
    Ok(())
}

/// Read exact compiler-recorded field occurrences. Existing retrieval and graph edges are unchanged.
pub fn field_uses(db: &Path, anchor: &str, options: &FieldUseOptions) -> Result<FieldUseResponse> {
    let scope = search::normalize_scope(options.scope.as_deref())?;
    explore::read(db, |s| {
        let id = s.resolve(anchor)?;
        let location = s.location(id)?;
        ensure!(
            matches!(location.kind.as_str(), "field" | "function"),
            "field-uses requires a field or function handle"
        );
        let request = explore::request((
            &s.generation,
            POLICY,
            anchor,
            &scope,
            &s.index.semantic_profile,
            &s.index.semantic_freshness,
        ))?;
        let offset = explore::page(options.cursor.as_deref(), &request, options.limit)?;
        let mut result = FieldUseResponse {
            version: 1,
            anchor: location,
            uses: vec![],
            locations: vec![],
            index: s.response().index,
            notices: vec![],
            next_cursor: None,
        };
        if !s.semantic_for(id)? {
            result.notices.push(s.graph_notice(id)?);
            return Ok(result);
        }
        result.notices.push(Notice::new("field_use_coverage", "Targets are compiler-resolved; access labels and associations describe syntax, not runtime data flow. Unmapped, external, generated, and ambiguous occurrences may be absent."));
        let mut statement = s.conn.prepare("SELECT e.from_id,e.to_id,e.path,e.start_byte,e.end_byte FROM semantic_edges e JOIN locations l ON l.id=e.to_id
            WHERE l.kind='field' AND e.kind='references' AND ((?2='field' AND e.to_id=?1) OR (?2='function' AND e.from_id=?1))
            AND (?3='' OR e.path=?3 OR substr(e.path,1,length(?3)+1)=?3||'/')
            ORDER BY e.path,e.start_byte,e.end_byte,e.to_id,e.from_id LIMIT ?4 OFFSET ?5")?;
        let rows = statement
            .query_map(
                params![
                    id,
                    result.anchor.kind,
                    scope,
                    (options.limit + 1) as i64,
                    offset as i64
                ],
                |r| {
                    Ok(Occurrence {
                        owner: r.get(0)?,
                        field: r.get(1)?,
                        path: r.get(2)?,
                        start: r.get::<_, u32>(3)? as usize,
                        end: r.get::<_, u32>(4)? as usize,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if rows.len() > options.limit {
            result.next_cursor = Some(format!("sc1:{request}:{}", offset + options.limit));
        }
        let mut parser = Parser::new();
        parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
        let mut trees = BTreeMap::new();
        let mut locations = BTreeSet::new();
        for occurrence in rows.into_iter().take(options.limit) {
            let source = &s.sources[&occurrence.path].source;
            ensure!(
                occurrence.start < occurrence.end
                    && occurrence.end <= source.len()
                    && source.is_char_boundary(occurrence.start)
                    && source.is_char_boundary(occurrence.end),
                "invalid compiler field occurrence"
            );
            let mut use_ = FieldUse {
                field: explore::handle(s.conn, &s.generation, occurrence.field)?,
                owner: explore::handle(s.conn, &s.generation, occurrence.owner)?,
                access: FieldAccess::Reference,
                evidence: evidence(source, &occurrence.path, occurrence.start, occurrence.end),
                context: None,
                associations: vec![],
                limitations: vec![],
            };
            locations.extend([occurrence.field, occurrence.owner]);
            if crate::source_language(&occurrence.path) == Some("typescript") {
                crate::languages::typescript::semantic::enrich(
                    s,
                    occurrence.owner,
                    occurrence.field,
                    &occurrence.path,
                    [occurrence.start, occurrence.end],
                    &mut locations,
                    &mut use_,
                )?;
            } else if source.len() > 2 * 1024 * 1024 {
                use_.limitations.push("syntax_file_budget".into());
            } else {
                if !trees.contains_key(&occurrence.path) {
                    trees.insert(
                        occurrence.path.clone(),
                        parser
                            .parse(source, None)
                            .context("field-use parser returned no tree")?,
                    );
                }
                let root = trees[&occurrence.path].root_node();
                if root.has_error() {
                    use_.limitations.push("parse_error".into());
                } else {
                    enrich(s, &occurrence, root, &mut locations, &mut use_)?;
                }
            }
            result.uses.push(use_);
        }
        locations.remove(&id);
        result.locations = locations
            .into_iter()
            .map(|id| s.location(id))
            .collect::<Result<_>>()?;
        if result.uses.is_empty() {
            result.notices.push(Notice::new("no_field_uses", "No compiler-recorded field uses match this anchor and use-site scope; this is not proof that the field is unused."));
        }
        Ok(result)
    })
}
