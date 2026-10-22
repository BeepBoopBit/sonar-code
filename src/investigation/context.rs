use std::collections::{BTreeMap, BTreeSet, VecDeque};

use anyhow::{Result, ensure};
use rusqlite::params;

use super::*;

const EDGE_LIMIT: usize = 64;
const NODE_LIMIT: usize = 128;
const ANCESTOR_LIMIT: usize = 8;
const PANEL_LIMIT: usize = 3;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    from: i64,
    to: i64,
    kind: String,
    path: String,
    line: usize,
    occurrences: usize,
}

struct Route {
    target: i64,
    edges: Vec<Edge>,
}

/// Eligibility applies before each per-node cap, including the evidence path.
fn edges(
    s: &explore::Snapshot<'_>,
    id: i64,
    mode: &str,
    allowed: &str,
    stats: &mut InvestigationTraversal,
) -> Result<Vec<Edge>> {
    let (table, condition) = match mode {
        "incoming" => (
            "semantic_edges",
            "e.to_id=?1 AND e.kind IN ('calls','references')",
        ),
        "outgoing" => ("semantic_edges", "e.from_id=?1 AND e.kind='calls'"),
        "documents" => (
            "navigation_edges",
            "(e.from_id=?1 OR e.to_id=?1) AND e.kind IN ('documents','embeds')",
        ),
        _ => unreachable!(),
    };
    let sql = format!("WITH allowed AS (SELECT value path FROM json_each(?2))
        SELECT e.from_id,e.to_id,e.kind,e.path,min(e.line),count(*)
        FROM {table} e JOIN locations a ON a.id=e.from_id JOIN files af ON af.id=a.file_id
        JOIN locations b ON b.id=e.to_id JOIN files bf ON bf.id=b.file_id
        WHERE {condition} AND e.from_id!=e.to_id
        AND af.path IN (SELECT path FROM allowed) AND bf.path IN (SELECT path FROM allowed)
        AND e.path IN (SELECT path FROM allowed)
        GROUP BY e.from_id,e.to_id,e.kind,e.path
        ORDER BY e.kind!='calls',af.path,a.start_byte,bf.path,b.start_byte,e.kind,e.path,min(e.line),e.from_id,e.to_id
        LIMIT ?3");
    let mut found = s
        .conn
        .prepare(&sql)?
        .query_map(params![id, allowed, (EDGE_LIMIT + 1) as i64], |r| {
            Ok(Edge {
                from: r.get(0)?,
                to: r.get(1)?,
                kind: r.get(2)?,
                path: r.get(3)?,
                line: r.get::<_, u32>(4)? as usize,
                occurrences: r.get::<_, u32>(5)? as usize,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    stats.edge_limit_hits += usize::from(found.len() > EDGE_LIMIT);
    found.truncate(EDGE_LIMIT);
    Ok(found)
}

fn parent(s: &explore::Snapshot<'_>, id: i64) -> Result<Option<Edge>> {
    let (p, path, line): (Option<i64>, String, Option<u32>) = s.conn.query_row(
        "SELECT l.parent_id,f.path,coalesce(l.definition,(SELECT min(start_line) FROM regions WHERE location_id=l.id))
        FROM locations l JOIN files f ON f.id=l.file_id WHERE l.id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(p.zip(line).map(|(p, line)| Edge {
        from: p,
        to: id,
        kind: "contains".into(),
        path,
        line: line as usize,
        occurrences: 1,
    }))
}

pub(super) fn run(
    s: &explore::Snapshot<'_>,
    anchor: &str,
    query: &str,
    terms: &[String],
    scope: &str,
    exclusions: &[String],
) -> Result<Investigation> {
    let id = s.resolve(anchor)?;
    let allowed: BTreeSet<_> = s
        .sources
        .keys()
        .filter(|p| {
            crate::search::in_scope(p, scope)
                && !exclusions.iter().any(|e| crate::search::in_scope(p, e))
        })
        .cloned()
        .collect();
    ensure!(
        allowed.contains(&s.location(id)?.path),
        "anchor is outside the scope or excluded"
    );
    let allowed_json = serde_json::to_string(&allowed)?;
    let mut response = empty(s, query, terms);
    response.anchor = Some(anchor.into());
    let mut stats = InvestigationTraversal {
        test_hops: 2,
        ancestor_limit: ANCESTOR_LIMIT,
        edges_per_lookup: EDGE_LIMIT,
        test_node_limit: NODE_LIMIT,
        ..Default::default()
    };
    let mut groups: BTreeMap<&str, Vec<Route>> = [
        "callers",
        "callees",
        "references",
        "test_code",
        "documentation",
        "linked_sources",
    ]
    .into_iter()
    .map(|g| (g, Vec::new()))
    .collect();

    if s.semantic_for(id)? {
        let mut visited = BTreeSet::from([id]);
        let mut queue = VecDeque::from([(id, Vec::<Edge>::new())]);
        while let Some((current, trail)) = queue.pop_front() {
            if trail.len() == 2 {
                continue;
            }
            for edge in edges(s, current, "incoming", &allowed_json, &mut stats)? {
                let target = edge.from;
                let mut route = trail.clone();
                route.push(edge.clone());
                if current == id {
                    groups
                        .get_mut(if edge.kind == "calls" {
                            "callers"
                        } else {
                            "references"
                        })
                        .unwrap()
                        .push(Route {
                            target,
                            edges: route.clone(),
                        });
                }
                if visited.contains(&target) {
                    continue;
                }
                if visited.len() == NODE_LIMIT {
                    stats.test_node_limit_hit = true;
                    continue;
                }
                visited.insert(target);
                if s.location(target)?.source_role == "test" {
                    groups.get_mut("test_code").unwrap().push(Route {
                        target,
                        edges: route.clone(),
                    });
                }
                queue.push_back((target, route));
            }
        }
        stats.test_nodes_visited = visited.len();
        for edge in edges(s, id, "outgoing", &allowed_json, &mut stats)? {
            groups.get_mut("callees").unwrap().push(Route {
                target: edge.to,
                edges: vec![edge],
            });
        }
        response.notices.push(Notice::new("semantic_coverage", if s.location(id)?.language.as_deref() == Some("typescript") {
            "Call/reference routes use the recorded compiler analysis profiles. Static connections do not prove execution order or runtime dispatch. Test-code endpoints can include helpers; they do not establish coverage of the requested behavior."
        } else {
            "Call/reference routes use the recorded Rust analysis profile. Static connections do not prove execution order or runtime dispatch. Test-code endpoints can include helpers; they do not establish coverage of the requested behavior."
        }));
        if s.location(id)?.language.as_deref() == Some("typescript")
            && let Some(notice) = crate::languages::typescript::semantic::coverage_notice(s.conn)?
        {
            response.notices.push(notice);
        }
    } else {
        response.notices.push(s.graph_notice(id)?);
    }

    if s.navigation_current {
        let mut current = id;
        let mut trail = Vec::new();
        for depth in 0..=ANCESTOR_LIMIT {
            for edge in edges(s, current, "documents", &allowed_json, &mut stats)? {
                let target = if edge.from == current {
                    edge.to
                } else {
                    edge.from
                };
                if target == id
                    || trail
                        .iter()
                        .any(|e: &Edge| e.from == target || e.to == target)
                {
                    continue;
                }
                let mut route = trail.clone();
                route.push(edge);
                let group = if s.location(target)?.source_role == "documentation" {
                    "documentation"
                } else {
                    "linked_sources"
                };
                groups.get_mut(group).unwrap().push(Route {
                    target,
                    edges: route,
                });
            }
            let Some(edge) = parent(s, current)? else {
                break;
            };
            if depth == ANCESTOR_LIMIT {
                stats.ancestor_limit_hit = true;
                break;
            }
            ensure!(
                allowed.contains(&edge.path),
                "containing source is outside the requested filters"
            );
            current = edge.from;
            trail.push(edge);
        }
        response.notices.push(Notice::new("literal_document_links", "Documentation and linked sources follow literal recorded links, possibly through containing declarations. A link to a containing file is not evidence that the document specifies this particular operation. Inspect every route's source evidence."));
    } else {
        response.notices.push(Notice::new("navigation_unavailable", "Literal document/resource connections are unavailable. Run `sonar index-navigation` after indexing."));
    }

    let mut locations = BTreeMap::new();
    locations.insert(id, reading(s, id, terms, window(s, id, terms)?)?);
    let mut link_ids = BTreeMap::new();
    for name in [
        "callers",
        "callees",
        "references",
        "test_code",
        "documentation",
        "linked_sources",
    ] {
        let mut routes = groups.remove(name).unwrap();
        // Preserve shortest recorded paths; query overlap only breaks equal-depth ties.
        let mut keys = BTreeMap::new();
        for route in &routes {
            if let std::collections::btree_map::Entry::Vacant(entry) = keys.entry(route.target) {
                let l = reading(s, route.target, terms, window(s, route.target, terms)?)?;
                let overlap = l
                    .overlap
                    .identity
                    .iter()
                    .chain(&l.overlap.focus)
                    .collect::<BTreeSet<_>>()
                    .len();
                entry.insert((
                    std::cmp::Reverse(overlap),
                    l.location.path,
                    l.location.range,
                    route.target,
                ));
            }
        }
        routes.sort_by(|a, b| {
            (a.edges.len(), &keys[&a.target], &a.edges).cmp(&(
                b.edges.len(),
                &keys[&b.target],
                &b.edges,
            ))
        });
        let mut seen = BTreeSet::new();
        routes.retain(|r| seen.insert(r.target));
        if routes.len() > PANEL_LIMIT {
            stats.panel_limit_hits.push(name.into());
        }
        let mut panel = InvestigationPanel {
            purpose: name.into(),
            leads: Vec::new(),
        };
        for route in routes.into_iter().take(PANEL_LIMIT) {
            let mut steps = Vec::new();
            for edge in route.edges {
                for other in [edge.from, edge.to] {
                    if let std::collections::btree_map::Entry::Vacant(entry) =
                        locations.entry(other)
                    {
                        entry.insert(reading(s, other, terms, window(s, other, terms)?)?);
                    }
                }
                let idx = if let Some(idx) = link_ids.get(&edge) {
                    *idx
                } else {
                    let idx = response.links.len();
                    response.links.push(InvestigationLink {
                        from: locations[&edge.from].location.handle.clone(),
                        to: locations[&edge.to].location.handle.clone(),
                        kind: edge.kind.clone(),
                        evidence: crate::Evidence {
                            path: edge.path.clone(),
                            line: edge.line,
                        },
                        occurrences: edge.occurrences,
                    });
                    link_ids.insert(edge, idx);
                    idx
                };
                steps.push(idx);
            }
            panel.leads.push(InvestigationLead {
                location: locations[&route.target].location.handle.clone(),
                steps,
            });
        }
        response.panels.push(panel);
    }
    response.locations = locations.into_values().collect();
    response.notices.push(Notice::new("bounded_context", "Each context panel contains at most three endpoints. Test code is reached through incoming calls/references within two hops and 128 nodes; each edge lookup admits at most 64 connections. Documentation checks at most eight containing ancestors. Empty panels do not establish absence. Use neighbors/follow/field-uses for further investigation."));
    if stats.test_node_limit_hit
        || stats.edge_limit_hits > 0
        || stats.ancestor_limit_hit
        || !stats.panel_limit_hits.is_empty()
    {
        response.notices.push(Notice::new("context_truncated", "Some candidate connections or endpoints were omitted by the reported limits. Narrow scope, exclude an irrelevant area, or continue with neighbors/follow from a returned handle."));
    }
    if !exclusions.is_empty() {
        let count = s
            .sources
            .keys()
            .filter(|p| exclusions.iter().any(|e| crate::search::in_scope(p, e)))
            .count();
        response.notices.push(Notice::new("path_exclusions", format!("{count} indexed files are omitted by explicit path exclusions. Exclusions do not change index freshness or prove feature absence.")));
    }
    response.traversal = Some(stats);
    finish(&mut response);
    Ok(response)
}
