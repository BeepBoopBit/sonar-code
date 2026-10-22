use sonar::{
    BriefOptions, QuantityOptions, QuantityQueryPolicy, QuantityRankingPolicy as Policy,
    RankingOptions, RetrievalMode, SearchEngine, SelectionPolicy, VariantScoring,
};
use std::{collections::BTreeMap, fs, path::Path};

fn engine(db: &Path, policy: Policy) -> SearchEngine {
    SearchEngine::open(db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_quantities(true)
        .with_quantity_options(QuantityOptions {
            ranking: policy,
            ..Default::default()
        })
        .with_ranking(RankingOptions {
            selection: SelectionPolicy::Ranked,
            ..Default::default()
        })
}

fn json(value: impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

#[test]
fn bounded_local_retains_common_scores_and_accounts_for_every_addition() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.rs"), "fn target(options: Options) { consume_output(options.max_bytes.unwrap_or(65536)); }\nfn unrelated() { let stack_bytes = 65536; reserve(stack_bytes); }\nfn opaque() { other(65536); }\nfn no_quantity() { consume_output(); }\n").unwrap();
    let db = temp.path().join("i.sqlite");
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let opts = BriefOptions::default();
    let baseline = engine(&db, Policy::Unified)
        .diagnose("output 64 KiB", &opts)
        .unwrap();
    let local = engine(&db, Policy::Local)
        .diagnose("output 64 KiB", &opts)
        .unwrap();
    let bounded = engine(&db, Policy::BoundedLocal)
        .diagnose("output 64 KiB", &opts)
        .unwrap();
    let original: BTreeMap<_, _> = baseline
        .candidates
        .iter()
        .map(|c| (c.location_id, c))
        .collect();
    assert_eq!(bounded.candidates.len(), original.len());
    assert_eq!(bounded.quantity_status.as_deref(), Some("current"));
    let mut boosted = 0;
    for c in &bounded.candidates {
        let old = original[&c.location_id];
        assert!(c.score >= old.score);
        let mut measurement = json(c.measurements.as_ref().unwrap());
        measurement["score"] = json(old.score);
        assert_eq!(measurement, json(old.measurements.as_ref().unwrap()));
        if let Some(q) = &c.quantity {
            let path = q.local.as_ref().unwrap();
            let contribution = path.contribution.as_ref().unwrap();
            assert_eq!(contribution.base_score, old.score);
            assert!((0.0..=0.25 / 61.0).contains(&contribution.addition));
            assert_eq!(c.score, old.score + contribution.addition);
            boosted += usize::from(contribution.addition > 0.0);
            let mut evidence = json(path);
            evidence.as_object_mut().unwrap().remove("contribution");
            let old_local = local
                .candidates
                .iter()
                .find(|v| v.location_id == c.location_id)
                .unwrap();
            assert_eq!(
                evidence,
                json(old_local.quantity.as_ref().unwrap().local.as_ref().unwrap())
            );
        } else {
            assert_eq!(c.score, old.score);
        }
    }
    assert!(boosted > 0);
    // Ordinary brief fields do not expose ranking diagnostics or code snippets.
    let brief = engine(&db, Policy::BoundedLocal)
        .brief("output 64 KiB", &opts)
        .unwrap();
    assert!(!json(brief).to_string().contains("contribution"));
}

#[test]
fn scope_recomputes_median_and_stale_or_missing_observations_fall_back() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("a.rs"),
        "fn output() { let output_bytes=65536; }\n",
    )
    .unwrap();
    fs::write(root.join("b.rs"), "fn stack() { let stack_bytes=65536; }\n").unwrap();
    let db = temp.path().join("i.sqlite");
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let bounded = engine(&db, Policy::BoundedLocal);
    let unified = engine(&db, Policy::Unified);
    let opts = BriefOptions {
        scope: Some("a.rs".into()),
        ..Default::default()
    };
    let d = bounded.diagnose("output 64 KiB", &opts).unwrap();
    assert!(d.candidates.iter().all(|c| c.path == "a.rs"));
    for q in d.candidates.iter().filter_map(|c| c.quantity.as_ref()) {
        let path = q.local.as_ref().unwrap();
        assert_eq!(path.contribution.as_ref().unwrap().median, path.strength);
        assert_eq!(path.contribution.as_ref().unwrap().addition, 0.0);
    }
    assert_eq!(
        json(bounded.brief("output 64 KiB", &opts).unwrap()),
        json(unified.brief("output 64 KiB", &opts).unwrap())
    );
    assert!(
        bounded
            .diagnose(
                "output 64 KiB",
                &BriefOptions {
                    scope: Some("absent".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .candidates
            .is_empty()
    );
    fs::write(root.join("a.rs"), "fn output() { let output_bytes=128; }\n").unwrap();
    assert!(
        bounded
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .candidates
            .is_empty()
    );
    sonar::index(&root, &db).unwrap();
    assert_eq!(
        bounded
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .quantity_status
            .as_deref(),
        Some("unavailable")
    );
    sonar::index_quantities(&db).unwrap();
    let plain = engine(&db, Policy::Legacy).with_quantities(false);
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DELETE FROM quantity_uses", []).unwrap();
    let opts = BriefOptions::default();
    let brief = bounded.brief("output 64 KiB", &opts).unwrap();
    assert!(
        brief
            .notices
            .iter()
            .any(|n| n.code == "quantities_unavailable")
    );
    let mut bounded_json = json(brief);
    bounded_json["notices"]
        .as_array_mut()
        .unwrap()
        .retain(|n| n["code"] != "quantities_unavailable");
    assert_eq!(
        bounded_json,
        json(plain.brief("output 64 KiB", &opts).unwrap())
    );
}

#[test]
fn exact_path_with_quantity_and_inactive_queries_preserve_complete_briefs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir_all(root.join("output 64 KiB")).unwrap();
    fs::write(
        root.join("output 64 KiB/mod.rs"),
        "const OUTPUT_BYTES: usize=65536;\n",
    )
    .unwrap();
    let db = temp.path().join("i.sqlite");
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let bounded = engine(&db, Policy::BoundedLocal);
    let plain = engine(&db, Policy::Legacy).with_quantities(false);
    let opts = BriefOptions::default();
    let d = bounded.diagnose("output 64 KiB/mod.rs", &opts).unwrap();
    assert_eq!(d.quantity_status.as_deref(), Some("exact_bypass"));
    assert!(d.candidates.iter().all(|c| c.quantity.is_none()));
    for query in [
        "output 64 KiB/mod.rs",
        "OUTPUT_BYTES",
        "output bytes",
        "64kb output",
        "64KiB2 output",
        "16 KiB and 64 KiB output",
    ] {
        assert_eq!(
            json(bounded.brief(query, &opts).unwrap()),
            json(plain.brief(query, &opts).unwrap()),
            "{query}"
        );
    }
    let unified = engine(&db, Policy::Unified);
    // An active query with no matching observations also retains unified scores.
    assert_eq!(
        json(bounded.brief("output 7 KiB", &opts).unwrap()),
        json(unified.brief("output 7 KiB", &opts).unwrap())
    );
}

#[test]
fn bounded_policy_rejects_query_variants_and_requires_explicit_quantity_opt_in() {
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("i.sqlite");
    let opts = BriefOptions::default();
    for policy in [QuantityQueryPolicy::Canonical, QuantityQueryPolicy::Expand] {
        let error = engine(&db, Policy::BoundedLocal)
            .with_quantity_options(QuantityOptions {
                query: policy,
                ranking: Policy::BoundedLocal,
            })
            .brief("output 64 KiB", &opts)
            .unwrap_err();
        assert!(error.to_string().contains("--quantity-query original"));
    }
    assert!(
        engine(&db, Policy::BoundedLocal)
            .with_quantities(false)
            .brief("output 64 KiB", &opts)
            .unwrap_err()
            .to_string()
            .contains("requires --quantities")
    );
    assert!(
        engine(&db, Policy::BoundedLocal)
            .with_variant_scoring(VariantScoring::Calibrated)
            .brief("output 64 KiB", &opts)
            .unwrap_err()
            .to_string()
            .contains("--variant-scoring joint")
    );
}
