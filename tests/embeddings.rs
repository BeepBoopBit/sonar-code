use std::{collections::HashMap, fs, path::Path};

use safetensors::tensor::{Dtype, TensorView, serialize_to_file};
use sonar::{BriefOptions, Embedder, RetrievalMode, SearchEngine, embed_index, index};
use tokenizers::{Tokenizer, models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace};

#[test]
fn exclusions_filter_dense_candidates_before_the_channel_cap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir_all(root.join("reports")).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("reports/noise.rs"),
        (0..550)
            .map(|i| format!("/// cat\nfn candidate_{i}() {{}}\n"))
            .collect::<String>(),
    )
    .unwrap();
    fs::write(root.join("keep.rs"), "/// cat dog\nfn allowed() {}\n").unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    // Fix both channels' view scores to test pool admission independently of
    // encoder pooling and identity-view ties in this tiny model.
    let conn = rusqlite::Connection::open(&db).unwrap();
    for (path, vector) in [
        ("reports/noise.rs", [1.0_f32, 0.0]),
        ("keep.rs", [0.0_f32, 1.0]),
    ] {
        let bytes: Vec<_> = vector.into_iter().flat_map(f32::to_le_bytes).collect();
        conn.execute(
            "UPDATE embeddings SET vector=?1 WHERE region_id IN
            (SELECT r.id FROM regions r JOIN locations l ON l.id=r.location_id
             JOIN files f ON f.id=l.file_id WHERE f.path=?2)",
            rusqlite::params![bytes, path],
        )
        .unwrap();
    }
    let base = SearchEngine::open(&db, RetrievalMode::Embedding, Some(&model))
        .unwrap()
        .with_kind(Some("function".into()));
    let opts = BriefOptions::default();
    assert!(
        !base
            .diagnose("feline", &opts)
            .unwrap()
            .candidates
            .iter()
            .any(|c| c.path == "keep.rs")
    );
    let filtered = base.with_exclusions(vec!["reports".into()]);
    let d = filtered.diagnose("feline", &opts).unwrap();
    assert_eq!(d.candidates.len(), 1);
    assert_eq!(d.candidates[0].symbol.as_deref(), Some("allowed"));
}

#[test]
fn static_input_policies_remove_batch_padding_and_retain_late_evidence() {
    let temp = tempfile::tempdir().unwrap();
    tiny_model(temp.path());
    let path = temp.path().join("tokenizer.json");
    let mut tokenizer = Tokenizer::from_file(&path).unwrap();
    tokenizer.with_padding(Some(tokenizers::PaddingParams {
        pad_id: 3,
        pad_token: "dog".into(),
        ..Default::default()
    }));
    tokenizer
        .with_truncation(Some(tokenizers::TruncationParams {
            max_length: 512,
            ..Default::default()
        }))
        .unwrap();
    tokenizer.save(&path, false).unwrap();
    for encoding in [sonar::EncodingPolicy::Unpadded, sonar::EncodingPolicy::Full] {
        let model = Embedder::load_with_encoding(temp.path(), encoding).unwrap();
        let single = model.encode(&["cat".into()]).unwrap();
        let batch = model
            .encode(&["cat".into(), "dog dog dog dog".into()])
            .unwrap();
        assert_eq!(single[0], batch[0]);
    }
    let legacy = Embedder::load(temp.path()).unwrap();
    let unpadded =
        Embedder::load_with_encoding(temp.path(), sonar::EncodingPolicy::Unpadded).unwrap();
    let full = Embedder::load_with_encoding(temp.path(), sonar::EncodingPolicy::Full).unwrap();
    assert_ne!(legacy.fingerprint(), unpadded.fingerprint());
    assert_ne!(unpadded.fingerprint(), full.fingerprint());
    let late = format!("{} {}", "cat ".repeat(600), "dog ".repeat(600));
    let a = unpadded.encode(std::slice::from_ref(&late)).unwrap();
    let b = full.encode(&[late]).unwrap();
    assert!(a[0].iter().zip(&b[0]).any(|(a, b)| (a - b).abs() > 0.1));
}

#[test]
fn encoding_and_context_are_persisted_and_switching_rebuilds_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(root.join("cat.rs"), "fn alternative() { dog(); }\n").unwrap();
    index(&root, &db).unwrap();
    let base = embed_index(&db, &model).unwrap();
    let changed = sonar::embed_index_with_options(
        &db,
        &model,
        sonar::Representation::Context,
        sonar::EncodingPolicy::Full,
        |_| {},
    )
    .unwrap();
    assert_eq!(changed.reused, 0);
    assert_eq!(changed.regions, base.regions);
    assert_ne!(changed.model_fingerprint, base.model_fingerprint);
    let cached = sonar::embed_index_with_options(
        &db,
        &model,
        sonar::Representation::Context,
        sonar::EncodingPolicy::Full,
        |_| {},
    )
    .unwrap();
    assert_eq!(cached.encoded, 0);
    let engine = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    let d = engine.diagnose("feline", &BriefOptions::default()).unwrap();
    assert_eq!(d.representation.as_deref(), Some("context"));
    assert_eq!(
        d.model_fingerprint.as_deref(),
        Some(changed.model_fingerprint.as_str())
    );
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute("DELETE FROM embeddings WHERE region_id=(SELECT min(id) FROM regions WHERE view!='combined')",[]).unwrap();
    assert!(
        engine
            .brief("feline", &BriefOptions::default())
            .unwrap_err()
            .to_string()
            .contains("incomplete")
    );
    sonar::embed_index_with_options(
        &db,
        &model,
        sonar::Representation::Context,
        sonar::EncodingPolicy::Full,
        |_| {},
    )
    .unwrap();
    embed_index(&db, &model).unwrap();
    assert!(
        engine
            .brief("feline", &BriefOptions::default())
            .unwrap_err()
            .to_string()
            .contains("model does not match")
    );
}

