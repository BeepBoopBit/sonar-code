use sonar::{
    BriefOptions, QuantityOptions, QuantityQueryPolicy as Query, QuantityRankingPolicy as Ranking,
    RetrievalMode, SearchEngine, VariantScoring,
};
use std::{fs, path::Path};

fn engine(db: &Path, ranking: Ranking, query: Query, variants: VariantScoring) -> SearchEngine {
    SearchEngine::open(db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_quantities(true)
        .with_quantity_options(QuantityOptions { query, ranking })
        .with_variant_scoring(variants)
}

#[test]
fn connected_value_use_does_not_borrow_relevance_from_an_enclosing_aggregate_call() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("i.sqlite");
    let source = "fn target(options: Options) { consume_output(options.max_bytes.unwrap_or(65536)); }\nfn distractor() { consume_output(Config { stack_bytes: 65536, output_bytes: 1 }); }\n";
    fs::write(root.join("lib.rs"), source).unwrap();
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let opts = BriefOptions::default();
    let local = engine(&db, Ranking::Local, Query::Original, VariantScoring::Joint);
    let d = local.diagnose("output 64 KiB", &opts).unwrap();
    assert_eq!(d.candidates[0].symbol.as_deref(), Some("target"));
    let c = d
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("distractor"))
        .unwrap();
    assert!(c.measurements.as_ref().unwrap().lexical > 0.0);
    let evidence = c.quantity.as_ref().unwrap().local.as_ref().unwrap();
    assert_eq!(evidence.coverage, 0.0);
    assert_eq!(evidence.strength, 0.0);
    assert!(evidence.chain.iter().all(|s| s.name != "consume_output"));
    for c in &d.candidates {
        if let Some(q) = &c.quantity {
            for s in &q.local.as_ref().unwrap().chain {
                assert!(s.start_byte < s.end_byte && s.end_byte <= source.len());
                if !s.name.is_empty() {
                    assert_eq!(&source[s.start_byte..s.end_byte], s.name);
                }
            }
        }
    }
    let brief = local.brief("output 64 KiB", &opts).unwrap();
    let first = brief
        .files
        .iter()
        .flat_map(|f| &f.locations)
        .find(|l| l.id == brief.starts[0])
        .unwrap();
    assert_eq!(first.focus, Some([1, 1]));
}

#[test]
fn local_observations_are_optional_source_bound_and_do_not_change_prior_quantity_modes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("i.sqlite");
    fs::write(root.join("lib.rs"), "const OUTPUT_BYTES: usize=65536;\n").unwrap();
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let opts = BriefOptions::default();
    let local = engine(
        &db,
        Ranking::Local,
        Query::Expand,
        VariantScoring::Calibrated,
    );
    let old = engine(
        &db,
        Ranking::Unified,
        Query::Original,
        VariantScoring::Joint,
    );
    let before = serde_json::to_value(old.diagnose("output 64 KiB", &opts).unwrap()).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DELETE FROM quantity_uses", []).unwrap();
    let d = local.diagnose("output 64 KiB", &opts).unwrap();
    assert_eq!(d.quantity_status.as_deref(), Some("unavailable"));
    assert!(d.query_variants.is_none());
    assert_eq!(
        before,
        serde_json::to_value(old.diagnose("output 64 KiB", &opts).unwrap()).unwrap()
    );
    sonar::index_quantities(&db).unwrap();
    for query in [
        "OUTPUT_BYTES",
        "lib.rs",
        "output bytes",
        "64kb output",
        "64KiB2 output",
    ] {
        assert_eq!(
            serde_json::to_value(old.brief(query, &opts).unwrap()).unwrap(),
            serde_json::to_value(local.brief(query, &opts).unwrap()).unwrap()
        );
    }
    assert!(
        local
            .diagnose(
                "output 64 KiB",
                &BriefOptions {
                    scope: Some("other".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .candidates
            .is_empty()
    );
    fs::write(root.join("lib.rs"), "const OUTPUT_BYTES: usize=128;\n").unwrap();
    assert!(
        local
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .candidates
            .is_empty()
    );
    sonar::index(&root, &db).unwrap();
    assert_eq!(
        local
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .quantity_status
            .as_deref(),
        Some("unavailable")
    );
}

#[test]
fn calibration_keeps_one_variant_unchanged_and_reports_separate_scores_for_two() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("i.sqlite");
    fs::write(root.join("lib.rs"),"const OUTPUT_BYTES: usize=65536;\n/// output KiB\nfn kib() {}\n/// output bytes\nfn raw() {}\n").unwrap();
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let opts = BriefOptions::default();
    let joint = engine(
        &db,
        Ranking::Unified,
        Query::Original,
        VariantScoring::Joint,
    );
    let calibrated = engine(
        &db,
        Ranking::Unified,
        Query::Original,
        VariantScoring::Calibrated,
    );
    assert_eq!(
        serde_json::to_value(joint.brief("output 64 KiB", &opts).unwrap()).unwrap(),
        serde_json::to_value(calibrated.brief("output 64 KiB", &opts).unwrap()).unwrap()
    );
    let d = engine(
        &db,
        Ranking::Unified,
        Query::Expand,
        VariantScoring::Calibrated,
    )
    .diagnose("output 64 KiB", &opts)
    .unwrap();
    for c in d.candidates {
        let v = c.measurements.unwrap().variants.unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].weight, 0.75);
        assert_eq!(v[1].weight, 0.25);
        assert!((c.score - (0.75 * v[0].score + 0.25 * v[1].score)).abs() < 1e-14);
    }
    assert!(
        engine(
            &db,
            Ranking::Legacy,
            Query::Original,
            VariantScoring::Calibrated
        )
        .brief("output 64 KiB", &opts)
        .is_err()
    );
}
