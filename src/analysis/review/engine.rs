//! Deterministic graph selection shared by explicit context and repository partitioning.
//! Scores are prioritization heuristics, not confidence or defect probabilities.
use super::super::{DependencyKind, DependencyRelationship};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

pub(super) struct Selection {
    pub seed: String,
    /// File, then (earlier parent file, original graph relationship index).
    pub order: Vec<(String, Option<(String, usize)>)>,
}

pub(super) struct Engine<'a> {
    pub relationships: &'a [DependencyRelationship],
    adjacent: BTreeMap<&'a str, Vec<(usize, &'a str)>>,
    pub shared: BTreeSet<String>,
    pub shared_threshold: usize,
}

pub(super) fn kind_weight(kind: DependencyKind) -> u64 {
    match kind {
        DependencyKind::Calls => 10,
        DependencyKind::Imports | DependencyKind::ReExports | DependencyKind::Implements => 8,
        DependencyKind::References => 4,
        DependencyKind::DeclaresModule => 2,
    }
}

impl<'a> Engine<'a> {
    pub fn new(relationships: &'a [DependencyRelationship]) -> Self {
        let mut adjacent: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        let mut consumers: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut dependencies: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (i, r) in relationships.iter().enumerate() {
            adjacent
                .entry(&r.from)
                .or_default()
                .push((i, r.to.as_str()));
            adjacent
                .entry(&r.to)
                .or_default()
                .push((i, r.from.as_str()));
            if r.kinds
                .iter()
                .any(|k| k.kind != DependencyKind::DeclaresModule)
            {
                consumers.entry(&r.to).or_default().insert(&r.from);
                dependencies.entry(&r.from).or_default().insert(&r.to);
            }
        }
        let root = adjacent.len().isqrt();
        let shared_threshold = (root + usize::from(root * root < adjacent.len())).max(8);
        let shared = consumers
            .into_iter()
            .filter(|(path, users)| {
                users.len() >= shared_threshold
                    && users.len() >= 2 * dependencies.get(path).map_or(1, |d| d.len().max(1))
                    && users
                        .iter()
                        .map(|p| Path::new(p).parent())
                        .collect::<BTreeSet<_>>()
                        .len()
                        >= 3
            })
            .map(|(path, _)| path.to_owned())
            .collect();
        Self {
            relationships,
            adjacent,
            shared,
            shared_threshold,
        }
    }

    fn strength(&self, index: usize, a: &str, b: &str) -> u64 {
        let relationship = &self.relationships[index];
        let weight = relationship
            .kinds
            .iter()
            .map(|k| kind_weight(k.kind) * k.occurrences.min(4) as u64)
            .max()
            .unwrap_or(0);
        let locality = Path::new(a)
            .parent()
            .into_iter()
            .flat_map(Path::components)
            .zip(Path::new(b).parent().into_iter().flat_map(Path::components))
            .take_while(|(a, b)| a == b)
            .count()
            .min(8) as u64;
        weight * (2 + locality)
    }

    pub fn seed(&self, allowed: &BTreeSet<String>) -> Option<String> {
        let mut best: Option<(&String, usize)> = None;
        for file in allowed {
            let degree = self
                .adjacent
                .get(file.as_str())
                .into_iter()
                .flatten()
                .filter(|(_, other)| allowed.contains(*other))
                .map(|(_, other)| *other)
                .collect::<BTreeSet<_>>()
                .len();
            if best.is_none_or(|(_, score)| degree > score) {
                best = Some((file, degree));
            }
        }
        best.map(|(file, _)| file.clone())
    }

    pub fn select(&self, seed: &str, allowed: &BTreeSet<String>, max_files: usize) -> Selection {
        self.select_prioritized(seed, allowed, max_files, &BTreeSet::new())
    }

    pub fn select_prioritized(
        &self,
        seed: &str,
        allowed: &BTreeSet<String>,
        max_files: usize,
        preferred: &BTreeSet<String>,
    ) -> Selection {
        let mut selected = BTreeSet::from([seed.to_owned()]);
        let mut order = vec![(seed.to_owned(), None)];
        while order.len() < max_files {
            // Aggregate support across selected files. Tie breaks follow path order.
            let mut candidates: BTreeMap<&str, (u64, &str, usize, u64)> = BTreeMap::new();
            for parent in &selected {
                for &(edge, other) in self.adjacent.get(parent.as_str()).into_iter().flatten() {
                    if selected.contains(other)
                        || !allowed.contains(other)
                        || (self.shared.contains(other) && !preferred.contains(other))
                    {
                        continue;
                    }
                    let strength = self.strength(edge, parent, other);
                    let candidate = candidates.entry(other).or_insert((0, parent, edge, 0));
                    candidate.0 += strength;
                    if strength > candidate.3 {
                        candidate.1 = parent;
                        candidate.2 = edge;
                        candidate.3 = strength;
                    }
                }
            }
            let mut best = None;
            for (path, candidate) in candidates {
                if best
                    .as_ref()
                    .is_none_or(|(old_path, old): &(&str, (u64, &str, usize, u64))| {
                        (preferred.contains(path), candidate.0)
                            > (preferred.contains(*old_path), old.0)
                    })
                {
                    best = Some((path, candidate));
                }
            }
            let Some((path, (_, parent, edge, _))) = best else {
                break;
            };
            let path = path.to_owned();
            let parent = parent.to_owned();
            selected.insert(path.clone());
            order.push((path, Some((parent, edge))));
        }
        Selection {
            seed: seed.to_owned(),
            order,
        }
    }

