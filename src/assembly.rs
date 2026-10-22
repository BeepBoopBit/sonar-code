//! Choose supporting evidence across the whole selected set, under one output budget.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::{
    ContextDiagnostic, Evidence, Link, RetrievalHit,
    candidate::Candidate,
    search::{GraphEdge, in_scope, load_parent},
};

pub(crate) struct Query<'a> {
    pub(crate) terms: &'a [String],
    pub(crate) vector: Option<&'a [f32]>,
}

type Assembly = (Vec<Link>, BTreeMap<i64, usize>, Vec<ContextDiagnostic>);

#[allow(clippy::too_many_arguments)]
pub(crate) fn related(
    conn: &Connection,
    selected: &mut Vec<Candidate>,
    ids: &mut BTreeMap<i64, String>,
    budget: usize,
    scope: &str,
    excluded: &BTreeSet<String>,
    semantic: bool,
    query: Option<Query<'_>>,
    extra: &[GraphEdge],
) -> Result<Assembly> {
    let starts: BTreeSet<_> = selected.iter().map(|c| c.id).collect();
    let mut edges = BTreeMap::new();
    for edge in extra {
        if starts.contains(&edge.from) || starts.contains(&edge.to) {
            edges.insert(
                (edge.from, edge.to, edge.kind.clone()),
                GraphEdge {
                    from: edge.from,
                    to: edge.to,
                    kind: edge.kind.clone(),
                    path: edge.path.clone(),
                    line: edge.line,
                },
            );
        }
    }
    let mut locations: BTreeMap<_, _> = selected.iter().map(|c| (c.id, c.clone())).collect();
    for start in selected.iter() {
        if semantic {
            for edge in crate::graph::neighborhood(conn, start.id)? {
                if edge.from != edge.to {
                    edges.insert((edge.from, edge.to, edge.kind.clone()), edge);
                }
            }
        }
        if let Some(parent_id) = start.parent {
            let parent = load_parent(conn, parent_id)?;
            if parent.kind != "file" {
                let edge = GraphEdge {
                    from: parent_id,
                    to: start.id,
                    kind: "contains".into(),
                    path: start.path.clone(),
                    line: start.definition.unwrap_or(start.start),
                };
                edges.insert((edge.from, edge.to, edge.kind.clone()), edge);
                locations.insert(parent_id, parent);
            }
        }
    }
    let mut usage_edges = BTreeSet::new();
    for start in selected.iter() {
        if let Some(u) = &start.usage {
            if u.contribution <= 0.0 {
                continue;
            }
            let edge = GraphEdge {
                from: u.caller_id,
                to: start.id,
                kind: "calls".into(),
                path: u.path.clone(),
                line: u.line,
            };
            let key = (edge.from, edge.to, edge.kind.clone());
            usage_edges.insert(key.clone());
            edges.insert(key, edge);
        }
    }
    // Load each endpoint once, and count independent selected starts supporting it.
    let mut support: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    let mut degree = BTreeMap::new();
    for edge in edges.values() {
        for (id, other) in [(edge.from, edge.to), (edge.to, edge.from)] {
            if let std::collections::btree_map::Entry::Vacant(entry) = locations.entry(id) {
                entry.insert(load_parent(conn, id)?);
            }
            if starts.contains(&other) {
                support.entry(id).or_default().insert(other);
            }
            if semantic && !degree.contains_key(&id) {
                let count: f64 = conn.query_row("SELECT (SELECT count(DISTINCT from_id) FROM semantic_edges WHERE to_id=?1)+(SELECT count(DISTINCT to_id) FROM semantic_edges WHERE from_id=?1)", [id], |r| r.get(0))?;
                degree.insert(id, count);
            }
        }
    }
    let mut pending: Vec<_> = edges
        .into_values()
        .filter(|e| {
            in_scope(&locations[&e.from].path, scope)
                && in_scope(&locations[&e.to].path, scope)
                && !excluded.contains(&locations[&e.from].path)
                && !excluded.contains(&locations[&e.to].path)
                && !excluded.contains(&e.path)
        })
        .collect();
    // Score every eligible endpoint, including neighbors outside the retrieval shortlist.
    // Reuse the query vector and stored vectors; no additional model inference is needed.
    let eligible: BTreeSet<_> = pending.iter().flat_map(|e| [e.from, e.to]).collect();
    let mut diagnostics = if let Some(query) = &query {
        relevance(conn, &locations, &eligible, query)?
    } else {
        Vec::new()
    };
    let relevance: BTreeMap<_, _> = diagnostics
        .iter()
        .map(|d| (d.location_id, d.relevance))
        .collect();
    let hits: BTreeMap<_, _> = diagnostics
        .iter()
        .filter(|d| d.relevance > 0.0)
        .map(|d| (d.location_id, d.winning_hit.clone()))
        .collect();
    let mut covered = BTreeSet::new();
    let mut links = Vec::new();
    let mut focus_sites = BTreeMap::new();
    let mut count = 0;
    while links.len() < 6 {
        let best = pending
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                count
                    + usize::from(!ids.contains_key(&e.from))
                    + usize::from(!ids.contains_key(&e.to))
                    <= budget
            })
            .max_by(|(_, a), (_, b)| {
                let value = |e: &GraphEdge| {
                    let relation = match e.kind.as_str() {
                        "calls" => 1.0,
                        "references" => 0.55,
                        _ => 0.3,
                    };
                    let fresh = [e.from, e.to]
                        .iter()
                        .filter(|id| starts.contains(*id) && !covered.contains(*id))
                        .count() as f64;
                    let independent = [e.from, e.to]
                        .iter()
                        .map(|id| support.get(id).map_or(0, BTreeSet::len))
                        .max()
                        .unwrap_or(0) as f64;
                    let target_degree = degree.get(&e.to).copied().unwrap_or(1.0);
                    // Resolved outward calls usually expose the implementation behind a test or wrapper.
                    let direction = if starts.contains(&e.from) { 1.0 } else { 0.7 };
                    let structural = relation * direction * (1.0 + 0.2 * independent)
                        / (1.0 + 0.1 * target_degree.ln_1p());
                    if query.is_some() {
                        // For a new location, score that endpoint rather than allowing a
                        // highly relevant start to make every unrelated neighbor look useful.
                        let endpoints: Vec<_> = [e.from, e.to]
                            .into_iter()
                            .filter(|id| !starts.contains(id))
                            .collect();
                        let endpoints = if endpoints.is_empty() {
                            vec![e.from, e.to]
                        } else {
                            endpoints
                        };
                        let task = endpoints
                            .iter()
                            .map(|id| relevance.get(id).copied().unwrap_or(0.0))
                            .sum::<f64>()
                            / endpoints.len() as f64;
                        structural * 0.35 + fresh * 0.4 + task * 2.0
                    } else {
                        structural + fresh * 0.8
                    }
                };
                let proof = |e: &GraphEdge| usage_edges.contains(&(e.from, e.to, e.kind.clone()));
                proof(a)
                    .cmp(&proof(b))
                    .then_with(|| value(a).total_cmp(&value(b)))
                    .then_with(|| b.path.cmp(&a.path))
                    .then_with(|| b.line.cmp(&a.line))
                    .then_with(|| b.from.cmp(&a.from))
                    .then_with(|| b.to.cmp(&a.to))
            })
            .map(|(i, _)| i);
        let Some(best) = best else { break };
        let edge = pending.remove(best);
        for id in [edge.from, edge.to] {
            if let std::collections::btree_map::Entry::Vacant(entry) = ids.entry(id) {
                count += 1;
                entry.insert(format!("c{count}"));
                let mut candidate = locations.remove(&id).unwrap();
                if let Some(hit) = hits.get(&id) {
                    candidate.use_hit(hit.clone());
                } else {
                    candidate.hit.channel = "relationship".into();
                }
                selected.push(candidate);
            }
            if starts.contains(&id) {
                covered.insert(id);
            }
        }
        if edge.kind != "contains" && !starts.contains(&edge.from) {
            focus_sites.entry(edge.from).or_insert(edge.line);
        }
        links.push(Link {
            from: ids[&edge.from].clone(),
            to: ids[&edge.to].clone(),
            kind: edge.kind,
            evidence: Evidence {
                path: edge.path,
                line: edge.line,
            },
        });
    }
    for d in &mut diagnostics {
        d.selected = ids.contains_key(&d.location_id);
    }
    Ok((links, focus_sites, diagnostics))
}