#[test]
fn score_fusion_preserves_pools_exact_identity_and_missing_channel_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("lib.rs"),
        "/// cat\nfn alternative() {}\nfn dog() {}\nfn cryptographic_verifier() {}\n",
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let mut pools = Vec::new();
    for method in [sonar::FusionMethod::Rrf, sonar::FusionMethod::Score] {
        let engine = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_ranking(sonar::RankingOptions {
                fusion: method,
                selection: sonar::SelectionPolicy::Ranked,
                context: sonar::ContextPolicy::Query,
                ..Default::default()
            });
        let d = engine.diagnose("feline", &BriefOptions::default()).unwrap();
        let mut pool: Vec<_> = d.candidates.iter().map(|c| c.location_id).collect();
        pool.sort_unstable();
        pools.push(pool);
        assert!(
            d.candidates
                .iter()
                .all(|c| c.lexical_score.is_none() && c.score.is_finite())
        );
        assert_eq!(d.candidates[0].symbol.as_deref(), Some("alternative"));
        let d = engine.diagnose("cat", &BriefOptions::default()).unwrap();
        assert!(
            d.candidates
                .iter()
                .any(|c| c.lexical_score.is_some_and(|s| s > 0.0))
        );
        assert!(d.candidates.iter().all(|c| {
            c.embedding_normalized
                .is_none_or(|s| (0.0..=1.0).contains(&s))
        }));
        for query in ["cryptographic_verifier", "lib.rs"] {
            let d = engine.diagnose(query, &BriefOptions::default()).unwrap();
            assert!(d.candidates[0].exact_path_match || d.candidates[0].exact_symbol_match);
            assert_eq!(d.candidates[0].selected_rank, Some(1));
        }
        let empty = engine
            .brief(
                "feline",
                &BriefOptions {
                    scope: Some("outside".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(empty.starts.is_empty());
    }
    assert_eq!(pools[0], pools[1]);
    for weight in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        let e = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_ranking(sonar::RankingOptions {
                lexical_weight: weight,
                ..Default::default()
            });
        assert!(
            e.brief("cat", &BriefOptions::default())
                .unwrap_err()
                .to_string()
                .contains("lexical weight")
        );
    }
    fs::write(root.join("lib.rs"), "fn changed() {}\n").unwrap();
    let e = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
        .unwrap()
        .with_ranking(sonar::RankingOptions {
            fusion: sonar::FusionMethod::Score,
            context: sonar::ContextPolicy::Query,
            ..Default::default()
        });
    let b = e.brief("cat", &BriefOptions::default()).unwrap();
    assert!(b.starts.is_empty() && b.links.is_empty());
    assert_eq!(b.index.syntax_freshness, "stale");
}

#[test]
fn contextual_pooling_is_independent_of_batch_padding_and_uses_context() {
    use candle_core::{DType, Device};
    use candle_nn::{VarBuilder, VarMap};
    use candle_transformers::models::bert::{BertModel, Config};
    let temp = tempfile::tempdir().unwrap();
    tiny_model(temp.path());
    let config_json = serde_json::json!({"vocab_size":5,"hidden_size":4,"num_hidden_layers":1,"num_attention_heads":2,
        "intermediate_size":8,"hidden_act":"gelu","hidden_dropout_prob":0.0,"max_position_embeddings":32,
        "type_vocab_size":2,"initializer_range":0.02,"layer_norm_eps":1e-12,"pad_token_id":0,"model_type":"bert"});
    let config: Config = serde_json::from_value(config_json.clone()).unwrap();
    let variables = VarMap::new();
    BertModel::load(
        VarBuilder::from_varmap(&variables, DType::F32, &Device::Cpu),
        &config,
    )
    .unwrap();
    variables
        .save(temp.path().join("model.safetensors"))
        .unwrap();
    fs::write(
        temp.path().join("config.json"),
        serde_json::to_vec(&config_json).unwrap(),
    )
    .unwrap();
    fs::write(
        temp.path().join("sentence_bert_config.json"),
        "{\"max_seq_length\":16}",
    )
    .unwrap();
    fs::create_dir(temp.path().join("1_Pooling")).unwrap();
    fs::write(
        temp.path().join("1_Pooling/config.json"),
        "{\"pooling_mode_mean_tokens\":true,\"pooling_mode_cls_token\":false}",
    )
    .unwrap();
    let model = Embedder::load(temp.path()).unwrap();
    let single = model.encode(&["cat".into()]).unwrap();
    let batch = model
        .encode(&["cat".into(), "dog canine dog canine".into()])
        .unwrap();
    for (a, b) in single[0].iter().zip(&batch[0]) {
        assert!((a - b).abs() < 1e-5);
    }
    assert!(
        batch[0]
            .iter()
            .zip(&batch[1])
            .any(|(a, b)| (a - b).abs() > 1e-4)
    );
    assert!((batch[0].iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);
    assert_eq!(model.dimensions(), 4);
}

#[test]
fn separate_views_preserve_description_signal_and_representation_switches_invalidate_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("lib.rs"),
        format!(
            "/// cat\n#[allow(dead_code)]\nfn unnamed() {{ {} }}\nfn dog() {{}}\n",
            "dog(); ".repeat(150)
        ),
    )
    .unwrap();
    index(&root, &db).unwrap();
    let separate = embed_index(&db, &model).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Embedding, Some(&model)).unwrap();
    let diagnostic = engine.diagnose("feline", &BriefOptions::default()).unwrap();
    let target = diagnostic
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("unnamed"))
        .unwrap();
    assert_eq!(target.view_ranks.get("description"), Some(&1));
    assert!(target.view_similarities["description"] > 0.99);
    let brief = engine.brief("feline", &BriefOptions::default()).unwrap();
    let target = brief
        .files
        .iter()
        .flat_map(|f| &f.locations)
        .find(|l| l.symbol.as_deref() == Some("unnamed"))
        .unwrap();
    assert_eq!(target.focus.unwrap()[0], 1);
    let combined = sonar::embed_index_with(&db, &model, sonar::Representation::Combined).unwrap();
    assert_eq!(combined.reused, 0);
    assert!(combined.regions < separate.regions);
    assert_eq!(
        engine
            .diagnose("feline", &BriefOptions::default())
            .unwrap()
            .representation
            .as_deref(),
        Some("combined")
    );
}

