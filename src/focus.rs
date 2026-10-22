//! Bounded reading ranges. This stage runs after selection and cannot change ranking or links.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use rusqlite::Connection;
#[cfg(test)]
use tree_sitter::Parser;
use tree_sitter::{Node, Tree};

use crate::{
    FocusDiagnostic, FocusOptions, FocusPolicy, candidate::Candidate, text, workspace::SourceFile,
};

// Lines alone do not bound reading cost: a minified line can be megabytes long.
const MAX_BYTES: usize = 4096;
const MAX_VISITS: usize = 4096;

pub(crate) struct ContextualQuery<'a> {
    pub model: &'a crate::Embedder,
    pub vector: &'a [f32],
    pub terms: &'a [String],
}

pub(crate) struct Resolver<'a> {
    pub(crate) files: &'a [SourceFile],
    pub(crate) options: FocusOptions,
    pub(crate) trees: BTreeMap<String, Tree>,
    pub(crate) lines: BTreeMap<String, Lines<'a>>,
    typescript: BTreeMap<String, crate::languages::typescript::syntax::Source>,
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(files: &'a [SourceFile], options: FocusOptions) -> Result<Self> {
        Ok(Self {
            files,
            options,
            trees: BTreeMap::new(),
            lines: BTreeMap::new(),
            typescript: BTreeMap::new(),
        })
    }

    pub(crate) fn resolve(
        &mut self,
        conn: &Connection,
        candidate: &Candidate,
        terms: &[String],
        site: Option<usize>,
        legacy: Option<[usize; 2]>,
    ) -> Result<FocusDiagnostic> {
        // Use the already freshness-checked source snapshot, never a second filesystem read.
        let file = self
            .files
            .binary_search_by(|f| f.path.cmp(&candidate.path))
            .ok()
            .map(|i| &self.files[i])
            .context("focus source is absent from the query snapshot")?;
        let lines = self
            .lines
            .entry(file.path.clone())
            .or_insert_with(|| Lines::new(&file.source));
        let semantic = matches!(
            candidate.hit.channel.as_str(),
            "embedding" | "coarse_embedding"
        ) && candidate.hit.view != "identity";
        let anchor = if let Some(line) = site {
            Some([line, line])
        } else if semantic {
            Some([candidate.hit.start, candidate.hit.end])
        } else {
            legacy
        };
        let mut diagnostic = FocusDiagnostic {
            location_id: candidate.id,
            path: candidate.path.clone(),
            symbol: candidate.symbol.clone(),
            anchor,
            range: legacy,
            reason: "legacy".into(),
            syntax_kind: None,
            lines: 0,
            bytes: 0,
            protected: None,
            contextual: None,
        };
        if self.options.policy != FocusPolicy::Legacy {
            if let Some(anchor) = anchor {
                let (start, end): (i64, i64) = conn.query_row(
                    "SELECT start_byte,end_byte FROM locations WHERE id=?1",
                    [candidate.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let extent = [
                    lines.line_at(start as usize),
                    lines.line_at((end as usize).saturating_sub(1)),
                ];
                // Attached documentation may precede the declaration's byte extent.
                let allowed = [
                    extent[0].min(anchor[0]),
                    extent[1].max(anchor[1]).min(lines.len()),
                ];
                let target = if semantic {
                    legacy.unwrap_or(anchor)
                } else {
                    anchor
                };
                let window = lines.window(target, allowed, self.options.max_lines, site);
                diagnostic.range = window;
                diagnostic.reason = if window.is_some() {
                    "bounded_window"
                } else {
                    "byte_budget"
                }
                .into();
                if self.options.policy == FocusPolicy::Anchored {
                    let mut protected = BTreeSet::new();
                    if let Some(window) = window {
                        for line in window[0]..=window[1] {
                            let source = text::searchable(lines.slice([line, line]));
                            if source
                                .split_whitespace()
                                .any(|s| terms.iter().any(|t| t == s))
                            {
                                protected.insert(line);
                            }
                        }
                        if let Some(site) = site {
                            protected.insert(site);
                        }
                        if let Some(q) = &candidate.quantity {
                            protected.extend(
                                (q.line..=q.end_line)
                                    .filter(|l| window[0] <= *l && *l <= window[1]),
                            );
                        }
                    }
                    diagnostic.protected = Some(crate::FocusProtection {
                        window,
                        lines: protected.iter().copied().collect(),
                        units: Vec::new(),
                    });
                    diagnostic.reason = if protected.is_empty() {
                        "anchored_no_witness"
                    } else if crate::languages::for_path(&file.path).is_none()
                        || candidate.kind == "documentation"
                    {
                        "anchored_non_rust"
                    } else {
                        if !self.trees.contains_key(&file.path) {
                            let tree = crate::languages::parse_tree(&file.path, &file.source)?;
                            self.trees.insert(file.path.clone(), tree);
                        }
                        let root = self.trees[&file.path].root_node();
                        let boundary = root
                            .descendant_for_byte_range(start as usize, end as usize)
                            .unwrap_or(root);
                        let boundary = crate::languages::for_path(&file.path)
                            .unwrap()
                            .focus_root(boundary);
                        match anchored_units_for(
                            crate::languages::for_path(&file.path).unwrap(),
                            boundary,
                            lines,
                            &protected,
                            extent,
                            self.options.max_lines,
                        ) {
                            Ok(units) => {
                                let target = units.iter().fold(
                                    [*protected.first().unwrap(), *protected.last().unwrap()],
                                    |span, u| [span[0].min(u.range[0]), span[1].max(u.range[1])],
                                );
                                diagnostic.protected.as_mut().unwrap().units = units;
                                if let Some(next) = window.and_then(|old| {
                                    lines.complete_window(
                                        old,
                                        target,
                                        allowed,
                                        self.options.max_lines,
                                    )
                                }) {
                                    diagnostic.range = Some(next);
                                    if Some(next) == window {
                                        "anchored_complete"
                                    } else {
                                        "anchored_shift"
                                    }
                                } else {
                                    "anchored_budget"
                                }
                            }
                            Err(reason) => reason,
                        }
                    }
                    .into();
                }
                if self.options.policy == FocusPolicy::Syntax {
                    let descriptive = candidate.kind == "documentation"
                        || crate::languages::for_path(&file.path).is_none()
                        || (semantic && candidate.hit.view == "description");
                    if descriptive {
                        // A comment match is evidence in its own right. Do not move it into code.
                        diagnostic.range = lines.window(
                            target,
                            [anchor[0], anchor[1]],
                            self.options.max_lines,
                            site,
                        );
                        diagnostic.reason = "descriptive_anchor".into();
                    } else {
                        if !self.trees.contains_key(&file.path) {
                            let tree = crate::languages::parse_tree(&file.path, &file.source)?;
                            self.trees.insert(file.path.clone(), tree);
                        }
                        let root = self.trees[&file.path].root_node();
                        let boundary = root
                            .descendant_for_byte_range(start as usize, end as usize)
                            .unwrap_or(root);
                        let boundary = crate::languages::for_path(&file.path)
                            .unwrap()
                            .focus_root(boundary);
                        if !boundary.has_error() && !boundary.is_missing() {
                            if let Some(unit) = select_for(
                                crate::languages::for_path(&file.path).unwrap(),
                                boundary,
                                lines,
                                anchor,
                                extent,
                                terms,
                                site,
                                self.options.max_lines,
                            ) {
                                diagnostic.range = Some(unit.range);
                                diagnostic.reason = "syntax_unit".into();
                                diagnostic.syntax_kind = Some(unit.kind);
                            } else {
                                diagnostic.reason = if window.is_some() {
                                    "no_fitting_unit"
                                } else {
                                    "byte_budget"
                                }
                                .into();
                            }
                        } else {
                            diagnostic.reason = "parse_error_window".into();
                        }
                    }
                }
            } else {
                diagnostic.reason = "no_anchor".into();
            }
        }
        if crate::source_language(&file.path) == Some("typescript")
            && candidate.kind == "function"
            && !candidate.exact()
            && self.options.policy != FocusPolicy::Anchored
        {
            if !self.typescript.contains_key(&file.path) {
                self.typescript.insert(
                    file.path.clone(),
                    crate::languages::typescript::syntax::Source::new(&file.path, &file.source)?,
                );
            }
            let extent: (usize, usize) = conn.query_row(
                "SELECT start_byte,end_byte FROM locations WHERE id=?1",
                [candidate.id],
                |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize)),
            )?;
            if let Some((range, kind)) = self.typescript[&file.path].focus(
                &file.source,
                [extent.0, extent.1],
                terms,
                self.options.max_lines,
                site,
            ) {
                diagnostic.range = Some(range);
                diagnostic.reason = "syntax_unit".into();
                diagnostic.syntax_kind = Some(kind.into());
            }
        }
        if let Some(range) = diagnostic.range {
            diagnostic.lines = range[1] - range[0] + 1;
            diagnostic.bytes = lines.slice(range).len();
        }
        Ok(diagnostic)
    }

    pub(crate) fn contextual(
        &mut self,
        conn: &Connection,
        candidate: &Candidate,
        query: ContextualQuery<'_>,
        site: Option<usize>,
        diagnostic: &mut FocusDiagnostic,
    ) -> Result<()> {
        if let Some(adapter) = crate::languages::for_path(&candidate.path) {
            adapter.contextual_focus(self, conn, candidate, query, site, diagnostic)?;
        }
        Ok(())
    }

    /// Score fixed-budget windows using their own text and existing region vectors.
    /// The old window remains the deterministic tie/fallback choice.
    pub(crate) fn evidence(
        &self,
        conn: &Connection,
        candidate: &Candidate,
        terms: &[String],
        vector: Option<&[f32]>,
        site: Option<usize>,
        diagnostic: &mut FocusDiagnostic,
    ) -> Result<()> {
        let lines = &self.lines[&candidate.path];
        let bounds: (u32,u32) = conn.query_row("SELECT min(start_line),max(end_line) FROM regions WHERE location_id=?1 AND view IN ('combined','description')",[candidate.id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let extent = [bounds.0 as usize, (bounds.1 as usize).min(lines.len())];
        let mut semantic: Vec<Option<f64>> = vec![None; extent[1] - extent[0] + 1];
        if let Some(vector) = vector {
            let mut stmt=conn.prepare("SELECT r.start_line,r.end_line,e.vector FROM regions r JOIN embeddings e ON e.region_id=r.id WHERE r.location_id=?1 AND r.view!='identity' ORDER BY r.id")?;
            let mut rows = stmt.query([candidate.id])?;
            while let Some(row) = rows.next()? {
                let first = (row.get::<_, u32>(0)? as usize).max(extent[0]);
                let last = (row.get::<_, u32>(1)? as usize).min(extent[1]);
                let score =
                    crate::embeddings::dot_bytes(vector, row.get_ref(2)?.as_blob()?)?.max(0.0);
                for line in first..=last {
                    let entry = &mut semantic[line - extent[0]];
                    *entry = Some(entry.unwrap_or(0.0).max(score));
                }
            }
        }
        let line_terms: Vec<BTreeSet<usize>> = (extent[0]..=extent[1])
            .map(|line| {
                let text = text::searchable(lines.slice([line, line]));
                let tokens: BTreeSet<_> = text.split_whitespace().collect();
                terms
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| tokens.contains(t.as_str()))
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        let mut windows = BTreeSet::new();
        if let Some(old) = diagnostic.range {
            windows.insert(old);
        }
        let width = self.options.max_lines.min(extent[1] - extent[0] + 1);
        for start in extent[0]..=extent[1] - width + 1 {
            if let Some(window) = lines.window(
                [start, start + width - 1],
                extent,
                self.options.max_lines,
                None,
            ) && site.is_none_or(|s| window[0] <= s && s <= window[1])
            {
                windows.insert(window);
            }
        }
        let mut measured = Vec::new();
        for range in windows {
            if range[0] < extent[0]
                || range[1] > extent[1]
                || !lines.fits(range, self.options.max_lines)
                || site.is_some_and(|s| s < range[0] || s > range[1])
            {
                continue;
            }
            let a = range[0] - extent[0];
            let b = range[1] - extent[0] + 1;
            let lexical = line_terms[a..b]
                .iter()
                .flatten()
                .copied()
                .collect::<BTreeSet<_>>()
                .len();
            let dense = semantic[a..b].iter().flatten().sum::<f64>() / (b - a) as f64;
            measured.push((range, lexical, dense));
        }
        let mut lex_values: Vec<_> = measured
            .iter()
            .map(|(_, l, _)| *l)
            .filter(|l| *l > 0)
            .collect();
        lex_values.sort_by(|a, b| b.cmp(a));
        lex_values.dedup();
        let mut dense_values: Vec<_> = measured
            .iter()
            .map(|(_, _, d)| *d)
            .filter(|d| *d > 0.0)
            .collect();
        dense_values.sort_by(|a, b| b.total_cmp(a));
        dense_values.dedup();
        let old = diagnostic.range;
        let score = |l: usize, d: f64| {
            lex_values
                .binary_search_by(|v| l.cmp(v))
                .ok()
                .map_or(0.0, |i| 1.0 / (61 + i) as f64)
                + dense_values
                    .binary_search_by(|v| d.total_cmp(v))
                    .ok()
                    .map_or(0.0, |i| 1.0 / (61 + i) as f64)
        };
        let distance = |r: [usize; 2]| old.map_or(0, |o| r[0].abs_diff(o[0]) + r[1].abs_diff(o[1]));
        if let Some((range, l, d)) = measured.into_iter().max_by(|(a, al, ad), (b, bl, bd)| {
            score(*al, *ad)
                .total_cmp(&score(*bl, *bd))
                .then_with(|| distance(*b).cmp(&distance(*a)))
                .then_with(|| b.cmp(a))
        }) && score(l, d) > 0.0
        {
            diagnostic.range = Some(range);
            diagnostic.anchor = Some(range);
            diagnostic.reason = if site.is_some() {
                "evidence_with_link_site"
            } else {
                "evidence_window"
            }
            .into();
            diagnostic.lines = range[1] - range[0] + 1;
            diagnostic.bytes = lines.slice(range).len();
        }
        Ok(())
    }
}

pub(crate) struct Lines<'a> {
    source: &'a str,
    offsets: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(source: &'a str) -> Self {
        let mut offsets = vec![0];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' && i + 1 < source.len() {
                offsets.push(i + 1);
            }
        }
        offsets.push(source.len());
        Self { source, offsets }
    }
    pub(crate) fn len(&self) -> usize {
        self.offsets.len() - 1
    }
    pub(crate) fn line_at(&self, byte: usize) -> usize {
        self.offsets[..self.len()]
            .partition_point(|offset| *offset <= byte)
            .max(1)
    }
    pub(crate) fn slice(&self, range: [usize; 2]) -> &'a str {
        &self.source[self.offsets[range[0] - 1]..self.offsets[range[1]]]
    }
    pub(crate) fn fits(&self, range: [usize; 2], budget: usize) -> bool {
        range[0] >= 1
            && range[0] <= range[1]
            && range[1] <= self.len()
            && range[1] - range[0] < budget
            && self.slice(range).len() <= MAX_BYTES
    }
    pub(crate) fn complete_window(
        &self,
        old: [usize; 2],
        target: [usize; 2],
        allowed: [usize; 2],
        budget: usize,
    ) -> Option<[usize; 2]> {
        if target[0] < allowed[0] || target[1] > allowed[1] || !self.fits(target, budget) {
            return None;
        }
        if old[0] <= target[0] && target[1] <= old[1] {
            return Some(old);
        }
        let width = budget.min(allowed[1] - allowed[0] + 1);
        let low = allowed[0].max((target[1] + 1).saturating_sub(width));
        let high = target[0].min(allowed[1] + 1 - width);
        let start = old[0].clamp(low, high);
        let mut next = [start, start + width - 1];
        while self.slice(next).len() > MAX_BYTES {
            // Only discard padding; concrete matches and their units are mandatory.
            if next[0] < target[0]
                && (next[1] == target[1] || target[0] - next[0] > next[1] - target[1])
            {
                next[0] += 1;
            } else if next[1] > target[1] {
                next[1] -= 1;
            } else {
                return None;
            }
        }
        Some(next)
    }
    pub(crate) fn window(
        &self,
        target: [usize; 2],
        allowed: [usize; 2],
        budget: usize,
        site: Option<usize>,
    ) -> Option<[usize; 2]> {
        let low = allowed[0].max(1).min(self.len());
        let high = allowed[1].max(low).min(self.len());
        let center = site
            .unwrap_or(target[0] + (target[1] - target[0]) / 2)
            .clamp(low, high);
        let width = budget.min(high - low + 1);
        let start = center
            .saturating_sub((width - 1) / 2)
            .max(low)
            .min(high - width + 1);
        let mut range = [start, start + width - 1];
        // Shrink toward the anchor, retaining call sites even with very long nearby lines.
        while self.slice(range).len() > MAX_BYTES {
            if range[0] == range[1] {
                return None;
            }
            if center - range[0] > range[1] - center {
                range[0] += 1;
            } else {
                range[1] -= 1;
            }
        }
        Some(range)
    }
}