pub(crate) fn relevance(
    conn: &Connection,
    locations: &BTreeMap<i64, Candidate>,
    eligible: &BTreeSet<i64>,
    query: &Query<'_>,
) -> Result<Vec<ContextDiagnostic>> {
    if eligible.is_empty() {
        return Ok(Vec::new());
    }
    let ids = serde_json::to_string(eligible)?;
    let mut lexical: BTreeMap<i64, (f64, RetrievalHit)> = BTreeMap::new();
    if !query.terms.is_empty() {
        // FTS statistics stay corpus-wide; endpoint restriction is not a new 512-item pool.
        let mut stmt = conn.prepare("SELECT r.location_id,r.id,r.start_line,r.end_line,r.view,
            -bm25(search,8.0,1.0,3.0,1.0),search.name||' '||search.path||' '||search.docs||' '||search.body
            FROM search JOIN regions r ON r.id=search.rowid
            WHERE search MATCH ?1 AND r.location_id IN (SELECT value FROM json_each(?2)) ORDER BY r.id")?;
        let mut rows = stmt.query(params![crate::text::match_expression(query.terms), ids])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let document: String = row.get(6)?;
            let tokens: BTreeSet<_> = document.split_whitespace().collect();
            let coverage = query
                .terms
                .iter()
                .filter(|t| tokens.contains(t.as_str()))
                .count() as f64
                / query.terms.len() as f64;
            let score = row.get::<_, f64>(5)? * (0.2 + coverage * coverage);
            if lexical.get(&id).is_none_or(|(old, _)| score > *old) {
                lexical.insert(id, (score, hit(row, "lexical")?));
            }
        }
    }
    let mut dense: BTreeMap<i64, (f64, RetrievalHit)> = BTreeMap::new();
    if let Some(vector) = query.vector {
        let mut stmt = conn.prepare(
            "SELECT r.location_id,r.id,r.start_line,r.end_line,r.view,e.vector
            FROM regions r JOIN embeddings e ON e.region_id=r.id
            WHERE r.location_id IN (SELECT value FROM json_each(?1)) ORDER BY r.id",
        )?;
        let mut rows = stmt.query([&ids])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let score = crate::embeddings::dot_bytes(vector, row.get_ref(5)?.as_blob()?)?;
            if dense.get(&id).is_none_or(|(old, _)| score > *old) {
                dense.insert(id, (score, hit(row, "embedding")?));
            }
        }
    }
    let max_lexical = lexical.values().map(|(s, _)| *s).fold(0.0, f64::max);
    let mut dense_range = crate::fusion::Range::default();
    for (score, _) in dense.values() {
        dense_range.add(*score);
    }
    let lexical_weight = match (query.terms.is_empty(), query.vector.is_some()) {
        (true, true) => 0.0,
        (_, false) => 1.0,
        _ => 0.5,
    };
    Ok(eligible
        .iter()
        .map(|id| {
            let l = lexical.get(id);
            let d = dense.get(id);
            let ls = l.map_or(0.0, |(s, _)| s / max_lexical.max(f64::EPSILON)) * lexical_weight;
            let ds = d.map_or(0.0, |(s, _)| dense_range.normalize(*s)) * (1.0 - lexical_weight);
            let winner = if ds > ls { d } else { l }.or(l).or(d);
            ContextDiagnostic {
                location_id: *id,
                path: locations[id].path.clone(),
                symbol: locations[id].symbol.clone(),
                lexical_score: l.map(|(s, _)| *s),
                semantic_similarity: d.map(|(s, _)| *s),
                relevance: ls + ds,
                winning_hit: winner.map_or_else(|| locations[id].hit.clone(), |(_, h)| h.clone()),
                selected: false,
            }
        })
        .collect())
}