#[test]
fn hybrid_preserves_exact_symbol_lookups_and_discovers_comment_only_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("lib.rs"),
        "fn cat() {}\n/// cat cat cat\nfn distraction() {}\n",
    )
    .unwrap();
    fs::write(root.join("notes.rs"), "// canine\n").unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    let diagnostic = engine.diagnose("cat", &BriefOptions::default()).unwrap();
    assert_eq!(diagnostic.candidates[0].symbol.as_deref(), Some("cat"));
    assert!(diagnostic.candidates[0].exact_symbol_match);
    assert!(
        !engine
            .brief(
                "dog",
                &BriefOptions {
                    scope: Some("notes.rs".into()),
                    ..Default::default()
                }
            )
            .unwrap()
            .starts
            .is_empty()
    );
}

#[test]
fn embedded_anchors_preserve_hybrid_channel_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(root.join("worker.yaml"), "description: cat\n").unwrap();
    fs::write(root.join("lib.rs"), "/// cat\nfn unrelated() {}\n").unwrap();
    sonar::index_with_resources(
        &root,
        &db,
        sonar::Chunking::Lines,
        sonar::ResourcePolicy::Yaml,
    )
    .unwrap();
    embed_index(&db, &model).unwrap();
    let engine = |enabled| {
        SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_anchors(enabled)
            .with_ranking(sonar::RankingOptions {
                selection: sonar::SelectionPolicy::Ranked,
                file_ranking: sonar::FileRankingPolicy::Standalone,
                ..Default::default()
            })
    };
    let opts = BriefOptions::default();
    let old = engine(false).diagnose("feline worker", &opts).unwrap();
    let new = engine(true).diagnose("feline worker", &opts).unwrap();
    let anchors = new.anchors.as_ref().unwrap();
    assert_eq!(anchors.status, "active");
    assert_eq!(old.model_fingerprint, new.model_fingerprint);
    for c in &old.candidates {
        let n = new
            .candidates
            .iter()
            .find(|n| n.location_id == c.location_id)
            .unwrap();
        assert_eq!(n.embedding_rank, c.embedding_rank);
        assert_eq!(n.view_ranks, c.view_ranks);
        assert_eq!(n.view_similarities, c.view_similarities);
        assert_eq!(n.lexical_rank, c.lexical_rank);
        let addition = anchors
            .channel
            .iter()
            .find(|a| a.location_id == c.location_id)
            .map_or(0., |a| a.addition);
        assert_eq!(n.score, c.score + addition);
    }
}

fn tiny_model(path: &Path) {
    fs::create_dir_all(path).unwrap();
    let vocabulary = ["[UNK]", "cat", "feline", "dog", "canine"]
        .into_iter()
        .enumerate()
        .map(|(i, token)| (token.to_owned(), i as u32))
        .collect();
    let mut tokenizer = Tokenizer::new(
        WordLevel::builder()
            .vocab(vocabulary)
            .unk_token("[UNK]".into())
            .build()
            .unwrap(),
    );
    tokenizer.with_pre_tokenizer(Some(Whitespace));
    tokenizer.save(path.join("tokenizer.json"), false).unwrap();
    fs::write(path.join("config.json"), "{\"normalize\":true}").unwrap();
    let values = [0.0f32, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 1.0];
    let bytes: Vec<_> = values.into_iter().flat_map(f32::to_le_bytes).collect();
    let tensor = TensorView::new(Dtype::F32, vec![5, 2], &bytes).unwrap();
    serialize_to_file(
        HashMap::from([("embeddings", tensor)]),
        &None,
        &path.join("model.safetensors"),
    )
    .unwrap();
}