    pub fn partition(&self, eligible: &BTreeSet<String>, max_files: usize) -> Vec<Selection> {
        let mut remaining: BTreeSet<_> = eligible.difference(&self.shared).cloned().collect();
        let mut selections = Vec::new();
        while let Some(seed) = self.seed(&remaining) {
            let selection = self.select(&seed, &remaining, max_files);
            for (file, _) in &selection.order {
                remaining.remove(file);
            }
            selections.push(selection);
        }
        selections
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{DependencyEvidence, DependencyKindSummary};
    fn edge(a: &str, b: &str, kind: DependencyKind) -> DependencyRelationship {
        DependencyRelationship {
            from: a.into(),
            to: b.into(),
            kinds: vec![DependencyKindSummary {
                kind,
                occurrences: 1,
                evidence: vec![DependencyEvidence {
                    path: a.into(),
                    line: 1,
                    bytes: [0, 1],
                    provider: "fixture".into(),
                }],
                evidence_truncated: false,
            }],
        }
    }
    #[test]
    fn budget_paths_weights_and_disjoint_partition_are_deterministic() {
        let relationships = vec![
            edge("src/a", "src/b", DependencyKind::Calls),
            edge("src/a", "src/c", DependencyKind::References),
            edge("src/b", "src/d", DependencyKind::Calls),
            edge("elsewhere/e", "elsewhere/f", DependencyKind::Calls),
        ];
        let eligible: BTreeSet<_> = [
            "src/a",
            "src/b",
            "src/c",
            "src/d",
            "elsewhere/e",
            "elsewhere/f",
            "isolated",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let engine = Engine::new(&relationships);
        let selected = engine.select("src/a", &eligible, 3);
        assert_eq!(
            selected
                .order
                .iter()
                .map(|(f, _)| f.as_str())
                .collect::<Vec<_>>(),
            vec!["src/a", "src/b", "src/d"]
        );
        assert_eq!(engine.select("src/a", &eligible, 1).order.len(), 1);
        let mut assigned = BTreeSet::new();
        for selection in engine.partition(&eligible, 2) {
            assert!(selection.order.len() <= 2);
            let mut seen = BTreeSet::new();
            for (file, parent) in selection.order {
                if let Some((parent, index)) = parent {
                    assert!(seen.contains(&parent));
                    let r = &relationships[index];
                    assert!(
                        (r.from == parent && r.to == file) || (r.to == parent && r.from == file)
                    );
                }
                assert!(assigned.insert(file.clone()));
                seen.insert(file);
            }
        }
        assert_eq!(assigned, eligible);
        let reversed: Vec<_> = relationships.into_iter().rev().collect();
        assert_eq!(
            Engine::new(&reversed)
                .select("src/a", &eligible, 3)
                .order
                .iter()
                .map(|(f, _)| f)
                .collect::<Vec<_>>(),
            selected.order.iter().map(|(f, _)| f).collect::<Vec<_>>()
        );
    }
    #[test]
    fn shared_hubs_do_not_merge_areas_and_explicit_hub_seeds_work() {
        let mut relationships = Vec::new();
        let mut eligible = BTreeSet::from(["shared/ports".into()]);
        for i in 0..9 {
            let path = format!("subsystem{}/consumer{i}", i % 3);
            eligible.insert(path.clone());
            relationships.push(edge(&path, "shared/ports", DependencyKind::References));
        }
        let engine = Engine::new(&relationships);
        assert!(engine.shared.contains("shared/ports"));
        assert_eq!(engine.partition(&eligible, 12).len(), 9);
        assert_eq!(
            engine
                .select("subsystem0/consumer0", &eligible, 12)
                .order
                .len(),
            1
        );
        let explicit = engine.select("shared/ports", &eligible, 3);
        assert_eq!(explicit.order.len(), 3);
        assert_eq!(explicit.order[0].0, "shared/ports");
    }
    #[test]
    fn widely_used_implementations_with_outgoing_work_remain_primary() {
        let mut relationships = Vec::new();
        for i in 0..12 {
            relationships.push(edge(
                &format!("area{}/caller{i}", i % 3),
                "implementation",
                DependencyKind::Calls,
            ));
        }
        for i in 0..7 {
            relationships.push(edge(
                "implementation",
                &format!("dependency{i}"),
                DependencyKind::Calls,
            ));
        }
        let engine = Engine::new(&relationships);
        assert!(!engine.shared.contains("implementation"));
    }
    #[test]
    fn explicit_consumer_priority_can_include_shared_files_without_changing_normal_selection() {
        let mut relationships = Vec::new();
        let mut eligible = BTreeSet::from(["ports".into()]);
        for i in 0..9 {
            let path = format!("area{}/consumer{i}", i % 3);
            eligible.insert(path.clone());
            relationships.push(edge(&path, "ports", DependencyKind::References));
        }
        relationships.push(edge("ports", "area0/consumer0", DependencyKind::Calls));
        let engine = Engine::new(&relationships);
        assert!(engine.shared.contains("ports"));
        assert_eq!(
            engine.select("area0/consumer0", &eligible, 2).order.len(),
            1
        );
        let selected = engine.select_prioritized(
            "area0/consumer0",
            &eligible,
            2,
            &BTreeSet::from(["ports".into()]),
        );
        assert_eq!(selected.order.len(), 2);
        assert_eq!(selected.order[1].0, "ports");
    }
}
