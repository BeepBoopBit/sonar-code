//! Query-guided exploration of recorded relationships, with an auditable route per match.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
};

use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::{
    Direction, Embedder, Evidence, FocusOptions, FocusPolicy, IndexState, NavigationLink,
    NavigationLocation, Notice, RetrievalMode, assembly, explore, search,
};

#[cfg(test)]
mod tests;

const MAX_NODES: usize = 256;
const EDGES_PER_DIRECTION: usize = 64;

#[derive(Debug, Clone, Copy, Default, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum FollowRelation {
    /// Recorded calls and references. Containment is not a behavioral connection.
    #[default]
    Semantic,
    Calls,
    References,
}

impl FollowRelation {
    fn name(self) -> &'static str {
        match self {
            Self::Semantic => "semantic",
            Self::Calls => "calls",
            Self::References => "references",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FollowOptions {
    pub relation: FollowRelation,
    pub direction: Direction,
    pub scope: Option<String>,
    /// Restrict returned endpoints (for example, "function"), while keeping intermediate kinds.
    pub kind: Option<String>,
    pub hops: usize,
    pub limit: usize,
    pub focus: FocusOptions,
}

impl Default for FollowOptions {
    fn default() -> Self {
        Self {
            relation: FollowRelation::Semantic,
            direction: Direction::Both,
            scope: None,
            kind: None,
            hops: 2,
            limit: 5,
            focus: FocusOptions {
                policy: FocusPolicy::Window,
                max_lines: 16,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FollowMatch {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<[usize; 2]>,
    /// Ordered zero-based indices into links; walk from anchor in either direction.
    /// Each link retains the recorded direction, even for an incoming traversal.
    pub steps: Vec<usize>,
}

#[derive(Debug, Serialize)]
pub struct FollowTraversal {
    pub relation: FollowRelation,
    pub direction: Direction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    pub hops: usize,
    pub node_limit: usize,
    pub edges_per_direction: usize,
    pub visited: usize,
    pub expanded: usize,
    pub edge_limit_hits: usize,
    pub node_limit_hit: bool,
    /// Locations reached at the requested hop boundary, whose neighbors were not inspected.
    pub depth_frontier: usize,
    pub matching: usize,
    pub unreturned_matches: usize,
}

impl FollowTraversal {
    fn new(options: &FollowOptions) -> Self {
        Self {
            relation: options.relation,
            direction: options.direction,
            kind: options.kind.clone(),
            hops: options.hops,
            node_limit: MAX_NODES,
            edges_per_direction: EDGES_PER_DIRECTION,
            visited: 1,
            expanded: 0,
            edge_limit_hits: 0,
            node_limit_hit: false,
            depth_frontier: 0,
            matching: 0,
            unreturned_matches: 0,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct FollowResponse {
    pub version: u32,
    pub query: String,
    pub anchor: NavigationLocation,
    pub matches: Vec<FollowMatch>,
    /// All route endpoints except anchor, including otherwise unranked intermediate locations.
    pub locations: Vec<NavigationLocation>,
    pub links: Vec<NavigationLink>,
    pub traversal: FollowTraversal,
    pub index: IndexState,
    pub notices: Vec<Notice>,
}

#[derive(Clone)]
struct Edge {
    from: i64,
    to: i64,
    kind: String,
    path: String,
    line: usize,
    occurrences: usize,
}

struct Visit {
    depth: usize,
    previous: Option<(i64, Edge)>,
}

// Restrict both endpoints before applying the edge budget. Separate directions prevent
// a heavily referenced symbol from hiding all of its outgoing implementation calls.
fn edges(
    conn: &Connection,
    id: i64,
    scope: &str,
    relation: FollowRelation,
    incoming: bool,
) -> Result<Vec<Edge>> {
    let endpoint = if incoming { "to_id" } else { "from_id" };
    let sql = format!("SELECT e.from_id,e.to_id,e.kind,e.path,min(e.line),count(*)
        FROM semantic_edges e JOIN locations a ON a.id=e.from_id JOIN files af ON af.id=a.file_id
        JOIN locations b ON b.id=e.to_id JOIN files bf ON bf.id=b.file_id
        WHERE e.{endpoint}=?1 AND e.from_id!=e.to_id
        AND e.kind IN ('calls','references') AND (?2='semantic' OR e.kind=?2)
        AND (?3='' OR af.path=?3 OR substr(af.path,1,length(?3)+1)=?3||'/')
        AND (?3='' OR bf.path=?3 OR substr(bf.path,1,length(?3)+1)=?3||'/')
        GROUP BY e.from_id,e.to_id,e.kind,e.path
        ORDER BY e.kind!='calls',af.path,a.start_byte,bf.path,b.start_byte,e.kind,e.path,min(e.line),e.from_id,e.to_id LIMIT ?4");
    Ok(conn
        .prepare(&sql)?
        .query_map(
            params![id, relation.name(), scope, (EDGES_PER_DIRECTION + 1) as i64],
            |r| {
                Ok(Edge {
                    from: r.get(0)?,
                    to: r.get(1)?,
                    kind: r.get(2)?,
                    path: r.get(3)?,
                    line: r.get::<_, u32>(4)? as usize,
                    occurrences: r.get::<_, u32>(5)? as usize,
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?)
}

fn walk(
    conn: &Connection,
    anchor: i64,
    scope: &str,
    options: &FollowOptions,
) -> Result<(BTreeMap<i64, Visit>, FollowTraversal)> {
    let mut visits = BTreeMap::from([(
        anchor,
        Visit {
            depth: 0,
            previous: None,
        },
    )]);
    let mut queue = VecDeque::from([anchor]);
    let mut stats = FollowTraversal::new(options);
    while let Some(id) = queue.pop_front() {
        let depth = visits[&id].depth;
        if depth == options.hops {
            stats.depth_frontier += 1;
            continue;
        }
        stats.expanded += 1;
        let directions: &[bool] = match options.direction {
            Direction::Incoming => &[true],
            Direction::Outgoing => &[false],
            Direction::Both => &[false, true],
        };
        for &incoming in directions {
            let found = edges(conn, id, scope, options.relation, incoming)?;
            stats.edge_limit_hits += usize::from(found.len() > EDGES_PER_DIRECTION);
            for edge in found.into_iter().take(EDGES_PER_DIRECTION) {
                let other = if incoming { edge.from } else { edge.to };
                if visits.contains_key(&other) {
                    continue;
                }
                if visits.len() == MAX_NODES {
                    stats.node_limit_hit = true;
                    continue;
                }
                visits.insert(
                    other,
                    Visit {
                        depth: depth + 1,
                        previous: Some((id, edge)),
                    },
                );
                queue.push_back(other);
            }
        }
    }
    stats.visited = visits.len();
    Ok((visits, stats))
}

pub(crate) fn run(
    db: &Path,
    anchor: &str,
    query: &str,
    options: &FollowOptions,
    mode: RetrievalMode,
    model: Option<&Embedder>,
) -> Result<FollowResponse> {
    search::validate_kind(options.kind.as_deref())?;
    ensure!(
        !query.trim().is_empty() && query.len() <= 16_384,
        "provide a nonempty query of at most 16 KiB"
    );
    ensure!(
        (1..=4).contains(&options.hops),
        "follow hops must be between 1 and 4"
    );
    ensure!(
        (1..=20).contains(&options.limit),
        "follow limit must be between 1 and 20"
    );
    ensure!(
        (1..=64).contains(&options.focus.max_lines),
        "focus lines must be between 1 and 64"
    );
    ensure!(
        options.focus.policy != FocusPolicy::Legacy,
        "follow requires a bounded focus policy, such as window"
    );
    ensure!(
        options.focus.policy != FocusPolicy::Contextual
            || model.is_some_and(Embedder::supports_contextual_focus),
        "contextual focus requires embedding or hybrid retrieval with a static model and legacy encoding"
    );
    let scope = search::normalize_scope(options.scope.as_deref())?;
    explore::read(db, |s| {
        let id = s.resolve(anchor)?;
        let anchor = s.location(id)?;
        ensure!(
            search::in_scope(&anchor.path, &scope),
            "anchor is outside the requested scope"
        );
        let mut response = FollowResponse {
            version: 1,
            query: query.into(),
            anchor,
            matches: vec![],
            locations: vec![],
            links: vec![],
            traversal: FollowTraversal::new(options),
            index: s.response().index,
            notices: vec![],
        };
        if !s.semantic_for(id)? {
            response.notices.push(s.graph_notice(id)?);
            return Ok(response);
        }
        response.notices.push(Notice::new("semantic_coverage",
            "Trails connect recorded static calls/references, not execution order, data flow or runtime dispatch. Unmapped/external and ambiguous targets are omitted."));
        let mut terms = if mode == RetrievalMode::Embedding {
            vec![]
        } else {
            crate::text::query_terms(query)
        };
        if terms.len() > 64 {
            terms.truncate(64);
            response.notices.push(Notice::new(
                "query_truncated",
                "Lexical matching uses the first 64 distinct searchable query terms.",
            ));
        }
        let vector = if let Some(model) = model {
            search::validate_embeddings(s.conn, &s.index.snapshot, model)?;
            let vector = model.encode(&[query.into()])?.remove(0);
            if vector.iter().all(|v| *v == 0.0) {
                None
            } else {
                Some(vector)
            }
        } else {
            None
        };
        let (visits, stats) = walk(s.conn, id, &scope, options)?;
        response.traversal = stats;
        if response.traversal.node_limit_hit || response.traversal.edge_limit_hits > 0 {
            response.notices.push(Notice::new("traversal_truncated",
                "Traversal reached a node or edge budget. Unvisited connections may contain other matches; narrow scope or relation, or continue from another handle."));
        }
        let eligible: BTreeSet<_> = visits
            .keys()
            .copied()
            .filter(|other| *other != id)
            .collect();
        let mut candidates = BTreeMap::new();
        for other in &eligible {
            let candidate = search::load_parent(s.conn, *other)?;
            if options
                .kind
                .as_ref()
                .is_none_or(|kind| candidate.kind == *kind)
            {
                candidates.insert(*other, candidate);
            }
        }
        let eligible = candidates.keys().copied().collect();
        let mut ranked = assembly::relevance(
            s.conn,
            &candidates,
            &eligible,
            &assembly::Query {
                terms: &terms,
                vector: vector.as_deref(),
            },
        )?;
        let identity = query.trim();
        let exact = |other: &i64| {
            let c = &candidates[other];
            c.symbol.as_deref() == Some(identity) || c.path == identity
        };
        ranked.retain(|d| exact(&d.location_id) || d.relevance > 0.0);
        ranked.sort_by(|a, b| {
            exact(&b.location_id)
                .cmp(&exact(&a.location_id))
                .then_with(|| b.relevance.total_cmp(&a.relevance))
                .then_with(|| {
                    visits[&a.location_id]
                        .depth
                        .cmp(&visits[&b.location_id].depth)
                })
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| {
                    candidates[&a.location_id]
                        .definition
                        .cmp(&candidates[&b.location_id].definition)
                })
                .then_with(|| a.location_id.cmp(&b.location_id))
        });
        response.traversal.matching = ranked.len();
        response.traversal.unreturned_matches = ranked.len().saturating_sub(options.limit);
        ranked.truncate(options.limit);
        if response.traversal.unreturned_matches > 0 {
            response.notices.push(Notice::new("results_truncated", "Additional matches were ranked within this neighborhood; increase --limit or refine the follow-up query."));
        }
        if ranked.is_empty() {
            response.notices.push(Notice::new("no_matches", "No matching endpoint was found within the inspected neighborhood. This does not establish absence elsewhere in the repository."));
        }
        let paths: BTreeSet<_> = ranked.iter().map(|d| d.path.as_str()).collect();
        let files: Vec<_> = paths
            .into_iter()
            .map(|path| {
                let f = &s.sources[path];
                crate::workspace::SourceFile {
                    path: f.path.clone(),
                    hash: f.hash.clone(),
                    source: f.source.clone(),
                }
            })
            .collect();
        let mut focuser = crate::focus::Resolver::new(&files, options.focus)?;
        let mut output_ids = BTreeSet::new();
        let mut link_ids = BTreeMap::new();
        for d in ranked {
            let is_exact = exact(&d.location_id);
            let mut candidate = candidates[&d.location_id].clone();
            candidate.use_hit(d.winning_hit);
            let legacy = search::location_focus(s.conn, &candidate, &terms, None)?;
            let mut focus = focuser.resolve(s.conn, &candidate, &terms, None, legacy)?;
            if options.focus.policy == FocusPolicy::Evidence {
                focuser.evidence(
                    s.conn,
                    &candidate,
                    &terms,
                    vector.as_deref(),
                    None,
                    &mut focus,
                )?;
            }
            if options.focus.policy == FocusPolicy::Contextual
                && !is_exact
                && let Some(vector) = vector.as_deref()
            {
                focuser.contextual(
                    s.conn,
                    &candidate,
                    crate::focus::ContextualQuery {
                        model: model.unwrap(),
                        vector,
                        terms: &terms,
                    },
                    None,
                    &mut focus,
                )?;
            }
            let mut path = vec![];
            let mut current = d.location_id;
            while let Some((previous, edge)) = &visits[&current].previous {
                path.push(edge);
                current = *previous;
            }
            path.reverse();
            let mut steps = vec![];
            for edge in path {
                output_ids.extend([edge.from, edge.to]);
                let key = (edge.from, edge.to, edge.kind.clone(), edge.path.clone());
                let index = if let Some(index) = link_ids.get(&key) {
                    *index
                } else {
                    let index = response.links.len();
                    response.links.push(NavigationLink {
                        from: explore::handle(s.conn, &s.generation, edge.from)?,
                        to: explore::handle(s.conn, &s.generation, edge.to)?,
                        kind: edge.kind.clone(),
                        evidence: Evidence {
                            path: edge.path.clone(),
                            line: edge.line,
                        },
                        occurrences: edge.occurrences,
                    });
                    link_ids.insert(key, index);
                    index
                };
                steps.push(index);
            }
            response.matches.push(FollowMatch {
                target: explore::handle(s.conn, &s.generation, d.location_id)?,
                focus: focus.range,
                steps,
            });
        }
        output_ids.remove(&id);
        response.locations = output_ids
            .into_iter()
            .map(|id| s.location(id))
            .collect::<Result<_>>()?;
        Ok(response)
    })
}