#[test]
fn contextual_focus_keeps_guard_and_assignment_without_changing_navigation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    let source = "struct S { value: usize }\nimpl S {\n fn change(&mut self) {\n  if cat {\n   dog();\n   dog();\n   dog();\n   self.value = feline;\n  }\n }\n}\n";
    fs::write(root.join("lib.rs"), source).unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let engine = |policy| {
        SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_focus(sonar::FocusOptions {
                policy,
                max_lines: 8,
            })
    };
    let options = BriefOptions {
        limit: 1,
        related: 0,
        ..Default::default()
    };
    let window = engine(sonar::FocusPolicy::Window);
    let contextual = engine(sonar::FocusPolicy::Contextual);
    let d = contextual.diagnose("feline action", &options).unwrap();
    assert_eq!(d.focus[0].reason, "contextual_fact");
    let span = d.focus[0].range.unwrap();
    assert!(span[0] <= 4 && span[1] >= 8 && span[1] - span[0] < 8);
    let detail = d.focus[0].contextual.as_ref().unwrap();
    assert_eq!(detail.operation.as_deref(), Some("assignment"));
    assert!(detail.context.iter().any(|c| c.kind == "guard"));
    let baseline = window.diagnose("feline action", &options).unwrap();
    assert_eq!(
        serde_json::to_value(&d.candidates).unwrap(),
        serde_json::to_value(&baseline.candidates).unwrap()
    );
    let mut a = serde_json::to_value(window.brief("feline action", &options).unwrap()).unwrap();
    let mut b = serde_json::to_value(contextual.brief("feline action", &options).unwrap()).unwrap();
    for brief in [&mut a, &mut b] {
        for file in brief["files"].as_array_mut().unwrap() {
            for loc in file["locations"].as_array_mut().unwrap() {
                loc.as_object_mut().unwrap().remove("focus");
            }
        }
    }
    assert_eq!(a, b);
    for q in ["S::change", "lib.rs"] {
        assert_eq!(
            serde_json::to_value(window.brief(q, &options).unwrap()).unwrap(),
            serde_json::to_value(contextual.brief(q, &options).unwrap()).unwrap()
        );
    }
    let no_model = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_focus(sonar::FocusOptions {
            policy: sonar::FocusPolicy::Contextual,
            max_lines: 8,
        });
    assert!(no_model.brief("feline", &options).is_err());
}

#[test]
fn contextual_focus_retains_window_for_bare_initializers_and_stale_sources() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("lib.rs"),
        "fn change() {\n let value = feline;\n dog();\n}\n",
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let options = BriefOptions {
        limit: 1,
        related: 0,
        ..Default::default()
    };
    let engine = |policy| {
        SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_focus(sonar::FocusOptions {
                policy,
                max_lines: 8,
            })
    };
    let a = engine(sonar::FocusPolicy::Window);
    let b = engine(sonar::FocusPolicy::Contextual);
    assert_eq!(
        serde_json::to_value(a.brief("feline", &options).unwrap()).unwrap(),
        serde_json::to_value(b.brief("feline", &options).unwrap()).unwrap()
    );
    let d = b.diagnose("feline", &options).unwrap();
    assert_eq!(d.focus[0].reason, "contextual_bare_value_fallback");
    fs::write(root.join("lib.rs"), "fn replacement() {}\n").unwrap();
    let brief = b.brief("feline", &options).unwrap();
    assert!(brief.starts.is_empty());
    assert_eq!(brief.index.syntax_freshness, "stale");
}

#[test]
fn unified_discovery_measures_semantics_beyond_top_k_and_handles_zero_query_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    let source: String = (0..600)
        .map(|i| format!("fn cat_{i}() {{ cat(); }}\n"))
        .collect();
    fs::write(root.join("cat.rs"), source).unwrap();
    fs::write(
        root.join("z.rs"),
        "fn opaque() { cat(); dog(); let cap_bytes = 65536; }\n",
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    sonar::index_quantities(&db).unwrap();
    let opts = BriefOptions::default();
    let plain = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    assert!(
        !plain
            .diagnose("feline 64 KiB", &opts)
            .unwrap()
            .candidates
            .iter()
            .any(|c| c.symbol.as_deref() == Some("opaque"))
    );
    let unified = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
        .unwrap()
        .with_quantities(true)
        .with_quantity_options(sonar::QuantityOptions {
            query: sonar::QuantityQueryPolicy::Expand,
            ranking: sonar::QuantityRankingPolicy::Unified,
        });
    let d = unified.diagnose("feline 64 KiB", &opts).unwrap();
    let owner = d
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("opaque"))
        .unwrap();
    assert_eq!(owner.embedding_rank, None);
    assert_eq!(owner.rank, 1);
    assert!(!owner.discovery.as_ref().unwrap().original_embedding);
    // The tiny model maps feline/cat to [1,0], dog to [0,1].
    assert!((owner.measurements.as_ref().unwrap().semantic.unwrap() - 0.5f64.sqrt()).abs() < 1e-6);
    let zero = unified.diagnose("mystery 64 KiB", &opts).unwrap();
    assert!(!zero.candidates.is_empty());
    assert!(
        zero.candidates
            .iter()
            .all(|c| c.measurements.as_ref().unwrap().semantic == Some(0.0))
    );
}

