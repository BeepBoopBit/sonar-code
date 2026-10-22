//! A bounded investigation workflow. Retrieval leads and recorded connections stay distinct.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::{BriefOptions, IndexState, NavigationLocation, Notice, SearchEngine, explore, text};

mod context;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Default)]
pub struct InvestigationOptions {
    pub scope: Option<String>,
    pub exclusions: Vec<String>,
    /// Expand a chosen handle instead of searching the repository again.
    pub from: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Investigation {
    pub version: u32,
    pub query: String,
    pub index: IndexState,
    pub query_terms: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<crate::RetrievalMode>,
    pub focus_policy: crate::FocusPolicy,
    pub panels: Vec<InvestigationPanel>,
    pub locations: Vec<InvestigationLocation>,
    pub links: Vec<InvestigationLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traversal: Option<InvestigationTraversal>,
    pub notices: Vec<Notice>,
}

#[derive(Debug, Serialize)]
pub struct InvestigationPanel {
    pub purpose: String,
    pub leads: Vec<InvestigationLead>,
}

#[derive(Debug, Serialize)]
pub struct InvestigationLead {
    pub location: String,
    /// Indices into links, ordered from the chosen anchor. Edges retain recorded direction.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<usize>,
}

#[derive(Debug, Serialize)]
pub struct InvestigationLocation {
    pub id: String,
    #[serde(flatten)]
    pub location: NavigationLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<[usize; 2]>,
    pub overlap: TermOverlap,
}

#[derive(Debug, Serialize)]
pub struct InvestigationLink {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub evidence: crate::Evidence,
    pub occurrences: usize,
}

/// Literal stemmed-term observations, never a relevance or confidence estimate.
#[derive(Debug, Serialize)]
pub struct TermOverlap {
    pub identity: Vec<String>,
    pub focus: Vec<String>,
    pub unseen: Vec<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct InvestigationTraversal {
    pub test_hops: usize,
    pub ancestor_limit: usize,
    pub edges_per_lookup: usize,
    pub test_node_limit: usize,
    pub test_nodes_visited: usize,
    pub edge_limit_hits: usize,
    pub test_node_limit_hit: bool,
    pub ancestor_limit_hit: bool,
    pub panel_limit_hits: Vec<String>,
}

/// Reuses a local model across investigations; no downloads or index mutations.
pub struct Investigator {
    db: PathBuf,
    engine: SearchEngine,
}

impl Investigator {
    /// Transports share the investigator's resident model for briefs and follow-up search.
    pub(crate) fn engine(&self) -> &SearchEngine {
        &self.engine
    }

    pub fn open(db: &Path, model: Option<&Path>) -> Result<Self> {
        Ok(Self {
            db: db.into(),
            engine: SearchEngine::open_navigation(db, model)?,
        })
    }