fn hit(row: &rusqlite::Row<'_>, channel: &str) -> rusqlite::Result<RetrievalHit> {
    Ok(RetrievalHit {
        region_id: row.get(1)?,
        start: row.get::<_, u32>(2)? as usize,
        end: row.get::<_, u32>(3)? as usize,
        view: row.get(4)?,
        channel: channel.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_context_finds_semantic_neighbor_without_lexical_overlap_and_obeys_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(
            root.join("lib.rs"),
            "fn entry() { generic(); animal(); }\nfn generic() {}\n/// cat\nfn animal() {}\n",
        )
        .unwrap();
        let db = tmp.path().join("index.sqlite");
        crate::index(&root, &db).unwrap();
        let conn = crate::store::open(&db, true).unwrap();
        let lookup = |s: &str| {
            conn.query_row("SELECT id FROM locations WHERE symbol=?1", [s], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        let (entry, generic, animal) = (lookup("entry"), lookup("generic"), lookup("animal"));
        for (to, start) in [(generic, 13), (animal, 24)] {
            conn.execute(
                "INSERT INTO semantic_edges VALUES (?1,?2,'calls','lib.rs',1,?3,?4)",
                params![entry, to, start, start + 7],
            )
            .unwrap();
        }
        // Stored two-dimensional fixture vectors deliberately map cat to the query for
        // feline. All lexical matches are absent; assembly must still find the neighbor.
        for (id, vector) in [
            (entry, [0.6_f32, 0.8]),
            (generic, [0.0, 1.0]),
            (animal, [1.0, 0.0]),
        ] {
            let bytes: Vec<_> = vector.into_iter().flat_map(f32::to_le_bytes).collect();
            conn.execute("INSERT INTO embeddings SELECT id,?2 FROM regions WHERE location_id=?1 AND view='identity'", params![id,bytes]).unwrap();
        }
        let terms = crate::text::query_terms("feline");
        for query_aware in [false, true] {
            let mut selected = vec![load_parent(&conn, entry).unwrap()];
            let mut ids = BTreeMap::from([(entry, "s1".into())]);
            let (links, _, diagnostics) = related(
                &conn,
                &mut selected,
                &mut ids,
                1,
                "",
                &BTreeSet::new(),
                true,
                query_aware.then_some(Query {
                    terms: &terms,
                    vector: Some(&[1.0, 0.0]),
                }),
                &[],
            )
            .unwrap();
            assert_eq!(links.len(), 1);
            assert_eq!(selected.len(), 2);
            assert_eq!(selected[1].id, if query_aware { animal } else { generic });
            if query_aware {
                assert!(diagnostics.iter().all(|d| d.lexical_score.is_none()));
                assert!(
                    diagnostics
                        .iter()
                        .any(|d| d.location_id == animal && d.selected && d.relevance > 0.0)
                );
            }
        }
        for (budget, scope) in [(0, ""), (1, "other.rs")] {
            let mut selected = vec![load_parent(&conn, entry).unwrap()];
            let mut ids = BTreeMap::from([(entry, "s1".into())]);
            let (links, _, _) = related(
                &conn,
                &mut selected,
                &mut ids,
                budget,
                scope,
                &BTreeSet::new(),
                true,
                Some(Query {
                    terms: &terms,
                    vector: Some(&[1.0, 0.0]),
                }),
                &[],
            )
            .unwrap();
            assert!(links.is_empty());
            assert_eq!(selected.len(), 1);
        }
        // Lexical-only context uses the same bounded endpoint pool without embeddings.
        let mut selected = vec![load_parent(&conn, entry).unwrap()];
        let mut ids = BTreeMap::from([(entry, "s1".into())]);
        related(
            &conn,
            &mut selected,
            &mut ids,
            1,
            "",
            &BTreeSet::new(),
            true,
            Some(Query {
                terms: &crate::text::query_terms("cat"),
                vector: None,
            }),
            &[],
        )
        .unwrap();
        assert_eq!(selected[1].id, animal);
    }
}