#[test]
fn hybrid_channel_coverage_keeps_an_independent_semantic_start() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(root.join("target.rs"), "/// cat\nfn cat() {}\n").unwrap();
    for i in 0..8 {
        fs::write(
            root.join(format!("shared_{i}.rs")),
            format!("/// feline dog\nfn shared_{i}() {{ dog(); }}\n"),
        )
        .unwrap();
    }
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let options = BriefOptions::default();
    let baseline = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    let before = baseline.diagnose("feline", &options).unwrap();
    let target = before
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("cat"))
        .unwrap();
    assert_eq!(target.lexical_rank, None);
    assert_eq!(target.embedding_rank, Some(1));
    assert_eq!(
        target.selected_rank, None,
        "shared weak matches should crowd out the independent leader in the baseline"
    );
    let covered = baseline.with_channel_coverage(true);
    let after = covered.diagnose("feline", &options).unwrap();
    assert!(after.channel_coverage);
    let target = after
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("cat"))
        .unwrap();
    assert_eq!(target.selected_rank, Some(5));
    assert!(target.selected_for_channel_coverage);
    // Selection must not alter discovery, scores, or ranks.
    assert_eq!(before.candidates.len(), after.candidates.len());
    for (a, b) in before.candidates.iter().zip(&after.candidates) {
        assert_eq!(
            (a.location_id, a.rank, a.score),
            (b.location_id, b.rank, b.score)
        );
    }
    let brief = covered.brief("feline", &options).unwrap();
    assert_eq!(brief.starts.len(), 5);
    assert!(
        brief
            .files
            .iter()
            .flat_map(|f| &f.locations)
            .any(|l| l.symbol.as_deref() == Some("cat") && brief.starts.contains(&l.id))
    );
    let one = covered
        .diagnose(
            "feline",
            &BriefOptions {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!one.channel_coverage);
    assert_eq!(
        one.candidates
            .iter()
            .find(|c| c.selected_rank == Some(1))
            .unwrap()
            .location_id,
        before
            .candidates
            .iter()
            .find(|c| c.selected_rank == Some(1))
            .unwrap()
            .location_id
    );
    let exact = covered
        .diagnose(
            "cat",
            &BriefOptions {
                limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        exact
            .candidates
            .iter()
            .find(|c| c.selected_rank == Some(1))
            .unwrap()
            .symbol
            .as_deref(),
        Some("cat")
    );
    let scoped = covered
        .brief(
            "feline",
            &BriefOptions {
                scope: Some("shared_0.rs".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(scoped.files.iter().all(|f| f.path == "shared_0.rs"));
}

#[test]
fn calibrated_variants_keep_zero_vector_positions_and_local_paths_use_one_context() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("i.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    let path = model.join("tokenizer.json");
    let old = Tokenizer::from_file(&path).unwrap();
    let mut vocab = old.get_vocab(true);
    vocab.remove("feline");
    vocab.insert("bytes".into(), 2);
    let mut tokenizer = Tokenizer::new(
        WordLevel::builder()
            .vocab(vocab.into_iter().collect())
            .unk_token("[UNK]".into())
            .build()
            .unwrap(),
    );
    tokenizer.with_pre_tokenizer(Some(Whitespace));
    tokenizer.save(path, false).unwrap();
    fs::write(
        root.join("lib.rs"),
        "fn opaque() { bytes(options.max_bytes.unwrap_or(65536)); }\n",
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    sonar::index_quantities(&db).unwrap();
    let opts = BriefOptions::default();
    let make = |ranking| {
        SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_quantities(true)
            .with_quantity_options(sonar::QuantityOptions {
                query: sonar::QuantityQueryPolicy::Expand,
                ranking,
            })
            .with_variant_scoring(sonar::VariantScoring::Calibrated)
    };
    let d = make(sonar::QuantityRankingPolicy::Unified)
        .diagnose("mystery 64 KiB", &opts)
        .unwrap();
    let c = d
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("opaque"))
        .unwrap();
    let variants = c.measurements.as_ref().unwrap().variants.as_ref().unwrap();
    assert_eq!(variants[0].semantic, Some(0.0));
    assert!(variants[1].semantic.unwrap() > 0.99);
    let local = make(sonar::QuantityRankingPolicy::Local);
    let a = local.diagnose("cat 64 KiB", &opts).unwrap();
    let b = local.diagnose("cat 65536 bytes", &opts).unwrap();
    let evidence = |d: &sonar::RetrievalDiagnostic| {
        d.candidates
            .iter()
            .find(|c| c.symbol.as_deref() == Some("opaque"))
            .unwrap()
            .quantity
            .as_ref()
            .unwrap()
            .local
            .as_ref()
            .unwrap()
            .strength
    };
    assert!((evidence(&a) - 0.5).abs() < 1e-6);
    assert_eq!(evidence(&a), evidence(&b));
}

#[test]
fn bounded_local_uses_semantic_paths_without_replacing_owner_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("i.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("lib.rs"),
        "fn first() { cat(65536); }\nfn second() { dog(65536); }\nfn third() { unknown(65536); }\n",
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    sonar::index_quantities(&db).unwrap();
    let make = |ranking| {
        SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_quantities(true)
            .with_quantity_options(sonar::QuantityOptions {
                ranking,
                ..Default::default()
            })
    };
    let opts = BriefOptions::default();
    let baseline = make(sonar::QuantityRankingPolicy::Unified)
        .diagnose("feline 64 KiB", &opts)
        .unwrap();
    let bounded = make(sonar::QuantityRankingPolicy::BoundedLocal)
        .diagnose("feline 64 KiB", &opts)
        .unwrap();
    let first = bounded
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("first"))
        .unwrap();
    let old = baseline
        .candidates
        .iter()
        .find(|c| c.location_id == first.location_id)
        .unwrap();
    let q = first.quantity.as_ref().unwrap();
    let local = q.local.as_ref().unwrap();
    assert_eq!(local.coverage, 0.0);
    assert!(local.semantic.unwrap() > 0.99);
    assert_eq!(q.unit, "unknown");
    assert!((local.strength - 0.25).abs() < 1e-6);
    let addition = local.contribution.as_ref().unwrap();
    assert_eq!(addition.base_score, old.score);
    assert!(addition.addition > 0.0);
    assert_eq!(first.score, old.score + addition.addition);
    assert_eq!(
        first.measurements.as_ref().unwrap().semantic,
        old.measurements.as_ref().unwrap().semantic
    );
}

#[test]
fn hybrid_channel_coverage_also_preserves_the_lexical_leader() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("literal.rs"),
        format!(
            "/// locate dog\nfn feline() {{ {} }}\n",
            "dog(); ".repeat(2)
        ),
    )
    .unwrap();
    for i in 0..16 {
        fs::write(
            root.join(format!("a_{i:02}.rs")),
            "/// feline\nfn cat() {}\n",
        )
        .unwrap();
    }
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let options = BriefOptions::default();
    let baseline = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    let before = baseline.diagnose("locate feline", &options).unwrap();
    let target = before
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("feline"))
        .unwrap();
    assert_eq!(target.lexical_rank, Some(1));
    assert_eq!(target.selected_rank, None);
    let after = baseline
        .with_channel_coverage(true)
        .diagnose("locate feline", &options)
        .unwrap();
    let target = after
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("feline"))
        .unwrap();
    assert!(target.selected_for_channel_coverage);
    assert_eq!(target.selected_rank, Some(5));
}

