//! Conservative, local syntax paths from an observed value to its direct use.
//! A call consumes its argument here; its return value is never assumed identical.
use serde::{Deserialize, Serialize};
use tree_sitter::Node;

pub(crate) const POLICY: &str =
    "value-use-v1|immutable-aliases3|statements16|steps8|chains4|chars1024";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueUseStep {
    pub kind: String,
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: usize,
    pub end_line: usize,
    pub argument: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValueUse {
    pub chains: Vec<Vec<ValueUseStep>>,
    pub bounded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValueUseMatch {
    pub chain: Vec<ValueUseStep>,
    pub coverage: f64,
    pub semantic: Option<f64>,
    pub strength: f64,
    pub bounded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contribution: Option<LocalContribution>,
}

/// Frozen, query-relative addition to the unified score; not confidence.
#[derive(Debug, Clone, Serialize)]
pub struct LocalContribution {
    pub base_score: f64,
    pub median: f64,
    pub addition: f64,
}

fn name(node: Node<'_>, source: &str) -> Option<String> {
    let valid = match node.kind() {
        "identifier"
        | "field_identifier"
        | "self"
        | "scoped_identifier"
        | "scoped_type_identifier" => true,
        "field_expression" => {
            node.child_by_field_name("value")
                .is_some_and(|n| name(n, source).is_some())
                && node
                    .child_by_field_name("field")
                    .is_some_and(|n| name(n, source).is_some())
        }
        _ => false,
    };
    let s = &source[node.byte_range()];
    (valid && s.chars().count() <= 256).then(|| s.to_owned())
}

fn step(node: Node<'_>, kind: &str, name: String, argument: Option<usize>) -> ValueUseStep {
    ValueUseStep {
        kind: kind.into(),
        name,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        argument,
    }
}

fn immutable_local(node: Node<'_>) -> bool {
    node.kind() == "let_declaration"
        && node
            .child_by_field_name("pattern")
            .is_some_and(|p| p.kind() == "identifier")
        && node.child_by_field_name("alternative").is_none()
        && !(0..node.child_count()).any(|i| {
            node.child(i)
                .is_some_and(|c| c.kind() == "mutable_specifier")
        })
}

/// Scan only straight-line statements. Opaque constructs are barriers, even when
/// a macro's token stream happens to spell the tracked identifier.
fn references<'a>(
    node: Node<'a>,
    variable: &str,
    source: &str,
    out: &mut Vec<Node<'a>>,
    depth: usize,
) -> bool {
    if depth > 32
        || matches!(
            node.kind(),
            "block"
                | "if_expression"
                | "match_expression"
                | "while_expression"
                | "for_expression"
                | "loop_expression"
                | "closure_expression"
                | "async_block"
                | "macro_invocation"
                | "macro_definition"
                | "function_item"
                | "const_item"
                | "static_item"
                | "assignment_expression"
                | "compound_assignment_expr"
        )
    {
        return false;
    }
    if node.kind() == "identifier" && &source[node.byte_range()] == variable {
        out.push(node);
        return out.len() <= 4;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .all(|n| references(n, variable, source, out, depth + 1))
}

struct Builder<'a> {
    source: &'a str,
    result: ValueUse,
}

impl Builder<'_> {
    fn finish(&mut self, chain: Vec<ValueUseStep>) {
        if self.result.chains.len() < 4 {
            self.result.chains.push(chain);
        } else {
            self.result.bounded = true;
        }
    }

    fn push(&mut self, chain: &mut Vec<ValueUseStep>, value: ValueUseStep) -> bool {
        if chain.len() >= 8
            || chain
                .iter()
                .map(|s| s.name.chars().count() + 1)
                .sum::<usize>()
                + value.name.chars().count()
                + 1
                > 1024
        {
            self.result.bounded = true;
            return false;
        }
        chain.push(value);
        true
    }

    fn follow(
        &mut self,
        declaration: Node<'_>,
        variable: &str,
        chain: Vec<ValueUseStep>,
        hops: usize,
    ) {
        if hops >= 3 {
            self.result.bounded = true;
            self.finish(chain);
            return;
        }
        if declaration.parent().is_none_or(|p| p.kind() != "block") {
            self.finish(chain);
            return;
        }
        let mut following = Vec::new();
        let mut next = declaration.next_named_sibling();
        while let Some(statement) = next {
            following.push(statement);
            if following.len() == 17 {
                break;
            }
            next = statement.next_named_sibling();
        }
        for statement in following.iter().take(16) {
            // A shadowing declaration terminates this binding, including `let x = x`.
            if statement.kind() == "let_declaration"
                && statement.child_by_field_name("pattern").is_some_and(|p| {
                    let mut names = Vec::new();
                    references(p, variable, self.source, &mut names, 0);
                    !names.is_empty()
                })
            {
                self.finish(chain);
                return;
            }
            let mut uses = Vec::new();
            if !references(*statement, variable, self.source, &mut uses, 0) {
                self.finish(chain);
                return;
            }
            if !uses.is_empty() {
                for usage in uses {
                    let mut next = chain.clone();
                    if self.push(&mut next, step(usage, "reference", variable.into(), None)) {
                        self.ascend(usage, next, hops + 1);
                    } else {
                        self.finish(next);
                    }
                }
                return;
            }
        }
        self.result.bounded |= following.len() > 16;
        self.finish(chain);
    }

    fn ascend(&mut self, mut node: Node<'_>, mut chain: Vec<ValueUseStep>, hops: usize) {
        for _ in 0..16 {
            let Some(parent) = node.parent() else {
                self.finish(chain);
                return;
            };
            if parent.kind() == "parenthesized_expression" {
                node = parent;
                continue;
            }
            let binding = match parent.kind() {
                "let_declaration" if parent.child_by_field_name("value") == Some(node) => {
                    Some(("pattern", "local"))
                }
                "const_item" | "static_item"
                    if parent.child_by_field_name("value") == Some(node) =>
                {
                    Some(("name", "constant"))
                }
                "field_initializer" if parent.child_by_field_name("value") == Some(node) => {
                    Some(("field", "field"))
                }
                "assignment_expression" if parent.child_by_field_name("right") == Some(node) => {
                    Some(("left", "assignment"))
                }
                _ => None,
            };
            if let Some((field, kind)) = binding {
                if let Some(binding) = parent.child_by_field_name(field)
                    && let Some(value) = name(binding, self.source)
                    && self.push(&mut chain, step(binding, kind, value.clone(), None))
                    && immutable_local(parent)
                {
                    self.follow(parent, &value, chain, hops);
                    return;
                }
                self.finish(chain);
                return;
            }
            if parent.kind() == "arguments"
                && let Some(call) = parent.parent().filter(|p| p.kind() == "call_expression")
                && let Some(function) = call.child_by_field_name("function")
                && let Some(callee) = name(function, self.source)
            {
                let argument = (0..parent.named_child_count())
                    .position(|i| parent.named_child(i) == Some(node));
                let normalized: String = callee.chars().filter(|c| !c.is_whitespace()).collect();
                if parent.named_child_count() == 1
                    && matches!(
                        normalized.as_str(),
                        "Duration::from_secs"
                            | "Duration::from_millis"
                            | "std::time::Duration::from_secs"
                            | "std::time::Duration::from_millis"
                            | "core::time::Duration::from_secs"
                            | "core::time::Duration::from_millis"
                    )
                {
                    if !self.push(&mut chain, step(function, "duration", callee, argument)) {
                        self.finish(chain);
                        return;
                    }
                    node = call;
                    continue;
                }
                if parent.named_child_count() == 1
                    && function.kind() == "field_expression"
                    && function
                        .child_by_field_name("field")
                        .is_some_and(|f| &self.source[f.byte_range()] == "unwrap_or")
                    && let Some(receiver) = function.child_by_field_name("value")
                    && let Some(value) = name(receiver, self.source)
                {
                    if !self.push(&mut chain, step(receiver, "fallback", value, argument)) {
                        self.finish(chain);
                        return;
                    }
                    node = call;
                    continue;
                }
                self.push(&mut chain, step(function, "argument", callee, argument));
                self.finish(chain);
                return;
            }
            self.finish(chain);
            return;
        }
        self.result.bounded = true;
        self.finish(chain);
    }
}

