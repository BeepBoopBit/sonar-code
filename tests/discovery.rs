use std::{collections::BTreeSet, fs, path::Path};

use sonar::{
    BriefOptions, QuantityOptions, QuantityQueryPolicy as Query, QuantityRankingPolicy as Ranking,
    RetrievalMode, SearchEngine,
};

fn engine(db: &Path, query: Query, ranking: Ranking) -> SearchEngine {
    SearchEngine::open(db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_quantities(true)
        .with_quantity_options(QuantityOptions { query, ranking })
}

#[test]
fn discovery_measures_and_can_promote_an_owner_outside_the_lexical_pool() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("index.sqlite");
    for i in 0..600 {
        fs::write(
            root.join(format!("read_{i}.rs")),
            format!("fn read_{i}() {{ read(); }}\n"),
        )
        .unwrap();
    }
    // Textual relevance appears elsewhere in the owner, not in the literal's binding.
    fs::write(
        root.join("z_owner.rs"),
        format!(
            "fn opaque() {{ read_channel(); let cap_bytes = 65536; {} }}\n",
            "padding(); ".repeat(150)
        ),
    )
    .unwrap();
    fs::write(root.join("z_zero.rs"), "const CAP_BYTES: usize = 65536;\n").unwrap();
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let opts = BriefOptions::default();
    let plain = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .diagnose("read 64 KiB", &opts)
        .unwrap();
    assert!(
        !plain
            .candidates
            .iter()
            .any(|c| c.symbol.as_deref() == Some("opaque"))
    );
    let old = engine(&db, Query::Original, Ranking::Structured)
        .diagnose("read 64 KiB", &opts)
        .unwrap();
    assert!(
        !old.candidates
            .iter()
            .any(|c| c.symbol.as_deref() == Some("opaque"))
    );
    let new = engine(&db, Query::Original, Ranking::Unified)
        .diagnose("read 64 KiB", &opts)
        .unwrap();
    let target = new
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("opaque"))
        .unwrap();
    assert_eq!(target.rank, 1);
    assert_eq!(target.lexical_rank, None);
    assert_eq!(target.quantity.as_ref().unwrap().matched_terms, 0);
    let m = target.measurements.as_ref().unwrap();
    assert!(m.lexical > 0.0 && m.lexical_normalized > 0.0);
    assert_eq!(m.semantic, None);
    assert_eq!(m.binding, 0.0);
    let proof = target.discovery.as_ref().unwrap();
    assert!(!proof.original_lexical && !proof.original_embedding && proof.quantity);
    let zero = new
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("CAP_BYTES"))
        .unwrap();
    assert_eq!(zero.measurements.as_ref().unwrap().lexical, 0.0);
    assert!(new.candidates.iter().all(|c| c.measurements.is_some()));
    let ids: BTreeSet<_> = new.candidates.iter().map(|c| c.location_id).collect();
    assert_eq!(ids.len(), new.candidates.len());
    assert!(
        plain
            .candidates
            .iter()
            .all(|c| ids.contains(&c.location_id))
    );
    let brief = engine(&db, Query::Original, Ranking::Unified)
        .brief("read 64 KiB", &opts)
        .unwrap();
    let location = brief
        .files
        .iter()
        .flat_map(|f| &f.locations)
        .find(|l| l.id == brief.starts[0])
        .unwrap();
    let q = target.quantity.as_ref().unwrap();
    assert_eq!(location.focus, Some([q.line, q.end_line]));
}

#[test]
fn expansion_retains_both_pools_deduplicates_and_bypasses_inactive_or_exact_queries() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("index.sqlite");
    fs::write(root.join("lib.rs"), "fn kib_handler() {}\n/// 65536 bytes of data\nfn encoded() {}\nconst CAP_BYTES: usize = 65536;\n").unwrap();
    sonar::index(&root, &db).unwrap();
    let opts = BriefOptions::default();
    let expanded = engine(&db, Query::Expand, Ranking::Unified);
    assert!(
        expanded
            .diagnose("data 64 KiB", &opts)
            .unwrap()
            .query_variants
            .is_none()
    );
    sonar::index_quantities(&db).unwrap();
    let plain = SearchEngine::open(&db, RetrievalMode::Lexical, None).unwrap();
    let d = expanded.diagnose("data 64 KiB", &opts).unwrap();
    assert_eq!(
        d.query_variants,
        Some(vec!["data 64 KiB".into(), "data 65536 bytes".into()])
    );
    let ids: BTreeSet<_> = d.candidates.iter().map(|c| c.location_id).collect();
    assert_eq!(ids.len(), d.candidates.len());
    for query in ["data 64 KiB", "data 65536 bytes"] {
        assert!(
            plain
                .diagnose(query, &opts)
                .unwrap()
                .candidates
                .iter()
                .all(|c| ids.contains(&c.location_id))
        );
    }
    for query in [
        "data 65536 bytes",
        "kib_handler",
        "lib.rs",
        "data 64kb",
        "data 64KiB2",
        "data 32 KiB and 64 KiB",
    ] {
        let a = engine(&db, Query::Original, Ranking::Unified)
            .diagnose(query, &opts)
            .unwrap();
        let b = expanded.diagnose(query, &opts).unwrap();
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        );
    }
    assert!(
        expanded
            .diagnose(
                "data 64 KiB",
                &BriefOptions {
                    scope: Some("elsewhere".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .candidates
            .is_empty()
    );
    fs::write(root.join("lib.rs"), "const CAP_BYTES: usize = 128;\n").unwrap();
    assert!(
        expanded
            .diagnose("data 64 KiB", &opts)
            .unwrap()
            .candidates
            .is_empty()
    );
    sonar::index(&root, &db).unwrap();
    let stale = expanded.diagnose("data 64 KiB", &opts).unwrap();
    assert!(stale.query_variants.is_none());
    assert_eq!(stale.quantity_status.as_deref(), Some("unavailable"));
}
