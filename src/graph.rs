//! Query-seeded, bounded graph ranking. Edges remain compiler evidence, never inferred calls.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use rusqlite::Connection;

use crate::{
    GraphMethod,
    candidate::{Candidate, sort},
    search::{GraphEdge, in_scope, load_parent},
};

const SEEDS: usize = 32;
const NODES: usize = 384;

pub(crate) fn neighborhood(conn: &Connection, id: i64) -> Result<Vec<GraphEdge>> {
    let mut result = Vec::new();
    // Separate directions prevent many incoming callers from hiding the outgoing implementation.
    for condition in ["from_id", "to_id"] {
        let mut stmt = conn.prepare(&format!("SELECT from_id,to_id,kind,path,min(line) FROM semantic_edges WHERE {condition}=?1 GROUP BY from_id,to_id,kind,path ORDER BY CASE kind WHEN 'calls' THEN 0 ELSE 1 END,path,min(line),from_id,to_id LIMIT 32"))?;
        result.extend(
            stmt.query_map([id], |r| {
                Ok(GraphEdge {
                    from: r.get(0)?,
                    to: r.get(1)?,
                    kind: r.get(2)?,
                    path: r.get(3)?,
                    line: r.get::<_, u32>(4)? as usize,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?,
        );
    }
    result.sort_by_key(|e| (e.from, e.to, e.kind.clone(), e.path.clone(), e.line));
    result
        .dedup_by(|a, b| a.from == b.from && a.to == b.to && a.kind == b.kind && a.path == b.path);
    Ok(result)
}

pub(crate) fn expand(
    conn: &Connection,
    candidates: &mut Vec<Candidate>,
    scope: &str,
    excluded: &BTreeSet<String>,
    method: GraphMethod,
) -> Result<()> {
    let mut nodes: BTreeMap<_, _> = candidates
        .iter()
        .take(SEEDS)
        .map(|c| (c.id, c.clone()))
        .collect();
    let seed_ids: Vec<_> = candidates.iter().take(SEEDS).map(|c| c.id).collect();
    let mut frontier = seed_ids.clone();
    let mut edges = BTreeMap::new();
    let mut expanded = BTreeSet::new();
    let mut degree: BTreeMap<i64, f64> = BTreeMap::new();
    for _ in 0..2 {
        let mut next = BTreeSet::new();
        for id in frontier {
            if !expanded.insert(id) {
                continue;
            }
            for edge in neighborhood(conn, id)? {
                let other = if edge.from == id { edge.to } else { edge.from };
                if other == id || excluded.contains(&edge.path) {
                    continue;
                }
                if !nodes.contains_key(&other) && nodes.len() >= NODES {
                    continue;
                }
                if let std::collections::btree_map::Entry::Vacant(entry) = nodes.entry(other) {
                    let mut candidate = load_parent(conn, other)?;
                    if !in_scope(&candidate.path, scope) || excluded.contains(&candidate.path) {
                        continue;
                    }
                    candidate.hit.channel = "graph".into();
                    candidate.hits = vec![candidate.hit.clone()];
                    entry.insert(candidate);
                }
                next.insert(other);
                edges.insert((edge.from, edge.to, edge.kind.clone()), edge);
            }
        }
        frontier = next
            .into_iter()
            .filter(|id| !expanded.contains(id))
            .collect();
    }
    for id in nodes.keys() {
        degree.insert(*id, conn.query_row("SELECT (SELECT count(DISTINCT from_id) FROM semantic_edges WHERE to_id=?1)+(SELECT count(DISTINCT to_id) FROM semantic_edges WHERE from_id=?1)", [id], |r| r.get::<_,f64>(0))?.max(1.0));
    }
    let mut adjacency: BTreeMap<i64, BTreeMap<i64, f64>> = BTreeMap::new();
    for edge in edges.values() {
        for (from, to, outward) in [(edge.from, edge.to, true), (edge.to, edge.from, false)] {
            let weight = match (edge.kind.as_str(), outward) {
                ("calls", true) => 1.0,
                ("calls", false) => 0.4,
                (_, true) => 0.25,
                (_, false) => 0.1,
            } / degree[&to].sqrt();
            adjacency
                .entry(from)
                .or_default()
                .entry(to)
                .and_modify(|w| *w = w.max(weight))
                .or_insert(weight);
        }
    }
    let mut teleport: BTreeMap<_, _> = seed_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, 1.0 / (60.0 + (i + 1) as f64)))
        .collect();
    let sum = teleport.values().sum::<f64>();
    for value in teleport.values_mut() {
        *value /= sum;
    }
    let mut mass = teleport.clone();
    let iterations = if method == GraphMethod::PageRank {
        24
    } else {
        2
    };
    for _ in 0..iterations {
        let mut next: BTreeMap<_, _> = teleport.iter().map(|(id, p)| (*id, 0.35 * p)).collect();
        let mut dangling = 0.0;
        for (id, p) in &mass {
            if let Some(out) = adjacency.get(id).filter(|out| !out.is_empty()) {
                let total: f64 = out.values().sum();
                for (to, w) in out {
                    *next.entry(*to).or_default() += 0.65 * p * w / total;
                }
            } else {
                dangling += 0.65 * p;
            }
        }
        for (id, p) in &teleport {
            *next.entry(*id).or_default() += dangling * p;
        }
        let delta: f64 = nodes
            .keys()
            .map(|id| (next.get(id).unwrap_or(&0.0) - mass.get(id).unwrap_or(&0.0)).abs())
            .sum();
        mass = next;
        if method == GraphMethod::PageRank && delta < 1e-8 {
            break;
        }
    }
    let largest = mass.values().copied().fold(0.0, f64::max).max(f64::EPSILON);
    let mut merged: BTreeMap<_, _> = candidates
        .drain(..)
        .enumerate()
        .map(|(i, mut c)| {
            c.score = 1.0 / (60.0 + (i + 1) as f64);
            (c.id, c)
        })
        .collect();
    for (id, p) in mass {
        let c = merged.entry(id).or_insert_with(|| {
            let mut c = nodes.remove(&id).unwrap();
            c.score = 0.0;
            c
        });
        c.graph_score = p;
        c.graph_seed_count = seed_ids
            .iter()
            .filter(|seed| adjacency.get(seed).is_some_and(|out| out.contains_key(&id)))
            .count();
        c.score += 0.8 / 61.0 * p / largest;
    }
    *candidates = merged.into_values().collect();
    sort(candidates);
    Ok(())
}
