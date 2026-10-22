use sonar::{BriefOptions, RetrievalMode, SearchEngine};
use std::fs;

fn structured_engine(db: &std::path::Path) -> SearchEngine {
    SearchEngine::open(db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_quantities(true)
        .with_quantity_options(sonar::QuantityOptions {
            query: sonar::QuantityQueryPolicy::Canonical,
            ranking: sonar::QuantityRankingPolicy::Structured,
        })
}

#[test]
fn structured_quantities_record_bindings_fallbacks_and_duration_coordinates() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("index.sqlite");
    let source = "fn output(opts: Options) { send_output(opts.max_bytes.unwrap_or(64 * 1024)); }\nconst OUTPUT_TIMEOUT: Duration = Duration::from_secs(2);\nconst OUTPUT_TIMEOUT_MS: u64 = 65536;\nconst OUTPUT_RATE_BYTES_PER_SECOND: u64 = 65536;\nfn configure() { let cfg = Settings { output_bytes: 64000 }; }\nfn arbitrary() { transform_output(65536); }\n#[test] fn output_test() { let output_bytes = 65536; }\n";
    fs::write(root.join("lib.rs"), source).unwrap();
    sonar::index(&root, &db).unwrap();
    sonar::index_quantities(&db).unwrap();
    let engine = structured_engine(&db);
    let q = engine
        .diagnose("output 64 KiB", &BriefOptions::default())
        .unwrap();
    let output = q
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("output"))
        .unwrap()
        .quantity
        .as_ref()
        .unwrap();
    assert_eq!(output.unit, "bytes");
    let binding = output.structure.as_ref().unwrap();
    assert_eq!(binding.receiver, "opts.max_bytes");
    assert_eq!(binding.role, "fallback_argument");
    assert_eq!(binding.unit_basis, "receiver_name");
    assert_eq!(binding.argument, Some(0));
    assert!(binding.callees.contains(&"send_output".into()));
    assert_eq!(&source[output.start_byte..output.end_byte], "64 * 1024");
    assert!(
        !q.candidates
            .iter()
            .any(|c| c.symbol.as_deref() == Some("OUTPUT_TIMEOUT_MS") && c.quantity.is_some())
    );
    let rate = q
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("OUTPUT_RATE_BYTES_PER_SECOND"))
        .unwrap()
        .quantity
        .as_ref()
        .unwrap();
    assert_eq!(rate.unit, "unknown");
    let timeout = engine
        .diagnose("output timeout 2000 ms", &BriefOptions::default())
        .unwrap();
    let proof = timeout
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("OUTPUT_TIMEOUT"))
        .unwrap()
        .quantity
        .as_ref()
        .unwrap();
    assert_eq!(proof.value, "2000");
    assert_eq!(proof.unit, "milliseconds");
    let structure = proof.structure.as_ref().unwrap();
    assert_eq!(structure.binding, "OUTPUT_TIMEOUT");
    assert_eq!(structure.unit_basis, "duration_constructor_spelling");
    assert_eq!(&source[proof.start_byte..proof.end_byte], "2");
    let config = engine
        .diagnose("output 64 KB", &BriefOptions::default())
        .unwrap();
    assert!(config.candidates.iter().any(|c| {
        c.symbol.as_deref() == Some("configure")
            && c.quantity
                .as_ref()
                .is_some_and(|q| q.structure.as_ref().unwrap().binding == "output_bytes")
    }));
    assert!(
        q.candidates
            .iter()
            .any(|c| c.symbol.as_deref() == Some("output_test") && c.quantity.is_some())
    );
    for c in q.candidates {
        if let Some(q) = c.quantity {
            let strength = q.structure.unwrap().strength;
            assert!((0.0..=1.0).contains(&strength));
        }
    }
}

