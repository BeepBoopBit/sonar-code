//! Literal source evidence for TypeScript ranking and bounded reading ranges.
use crate::{candidate::Candidate, text, workspace::SourceFile};
use anyhow::Result;
use oxc_allocator::Allocator;
use oxc_ast::AstKind;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

struct Unit {
    start: usize,
    end: usize,
    kind: &'static str,
    priority: usize,
}
#[derive(Default)]
struct Units {
    units: Vec<Unit>,
    literals: Vec<(usize, usize, BTreeSet<String>)>,
}
impl<'a> Visit<'a> for Units {
    fn enter_node(&mut self, node: AstKind<'a>) {
        if let AstKind::StringLiteral(literal) = node {
            self.literals.push((
                literal.span.start as usize,
                literal.span.end as usize,
                text::query_terms(&literal.value).into_iter().collect(),
            ));
        }
        let (kind, priority) = match node {
            AstKind::IfStatement(_) => ("if_statement", 2),
            AstKind::SwitchCase(_) => ("switch_case", 2),
            AstKind::ForStatement(_)
            | AstKind::ForInStatement(_)
            | AstKind::ForOfStatement(_)
            | AstKind::WhileStatement(_) => ("loop_statement", 1),
            AstKind::ReturnStatement(_) => ("return_statement", 1),
            AstKind::ThrowStatement(_) => ("throw_statement", 1),
            AstKind::VariableDeclaration(_) => ("lexical_declaration", 0),
            AstKind::ExpressionStatement(_) => ("expression_statement", 0),
            _ => return,
        };
        let span = node.span();
        self.units.push(Unit {
            start: span.start as usize,
            end: span.end as usize,
            kind,
            priority,
        });
    }
}
pub(crate) struct Source {
    starts: Vec<usize>,
    end: usize,
    tokens: Vec<BTreeSet<String>>,
    frequencies: BTreeMap<String, usize>,
    units: Vec<Unit>,
    literals: Vec<(usize, usize, BTreeSet<String>)>,
}
impl Source {
    pub(crate) fn new(path: &str, source: &str) -> Result<Self> {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, source, SourceType::from_path(path)?).parse();
        let mut code = source.as_bytes().to_vec();
        for c in &parsed.program.comments {
            for b in &mut code[c.span.start as usize..c.span.end as usize] {
                if !matches!(*b, b'\n' | b'\r') {
                    *b = b' ';
                }
            }
        }
        let code = String::from_utf8(code)?;
        let starts = std::iter::once(0)
            .chain(
                source
                    .bytes()
                    .enumerate()
                    .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
            )
            .collect();
        let tokens: Vec<BTreeSet<String>> = code
            .split('\n')
            .map(|l| text::query_terms(l).into_iter().collect())
            .collect();
        let mut frequencies = BTreeMap::new();
        for line in &tokens {
            for token in line {
                *frequencies.entry(token.clone()).or_insert(0) += 1;
            }
        }
        let mut units = Units::default();
        units.visit_program(&parsed.program);
        Ok(Self {
            starts,
            end: source.len(),
            tokens,
            frequencies,
            units: units.units,
            literals: units.literals,
        })
    }
    fn line(&self, byte: usize) -> usize {
        self.starts.partition_point(|s| *s <= byte)
    }
    fn best_coverage(&self, extent: [usize; 2], terms: &[String], budget: usize) -> usize {
        let low = self.line(extent[0]) - 1;
        let high = self.line(extent[1].saturating_sub(1));
        let mut counts = vec![0usize; terms.len()];
        let mut first = low;
        let mut best = 0;
        for line in low..high {
            for (i, term) in terms.iter().enumerate() {
                counts[i] += usize::from(self.tokens[line].contains(term));
            }
            let end = self.starts.get(line + 1).copied().unwrap_or(self.end);
            while first <= line && (line - first >= budget || end - self.starts[first] > 4096) {
                for (i, term) in terms.iter().enumerate() {
                    counts[i] -= usize::from(self.tokens[first].contains(term));
                }
                first += 1;
            }
            best = best.max(counts.iter().filter(|n| **n > 0).count());
        }
        best
    }
    pub(crate) fn focus(
        &self,
        source: &str,
        extent: [usize; 2],
        terms: &[String],
        budget: usize,
        site: Option<usize>,
    ) -> Option<([usize; 2], &'static str)> {
        let low = self.line(extent[0]);
        let high = self.line(extent[1].saturating_sub(1));
        let width = budget.min(high - low + 1);
        let weights: Vec<_> = terms
            .iter()
            .map(|t| {
                (1.0 + self.tokens.len() as f64
                    / (1 + self.frequencies.get(t).copied().unwrap_or(0)) as f64)
                    .ln()
            })
            .collect();
        let mut choices = Vec::new();
        for unit in &self.units {
            if unit.start >= extent[0] && unit.end <= extent[1] {
                let range = [self.line(unit.start), self.line(unit.end.saturating_sub(1))];
                if range[1] - range[0] < budget {
                    choices.push((range, unit.kind, unit.priority));
                }
            }
        }
        for line in low..=high - width + 1 {
            choices.push(([line, line + width - 1], "typescript_window", 0));
        }
        let mut best: Option<(f64, usize, usize, [usize; 2], &'static str)> = None;
        for (range, kind, priority) in choices {
            if site.is_some_and(|s| s < range[0] || s > range[1]) {
                continue;
            }
            let start = self.starts[range[0] - 1];
            let end = self.starts.get(range[1]).copied().unwrap_or(source.len());
            if end - start > 4096 {
                continue;
            }
            let found: BTreeSet<_> = self.tokens[range[0] - 1..range[1]]
                .iter()
                .flat_map(|l| l.iter())
                .collect();
            let score: f64 = terms
                .iter()
                .zip(&weights)
                .filter(|(t, _)| found.contains(t))
                .map(|(term, w)| {
                    w * if self
                        .literals
                        .iter()
                        .any(|(a, b, words)| *a >= start && *b <= end && words.contains(term))
                    {
                        1.5
                    } else {
                        1.0
                    }
                })
                .sum();
            let score = score * (1.0 + 0.1 * priority as f64);
            if score == 0.0 {
                continue;
            }
            let len = range[1] - range[0];
            if best.as_ref().is_none_or(|(old, p, l, _, _)| {
                score > *old + 1e-9
                    || ((score - old).abs() < 1e-9
                        && (priority > *p || (priority == *p && len < *l)))
            }) {
                best = Some((score, priority, len, range, kind));
            }
        }
        best.map(|(_, _, _, range, kind)| (range, kind))
    }
}
pub(crate) fn rank(
    conn: &Connection,
    files: &[SourceFile],
    candidates: &mut [Candidate],
    terms: &[String],
) -> Result<()> {
    if terms.is_empty() || candidates.iter().any(Candidate::exact) {
        return Ok(());
    }
    let mut parsed = BTreeMap::new();
    let sources: BTreeMap<_, _> = files
        .iter()
        .map(|f| (f.path.as_str(), f.source.as_str()))
        .collect();
    for c in candidates
        .iter_mut()
        .filter(|c| crate::source_language(&c.path) == Some("typescript"))
    {
        let source = sources[c.path.as_str()];
        if !parsed.contains_key(&c.path) {
            parsed.insert(c.path.clone(), Source::new(&c.path, source)?);
        }
        let syntax = &parsed[&c.path];
        let (start, end): (usize, usize) = conn.query_row(
            "SELECT start_byte,end_byte FROM locations WHERE id=?1",
            [c.id],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
        )?;
        let identity: BTreeSet<_> =
            text::query_terms(&format!("{} {}", c.path, c.symbol.as_deref().unwrap_or("")))
                .into_iter()
                .collect();
        let name =
            terms.iter().filter(|t| identity.contains(*t)).count() as f64 / terms.len() as f64;
        let short = c
            .symbol
            .as_deref()
            .unwrap_or("")
            .rsplit('.')
            .next()
            .unwrap_or("");
        let words = text::words(short);
        let parts: BTreeSet<_> = if words.len() > 1 {
            text::query_terms(&words[1..].join(" "))
                .into_iter()
                .collect()
        } else {
            BTreeSet::new()
        };
        let matching = parts.iter().filter(|t| terms.contains(t)).count();
        let complete = if matching >= 2 {
            matching as f64 / parts.len() as f64
        } else {
            0.0
        };
        let compound = c
            .path
            .split('/')
            .filter_map(|part| {
                let words = text::query_terms(part);
                (words.len() >= 2 && words.iter().all(|w| terms.contains(w))).then_some(words.len())
            })
            .max()
            .unwrap_or(0) as f64;
        let coverage = syntax.best_coverage([start, end], terms, 16) as f64 / terms.len() as f64;
        let callable = if c.kind == "function" { 1.15 } else { 1.0 };
        let role = if c.is_test
            && !terms
                .iter()
                .any(|t| matches!(t.as_str(), "test" | "spec" | "assert" | "expect"))
        {
            0.75
        } else {
            1.0
        };
        let benchmark = if c
            .path
            .split('/')
            .any(|p| matches!(p, "bench" | "benches" | "benchmark" | "benchmarks"))
            && !terms.iter().any(|t| {
                matches!(
                    t.as_str(),
                    "benchmark" | "bench" | "perform" | "measur" | "speed"
                )
            }) {
            0.5
        } else {
            1.0
        };
        c.score *=
            (1.0 + 2.0 * name + 2.0 * complete + compound + coverage) * callable * role * benchmark;
    }
    crate::candidate::sort(candidates);
    Ok(())
}