#[test]
fn static_pooling_normalization_and_unknown_tokens_are_well_defined() {
    let temp = tempfile::tempdir().unwrap();
    tiny_model(temp.path());
    let model = Embedder::load(temp.path()).unwrap();
    let vectors = model
        .encode(&[
            "cat feline".into(),
            "cat dog".into(),
            "unseen".into(),
            "".into(),
        ])
        .unwrap();
    assert_eq!(vectors[0], vec![1.0, 0.0]);
    assert!((vectors[1][0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    assert_eq!(vectors[2], vec![0.0, 0.0]);
    assert_eq!(vectors[3], vec![0.0, 0.0]);
    assert_eq!(model.dimensions(), 2);
}

#[test]
fn embeddings_find_a_location_without_lexical_overlap_and_keep_the_brief_contract() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.rs"), "fn cat() {}\n").unwrap();
    fs::write(root.join("b.rs"), "fn dog() {}\n").unwrap();
    tiny_model(&model);
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let options = BriefOptions::default();
    assert!(
        sonar::brief(&db, "feline", &options)
            .unwrap()
            .starts
            .is_empty()
    );
    for mode in [RetrievalMode::Embedding, RetrievalMode::Hybrid] {
        let engine = SearchEngine::open(&db, mode, Some(&model)).unwrap();
        let brief = engine.brief("feline", &options).unwrap();
        assert!(
            brief
                .files
                .iter()
                .flat_map(|f| &f.locations)
                .any(|l| l.symbol.as_deref() == Some("cat"))
        );
        assert_eq!(brief.index.semantic_freshness, "unavailable");
        let json = serde_json::to_value(&brief).unwrap();
        assert!(json.get("candidates").is_none());
        let diagnostic = engine.diagnose("feline", &options).unwrap();
        assert!(
            diagnostic
                .candidates
                .iter()
                .all(|c| c.lexical_rank.is_none())
        );
        assert!(
            diagnostic
                .candidates
                .iter()
                .all(|c| c.embedding_rank.is_some())
        );
        let scoped = engine
            .brief(
                "feline",
                &BriefOptions {
                    scope: Some("a.rs".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(scoped.files.iter().all(|f| f.path == "a.rs"));
    }
}

#[test]
fn vectors_are_reused_invalidated_and_bound_to_the_model_and_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.rs"), "fn cat() {}\n").unwrap();
    fs::write(root.join("b.rs"), "fn dog() {}\n").unwrap();
    tiny_model(&model);
    index(&root, &db).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    assert!(engine.brief("cat", &BriefOptions::default()).is_err());
    let first = embed_index(&db, &model).unwrap();
    assert_eq!(first.reused, 0);
    let unchanged = embed_index(&db, &model).unwrap();
    assert_eq!(unchanged.encoded, 0);
    assert_eq!(unchanged.reused, first.regions);
    fs::write(root.join("a.rs"), "fn cat() { dog(); }\n").unwrap();
    let stale = engine.brief("cat", &BriefOptions::default()).unwrap();
    assert_eq!(stale.index.syntax_freshness, "stale");
    assert!(stale.files.iter().all(|file| file.path != "a.rs"));
    index(&root, &db).unwrap();
    assert!(engine.brief("cat", &BriefOptions::default()).is_err());
    let refreshed = embed_index(&db, &model).unwrap();
    assert!(refreshed.encoded > 0 && refreshed.reused > 0);
    fs::write(model.join("config.json"), "{\"normalize\": true}\n").unwrap();
    let changed_model = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    assert!(
        changed_model
            .brief("cat", &BriefOptions::default())
            .is_err()
    );
    let rebuilt = embed_index(&db, &model).unwrap();
    assert_eq!(rebuilt.reused, 0);
    assert_ne!(rebuilt.model_fingerprint, first.model_fingerprint);
    assert!(engine.brief("cat", &BriefOptions::default()).is_err());
    assert!(changed_model.brief("cat", &BriefOptions::default()).is_ok());
}

#[test]
fn readers_keep_the_committed_generation_and_corrupt_vectors_can_be_repaired() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("lib.rs"), "fn cat() {}\n").unwrap();
    tiny_model(&model);
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Embedding, Some(&model)).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let journal: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    conn.execute_batch("BEGIN IMMEDIATE; DELETE FROM embeddings;")
        .unwrap();
    assert!(
        !engine
            .brief("cat", &BriefOptions::default())
            .unwrap()
            .starts
            .is_empty()
    );
    conn.execute_batch("ROLLBACK;").unwrap();
    conn.execute("UPDATE embeddings SET vector=zeroblob(4) WHERE region_id=(SELECT min(region_id) FROM embeddings)", []).unwrap();
    assert!(engine.brief("cat", &BriefOptions::default()).is_err());
    let repaired = embed_index(&db, &model).unwrap();
    assert_eq!(repaired.encoded, 1);
    assert!(engine.brief("cat", &BriefOptions::default()).is_ok());
}

#[test]
fn upgrades_the_lexical_database_and_reconstructs_raw_embedding_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let db = temp.path().join("index.sqlite");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("lib.rs"),
        "/// Natural documentation.\nfn cat() {}\n",
    )
    .unwrap();
    index(&root, &db).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "DROP TABLE semantic_edges; DROP TABLE embeddings;
        DROP INDEX regions_view; ALTER TABLE regions DROP COLUMN view;
        ALTER TABLE locations DROP COLUMN start_byte;
        ALTER TABLE locations DROP COLUMN end_byte;
        ALTER TABLE locations DROP COLUMN name_start;
        ALTER TABLE locations DROP COLUMN name_end;
        ALTER TABLE locations DROP COLUMN is_test;
        ALTER TABLE regions DROP COLUMN embedding_text; PRAGMA user_version=1;
        DELETE FROM metadata WHERE key='extraction_version';",
    )
    .unwrap();
    assert!(sonar::brief(&db, "cat", &BriefOptions::default()).is_err());
    let migrated = index(&root, &db).unwrap();
    assert_eq!(migrated.changed, 1);
    let found: u32 = conn
        .query_row(
            "SELECT count(*) FROM regions WHERE embedding_text LIKE '%Natural documentation.%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(found > 0);
    assert!(sonar::brief(&db, "cat", &BriefOptions::default()).is_ok());
}

