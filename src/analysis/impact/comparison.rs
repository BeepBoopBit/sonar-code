use super::super::{
    DependencyKind, ReviewCluster, ReviewContextOptions, ReviewSeed, graph::GraphData, review,
};
use super::{
    BaselineConsumer, DeclarationChange, ImpactOptions, RelationshipChange, VersionedDeclaration,
    baseline::Catalog,
    consumers, git,
    matching::{self, Pair},
};
use crate::{explore, search};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

pub(super) struct Comparison<'a, 'b> {
    pub old: &'a explore::Snapshot<'b>,
    pub old_graph: &'a GraphData,
    pub before: Catalog,
    pub after: Catalog,
    pub pairs: Vec<Pair>,
    pub commit: &'a str,
}

pub(super) struct Compared {
    pub before_ids: BTreeSet<i64>,
    pub after_ids: BTreeSet<i64>,
    pub changes: Vec<DeclarationChange>,
    pub consumers: Vec<BaselineConsumer>,
    pub preferred: BTreeSet<String>,
    pub cluster: Option<ReviewCluster>,
    pub relationships: Vec<RelationshipChange>,
    pub gaps: Vec<String>,
}

impl<'a, 'b> Comparison<'a, 'b> {
    pub fn new(
        old: &'a explore::Snapshot<'b>,
        old_graph: &'a GraphData,
        current: &explore::Snapshot<'_>,
        commit: &'a str,
    ) -> Result<Self> {
        let before = Catalog::load(old, "baseline", Some(commit))?;
        let after = Catalog::load(current, "current", None)?;
        let mut pairs = matching::pair(&before, &after);
        pairs.sort_by_key(|p| {
            let d = p
                .before
                .map(|id| &before.declarations[&id])
                .or_else(|| p.after.map(|id| &after.declarations[&id]))
                .unwrap();
            (
                d.location.path.clone(),
                d.location.range,
                d.location.kind.clone(),
                d.location.id.clone(),
            )
        });
        Ok(Self {
            old,
            old_graph,
            before,
            after,
            pairs,
            commit,
        })
    }
    pub fn compare(
        &self,
        current: &explore::Snapshot<'_>,
        current_graph: &GraphData,
        change: &git::Change,
        options: &ImpactOptions,
    ) -> Result<Compared> {
        let mut before_ids = self.before.overlap(
            &change.path,
            change.hunks.iter().map(|h| (h.old_start, h.old_lines)),
        );
        let mut after_ids = self.after.overlap(
            &change.path,
            change.hunks.iter().map(|h| (h.new_start, h.new_lines)),
        );
        let affected: Vec<_> = self
            .pairs
            .iter()
            .filter(|p| {
                p.before.is_some_and(|id| before_ids.contains(&id))
                    || p.after.is_some_and(|id| after_ids.contains(&id))
            })
            .collect();
        let changes = affected
            .iter()
            .map(|p| p.report(&self.before, &self.after))
            .collect::<Vec<_>>();
        for p in affected {
            if let Some(id) = p.before {
                before_ids.insert(id);
            }
            if let Some(id) = p.after
                && self.after.declarations[&id].location.path == change.path
            {
                after_ids.insert(id);
            }
        }
        let mut gaps = Vec::new();
        if changes.iter().any(|c| c.uncertain) {
            gaps.push("uncertain_declaration_identity".into());
        }
        if self.before.invalid_files.contains(&change.path) {
            gaps.push("baseline_parse_errors".into());
        }
        if !before_ids.is_empty()
            && before_ids.iter().any(|id| {
                let language = &self.before.declarations[id].location.language;
                !self
                    .old
                    .index
                    .languages
                    .iter()
                    .any(|l| &l.language == language && l.semantic_freshness == "current")
            })
        {
            gaps.push("baseline_compiler_consumers_unavailable".into());
        }
        let incoming = consumers(self.old, &before_ids.iter().copied().collect::<Vec<_>>())?;
        let preferred = incoming.iter().map(|c| c.from_path.clone()).collect();
        let mut old_consumers = Vec::new();
        for consumer in incoming {
            let location = self.location(consumer.from)?;
            let target = self.location(consumer.to)?.id;
            let mut evidence = consumer.evidence;
            evidence.provider = format!("baseline:{}", location.language);
            old_consumers.push(BaselineConsumer {
                location,
                target,
                kind: consumer.kind,
                occurrences: consumer.occurrences,
                evidence,
            });
        }
        let scope = search::normalize_scope(options.scope.as_deref())?;
        let old_seed = before_ids
            .iter()
            .map(|id| &self.before.declarations[id].location)
            .find(|l| search::in_scope(&l.path, &scope));
        let cluster = if let Some(seed) = old_seed {
            let (mut cluster, _) = review::context_cluster(
                self.old,
                self.old_graph,
                ReviewSeed {
                    path: seed.path.clone(),
                    symbol: seed.symbol.clone(),
                    range: Some(seed.range),
                },
                &ReviewContextOptions {
                    max_files: options.max_files,
                    scope: options.scope.clone(),
                },
                &preferred,
            )?;
            cluster.id = format!(
                "baseline:{}:{}",
                self.commit,
                explore::request(&cluster.files)?
            );
            for step in &mut cluster.reading_path {
                if let Some(via) = &mut step.via {
                    via.evidence.provider = "baseline".into();
                }
            }
            for test in &mut cluster.related_tests {
                test.connection.evidence.provider = "baseline".into();
            }
            for boundary in &mut cluster.boundary {
                for kind in &mut boundary.relationship.kinds {
                    for evidence in &mut kind.evidence {
                        evidence.provider = "baseline".into();
                    }
                }
            }
            Some(cluster)
        } else {
            None
        };
        let relationships = self.relationships(current, current_graph, &change.path);
        Ok(Compared {
            before_ids,
            after_ids,
            changes,
            consumers: old_consumers,
            preferred,
            cluster,
            relationships,
            gaps,
        })
    }
    fn location(&self, id: i64) -> Result<VersionedDeclaration> {
        if let Some(d) = self.before.declarations.get(&id) {
            return Ok(d.location.clone());
        }
        let l = self.old.location(id)?;
        Ok(VersionedDeclaration {
            id: format!(
                "decl:{}",
                explore::request((self.commit, &l.path, &l.kind, &l.symbol, l.range))?
            ),
            snapshot: "baseline".into(),
            commit: Some(self.commit.into()),
            language: crate::source_language(&l.path).unwrap_or("unknown").into(),
            path: l.path,
            symbol: l.symbol,
            kind: l.kind,
            range: l.range,
            signature: String::new(),
            signature_truncated: false,
        })
    }
    fn relationships(
        &self,
        current: &explore::Snapshot<'_>,
        data: &GraphData,
        path: &str,
    ) -> Vec<RelationshipChange> {
        let mut counts: BTreeMap<(String, String, DependencyKind), (usize, usize)> =
            BTreeMap::new();
        for (old, graph) in [(true, self.old_graph), (false, data)] {
            for r in graph
                .relationships
                .iter()
                .filter(|r| r.from == path || r.to == path)
            {
                if self.before.invalid_files.contains(&r.from)
                    || self.after.invalid_files.contains(&r.from)
                {
                    continue;
                }
                for kind in &r.kinds {
                    if kind.kind != DependencyKind::DeclaresModule {
                        let Some(language) = crate::source_language(&r.from) else {
                            continue;
                        };
                        if ![self.old, current].iter().all(|s| {
                            s.index.languages.iter().any(|l| {
                                l.language == language && l.semantic_freshness == "current"
                            })
                        }) {
                            continue;
                        }
                    }
                    let count = counts
                        .entry((r.from.clone(), r.to.clone(), kind.kind))
                        .or_default();
                    if old {
                        count.0 = kind.occurrences;
                    } else {
                        count.1 = kind.occurrences;
                    }
                }
            }
        }
        counts
            .into_iter()
            .filter(|(_, counts)| counts.0 != counts.1)
            .map(|((from, to, kind), (before, after))| RelationshipChange {
                from,
                to,
                kind,
                before_occurrences: before,
                after_occurrences: after,
                status: if before == 0 {
                    "observed_added"
                } else if after == 0 {
                    "observed_removed"
                } else {
                    "count_changed"
                }
                .into(),
            })
            .collect()
    }
    pub fn graph_fingerprint(&self) -> Result<String> {
        // Exclude temporary roots, SQLite IDs and provider fingerprints from continuation identity.
        let mut stmt=self.old.conn.prepare("SELECT a.path,x.kind,x.symbol,x.start_byte,x.end_byte,b.path,y.kind,y.symbol,y.start_byte,y.end_byte,e.kind,e.path,e.line,e.start_byte,e.end_byte
            FROM semantic_edges e JOIN locations x ON x.id=e.from_id JOIN files a ON a.id=x.file_id
            JOIN locations y ON y.id=e.to_id JOIN files b ON b.id=y.file_id
            ORDER BY a.path,x.start_byte,x.end_byte,b.path,y.start_byte,y.end_byte,e.kind,e.path,e.start_byte,e.end_byte")?;
        let mut hash = blake3::Hasher::new();
        for row in stmt.query_map([], |r| {
            let mut values = Vec::new();
            for i in 0..15 {
                values.push(match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Text(bytes) => {
                        serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())
                    }
                    rusqlite::types::ValueRef::Integer(n) => serde_json::Value::from(n),
                    _ => serde_json::Value::Null,
                });
            }
            Ok(values)
        })? {
            hash.update(&serde_json::to_vec(&row?)?);
        }
        Ok(hash.finalize().to_hex().to_string())
    }
}