#[test]
fn canonical_queries_are_equivalent_but_inactive_exact_and_stale_queries_keep_behavior() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("index.sqlite");
    fs::write(
        root.join("lib.rs"),
        "const OUTPUT_BYTES: usize = 65536;\nconst OUTPUT_MS: u64 = 2;\n",
    )
    .unwrap();
    sonar::index(&root, &db).unwrap();
    let plain = SearchEngine::open(&db, RetrievalMode::Lexical, None).unwrap();
    let enhanced = structured_engine(&db);
    let opts = BriefOptions::default();
    assert!(
        enhanced
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .quantity_query
            .is_none()
    );
    sonar::index_quantities(&db).unwrap();
    let first = enhanced.brief("output 64 KiB", &opts).unwrap();
    let second = enhanced.brief("output 65,536 bytes", &opts).unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(second).unwrap()
    );
    for q in [
        "OUTPUT_BYTES",
        "lib.rs",
        "output bytes",
        "output 64kb",
        "output 64KiB2",
        "output 16 KiB and 64 KiB",
    ] {
        assert_eq!(
            serde_json::to_value(plain.brief(q, &opts).unwrap()).unwrap(),
            serde_json::to_value(enhanced.brief(q, &opts).unwrap()).unwrap()
        );
    }
    let scoped = enhanced
        .brief(
            "output 64 KiB",
            &BriefOptions {
                scope: Some("elsewhere".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(scoped.starts.is_empty());
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute("DELETE FROM quantity_structure", []).unwrap();
    assert_eq!(
        enhanced
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .quantity_status
            .as_deref(),
        Some("unavailable")
    );
    assert!(
        enhanced
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .quantity_query
            .is_none()
    );
    sonar::index_quantities(&db).unwrap();
    fs::write(root.join("lib.rs"), "const OUTPUT_BYTES: usize = 128;\n").unwrap();
    assert!(
        enhanced
            .brief("output 64 KiB", &opts)
            .unwrap()
            .starts
            .is_empty()
    );
    sonar::index(&root, &db).unwrap();
    assert_eq!(
        enhanced
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .quantity_status
            .as_deref(),
        Some("unavailable")
    );
    sonar::index_quantities(&db).unwrap();
    assert_eq!(
        enhanced
            .diagnose("output 64 KiB", &opts)
            .unwrap()
            .quantity_status
            .as_deref(),
        Some("no_evidence")
    );
}

#[test]
fn quantities_require_context_keep_units_separate_and_preserve_source_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = temp.path().join("index.sqlite");
    let source = "const READ_BYTES: usize = 64 * 1024;\nconst READ_TIMEOUT_MS: u64 = 65536;\nconst NETWORK_BYTES: usize = 65536;\nconst READ_DECIMAL_BYTES: usize = 64000;\nconst POLL_SECONDS: u64 = 2;\n";
    fs::write(root.join("lib.rs"), source).unwrap();
    sonar::index(&root, &db).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Lexical, None).unwrap();
    let before = serde_json::to_value(
        engine
            .brief("read 64 KiB", &BriefOptions::default())
            .unwrap(),
    )
    .unwrap();
    let report = sonar::index_quantities(&db).unwrap();
    assert_eq!(report.facts, 5);
    assert_eq!(
        before,
        serde_json::to_value(
            engine
                .brief("read 64 KiB", &BriefOptions::default())
                .unwrap()
        )
        .unwrap()
    );
    let enabled = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_quantities(true);
    for (query, expected, unit) in [
        ("read 64 KiB", "READ_BYTES", "bytes"),
        ("read 64 KB", "READ_DECIMAL_BYTES", "bytes"),
        ("poll 2000 ms", "POLL_SECONDS", "milliseconds"),
    ] {
        let d = enabled.diagnose(query, &BriefOptions::default()).unwrap();
        assert_eq!(d.quantity_status.as_deref(), Some("current"));
        let observations: Vec<_> = d
            .candidates
            .iter()
            .filter(|c| c.quantity.is_some())
            .collect();
        assert_eq!(observations.len(), 1);
        let c = observations[0];
        assert_eq!(c.symbol.as_deref(), Some(expected));
        let evidence = c.quantity.as_ref().unwrap();
        assert_eq!(evidence.unit, unit);
        assert!(
            source.is_char_boundary(evidence.start_byte)
                && source.is_char_boundary(evidence.end_byte)
        );
        assert_eq!(
            source[..evidence.start_byte]
                .bytes()
                .filter(|b| *b == b'\n')
                .count()
                + 1,
            evidence.line
        );
        let brief = enabled.brief(query, &BriefOptions::default()).unwrap();
        let loc = brief
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .find(|l| l.symbol.as_deref() == Some(expected))
            .unwrap();
        assert_eq!(loc.focus, Some([evidence.line, evidence.end_line]));
    }
    let empty = enabled
        .brief(
            "read 64 KiB",
            &BriefOptions {
                scope: Some("other".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(empty.starts.is_empty());
    for query in [
        "READ_BYTES",
        "64kb read",
        "read 32 KiB versus 64 KiB",
        "65,5346 bytes read",
    ] {
        let a = engine.brief(query, &BriefOptions::default()).unwrap();
        let b = enabled.brief(query, &BriefOptions::default()).unwrap();
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        );
    }
    fs::write(
        root.join("lib.rs"),
        "const READ_BYTES: usize = 32 * 1024;\n",
    )
    .unwrap();
    assert!(sonar::index_quantities(&db).is_err());
    assert!(
        enabled
            .brief("read 64 KiB", &BriefOptions::default())
            .unwrap()
            .starts
            .is_empty()
    );
    sonar::index(&root, &db).unwrap();
    assert_eq!(
        enabled
            .diagnose("read 64 KiB", &BriefOptions::default())
            .unwrap()
            .quantity_status
            .as_deref(),
        Some("unavailable")
    );
    sonar::index_quantities(&db).unwrap();
    assert_eq!(
        enabled
            .diagnose("read 64 KiB", &BriefOptions::default())
            .unwrap()
            .quantity_status
            .as_deref(),
        Some("no_evidence")
    );
}
