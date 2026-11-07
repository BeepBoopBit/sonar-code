use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

use super::{AnalysisBatch, provider::AnalysisInput};
use crate::workspace::SourceFile;

const POLICY: &str = "dependency-facts-v1";

pub(crate) fn prepare(conn: &Connection, files: &[SourceFile], snapshot: &str) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS analysis_batches (
        provider TEXT PRIMARY KEY, policy TEXT NOT NULL, snapshot TEXT NOT NULL, batch TEXT NOT NULL
    )",
    )?;
    let input = AnalysisInput {
        files: files.iter().map(|f| (f.path.as_str(), f)).collect(),
    };
    // Replacement also removes facts belonging to retired providers.
    let mut batches = Vec::new();
    for adapter in crate::languages::ADAPTERS {
        let Some(provider) = adapter.dependency_provider() else {
            continue;
        };
        let saved: Option<String> = conn.query_row(
            "SELECT batch FROM analysis_batches WHERE provider=?1 AND policy=?2 AND snapshot=?3",
            params![provider.id(), POLICY, snapshot], |r| r.get(0),
        ).optional()?;
        let batch: AnalysisBatch = match saved {
            Some(json) => serde_json::from_str(&json)?,
            None => provider.analyze(&input)?,
        };
        ensure!(
            batch.coverage.provider == provider.id(),
            "provider identity mismatch"
        );
        validate(&input, &batch)?;
        batches.push(batch);
    }
    conn.execute("DELETE FROM analysis_batches", [])?;
    for batch in batches {
        conn.execute(
            "INSERT INTO analysis_batches VALUES (?1,?2,?3,?4)",
            params![
                batch.coverage.provider,
                POLICY,
                snapshot,
                serde_json::to_string(&batch)?
            ],
        )?;
    }
    Ok(())
}

fn validate(input: &AnalysisInput<'_>, batch: &AnalysisBatch) -> Result<()> {
    let mut covered = std::collections::BTreeSet::new();
    for path in batch
        .coverage
        .analyzed_files
        .iter()
        .chain(&batch.coverage.skipped_files)
    {
        ensure!(
            input.files.contains_key(path.as_str()),
            "unknown coverage file: {path}"
        );
        ensure!(covered.insert(path), "duplicate coverage file: {path}");
    }
    for (from, to, evidence) in batch
        .relationships
        .iter()
        .map(|r| (&r.from, Some(&r.to), &r.evidence))
        .chain(
            batch
                .unresolved
                .iter()
                .map(|r| (&r.from, None, &r.evidence)),
        )
    {
        ensure!(
            batch.coverage.analyzed_files.contains(from),
            "fact source was not analyzed"
        );
        let file = input
            .files
            .get(from.as_str())
            .ok_or_else(|| anyhow::anyhow!("unknown fact source: {from}"))?;
        ensure!(
            to.is_none_or(|p| input.files.contains_key(p.as_str())),
            "unknown dependency target"
        );
        ensure!(
            evidence.path == *from && evidence.provider == batch.coverage.provider,
            "invalid dependency provenance"
        );
        let [start, end] = evidence.bytes;
        ensure!(
            start < end && file.source.get(start..end).is_some(),
            "invalid dependency span"
        );
        ensure!(
            evidence.line == file.source[..start].bytes().filter(|b| *b == b'\n').count() + 1,
            "invalid dependency line"
        );
    }
    Ok(())
}

pub(crate) fn load(conn: &Connection, snapshot: &str) -> Result<Vec<AnalysisBatch>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analysis_batches')",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        exists,
        "dependency facts unavailable; run `sonar init` or `sonar index <root>`"
    );
    let mut stmt =
        conn.prepare("SELECT policy,snapshot,batch FROM analysis_batches ORDER BY provider")?;
    let records = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    ensure!(
        !records.is_empty(),
        "dependency facts unavailable; refresh the index"
    );
    let expected: std::collections::BTreeSet<_> = crate::languages::ADAPTERS
        .iter()
        .filter_map(|a| a.dependency_provider().map(|p| p.id()))
        .collect();
    let batches = records
        .into_iter()
        .map(|(policy, saved, json)| {
            ensure!(
                policy == POLICY && saved == snapshot,
                "dependency facts stale; refresh the index"
            );
            Ok(serde_json::from_str(&json)?)
        })
        .collect::<Result<Vec<AnalysisBatch>>>()?;
    let actual: std::collections::BTreeSet<_> = batches
        .iter()
        .map(|b| b.coverage.provider.as_str())
        .collect();
    ensure!(
        actual == expected,
        "dependency providers changed; refresh the index"
    );
    Ok(batches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{AnalysisCoverage, DependencyEvidence, DependencyFact, DependencyKind};

    #[test]
    fn provider_contract_rejects_missing_targets_and_invalid_evidence() {
        let source = SourceFile {
            path: "lib.rs".into(),
            hash: "fixture".into(),
            source: "mod a;".into(),
        };
        let target = SourceFile {
            path: "a.rs".into(),
            hash: "fixture".into(),
            source: String::new(),
        };
        let input = AnalysisInput {
            files: [("lib.rs", &source), ("a.rs", &target)]
                .into_iter()
                .collect(),
        };
        let mut batch = AnalysisBatch {
            coverage: AnalysisCoverage {
                language: "fixture".into(),
                provider: "fixture-v1".into(),
                analyzed_files: vec!["lib.rs".into()],
                skipped_files: vec![],
                limitations: vec![],
            },
            relationships: vec![DependencyFact {
                from: "lib.rs".into(),
                to: "a.rs".into(),
                kind: DependencyKind::DeclaresModule,
                evidence: DependencyEvidence {
                    path: "lib.rs".into(),
                    line: 1,
                    bytes: [0, 6],
                    provider: "fixture-v1".into(),
                },
            }],
            unresolved: vec![],
        };
        validate(&input, &batch).unwrap();
        batch.relationships[0].to = "missing.rs".into();
        assert!(validate(&input, &batch).is_err());
        batch.relationships[0].to = "a.rs".into();
        batch.relationships[0].evidence.bytes = [0, 100];
        assert!(validate(&input, &batch).is_err());
        batch.relationships[0].evidence.bytes = [0, 6];
        batch.relationships[0].evidence.provider = "wrong-provider".into();
        assert!(validate(&input, &batch).is_err());
    }
}