/// Complete the units containing the original witnesses, without ranking remote
/// text. A tuple binds test data to its case; a branch binds an action to its guard.
/// Unsupported, ambiguous or oversized boundaries cause a full window fallback.
fn anchored_units_for(
    adapter: &dyn crate::languages::LanguageAdapter,
    root: Node<'_>,
    lines: &Lines<'_>,
    protected: &BTreeSet<usize>,
    extent: [usize; 2],
    budget: usize,
) -> Result<Vec<crate::FocusUnit>, &'static str> {
    if root.has_error() || root.is_missing() {
        return Err("anchored_parse_error");
    }
    if protected.iter().any(|l| *l < extent[0] || *l > extent[1]) {
        return Err("anchored_unbound_witness");
    }
    let mut choices: BTreeMap<usize, Vec<(usize, usize, crate::FocusUnit)>> = BTreeMap::new();
    let mut stack = vec![root];
    let mut visited = 0;
    while let Some(node) = stack.pop() {
        visited += 1;
        if visited > MAX_VISITS {
            return Err("anchored_visit_limit");
        }
        let span = range(node);
        let witnesses: Vec<_> = protected.range(span[0]..=span[1]).copied().collect();
        if witnesses.is_empty() {
            continue;
        }
        if node != root && adapter.is_function(node.kind()) {
            return Err("anchored_nested_owner");
        }
        if adapter.is_comment(node.kind()) {
            return Err("anchored_comment");
        }
        let priority = adapter.anchor_priority(node.kind());
        if let Some(priority) = priority {
            for line in witnesses {
                choices.entry(line).or_default().push((
                    priority,
                    node.byte_range().len(),
                    crate::FocusUnit {
                        range: span,
                        kind: node.kind().to_owned(),
                    },
                ));
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    let mut units = BTreeSet::new();
    for line in protected {
        let mut candidates = choices.remove(line).ok_or("anchored_unbound_witness")?;
        candidates.sort();
        let unit = candidates
            .into_iter()
            .next()
            .ok_or("anchored_unbound_witness")?
            .2;
        if unit.range[0] < extent[0] || unit.range[1] > extent[1] || !lines.fits(unit.range, budget)
        {
            return Err("anchored_budget");
        }
        units.insert(unit);
    }
    Ok(units.into_iter().collect())
}

struct Unit {
    range: [usize; 2],
    kind: String,
    context_bonus: f64,
}

fn range(node: Node<'_>) -> [usize; 2] {
    [
        node.start_position().row + 1,
        node.end_position().row + usize::from(node.end_position().column > 0),
    ]
}

fn intersects(a: [usize; 2], b: [usize; 2]) -> bool {
    a[0] <= b[1] && b[0] <= a[1]
}

#[allow(clippy::too_many_arguments)]
fn select_for(
    adapter: &dyn crate::languages::LanguageAdapter,
    root: Node<'_>,
    lines: &Lines<'_>,
    anchor: [usize; 2],
    extent: [usize; 2],
    terms: &[String],
    site: Option<usize>,
    budget: usize,
) -> Option<Unit> {
    let anchor = [
        anchor[0].max(1).min(lines.len()),
        anchor[1].max(1).min(lines.len()),
    ];
    let mut units = Vec::new();
    let mut stack = vec![root];
    let mut visited = 0;
    while let Some(node) = stack.pop() {
        visited += 1;
        if visited > MAX_VISITS {
            break;
        }
        let span = range(node);
        if !intersects(span, anchor) || node.is_error() || node.is_missing() {
            continue;
        }
        // Nested function declarations have independent navigation locations.
        if node != root && adapter.is_function(node.kind()) {
            continue;
        }
        if let Some(bonus) = adapter
            .focus_bonus(node.kind())
            .filter(|_| adapter.standalone_call(node))
            && span[0] >= extent[0]
            && span[1] <= extent[1]
            && lines.fits(span, budget)
            && !node.has_error()
        {
            units.push(Unit {
                range: span,
                kind: node.kind().into(),
                context_bonus: bonus,
            });
        }
        let mut cursor = node.walk();
        let children: Vec<_> = node.named_children(&mut cursor).collect();
        if node.kind() == "block" {
            // Small adjacent statement groups can preserve setup followed by a call/assertion.
            for group in children.windows(2).chain(children.windows(3)) {
                if group
                    .iter()
                    .all(|n| adapter.is_statement(n.kind()) && !n.has_error())
                {
                    let span = [range(group[0])[0], range(*group.last().unwrap())[1]];
                    if intersects(span, anchor)
                        && span[0] >= extent[0]
                        && span[1] <= extent[1]
                        && lines.fits(span, budget)
                    {
                        units.push(Unit {
                            range: span,
                            kind: "statement_group".into(),
                            context_bonus: 0.1,
                        });
                    }
                }
            }
        }
        stack.extend(children.into_iter().rev());
    }
    if let Some(site) = site {
        // A verified occurrence is a stronger anchor than any query word.
        return units
            .into_iter()
            .filter(|u| u.range[0] <= site && site <= u.range[1])
            .min_by_key(|u| {
                (
                    u.range[1] - u.range[0],
                    adapter.is_call(&u.kind),
                    u.range[0],
                    u.kind.clone(),
                )
            });
    }
    // Only score text inside the retrieval anchor. Extra context cannot pull focus toward
    // an incidental keyword elsewhere in the function.
    let matches: Vec<BTreeSet<usize>> = (anchor[0]..=anchor[1])
        .map(|line| {
            let searchable = text::searchable(lines.slice([line, line]));
            let tokens: BTreeSet<_> = searchable.split_whitespace().collect();
            terms
                .iter()
                .enumerate()
                .filter(|(_, t)| tokens.contains(t.as_str()))
                .map(|(i, _)| i)
                .collect()
        })
        .collect();
    let count = |u: &Unit| {
        let low = u.range[0].max(anchor[0]);
        let high = u.range[1].min(anchor[1]);
        matches[low - anchor[0]..=high - anchor[0]]
            .iter()
            .flat_map(|m| m.iter().copied())
            .collect::<BTreeSet<_>>()
            .len()
    };
    if matches.iter().all(BTreeSet::is_empty) {
        // A semantic match with no lexical overlap does not justify guessing one child.
        return units
            .into_iter()
            .filter(|u| u.range[0] <= anchor[0] && u.range[1] >= anchor[1])
            .min_by_key(|u| (u.range[1] - u.range[0], u.range[0], u.kind.clone()));
    }
    units.into_iter().filter(|u| count(u) > 0).max_by(|a, b| {
        let score = |u: &Unit| {
            4.0 * count(u) as f64 + u.context_bonus - 0.04 * (u.range[1] - u.range[0] + 1) as f64
        };
        score(a)
            .total_cmp(&score(b))
            .then_with(|| b.range[0].cmp(&a.range[0]))
            .then_with(|| b.kind.cmp(&a.kind))
    })
}

#[cfg(test)]
fn anchored_units(
    root: Node<'_>,
    lines: &Lines<'_>,
    protected: &BTreeSet<usize>,
    extent: [usize; 2],
    budget: usize,
) -> Result<Vec<crate::FocusUnit>, &'static str> {
    anchored_units_for(
        crate::languages::for_path("test.rs").unwrap(),
        root,
        lines,
        protected,
        extent,
        budget,
    )
}
#[cfg(test)]
fn select(
    root: Node<'_>,
    lines: &Lines<'_>,
    anchor: [usize; 2],
    extent: [usize; 2],
    terms: &[String],
    site: Option<usize>,
    budget: usize,
) -> Option<Unit> {
    select_for(
        crate::languages::for_path("test.rs").unwrap(),
        root,
        lines,
        anchor,
        extent,
        terms,
        site,
        budget,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(source: &str) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn completes_a_guard_branch_without_selecting_the_whole_long_function() {
        let source = "fn dispatch() {\n    prepare();\n    if permission.denied() {\n        record(\n            \"blocked\",\n            actor,\n        );\n        return Err(Forbidden);\n    }\n    unrelated();\n    finish();\n}\n";
        let tree = tree(source);
        let lines = Lines::new(source);
        let root = tree.root_node().named_child(0).unwrap();
        let selected = select(
            root,
            &lines,
            [3, 5],
            [1, 12],
            &text::query_terms("permission denied"),
            None,
            8,
        )
        .unwrap();
        assert_eq!(selected.range, [3, 9]);
        assert_eq!(selected.kind, "if_expression");
        assert!(lines.fits(selected.range, 8));
        assert!(
            select(
                root,
                &lines,
                [3, 5],
                [1, 12],
                &text::query_terms("permission denied"),
                None,
                2
            )
            .is_none()
        );
    }

    #[test]
    fn verified_site_retains_the_complete_multiline_call_and_ignores_remote_words() {
        let source = "fn run() {\n    call(\n        first,\n        second,\n        third,\n    );\n    unrelated_keyword();\n}\n";
        let tree = tree(source);
        let lines = Lines::new(source);
        let root = tree.root_node().named_child(0).unwrap();
        let selected = select(
            root,
            &lines,
            [3, 3],
            [1, 8],
            &text::query_terms("unrelated keyword"),
            Some(3),
            8,
        )
        .unwrap();
        assert_eq!(selected.range, [2, 6]);
        assert_eq!(selected.kind, "expression_statement");
    }

    #[test]
    fn zero_overlap_keeps_the_full_semantic_anchor_or_falls_back_instead_of_guessing() {
        let source = "fn run() {\n    first();\n    second();\n    third();\n    fourth();\n}\n";
        let tree = tree(source);
        let lines = Lines::new(source);
        let root = tree.root_node().named_child(0).unwrap();
        let terms = text::query_terms("authentication");
        let selected = select(root, &lines, [2, 5], [1, 6], &terms, None, 8).unwrap();
        assert!(selected.range[0] <= 2 && selected.range[1] >= 5);
        assert!(select(root, &lines, [2, 5], [1, 6], &terms, None, 2).is_none());
    }

    #[test]
    fn unicode_crlf_and_oversized_lines_obey_source_and_byte_budgets() {
        let source = "π();\r\n猫();\r\nend();";
        let lines = Lines::new(source);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines.line_at(8), 2);
        assert_eq!(lines.slice([2, 2]), "猫();\r\n");
        assert_eq!(lines.window([1, 1], [1, 3], 1, Some(1)), Some([1, 1]));
        assert_eq!(lines.window([3, 3], [1, 3], 64, Some(3)), Some([1, 3]));
        let long = format!("{}\nshort();\n", "猫".repeat(2000));
        let lines = Lines::new(&long);
        assert_eq!(lines.window([1, 1], [1, 2], 2, Some(1)), None);
        assert_eq!(lines.window([2, 2], [1, 2], 2, Some(2)), Some([2, 2]));
    }

    #[test]
    fn anchored_completion_keeps_a_guard_and_action_with_the_original_match() {
        let source = "fn run() {\n    prepare();\n    if rejected {\n        log_error();\n        cleanup();\n        return;\n    }\n    finish();\n}\n";
        let tree = tree(source);
        let lines = Lines::new(source);
        let protected = BTreeSet::from([4]);
        let units = anchored_units(
            tree.root_node().named_child(0).unwrap(),
            &lines,
            &protected,
            [1, 9],
            6,
        )
        .unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].range, [3, 7]);
        let completed = lines
            .complete_window([1, 6], units[0].range, [1, 9], 6)
            .unwrap();
        assert_eq!(completed, [2, 7]);
        assert!(completed[0] <= 4 && 4 <= completed[1]);
    }

    #[test]
    fn anchored_tuple_retains_entity_and_expected_behavior_together() {
        let source = "fn cases() {\n    let cases = [\n        (\n            \"first_agent\",\n            \"Permission denied\",\n            Expected::Permission,\n        ),\n        (\n            \"second_agent\",\n            \"Trust folder\",\n            Expected::Trust,\n        ),\n    ];\n}\n";
        let tree = tree(source);
        let lines = Lines::new(source);
        let root = tree.root_node().named_child(0).unwrap();
        let first = anchored_units(root, &lines, &BTreeSet::from([5]), [1, 14], 6).unwrap();
        assert_eq!(first[0].range, [3, 7]);
        assert_eq!(first[0].kind, "tuple_expression");
        // Two different cases cannot be forced into a smaller window by dropping one witness.
        let both = anchored_units(root, &lines, &BTreeSet::from([5, 10]), [1, 14], 6).unwrap();
        assert_eq!(both.len(), 2);
        assert!(
            lines
                .complete_window([5, 10], [3, 12], [1, 14], 6)
                .is_none()
        );
    }

    #[test]
    fn anchored_oversized_branches_comments_and_bad_syntax_preserve_fallback() {
        let source = "fn run() {\n    if rejected {\n        error();\n        cleanup();\n        finish();\n    }\n    // unrelated diagnostic\n}\n";
        let tree = tree(source);
        let lines = Lines::new(source);
        let root = tree.root_node().named_child(0).unwrap();
        assert_eq!(
            anchored_units(root, &lines, &BTreeSet::from([3]), [1, 8], 3).unwrap_err(),
            "anchored_budget"
        );
        assert_eq!(
            anchored_units(root, &lines, &BTreeSet::from([7]), [1, 8], 16).unwrap_err(),
            "anchored_comment"
        );
        let bad = "fn run() { if broken {";
        let bad_tree = super::tests::tree(bad);
        assert_eq!(
            anchored_units(
                bad_tree.root_node(),
                &Lines::new(bad),
                &BTreeSet::from([1]),
                [1, 1],
                16
            )
            .unwrap_err(),
            "anchored_parse_error"
        );
    }

    #[test]
    fn anchored_completion_never_discards_a_witness_to_fit_utf8_byte_budget() {
        let source = format!("short();\r\n{}\r\n猫();\r\nend();\r\n", "é".repeat(2050));
        let lines = Lines::new(&source);
        assert!(lines.complete_window([3, 4], [2, 4], [1, 4], 4).is_none());
        assert_eq!(
            lines.complete_window([4, 4], [3, 4], [1, 4], 4),
            Some([3, 4])
        );
    }
}
