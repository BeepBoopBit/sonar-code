//! Let a retrieved declaration support its already-retrieved, compiler-recorded users.
use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::{candidate::Candidate, navigation::*};

const SEEDS: usize = 8;
const CANDIDATES: usize = 64;
const USERS: usize = 64;

pub(crate) fn apply(
    conn: &Connection,
    candidates: &mut [Candidate],
    available: bool,
) -> Result<NavigationDiagnostic> {
    let mut d = NavigationDiagnostic {
        policy: NavigationPolicy::Uses,
        status: "unavailable".into(),
        seeds: Vec::new(),
        truncated: false,
        channel: Vec::new(),
        role_corrections: Vec::new(),
        edges: Vec::new(),
    };
    if !available {
        return Ok(d);
    }
    if candidates.iter().any(Candidate::exact) {
        d.status = "exact_bypass".into();
        return Ok(d);
    }
    d.status = "inactive".into();
    let eligible: BTreeMap<_, _> = candidates
        .iter()
        .take(CANDIDATES)
        .enumerate()
        .filter(|(_, c)| c.kind == "function")
        .map(|(i, c)| (c.id, i))
        .collect();
    let seeds: Vec<_> = candidates
        .iter()
        .take(SEEDS)
        .filter(|c| matches!(c.kind.as_str(), "field" | "variant"))
        .map(|c| (c.id, c.score))
        .collect();
    // One contribution per endpoint prevents several fields from the same concept
    // from voting repeatedly. Degree uses all recorded function users, even outside
    // the retrieval pool/scope, so filtering cannot inflate a generic declaration.
    let mut support: BTreeMap<i64, (f64, i64, usize, NavigationEdge)> = BTreeMap::new();
    let mut users = conn.prepare(
        "SELECT DISTINCT e.from_id FROM semantic_edges e JOIN locations l ON l.id=e.from_id
         WHERE e.to_id=?1 AND e.kind='references' AND l.kind='function'
         ORDER BY e.from_id LIMIT ?2",
    )?;
    let mut evidence = conn.prepare(
        "SELECT path,line,start_byte,end_byte FROM semantic_edges
         WHERE from_id=?1 AND to_id=?2 AND kind='references'
         ORDER BY path,start_byte,end_byte LIMIT 1",
    )?;
    for (seed, score) in seeds {
        d.seeds.push(seed);
        let ids = users
            .query_map(params![seed, (USERS + 1) as i64], |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > USERS {
            d.truncated = true;
            continue;
        }
        if ids.is_empty() {
            continue;
        }
        let addition = 0.5 * score / (ids.len() as f64).sqrt();
        for id in &ids {
            if !eligible.contains_key(id)
                || support.get(id).is_some_and(|(old, ..)| *old >= addition)
            {
                continue;
            }
            let edge = evidence.query_row(params![id, seed], |r| {
                Ok(NavigationEdge {
                    from: *id,
                    to: seed,
                    kind: "references".into(),
                    path: r.get(0)?,
                    line: r.get::<_, u32>(1)? as usize,
                    bytes: [r.get::<_, i64>(2)? as usize, r.get::<_, i64>(3)? as usize],
                })
            })?;
            support.insert(*id, (addition, seed, ids.len(), edge));
        }
    }
    for (id, (addition, seed, users, edge)) in support {
        let index = eligible[&id];
        let c = &mut candidates[index];
        d.channel.push(NavigationContribution {
            location_id: id,
            rank: index + 1,
            relevance: 0.5 / (users as f64).sqrt(),
            base_score: c.score,
            addition,
            admitted: false,
            reference_support: Some(ReferenceSupport { seed, users }),
        });
        d.edges.push(edge);
        c.score += addition;
    }
    if !d.channel.is_empty() {
        d.status = "active".into();
        crate::candidate::sort(candidates);
    }
    Ok(d)
}

#[cfg(test)]
mod tests;
