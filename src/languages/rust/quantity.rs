//! Exact quantity evidence, built explicitly from current Rust syntax.
//! Values are observations of literal expressions, never runtime values or data-flow proofs.
use crate::{
    candidate::{Candidate, sort},
    store, text, workspace,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use tree_sitter::{Node, Parser};

const POLICY: &str = "rust-quantity-v1|unsigned-literal-arithmetic-depth8|local-name-unit|4-enclosing-calls256|exact-context|rrf60";
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS quantity_facts (
    location_id INTEGER NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    region_id INTEGER NOT NULL REFERENCES regions(id) ON DELETE CASCADE,
    value TEXT NOT NULL, unit TEXT NOT NULL, line INTEGER NOT NULL, end_line INTEGER NOT NULL,
    start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL, context TEXT NOT NULL,
    PRIMARY KEY(location_id,start_byte,end_byte));
    CREATE INDEX IF NOT EXISTS quantity_values ON quantity_facts(value);";

const STRUCTURED_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS quantity_structure (
    location_id INTEGER NOT NULL, start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
    value TEXT, unit TEXT NOT NULL, evidence TEXT NOT NULL,
    PRIMARY KEY(location_id,start_byte,end_byte),
    FOREIGN KEY(location_id,start_byte,end_byte) REFERENCES quantity_facts(location_id,start_byte,end_byte) ON DELETE CASCADE);
    CREATE INDEX IF NOT EXISTS quantity_structure_values ON quantity_structure(value);";

const USE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS quantity_uses (
    location_id INTEGER NOT NULL, start_byte INTEGER NOT NULL, end_byte INTEGER NOT NULL,
    evidence TEXT NOT NULL, PRIMARY KEY(location_id,start_byte,end_byte),
    FOREIGN KEY(location_id,start_byte,end_byte) REFERENCES quantity_facts(location_id,start_byte,end_byte) ON DELETE CASCADE);";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum QuantityQueryPolicy {
    #[default]
    Original,
    Canonical,
    Expand,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum QuantityRankingPolicy {
    #[default]
    Legacy,
    Structured,
    Unified,
    Local,
    BoundedLocal,
}

impl QuantityRankingPolicy {
    pub(crate) fn needs_local(self) -> bool {
        matches!(self, Self::Local | Self::BoundedLocal)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct QuantityOptions {
    pub query: QuantityQueryPolicy,
    pub ranking: QuantityRankingPolicy,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuantityEvidence {
    pub value: String,
    /// "unknown" means no unit was established from the local source binding.
    pub unit: String,
    pub line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub matched_terms: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure: Option<crate::QuantityStructure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<crate::ValueUseMatch>,
}

#[derive(Debug, Serialize)]
pub struct QuantityReport {
    pub snapshot: String,
    pub policy: String,
    pub facts: usize,
    pub skipped_parse_error_files: usize,
    pub unbound_expressions: usize,
}

struct Fact {
    value: u64,
    unit: &'static str,
    start: usize,
    end: usize,
    line: usize,
    end_line: usize,
    context: String,
    structured_value: Option<u64>,
    structured_unit: &'static str,
    structure: crate::QuantityStructure,
    value_use: crate::ValueUse,
}

fn integer(s: &str) -> Option<u64> {
    let raw = s.replace('_', "");
    let (digits, radix) = if let Some(s) = raw.strip_prefix("0x") {
        (s, 16)
    } else if let Some(s) = raw.strip_prefix("0o") {
        (s, 8)
    } else if let Some(s) = raw.strip_prefix("0b") {
        (s, 2)
    } else {
        (raw.as_str(), 10)
    };
    let end = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    let suffix = &digits[end..];
    let max = match suffix {
        "" | "u64" | "usize" | "u128" => u64::MAX,
        "u8" => u8::MAX as u64,
        "u16" => u16::MAX as u64,
        "u32" => u32::MAX as u64,
        _ => return None,
    };
    let value = u64::from_str_radix(&digits[..end], radix).ok()?;
    (value <= max).then_some(value)
}

fn value(node: Node<'_>, source: &str, depth: usize) -> Option<u64> {
    if depth > 8 {
        return None;
    }
    match node.kind() {
        "integer_literal" => integer(&source[node.byte_range()]),
        "parenthesized_expression" => value(node.named_child(0)?, source, depth + 1),
        "binary_expression" => {
            // Arithmetic on suffixed integers depends on type widths; leave it unknown.
            let raw = &source[node.byte_range()];
            if raw.contains("usize")
                || ["u8", "u16", "u32", "u64", "u128"]
                    .iter()
                    .any(|s| raw.contains(s))
            {
                return None;
            }
            let a = value(node.child_by_field_name("left")?, source, depth + 1)?;
            let b = value(node.child_by_field_name("right")?, source, depth + 1)?;
            let op = node.child_by_field_name("operator")?;
            match &source[op.byte_range()] {
                "+" => a.checked_add(b),
                "*" => a.checked_mul(b),
                "<<" => {
                    let shift = u32::try_from(b).ok()?;
                    if shift >= 64 || a > (u64::MAX >> shift) {
                        None
                    } else {
                        Some(a << shift)
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn binding(node: Node<'_>, source: &str) -> String {
    let mut parent = node.parent();
    for _ in 0..8 {
        let Some(n) = parent else { break };
        let field = match n.kind() {
            "const_item" | "static_item" => Some("name"),
            "let_declaration" => Some("pattern"),
            "field_initializer" => Some("field"),
            "assignment_expression" => Some("left"),
            "binary_expression" | "parenthesized_expression" => None,
            _ => break,
        };
        if let Some(field) = field {
            return n
                .child_by_field_name(field)
                .map(|v| source[v.byte_range()].to_owned())
                .unwrap_or_default();
        }
        parent = n.parent();
    }
    String::new()
}

fn unit(name: &str) -> (&'static str, u64) {
    let terms = text::searchable(name);
    let tokens: BTreeSet<_> = terms.split_whitespace().collect();
    if tokens.contains("byte") {
        ("bytes", 1)
    } else if tokens.contains("ms") || tokens.contains("millisecond") {
        ("milliseconds", 1)
    } else if tokens.contains("second") || tokens.contains("secs") {
        ("milliseconds", 1000)
    } else {
        ("unknown", 1)
    }
}

fn enclosing_calls(node: Node<'_>, source: &str) -> String {
    let mut current = node.parent();
    let mut names = Vec::new();
    // Ancestors establish syntactic argument containment, not resolved call/data flow.
    for _ in 0..16 {
        let Some(parent) = current else { break };
        if matches!(
            parent.kind(),
            "block" | "let_declaration" | "expression_statement" | "const_item" | "static_item"
        ) {
            break;
        }
        if parent.kind() == "call_expression"
            && let Some(callee) = parent.child_by_field_name("function")
        {
            names.push(
                source[callee.byte_range()]
                    .chars()
                    .take(256)
                    .collect::<String>(),
            );
            if names.len() == 4 {
                break;
            }
        }
        current = parent.parent();
    }
    text::searchable(&names.join(" "))
}

fn walk(node: Node<'_>, source: &str, facts: &mut Vec<Fact>) {
    if matches!(
        node.kind(),
        "macro_invocation"
            | "macro_definition"
            | "attribute_item"
            | "token_tree"
            | "unary_expression"
    ) {
        return;
    }
    if matches!(
        node.kind(),
        "integer_literal" | "binary_expression" | "parenthesized_expression"
    ) {
        if let Some(raw) = value(node, source, 0) {
            let name = binding(node, source);
            let (unit, multiplier) = unit(&name);
            if let Some(value) = raw.checked_mul(multiplier) {
                let (structure, structured_unit, scale) =
                    crate::quantity_structure::observe(node, source);
                // Binding and enclosing call spellings carry quantity context; the enclosing
                // symbol is added during indexing. Nearby unrelated statements add no votes.
                facts.push(Fact {
                    value,
                    unit,
                    start: node.start_byte(),
                    end: node.end_byte(),
                    line: node.start_position().row + 1,
                    end_line: node.end_position().row + 1,
                    context: format!(
                        "{} {}",
                        text::searchable(&name),
                        enclosing_calls(node, source)
                    ),
                    structured_value: raw.checked_mul(scale),
                    structured_unit,
                    structure,
                    value_use: crate::value_use::observe(node, source),
                });
            }
            return;
        }
        // Do not mistake an operand of an unsupported calculation for its result.
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, source, facts);
    }
}

/// Build an optional fact index atomically, without altering ordinary source retrieval.
pub fn index_quantities(db: &Path) -> Result<QuantityReport> {
    let mut conn = store::open(db, true)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let root = store::metadata(&tx, "root")?.context("index sources first")?;
    let snapshot = store::metadata(&tx, "snapshot")?.context("index sources first")?;
    let files = workspace::scan_index(&tx, Path::new(&root))?;
    ensure!(
        workspace::snapshot(&files) == snapshot,
        "quantity indexing requires current sources; run sonar index first"
    );
    tx.execute_batch(SCHEMA)?;
    tx.execute_batch(STRUCTURED_SCHEMA)?;
    tx.execute_batch(USE_SCHEMA)?;
    tx.execute("DELETE FROM quantity_facts", [])?;
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let mut report = QuantityReport {
        snapshot: snapshot.clone(),
        policy: POLICY.into(),
        facts: 0,
        skipped_parse_error_files: 0,
        unbound_expressions: 0,
    };
    for file in files.iter().filter(|f| f.path.ends_with(".rs")) {
        let tree = parser
            .parse(&file.source, None)
            .context("Rust parser did not produce a tree")?;
        if tree.root_node().has_error() {
            report.skipped_parse_error_files += 1;
            continue;
        }
        let mut facts = Vec::new();
        walk(tree.root_node(), &file.source, &mut facts);
        for fact in facts {
            let owner=tx.query_row("SELECT l.id,coalesce(l.symbol,'') FROM locations l JOIN files f ON f.id=l.file_id WHERE f.path=?1 AND l.kind!='file' AND l.start_byte<=?2 AND l.end_byte>=?3 ORDER BY l.end_byte-l.start_byte,l.id LIMIT 1",params![file.path,fact.start as i64,fact.end as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)));
            let (id, symbol) = match owner {
                Ok(owner) => owner,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    report.unbound_expressions += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let region=tx.query_row("SELECT id FROM regions WHERE location_id=?1 AND view='combined' AND start_line<=?2 AND end_line>=?3 ORDER BY end_line-start_line,id LIMIT 1",params![id,fact.line as i64,fact.end_line as i64],|r|r.get::<_,i64>(0));
            let region = match region {
                Ok(id) => id,
                Err(rusqlite::Error::QueryReturnedNoRows) => {
                    report.unbound_expressions += 1;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let context = format!("{} {}", fact.context, text::searchable(&symbol));
            tx.execute(
                "INSERT OR IGNORE INTO quantity_facts VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    id,
                    region,
                    fact.value.to_string(),
                    fact.unit,
                    fact.line as i64,
                    fact.end_line as i64,
                    fact.start as i64,
                    fact.end as i64,
                    context
                ],
            )?;
            tx.execute(
                "INSERT INTO quantity_structure VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    id,
                    fact.start as i64,
                    fact.end as i64,
                    fact.structured_value.map(|v| v.to_string()),
                    fact.structured_unit,
                    serde_json::to_string(&fact.structure)?
                ],
            )?;
            report.facts += 1;
            tx.execute(
                "INSERT INTO quantity_uses VALUES (?1,?2,?3,?4)",
                params![
                    id,
                    fact.start as i64,
                    fact.end as i64,
                    serde_json::to_string(&fact.value_use)?
                ],
            )?;
        }
    }
    for (key, v) in [
        ("quantity_policy", POLICY),
        ("quantity_snapshot", snapshot.as_str()),
        ("quantity_use_policy", crate::value_use::POLICY),
        (
            "quantity_structure_policy",
            crate::quantity_structure::POLICY,
        ),
    ] {
        tx.execute("INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,v])?;
    }
    tx.commit()?;
    Ok(report)
}

struct Query {
    value: u64,
    unit: &'static str,
    terms: Vec<String>,
    start: usize,
    end: usize,
}

fn query(input: &str) -> Option<Query> {
    let chars: Vec<char> = input.chars().collect();
    let mut quantities = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit()
            || (i > 0
                && (chars[i - 1].is_alphanumeric()
                    || matches!(chars[i - 1], '_' | '-' | '.' | ',')))
        {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_digit() || matches!(chars[i], '_' | ',' | '.'))
        {
            i += 1;
        }
        let number: String = chars[start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let unit_start = i;
        while i < chars.len() && chars[i].is_alphabetic() {
            i += 1;
        }
        let spelling: String = chars[unit_start..i].iter().collect();
        let (unit, multiplier) = match spelling.as_str() {
            "B" | "byte" | "bytes" => ("bytes", 1),
            "KiB" | "kib" => ("bytes", 1024),
            "MiB" | "mib" => ("bytes", 1024 * 1024),
            "GiB" | "gib" => ("bytes", 1024 * 1024 * 1024),
            "KB" | "kB" => ("bytes", 1000),
            "MB" => ("bytes", 1000000),
            "GB" => ("bytes", 1000000000),
            "ms" | "millisecond" | "milliseconds" => ("milliseconds", 1),
            "s" | "second" | "seconds" => ("milliseconds", 1000),
            _ => continue,
        };
        // A unit inside an identifier (e.g. KiB2 or KiB_limit) is not a quantity.
        if chars
            .get(i)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_')
        {
            return None;
        }
        if number.contains('.')
            || number.contains(',')
                && (number.split(',').next()?.len() > 3
                    || number.split(',').skip(1).any(|s| s.len() != 3))
        {
            return None;
        }
        let n = number
            .replace([',', '_'], "")
            .parse::<u64>()
            .ok()?
            .checked_mul(multiplier)?;
        quantities.push((n, unit, start, i));
    }
    if quantities.len() != 1 {
        return None;
    }
    let (value, unit, start, end) = quantities[0];
    let remainder: String = chars[..start].iter().chain(chars[end..].iter()).collect();
    let terms = text::query_terms(&remainder)
        .into_iter()
        .filter(|s| {
            !matches!(s.as_str(), "byte" | "ms" | "second" | "millisecond")
                && s.chars().any(char::is_alphabetic)
        })
        .take(64)
        .collect::<Vec<_>>();
    (!terms.is_empty()).then_some(Query {
        value,
        unit,
        terms,
        start,
        end,
    })
}

pub(crate) fn current(conn: &Connection, structured: bool) -> Result<bool> {
    if store::metadata(conn, "quantity_policy")?.as_deref() != Some(POLICY)
        || store::metadata(conn, "quantity_snapshot")? != store::metadata(conn, "snapshot")?
    {
        return Ok(false);
    }
    if !structured {
        return Ok(true);
    }
    if store::metadata(conn, "quantity_structure_policy")?.as_deref()
        != Some(crate::quantity_structure::POLICY)
    {
        return Ok(false);
    }
    // An older executable can rebuild legacy facts without refreshing structured observations.
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM quantity_structure)=(SELECT count(*) FROM quantity_facts)",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn canonical_query(input: &str) -> Option<String> {
    let q = query(input)?;
    let before: String = input.chars().take(q.start).collect();
    let after: String = input.chars().skip(q.end).collect();
    Some(format!("{before}{} {}{after}", q.value, q.unit))
}

pub(crate) fn context_query(input: &str) -> Option<(String, Vec<String>)> {
    let q = query(input)?;
    let before: String = input.chars().take(q.start).collect();
    let after: String = input.chars().skip(q.end).collect();
    Some((format!("{before} {after}"), q.terms))
}

pub(crate) fn local_current(conn: &Connection) -> Result<bool> {
    if !current(conn, true)?
        || store::metadata(conn, "quantity_use_policy")?.as_deref()
            != Some(crate::value_use::POLICY)
    {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT (SELECT count(*) FROM quantity_uses)=(SELECT count(*) FROM quantity_facts)",
        [],
        |r| r.get(0),
    )?)
}

pub(crate) fn apply(
    conn: &Connection,
    input: &str,
    scope: &str,
    stale: &BTreeSet<String>,
    candidates: &mut Vec<Candidate>,
    policy: QuantityRankingPolicy,
) -> Result<&'static str> {
    let Some(q) = query(input) else {
        return Ok("inactive");
    };
    let structured = policy != QuantityRankingPolicy::Legacy;
    if !current(conn, structured)? || policy.needs_local() && !local_current(conn)? {
        return Ok("unavailable");
    }
    if candidates.iter().any(Candidate::exact) {
        return Ok("exact_bypass");
    }
    let (join, unit, value, evidence) = if structured {
        (
            "JOIN quantity_structure s ON s.location_id=q.location_id AND s.start_byte=q.start_byte AND s.end_byte=q.end_byte",
            "s.unit",
            "s.value",
            "s.evidence",
        )
    } else {
        ("", "q.unit", "q.value", "NULL")
    };
    let mut stmt=conn.prepare(&format!("SELECT l.id,f.path,l.symbol,l.kind,l.definition,l.parent_id,r.start_line,r.source,0.0,l.is_test,r.id AS region_id,r.end_line,r.view,{unit},q.line,q.end_line,q.start_byte,q.end_byte,q.context,{evidence} FROM quantity_facts q {join} JOIN locations l ON l.id=q.location_id JOIN files f ON f.id=l.file_id JOIN regions r ON r.id=q.region_id WHERE {value}=?1 AND {unit} IN (?2,'unknown') AND (?3='' OR f.path=?3 OR substr(f.path,1,length(?3)+1)=?3||'/') AND f.path NOT IN (SELECT value FROM json_each(?4)) ORDER BY f.path,q.start_byte,l.id"))?;
    let mut rows = stmt.query(params![
        q.value.to_string(),
        q.unit,
        scope,
        serde_json::to_string(stale)?
    ])?;
    let mut best: BTreeMap<i64, Candidate> = BTreeMap::new();
    while let Some(row) = rows.next()? {
        let context: String = row.get(18)?;
        let tokens: BTreeSet<_> = context.split_whitespace().collect();
        let mut matched = q
            .terms
            .iter()
            .filter(|t| tokens.contains(t.as_str()))
            .count();
        let mut c = crate::search::candidate_from_row(row)?;
        let unit: String = row.get(13)?;
        c.score = matched as f64 / q.terms.len() as f64 * if unit == "unknown" { 0.5 } else { 1.0 };
        let mut structure: Option<crate::QuantityStructure> = row
            .get::<_, Option<String>>(19)?
            .map(|s| serde_json::from_str(&s))
            .transpose()?;
        if let Some(evidence) = structure.as_mut() {
            (c.score, matched) = crate::quantity_structure::strength(
                evidence,
                c.symbol.as_deref().unwrap_or(""),
                &q.terms,
                &unit,
            );
            evidence.strength = c.score;
        }
        if matched == 0
            && !matches!(
                policy,
                QuantityRankingPolicy::Unified
                    | QuantityRankingPolicy::Local
                    | QuantityRankingPolicy::BoundedLocal
            )
        {
            continue;
        }
        c.quantity = Some(QuantityEvidence {
            value: q.value.to_string(),
            unit,
            line: row.get::<_, u32>(14)? as usize,
            end_line: row.get::<_, u32>(15)? as usize,
            start_byte: row.get::<_, u32>(16)? as usize,
            end_byte: row.get::<_, u32>(17)? as usize,
            matched_terms: matched,
            structure,
            local: None,
        });
        c.hit.channel = "quantity".into();
        c.hits = vec![c.hit.clone()];
        if best.get(&c.id).is_none_or(|old| c.score > old.score) {
            best.insert(c.id, c);
        }
    }
    let mut quantities: Vec<_> = best.into_values().collect();
    sort(&mut quantities);
    quantities.truncate(512);
    if quantities.is_empty() {
        return Ok("no_evidence");
    }
    for (rank, c) in candidates.iter_mut().enumerate() {
        c.score = 1.0 / (60.0 + rank as f64 + 1.0);
    }
    for (rank, mut c) in quantities.into_iter().enumerate() {
        let contribution = if structured { c.score } else { 1.0 } / (60.0 + rank as f64 + 1.0);
        if let Some(old) = candidates.iter_mut().find(|old| old.id == c.id) {
            old.score += contribution;
            old.quantity = c.quantity;
            old.hits.push(c.hit.clone());
            old.use_hit(c.hit);
        } else {
            c.score = contribution;
            candidates.push(c);
        }
    }
    sort(candidates);
    Ok("current")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn query_units_and_ambiguity() {
        assert_eq!(query("output read 64 KiB?").unwrap().value, 65536);
        assert_eq!(query("output read 64 KB?").unwrap().value, 64000);
        assert_eq!(query("poll interval 2 seconds").unwrap().value, 2000);
        assert_eq!(query("read 65,536 bytes").unwrap().value, 65536);
        for q in [
            "64KiB2 read",
            "64KiB_limit read",
            "64kb output",
            "65,5346 bytes read",
            "-64 KiB read",
            "1.5 KiB read",
            "32 KiB versus 64 KiB",
            "64 KiB",
        ] {
            assert!(query(q).is_none(), "{q}");
        }
    }

    #[test]
    fn canonical_units_preserve_surrounding_text_and_real_distinctions() {
        assert_eq!(
            canonical_query("🦀 output 64 KiB?"),
            canonical_query("🦀 output 65,536 bytes?")
        );
        assert_ne!(
            canonical_query("output 64 KB"),
            canonical_query("output 64 KiB")
        );
        assert_eq!(
            canonical_query("timeout 2 seconds"),
            canonical_query("timeout 2000 ms")
        );
        assert_eq!(canonical_query("output 64kb"), None);
        assert_eq!(canonical_query("output 64KiB2"), None);
        assert_eq!(canonical_query("output 1.5 KiB"), None);
    }
    #[test]
    fn arithmetic_does_not_execute_code_or_split_unsupported_expressions() {
        let source = "const READ_BYTES: usize = 64 * 1024; const OTHER: usize = 1 << 16; const NO: usize = usize::MAX; const OVER: u64 = 18446744073709551615 + 1; fn f() { let read_bytes = get_limit() + 65536; println!(\"{}\",65536); let x = -65536; }";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut facts = Vec::new();
        walk(tree.root_node(), source, &mut facts);
        assert_eq!(facts.len(), 2);
        assert!(facts.iter().all(|f| f.value == 65536));
        assert_eq!(facts[0].unit, "bytes");
    }
}
