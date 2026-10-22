use crate::{
    FocusDiagnostic,
    candidate::Candidate,
    focus::{ContextualQuery, Resolver},
};
use anyhow::{Context, Result};
use rusqlite::Connection;
impl Resolver<'_> {
    /// Apply the repaired source-fact policy only to returned function starts.
    /// Facts use this query's checked source bytes and the already loaded encoder.
    pub(crate) fn rust_contextual(
        &mut self,
        conn: &Connection,
        candidate: &Candidate,
        query: ContextualQuery<'_>,
        site: Option<usize>,
        diagnostic: &mut FocusDiagnostic,
    ) -> Result<()> {
        let ContextualQuery {
            model,
            vector,
            terms,
        } = query;
        if candidate.kind != "function" {
            return Ok(());
        }
        let file = &self.files[self
            .files
            .binary_search_by(|f| f.path.cmp(&candidate.path))
            .unwrap()];
        if !self.trees.contains_key(&file.path) {
            let tree = crate::languages::parse_tree(&file.path, &file.source)?;
            self.trees.insert(file.path.clone(), tree);
        }
        let tree = &self.trees[&file.path];
        let (start_byte, end_byte): (usize, usize) = conn.query_row(
            "SELECT start_byte,end_byte FROM locations WHERE id=?1",
            [candidate.id],
            |r| Ok((r.get::<_, u32>(0)? as usize, r.get::<_, u32>(1)? as usize)),
        )?;
        let lines = &self.lines[&file.path];
        let owner = crate::SourceSpan {
            start_byte,
            end_byte,
            start_line: lines.line_at(start_byte),
            end_line: lines.line_at(end_byte.saturating_sub(1)),
        };
        let node = tree
            .root_node()
            .descendant_for_byte_range(start_byte, end_byte)
            .context("missing focus owner")?;
        if node.has_error()
            || node.kind() != "function_item"
            || node.child_by_field_name("body").is_none()
        {
            diagnostic.reason = "contextual_unsupported_owner".into();
            return Ok(());
        }
        let views = crate::focus_facts::build_for_query(
            &crate::focus_facts::Owner {
                location_id: candidate.id,
                source_hash: &file.hash,
                symbol: candidate.symbol.as_deref(),
                source_role: candidate.role(),
                span: Some(&owner),
                status: "current",
                evidence_truncated: false,
            },
            &file.source,
            tree,
            terms,
        )?;
        diagnostic.contextual = Some(crate::ContextualFocusDiagnostic {
            facts: views.facts.len(),
            operation: None,
            occurrence: None,
            context: Vec::new(),
        });
        if views.facts.is_empty() || vector.iter().all(|v| *v == 0.0) {
            diagnostic.reason = format!(
                "contextual_{}",
                if views.facts.is_empty() {
                    views.status.as_str()
                } else {
                    "zero_query"
                }
            );
            return Ok(());
        }
        // Legacy static pooling depends on batch partners; the evaluated policy uses singletons.
        let mut winner = None;
        for (i, fact) in views.facts.iter().enumerate() {
            let encoded = model.encode(std::slice::from_ref(&fact.contextual))?;
            let score: f64 = vector
                .iter()
                .zip(&encoded[0])
                .map(|(a, b)| f64::from(*a) * f64::from(*b))
                .sum();
            if winner.is_none_or(|(_, best)| score > best) {
                winner = Some((i, score));
            }
        }
        let fact = &views.facts[winner.unwrap().0];
        let details = diagnostic.contextual.as_mut().unwrap();
        details.operation = Some(fact.observation.operation.clone());
        details.occurrence = Some(fact.observation.span.clone());
        // Tree-sitter identifier identity handles Unicode without approximating Rust's identifier grammar.
        let occurrence = &fact.observation.span;
        let bare = tree
            .root_node()
            .descendant_for_byte_range(occurrence.start_byte, occurrence.end_byte)
            .is_some_and(|n| n.kind() == "identifier");
        if bare
            && matches!(
                fact.observation.operation.as_str(),
                "local initializer" | "returned local initializer" | "returned expression"
            )
        {
            diagnostic.reason = "contextual_bare_value_fallback".into();
            return Ok(());
        }
        let anchor = [
            lines.line_at(occurrence.start_byte),
            lines.line_at(occurrence.end_byte - 1),
        ];
        let allowed = [owner.start_line, owner.end_line];
        if !lines.fits(anchor, self.options.max_lines) {
            diagnostic.reason = "contextual_anchor_budget".into();
            return Ok(());
        }
        let Some(window) = lines.window(anchor, allowed, self.options.max_lines, None) else {
            return Ok(());
        };
        let mut context: Vec<_> = fact.context.iter().collect();
        context.sort_by_key(|c| match c.relation.as_str() {
            "guard" | "else guard" | "match pattern" => 0,
            "statement" => 1,
            "matched value" => 2,
            "local binding" => 3,
            _ => 4,
        });
        let mut protected = anchor;
        for c in context {
            let range = [
                lines.line_at(c.span.start_byte),
                lines.line_at(c.span.end_byte - 1),
            ];
            let next = [protected[0].min(range[0]), protected[1].max(range[1])];
            if next != protected && lines.fits(next, self.options.max_lines) {
                protected = next;
                details.context.push(crate::FocusUnit {
                    range,
                    kind: c.relation.clone(),
                });
            }
        }
        let range = lines
            .complete_window(window, protected, allowed, self.options.max_lines)
            .unwrap_or(window);
        if site.is_some_and(|s| s < range[0] || s > range[1]) {
            diagnostic.reason = "contextual_link_site_fallback".into();
            return Ok(());
        }
        diagnostic.range = Some(range);
        diagnostic.anchor = Some(anchor);
        diagnostic.reason = "contextual_fact".into();
        diagnostic.lines = range[1] - range[0] + 1;
        diagnostic.bytes = lines.slice(range).len();
        Ok(())
    }
}
