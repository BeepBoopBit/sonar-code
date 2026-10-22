//! Bounded syntax observations; names do not prove resolved types or runtime data flow.
use crate::text;
use serde::{Deserialize, Serialize};
use tree_sitter::Node;

pub(crate) const POLICY: &str =
    "quantity-structure-v1|bindings-receivers-calls4|duration-spelling|weighted-rrf60";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuantityStructure {
    pub role: String,
    pub binding: String,
    pub receiver: String,
    pub callees: Vec<String>,
    pub argument: Option<usize>,
    pub unit_basis: String,
    pub strength: f64,
}

fn name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "self"
        | "scoped_identifier"
        | "scoped_type_identifier" => {
            let s = &source[node.byte_range()];
            (s.chars().count() <= 256).then(|| s.to_owned())
        }
        "field_expression" => {
            let left = name(node.child_by_field_name("value")?, source)?;
            let right = name(node.child_by_field_name("field")?, source)?;
            let s = format!("{left}.{right}");
            (s.chars().count() <= 256).then_some(s)
        }
        _ => None,
    }
}

fn duration(callee: &str) -> Option<u64> {
    match callee {
        "Duration::from_secs"
        | "std::time::Duration::from_secs"
        | "core::time::Duration::from_secs" => Some(1000),
        "Duration::from_millis"
        | "std::time::Duration::from_millis"
        | "core::time::Duration::from_millis" => Some(1),
        _ => None,
    }
}

fn named_unit(s: &str) -> (&'static str, u64) {
    let terms = text::searchable(s);
    let words: Vec<_> = terms.split_whitespace().collect();
    if words.contains(&"per") {
        return ("unknown", 1);
    }
    match s
        .rsplit(['.', ':', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "byte" | "bytes" => ("bytes", 1),
        "ms" | "millis" | "millisecond" | "milliseconds" => ("milliseconds", 1),
        "secs" | "second" | "seconds" => ("milliseconds", 1000),
        _ => ("unknown", 1),
    }
}

pub(crate) fn observe(node: Node<'_>, source: &str) -> (QuantityStructure, &'static str, u64) {
    let mut evidence = QuantityStructure {
        role: "literal".into(),
        unit_basis: "unknown".into(),
        ..Default::default()
    };
    let mut current = node;
    let mut multiplier = None;
    // Associate only transparent parentheses and recognized argument wrappers with a binding.
    for _ in 0..8 {
        let Some(parent) = current.parent() else {
            break;
        };
        let field = match parent.kind() {
            "const_item" | "static_item" => Some(("name", "constant")),
            "let_declaration" => Some(("pattern", "local")),
            "field_initializer" => Some(("field", "field")),
            "assignment_expression" if parent.child_by_field_name("right") == Some(current) => {
                Some(("left", "assignment"))
            }
            _ => None,
        };
        if let Some((field, role)) = field {
            evidence.binding = parent
                .child_by_field_name(field)
                .and_then(|n| name(n, source))
                .unwrap_or_default();
            if evidence.role == "literal" {
                evidence.role = role.into();
            }
            break;
        }
        if parent.kind() == "parenthesized_expression" {
            current = parent;
            continue;
        }
        if parent.kind() != "arguments" {
            break;
        }
        let Some(call) = parent.parent().filter(|n| n.kind() == "call_expression") else {
            break;
        };
        let Some(function) = call.child_by_field_name("function") else {
            break;
        };
        evidence.argument.get_or_insert_with(|| {
            (0..parent.named_child_count())
                .position(|i| parent.named_child(i) == Some(current))
                .unwrap_or(0)
        });
        if parent.named_child_count() != 1 {
            break;
        }
        let callee = name(function, source).unwrap_or_default();
        if let Some(scale) = duration(&callee) {
            if multiplier.is_some() {
                break;
            }
            multiplier = Some(scale);
            evidence.role = "duration_argument".into();
            current = call;
            continue;
        }
        if function.kind() == "field_expression"
            && function
                .child_by_field_name("field")
                .is_some_and(|n| &source[n.byte_range()] == "unwrap_or")
        {
            evidence.receiver = function
                .child_by_field_name("value")
                .and_then(|n| name(n, source))
                .unwrap_or_default();
            evidence.role = "fallback_argument".into();
            current = call;
            continue;
        }
        break;
    }
    let mut ancestor = node.parent();
    for _ in 0..16 {
        let Some(parent) = ancestor else { break };
        if matches!(
            parent.kind(),
            "block" | "let_declaration" | "expression_statement" | "const_item" | "static_item"
        ) {
            break;
        }
        if parent.kind() == "call_expression"
            && let Some(callee) = parent
                .child_by_field_name("function")
                .and_then(|n| name(n, source))
        {
            evidence.callees.push(callee);
            if evidence.callees.len() == 4 {
                break;
            }
        }
        ancestor = parent.parent();
    }
    if let Some(scale) = multiplier {
        evidence.unit_basis = "duration_constructor_spelling".into();
        return (evidence, "milliseconds", scale);
    }
    let binding = named_unit(&evidence.binding);
    let receiver = named_unit(&evidence.receiver);
    let (unit, scale) = if binding.0 != "unknown" && receiver.0 != "unknown" && binding != receiver
    {
        ("unknown", 1)
    } else if receiver.0 != "unknown" {
        evidence.unit_basis = "receiver_name".into();
        receiver
    } else if binding.0 != "unknown" {
        evidence.unit_basis = "binding_name".into();
        binding
    } else {
        ("unknown", 1)
    };
    (evidence, unit, scale)
}

pub(crate) fn strength(
    evidence: &QuantityStructure,
    symbol: &str,
    terms: &[String],
    unit: &str,
) -> (f64, usize) {
    let fields = [
        (&evidence.binding, 2.0_f64),
        (&evidence.receiver, 2.0),
        (&evidence.callees.join(" "), 1.0),
        (&symbol.to_owned(), 0.5),
    ];
    let fields: Vec<_> = fields
        .iter()
        .map(|(s, w)| (text::searchable(s), *w))
        .collect();
    let mut matched = 0;
    let sum: f64 = terms
        .iter()
        .map(|t| {
            let weight = fields
                .iter()
                .filter(|(s, _)| s.split_whitespace().any(|w| w == t))
                .map(|(_, w)| *w)
                .fold(0.0_f64, f64::max);
            matched += usize::from(weight > 0.0);
            weight
        })
        .sum();
    (
        sum / (2.0 * terms.len().max(1) as f64) * if unit == "unknown" { 0.5 } else { 1.0 },
        matched,
    )
}