pub(crate) fn observe(node: Node<'_>, source: &str) -> ValueUse {
    let mut builder = Builder {
        source,
        result: ValueUse::default(),
    };
    builder.ascend(node, vec![step(node, "value", String::new(), None)], 0);
    builder.result
}

pub(crate) fn document(chain: &[ValueUseStep]) -> String {
    let mut names = Vec::new();
    for s in chain {
        if !matches!(s.kind.as_str(), "value" | "duration" | "reference")
            && !s.name.is_empty()
            && !names.contains(&s.name.as_str())
        {
            names.push(s.name.as_str());
        }
    }
    names.join(" ")
}

pub(crate) fn rank(
    conn: &rusqlite::Connection,
    candidates: &mut [crate::candidate::Candidate],
    query: &str,
    model: Option<&crate::Embedder>,
) -> anyhow::Result<()> {
    measure(conn, candidates, query, model)?;
    for candidate in candidates.iter_mut() {
        if let Some(local) = candidate.quantity.as_ref().and_then(|q| q.local.as_ref()) {
            candidate.score = local.strength / 61.0;
            candidate
                .measurements
                .as_mut()
                .expect("common measurements precede local scoring")
                .score = candidate.score;
        }
    }
    crate::candidate::sort(candidates);
    Ok(())
}

