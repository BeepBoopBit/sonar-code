//! One graph-loading path for dependency navigation and review clustering.
use super::{
    DependencyCoverage, DependencyEvidence, DependencyFact, DependencyKind, DependencyKindSummary,
    DependencyRelationship, UnresolvedDependency,
};
use crate::{Notice, explore};
use anyhow::Result;
use std::collections::BTreeMap;

pub(super) struct GraphData {
    pub relationships: Vec<DependencyRelationship>,
    pub unresolved: Vec<UnresolvedDependency>,
    pub coverage: Vec<DependencyCoverage>,
    pub notices: Vec<Notice>,
}

pub(super) fn load(snapshot: &explore::Snapshot<'_>) -> Result<GraphData> {
    let batches = super::store::load(snapshot.conn, &snapshot.index.snapshot)?;
    let mut facts = Vec::new();
    let mut unresolved = Vec::new();
    let mut coverage = Vec::new();
    for batch in batches {
        facts.extend(batch.relationships);
        unresolved.extend(batch.unresolved);
        let skipped_files = batch.coverage.skipped_files;
        let skipped_total = skipped_files.len();
        coverage.push(DependencyCoverage {
            language: batch.coverage.language,
            provider: batch.coverage.provider,
            analyzed_files: batch.coverage.analyzed_files.len(),
            skipped_files,
            skipped_total,
            limitations: batch.coverage.limitations,
        });
    }
    let mut notices = vec![Notice::new(
        "static_dependencies",
        "Connections are inspection targets, not execution flow, test coverage, or confirmed regressions. Unresolved incoming dependencies cannot be attributed to their targets.",
    )];
    let fresh: BTreeMap<_, _> = snapshot
        .index
        .languages
        .iter()
        .filter(|l| l.semantic_freshness == "current")
        .map(|l| {
            (
                l.language.as_str(),
                l.semantic_profile.as_deref().unwrap_or("compiler"),
            )
        })
        .collect();
    let mut stmt = snapshot.conn.prepare(
        "SELECT a.path,b.path,e.kind,e.path,e.line,e.start_byte,e.end_byte
        FROM semantic_edges e JOIN locations x ON x.id=e.from_id JOIN files a ON a.id=x.file_id
        JOIN locations y ON y.id=e.to_id JOIN files b ON b.id=y.file_id
        ORDER BY a.path,b.path,e.kind,e.path,e.start_byte,e.end_byte",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, u32>(4)? as usize,
            r.get::<_, u32>(5)? as usize,
            r.get::<_, u32>(6)? as usize,
        ))
    })?;
    let mut binding_sites: BTreeMap<&str, std::collections::BTreeSet<[usize; 2]>> = BTreeMap::new();
    for dependency in &unresolved {
        binding_sites
            .entry(&dependency.evidence.path)
            .or_default()
            .insert(dependency.evidence.bytes);
    }
    let mut bindings: BTreeMap<(String, [usize; 2]), Vec<DependencyFact>> = BTreeMap::new();
    for row in rows {
        let (from, to, kind, evidence_path, line, start, end) = row?;
        let Some(language) = crate::source_language(&from) else {
            continue;
        };
        let Some(profile) = fresh.get(language) else {
            continue;
        };
        let kind = match kind.as_str() {
            "calls" => DependencyKind::Calls,
            "references" => DependencyKind::References,
            "implements" => DependencyKind::Implements,
            _ => continue,
        };
        let fact = DependencyFact {
            from,
            to,
            kind,
            evidence: DependencyEvidence {
                path: evidence_path,
                line,
                bytes: [start, end],
                provider: format!("{language}:{profile}"),
            },
        };
        if binding_sites
            .get(fact.evidence.path.as_str())
            .is_some_and(|sites| sites.contains(&fact.evidence.bytes))
        {
            bindings
                .entry((fact.evidence.path.clone(), fact.evidence.bytes))
                .or_default()
                .push(fact.clone());
        }
        if fact.from != fact.to {
            facts.push(fact);
        }
    }
    let mut remaining = Vec::new();
    for dependency in unresolved {
        let provider =
            crate::languages::for_path(&dependency.from).and_then(|a| a.dependency_provider());
        let exact = bindings
            .get(&(dependency.evidence.path.clone(), dependency.evidence.bytes))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match provider.and_then(|p| p.resolve(&dependency, exact)) {
            Some(fact) => {
                if fact.from != fact.to {
                    facts.push(fact);
                }
            }
            None => remaining.push(dependency),
        }
    }
    let unresolved = remaining;
    for language in &snapshot.index.languages {
        if language.semantic_freshness != "current" {
            notices.push(Notice::new(
                "semantic_dependencies_unavailable",
                format!(
                    "{} compiler relationships are {}; file dependencies may be incomplete.",
                    language.language, language.semantic_freshness
                ),
            ));
        }
        if !coverage.iter().any(|c| c.language == language.language) {
            notices.push(Notice::new(
                "syntax_dependencies_unavailable",
                format!(
                    "{} has no syntax dependency provider yet.",
                    language.language
                ),
            ));
        }
    }
    Ok(GraphData {
        relationships: group(facts),
        unresolved,
        coverage,
        notices,
    })
}

const EVIDENCE_PER_KIND: usize = 3;

pub(super) fn group(mut facts: Vec<DependencyFact>) -> Vec<DependencyRelationship> {
    facts.sort();
    facts.dedup();
    let mut groups: BTreeMap<(String, String), BTreeMap<DependencyKind, DependencyKindSummary>> =
        BTreeMap::new();
    for fact in facts {
        let summary = groups
            .entry((fact.from, fact.to))
            .or_default()
            .entry(fact.kind)
            .or_insert_with(|| DependencyKindSummary {
                kind: fact.kind,
                occurrences: 0,
                evidence: Vec::new(),
                evidence_truncated: false,
            });
        summary.occurrences += 1;
        if summary.evidence.len() < EVIDENCE_PER_KIND {
            summary.evidence.push(fact.evidence);
        } else {
            summary.evidence_truncated = true;
        }
    }
    groups
        .into_iter()
        .map(|((from, to), kinds)| DependencyRelationship {
            from,
            to,
            kinds: kinds.into_values().collect(),
        })
        .collect()
}