    pub fn investigate(
        &self,
        query: &str,
        options: &InvestigationOptions,
    ) -> Result<Investigation> {
        ensure!(!query.trim().is_empty(), "query must not be empty");
        ensure!(query.len() <= 16_384, "query exceeds the 16 KiB limit");
        let scope = crate::search::normalize_scope(options.scope.as_deref())?;
        let exclusions = crate::exclusions::normalize(&options.exclusions)?;
        let terms: Vec<_> = text::query_terms(query).into_iter().take(64).collect();
        if let Some(anchor) = &options.from {
            return explore::read(&self.db, |s| {
                context::run(s, anchor, query, &terms, &scope, &exclusions)
            });
        }
        // Do not turn the filtered panels into a reranking of the primary result.
        // Each kind filter operates before the retrieval pool cap.
        let engine = &self.engine;
        let mut results = Vec::new();
        for (purpose, kind, limit) in [
            ("primary", None, 5),
            ("functions", Some("function"), 3),
            ("documentation", Some("documentation"), 3),
        ] {
            let (brief, diagnostic) = engine.inspect_filtered(
                query,
                &BriefOptions {
                    scope: options.scope.clone(),
                    limit,
                    related: 0,
                },
                kind,
                (purpose == "functions").then_some(crate::SourceRole::Implementation),
                &exclusions,
            )?;
            let exact = diagnostic
                .candidates
                .iter()
                .any(|c| c.exact_symbol_match || c.exact_path_match);
            results.push((purpose, brief, diagnostic));
            if exact {
                break;
            }
        }
        explore::read(&self.db, |s| {
            let mut response = empty(s, query, &terms);
            let mut selected = BTreeMap::new();
            for (purpose, brief, diagnostic) in results {
                ensure!(
                    serde_json::to_value(&brief.index)? == serde_json::to_value(&s.index)?,
                    "index state changed during investigation; retry after indexing and resolving"
                );
                if purpose == "primary" {
                    response.retrieval = Some(diagnostic.mode);
                    response.focus_policy = diagnostic.focus_policy;
                    let lexical = diagnostic
                        .candidates
                        .iter()
                        .filter_map(|c| c.lexical_rank.map(|r| (r, c.location_id)))
                        .min();
                    let embedding = diagnostic
                        .candidates
                        .iter()
                        .filter_map(|c| c.embedding_rank.map(|r| (r, c.location_id)))
                        .min();
                    if lexical.zip(embedding).is_some_and(|(a, b)| a.1 != b.1) {
                        response.notices.push(Notice::new("channel_leaders_differ", "Lexical and embedding retrieval have different leading locations. This is channel disagreement, not a confidence estimate."));
                    }
                }
                let locations: BTreeMap<_, _> = brief
                    .files
                    .into_iter()
                    .flat_map(|f| f.locations)
                    .map(|l| (l.id.clone(), l))
                    .collect();
                let mut panel = InvestigationPanel {
                    purpose: purpose.into(),
                    leads: Vec::new(),
                };
                for start in brief.starts {
                    let l = &locations[&start];
                    let handle = l
                        .handle
                        .as_ref()
                        .context("investigation requires navigation handles")?;
                    let id = s.resolve(handle)?;
                    selected.entry(id).or_insert(l.focus);
                    panel.leads.push(InvestigationLead {
                        location: handle.clone(),
                        steps: Vec::new(),
                    });
                }
                response.panels.push(panel);
                for notice in brief.notices {
                    if !response
                        .notices
                        .iter()
                        .any(|n| n.code == notice.code && n.message == notice.message)
                    {
                        response.notices.push(notice);
                    }
                }
            }
            for (id, focus) in selected {
                response.locations.push(reading(s, id, &terms, focus)?);
            }
            response.notices.push(Notice::new("independent_leads", "Primary retains five navigation-profile starts. Implementation functions and documentation are independent three-start searches, not connections or confirmed owners/contracts. Duplicate handles share one location. Exact identity queries omit the additional searches."));
            finish(&mut response);
            Ok(response)
        })
    }
}

fn empty(s: &explore::Snapshot<'_>, query: &str, terms: &[String]) -> Investigation {
    Investigation {
        version: 1,
        query: query.into(),
        index: s.response().index,
        query_terms: terms.to_vec(),
        retrieval: None,
        focus_policy: crate::FocusPolicy::Window,
        panels: Vec::new(),
        locations: Vec::new(),
        links: Vec::new(),
        anchor: None,
        traversal: None,
        notices: Vec::new(),
    }
}

fn reading(
    s: &explore::Snapshot<'_>,
    id: i64,
    terms: &[String],
    focus: Option<[usize; 2]>,
) -> Result<InvestigationLocation> {
    let location = s.location(id)?;
    let source = &s.sources[&location.path].source;
    let identity = text::query_terms(&format!(
        "{} {} {}",
        location.path,
        location.symbol.as_deref().unwrap_or(""),
        location.title.as_deref().unwrap_or("")
    ))
    .into_iter()
    .collect::<BTreeSet<_>>();
    let body = focus
        .map(|[lo, hi]| {
            source
                .lines()
                .skip(lo - 1)
                .take(hi - lo + 1)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let tokens: BTreeSet<_> = text::query_terms(&body).into_iter().collect();
    let overlap = TermOverlap {
        identity: terms
            .iter()
            .filter(|t| identity.contains(*t))
            .cloned()
            .collect(),
        focus: terms
            .iter()
            .filter(|t| tokens.contains(*t))
            .cloned()
            .collect(),
        unseen: terms
            .iter()
            .filter(|t| !identity.contains(*t) && !tokens.contains(*t))
            .cloned()
            .collect(),
    };
    Ok(InvestigationLocation {
        id: String::new(),
        location,
        focus,
        overlap,
    })
}

fn window(s: &explore::Snapshot<'_>, id: i64, terms: &[String]) -> Result<Option<[usize; 2]>> {
    let location = s.location(id)?;
    let [lo, hi] = location.range;
    let lines: Vec<_> = s.sources[&location.path].source.lines().collect();
    let body = lines
        .iter()
        .skip(lo - 1)
        .take(hi - lo + 1)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let (range, _) = text::focus(&body, lo, terms);
    // An oversized single line has no safe bounded preview.
    let mut bytes = 0;
    let mut end = None;
    for line in range[0]..=range[1].min(hi) {
        bytes += lines.get(line - 1).map_or(0, |s| s.len() + 1);
        if bytes > 4096 {
            break;
        }
        end = Some(line);
    }
    Ok(end.map(|end| [range[0], end]))
}

fn finish(response: &mut Investigation) {
    // Return each long generation-bound handle once; routes use response-local IDs.
    let mut ids = BTreeMap::new();
    for (i, l) in response.locations.iter_mut().enumerate() {
        l.id = format!("n{}", i + 1);
        ids.insert(l.location.handle.clone(), l.id.clone());
        // Containment is explicit in trails; omit unused parent handles.
        l.location.parent = None;
    }
    for panel in &mut response.panels {
        for lead in &mut panel.leads {
            lead.location = ids[&lead.location].clone();
        }
    }
    for link in &mut response.links {
        link.from = ids[&link.from].clone();
        link.to = ids[&link.to].clone();
    }
    if let Some(anchor) = &mut response.anchor {
        *anchor = ids[anchor].clone();
    }
    response.notices.push(Notice::new("term_overlap_only", "Overlap lists stemmed query terms seen in a path, symbol, heading or returned focus. Unseen terms can occur elsewhere or use different vocabulary. Overlap does not establish behavior, relevance, confidence or feature absence."));
    if !response.locations.is_empty()
        && response
            .locations
            .iter()
            .all(|l| l.overlap.focus.is_empty())
    {
        response.notices.push(Notice::new("no_focus_term_overlap", "None of the returned reading ranges contains a searchable query term. Inspect the identities and scope before treating these as useful leads."));
    }
    if text::query_terms(&response.query).len() > 64
        && !response.notices.iter().any(|n| n.code == "query_truncated")
    {
        response.notices.push(Notice::new(
            "query_truncated",
            "Overlap and focus use only the first 64 distinct searchable query terms.",
        ));
    }
}