pub(crate) fn rank_bounded(
    conn: &rusqlite::Connection,
    candidates: &mut [crate::candidate::Candidate],
    query: &str,
    model: Option<&crate::Embedder>,
) -> anyhow::Result<()> {
    // The query pipeline also bypasses quantity retrieval for exact identities.
    if candidates.iter().any(crate::candidate::Candidate::exact) {
        return Ok(());
    }
    measure(conn, candidates, query, model)?;
    let mut strengths: Vec<_> = candidates
        .iter()
        .filter_map(|c| c.quantity.as_ref()?.local.as_ref().map(|l| l.strength))
        .collect();
    let Some(median) = median(&mut strengths) else {
        return Ok(());
    };
    for candidate in candidates.iter_mut() {
        if let Some(local) = candidate.quantity.as_mut().and_then(|q| q.local.as_mut()) {
            let addition = addition(local.strength, median);
            local.contribution = Some(LocalContribution {
                base_score: candidate.score,
                median,
                addition,
            });
            candidate.score += addition;
            candidate
                .measurements
                .as_mut()
                .expect("common measurements precede local scoring")
                .score = candidate.score;
        }
    }
    crate::candidate::sort(candidates);
    Ok(())
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

fn addition(strength: f64, median: f64) -> f64 {
    (0.25 / 61.0) * (strength - median).max(0.0)
}

fn measure(
    conn: &rusqlite::Connection,
    candidates: &mut [crate::candidate::Candidate],
    query: &str,
    model: Option<&crate::Embedder>,
) -> anyhow::Result<()> {
    let (context, terms) = crate::quantity::context_query(query).expect("active quantity query");
    let vector = model
        .map(|m| m.encode(&[context]))
        .transpose()?
        .map(|mut v| v.remove(0));
    let mut stmt = conn.prepare(
        "SELECT evidence FROM quantity_uses WHERE location_id=?1 AND start_byte=?2 AND end_byte=?3",
    )?;
    for candidate in candidates.iter_mut() {
        let Some(q) = candidate.quantity.as_mut() else {
            continue;
        };
        let raw: String = stmt.query_row(
            rusqlite::params![candidate.id, q.start_byte as i64, q.end_byte as i64],
            |r| r.get(0),
        )?;
        let evidence: ValueUse = serde_json::from_str(&raw)?;
        let mut best: Option<ValueUseMatch> = None;
        for chain in evidence.chains {
            let document = document(&chain);
            let tokens = crate::text::searchable(&document);
            let tokens: std::collections::BTreeSet<_> = tokens.split_whitespace().collect();
            let coverage = terms.iter().filter(|t| tokens.contains(t.as_str())).count() as f64
                / terms.len().max(1) as f64;
            // Each chain is encoded alone, independent of legacy tokenizer batch padding.
            let semantic = if document.is_empty() {
                model.map(|_| 0.0)
            } else if let (Some(model), Some(query)) = (model, &vector) {
                let value = model.encode(&[document])?.remove(0);
                Some(
                    query
                        .iter()
                        .zip(value)
                        .map(|(a, b)| f64::from(*a) * f64::from(b))
                        .sum::<f64>()
                        .clamp(0.0, 1.0),
                )
            } else {
                None
            };
            let strength = semantic.map_or(coverage, |s| (coverage + s) / 2.0)
                * if q.unit == "unknown" { 0.5 } else { 1.0 };
            if best.as_ref().is_none_or(|old| strength > old.strength) {
                best = Some(ValueUseMatch {
                    chain,
                    coverage,
                    semantic,
                    strength,
                    bounded: evidence.bounded,
                    contribution: None,
                });
            }
        }
        anyhow::ensure!(
            best.is_some(),
            "quantity use record has no source path; rebuild index-quantities"
        );
        q.local = best;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_addition_handles_empty_singleton_flat_and_even_distributions() {
        assert_eq!(median(&mut []), None);
        for mut values in [vec![0.8], vec![0.8, 0.8], vec![0.8, 0.8, 0.8]] {
            let m = median(&mut values).unwrap();
            assert!(values.iter().all(|v| addition(*v, m) == 0.0));
        }
        assert_eq!(median(&mut [0.9, 0.1, 0.7, 0.3]), Some(0.5));
        assert_eq!(median(&mut [0.9, 0.1, 0.4]), Some(0.4));
        assert_eq!(addition(0.3, 0.5), 0.0);
        assert_eq!(addition(1.0, 0.0), 0.25 / 61.0);
    }

    fn extract(source: &str) -> ValueUse {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        assert!(!tree.root_node().has_error(), "{source}");
        let at = source.find("65536").unwrap();
        let node = tree
            .root_node()
            .descendant_for_byte_range(at, at + 5)
            .unwrap();
        observe(node, source)
    }

    #[test]
    fn fallback_and_aliases_retain_direct_consumers_and_exact_coordinates() {
        let source = "// 🦀\r\nfn f() { let size = opts.max_bytes.unwrap_or(65536); let count = (size); read_output(count); }";
        let use_ = extract(source);
        assert_eq!(use_.chains.len(), 1);
        let chain = &use_.chains[0];
        assert_eq!(chain.last().unwrap().name, "read_output");
        assert_eq!(chain.last().unwrap().argument, Some(0));
        assert!(document(chain).contains("opts.max_bytes"));
        for step in chain {
            assert!(
                source.is_char_boundary(step.start_byte) && source.is_char_boundary(step.end_byte)
            );
            assert_eq!(
                source[..step.start_byte]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1,
                step.line
            );
            if !step.name.is_empty() {
                assert_eq!(&source[step.start_byte..step.end_byte], step.name);
            }
        }
        assert!(chain.len() <= 8);
    }

    #[test]
    fn aggregate_fields_and_opaque_call_returns_do_not_gain_enclosing_context() {
        let field =
            extract("fn f() { read_output(Config { stack_bytes: 65536, output_bytes: 1 }); }");
        assert_eq!(document(&field.chains[0]), "stack_bytes");
        let call = extract("fn f() { let bytes = opaque(65536); read_output(bytes); }");
        assert_eq!(document(&call.chains[0]), "opaque");
        let direct = extract("fn f() { read_output(0, options.max_bytes.unwrap_or(65536)); }");
        assert_eq!(direct.chains[0].last().unwrap().argument, Some(1));
        assert_eq!(document(&direct.chains[0]), "options.max_bytes read_output");
    }

    #[test]
    fn uncertain_mutation_shadowing_and_control_flow_stop_alias_propagation() {
        for source in [
            "fn f() { let mut n=65536; read_output(n); }",
            "fn f() { let n=65536; let n=0; read_output(n); }",
            "fn f() { let n=65536; let n=n; read_output(n); }",
            "fn f() { let n=65536; n=0; read_output(n); }",
            "fn f() { let n=65536; n+=1; read_output(n); }",
            "fn f() { let n=65536; let other=&n; read_output(other); }",
            "fn f() { let n=65536; let other=n+1; read_output(other); }",
            "fn f() { let n=65536; let other=opaque(n); read_output(other); }",
            "fn f() { let n=65536; if ok { read_output(n); } }",
            "fn f() { let n=65536; while ok { read_output(n); } }",
            "fn f() { let n=65536; let closure=||read_output(n); }",
            "fn f() { let n=65536; unrelated_macro!(); read_output(n); }",
        ] {
            let use_ = extract(source);
            assert!(
                !use_
                    .chains
                    .iter()
                    .any(|c| document(c).contains("read_output")),
                "{source}"
            );
        }
    }

    #[test]
    fn scanning_and_path_bounds_preserve_only_observed_prefixes() {
        let source = format!(
            "fn f() {{ let n=65536; {} read_output(n); }}",
            "unrelated();".repeat(17)
        );
        let use_ = extract(&source);
        assert!(use_.bounded);
        assert!(!document(&use_.chains[0]).contains("read_output"));
        let long =
            extract("fn f() { let a=65536; let b=a; let c=b; let d=c; let e=d; read_output(e); }");
        assert!(long.bounded && long.chains.iter().all(|c| c.len() <= 8));
        assert!(!document(&long.chains[0]).contains("read_output"));
    }
}