#[test]
fn exact_lookup_works_when_the_embedding_query_has_no_known_tokens() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(root.join("lib.rs"), "fn cryptographic_verifier() {}\n").unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Embedding, Some(&model)).unwrap();
    let result = engine
        .diagnose("cryptographic_verifier", &BriefOptions::default())
        .unwrap();
    assert!(result.candidates[0].exact_symbol_match);
    assert_eq!(result.candidates[0].winning_hit.channel, "exact");
}

#[test]
fn semantic_region_is_not_replaced_by_an_incidental_lexical_comment() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    let source = format!(
        "/// dog\nfn locate() {{\n{}\ncat();\n}}",
        "    irrelevant();\n".repeat(90)
    );
    fs::write(
        root.join("lib.rs"),
        source
            + "
/// cat
fn cat() {}
",
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Embedding, Some(&model)).unwrap();
    let diagnostic = engine
        .diagnose("feline feline dog", &BriefOptions::default())
        .unwrap();
    let candidate = diagnostic
        .candidates
        .iter()
        .find(|c| c.symbol.as_deref() == Some("locate"))
        .unwrap();
    assert_eq!(candidate.winning_hit.view, "implementation");
    let response = engine
        .brief("feline feline dog", &BriefOptions::default())
        .unwrap();
    let location = response
        .files
        .iter()
        .flat_map(|f| &f.locations)
        .find(|l| l.symbol.as_deref() == Some("locate"))
        .unwrap();
    assert!(location.focus.unwrap()[0] > 60);
}

#[test]
fn set_selection_keeps_the_same_pool_and_exact_lookup_under_both_policies() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("lib.rs"),
        "fn cat() {}\nfn dog() {}\n/// cat\nfn alternative() {}\n",
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let mut pools = Vec::new();
    for selection in [
        sonar::SelectionPolicy::Baseline,
        sonar::SelectionPolicy::Ranked,
        sonar::SelectionPolicy::Mmr,
        sonar::SelectionPolicy::Coverage,
    ] {
        let engine = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
            .unwrap()
            .with_ranking(sonar::RankingOptions {
                selection,
                ..Default::default()
            });
        let d = engine.diagnose("feline", &BriefOptions::default()).unwrap();
        pools.push(
            d.candidates
                .iter()
                .map(|c| (c.location_id, c.score))
                .collect::<Vec<_>>(),
        );
        let exact = engine.diagnose("cat", &BriefOptions::default()).unwrap();
        assert_eq!(exact.candidates[0].selected_rank, Some(1));
        assert!(exact.candidates[0].exact_symbol_match);
    }
    assert_eq!(pools[0], pools[1]);
    assert_eq!(pools[1], pools[2]);
    assert_eq!(pools[2], pools[3]);
}

#[test]
fn ast_regions_can_be_embedded_and_survive_an_incremental_noop() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    fs::write(
        root.join("lib.rs"),
        format!("fn cat() {{\n{}\n}}", "dog();\n".repeat(90)),
    )
    .unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    sonar::index_with(&root, &db, sonar::Chunking::Ast).unwrap();
    let engine = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model)).unwrap();
    assert!(engine.brief("cat", &BriefOptions::default()).is_err());
    let first = embed_index(&db, &model).unwrap();
    assert!(first.encoded > 0);
    assert!(engine.brief("cat", &BriefOptions::default()).is_ok());
    assert_eq!(
        sonar::index_with(&root, &db, sonar::Chunking::Ast)
            .unwrap()
            .changed,
        0
    );
    let next = embed_index(&db, &model).unwrap();
    assert_eq!(next.encoded, 0);
    assert_eq!(next.reused, first.encoded);
}

#[test]
fn coarse_discovery_preserves_direct_candidates_and_respects_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir_all(root.join("src")).unwrap();
    tiny_model(&model);
    fs::write(root.join("src/lib.rs"), "fn cat() {}\nfn dog() {}\n").unwrap();
    fs::write(root.join("outside.rs"), "fn feline() {}\n").unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let options = BriefOptions {
        scope: Some("src".into()),
        ..Default::default()
    };
    let direct = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
        .unwrap()
        .diagnose("feline", &options)
        .unwrap();
    let coarse = SearchEngine::open(&db, RetrievalMode::Hybrid, Some(&model))
        .unwrap()
        .with_ranking(sonar::RankingOptions {
            coarse: true,
            ..Default::default()
        })
        .diagnose("feline", &options)
        .unwrap();
    assert!(coarse.candidates.iter().any(|c| c.coarse_rank.is_some()));
    assert!(coarse.candidates.iter().all(|c| c.path.starts_with("src/")));
    assert!(direct.candidates.iter().all(|c| {
        coarse
            .candidates
            .iter()
            .any(|other| other.location_id == c.location_id)
    }));
}

#[test]
fn focus_policies_preserve_retrieval_and_semantic_anchor_under_reading_budgets() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    let model = temp.path().join("model");
    let db = temp.path().join("db");
    fs::create_dir(&root).unwrap();
    tiny_model(&model);
    let source = format!(
        "/// dog\nfn locate() {{\n{}\ncat(\n    1,\n    2,\n);\n}}\n/// cat\nfn cat(a: usize, b: usize) {{}}\n",
        "    irrelevant();\n".repeat(90)
    );
    fs::write(root.join("lib.rs"), &source).unwrap();
    index(&root, &db).unwrap();
    embed_index(&db, &model).unwrap();
    let mut pools = Vec::new();
    let mut briefs = Vec::new();
    for policy in [
        sonar::FocusPolicy::Legacy,
        sonar::FocusPolicy::Window,
        sonar::FocusPolicy::Syntax,
        sonar::FocusPolicy::Anchored,
    ] {
        let engine = SearchEngine::open(&db, RetrievalMode::Embedding, Some(&model))
            .unwrap()
            .with_focus(sonar::FocusOptions {
                policy,
                max_lines: 16,
            });
        let d = engine
            .diagnose("feline feline dog", &BriefOptions::default())
            .unwrap();
        pools.push(serde_json::to_value(&d.candidates).unwrap());
        let b = engine
            .brief("feline feline dog", &BriefOptions::default())
            .unwrap();
        for f in &b.files {
            for loc in &f.locations {
                let detail = d
                    .focus
                    .iter()
                    .find(|r| r.path == f.path && r.symbol == loc.symbol)
                    .unwrap();
                assert_eq!(detail.range, loc.focus);
                if policy != sonar::FocusPolicy::Legacy {
                    assert!(detail.lines <= 16 && detail.bytes <= 4096);
                    if let (Some(a), Some(r)) = (detail.anchor, detail.range) {
                        assert!(r[0] <= a[1] && r[1] >= a[0]);
                    }
                }
                if loc.symbol.as_deref() == Some("locate") {
                    assert!(loc.focus.unwrap()[0] > 60);
                }
            }
        }
        let mut value = serde_json::to_value(&b).unwrap();
        for f in value["files"].as_array_mut().unwrap() {
            for loc in f["locations"].as_array_mut().unwrap() {
                loc.as_object_mut().unwrap().remove("focus");
            }
        }
        briefs.push(value);
    }
    assert!(pools.windows(2).all(|w| w[0] == w[1]));
    assert!(briefs.windows(2).all(|w| w[0] == w[1]));
    let mut semantic_only = Vec::new();
    for policy in [sonar::FocusPolicy::Window, sonar::FocusPolicy::Anchored] {
        let engine = SearchEngine::open(&db, RetrievalMode::Embedding, Some(&model))
            .unwrap()
            .with_focus(sonar::FocusOptions {
                policy,
                max_lines: 16,
            });
        semantic_only.push(
            serde_json::to_value(engine.brief("feline", &BriefOptions::default()).unwrap())
                .unwrap(),
        );
        if policy == sonar::FocusPolicy::Anchored {
            let d = engine.diagnose("feline", &BriefOptions::default()).unwrap();
            assert!(
                d.focus
                    .iter()
                    .all(|f| f.reason == "anchored_no_witness" || f.reason == "no_anchor")
            );
        }
    }
    assert_eq!(semantic_only[0], semantic_only[1]);
    let bad = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_focus(sonar::FocusOptions {
            policy: sonar::FocusPolicy::Syntax,
            max_lines: 0,
        });
    assert!(
        bad.brief("cat", &BriefOptions::default())
            .unwrap_err()
            .to_string()
            .contains("focus lines")
    );
}
