use super::*;
use crate::{BriefOptions, NavigationPolicy, RankingOptions, RetrievalMode, SearchEngine};
use std::{fs, path::PathBuf};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    conn: Connection,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join("inner")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("outer.rs"),
            "struct Other { token: u64 }\nfn outside() {}\n",
        )
        .unwrap();
        let mut source =
            String::from("// π🙂\nstruct S { token: u64, limit: u64 }\nenum Action { Ready }\n");
        for i in 0..80 {
            source.push_str(&format!(
                "fn user_{i}(s: S) {{ let _ = s.token; let _ = s.token; let _ = s.limit; let _ = Action::Ready; }}\n"
            ));
        }
        fs::write(root.join("inner/lib.rs"), source).unwrap();
        let db = temp.path().join("index.sqlite");
        let index = crate::index(&root, &db).unwrap();
        let conn = crate::store::open(&db, true).unwrap();
        // Explicit graph fixtures isolate ranking from SCIP extraction. The repository
        // validation separately checks this policy against a real imported SCIP graph.
        for (key, value) in [
            ("semantic_snapshot", index.snapshot),
            ("semantic_policy", "sonar-scip-utf8-source-calls-v1".into()),
            (
                "semantic_inputs",
                crate::languages::rust::inputs::semantic_inputs(&root).unwrap(),
            ),
            ("semantic_profile", "fixture".into()),
        ] {
            conn.execute(
                "INSERT OR REPLACE INTO metadata VALUES (?1,?2)",
                params![key, value],
            )
            .unwrap();
        }
        Self {
            _temp: temp,
            root,
            db,
            conn,
        }
    }

    fn candidate(&self, symbol: &str, score: f64) -> Candidate {
        let id = self
            .conn
            .query_row("SELECT id FROM locations WHERE symbol=?1", [symbol], |r| {
                r.get(0)
            })
            .unwrap();
        let mut c = crate::search::load_parent(&self.conn, id).unwrap();
        c.score = score;
        c
    }

    fn reference(&self, user: usize, field: &str, occurrence: usize) {
        let from = self.candidate(&format!("user_{user}"), 0.0).id;
        let to = self.candidate(field, 0.0).id;
        let source = fs::read_to_string(self.root.join("inner/lib.rs")).unwrap();
        let name = field.rsplit("::").next().unwrap();
        let start = source.find(&format!("fn user_{user}(")).unwrap();
        let start = start
            + source[start..]
                .match_indices(name)
                .nth(occurrence)
                .unwrap()
                .0;
        let line = source[..start].bytes().filter(|b| *b == b'\n').count() + 1;
        self.conn
            .execute(
                "INSERT INTO semantic_edges VALUES (?1,?2,'references','inner/lib.rs',?3,?4,?5)",
                params![
                    from,
                    to,
                    line as i64,
                    start as i64,
                    (start + name.len()) as i64
                ],
            )
            .unwrap();
    }

    fn engine(&self, policy: NavigationPolicy) -> SearchEngine {
        SearchEngine::open(&self.db, RetrievalMode::Lexical, None)
            .unwrap()
            .with_ranking(RankingOptions {
                selection: crate::SelectionPolicy::Ranked,
                ..Default::default()
            })
            .with_navigation(policy)
    }
}

#[test]
fn only_recorded_identity_supports_users_and_repeated_fields_do_not_add_votes() {
    let f = Fixture::new();
    f.reference(0, "S::token", 0);
    f.reference(0, "S::token", 1);
    f.reference(0, "S::limit", 0);
    let original = vec![
        f.candidate("Other::token", 1.0),
        f.candidate("S::token", 0.9),
        f.candidate("S::limit", 0.8),
        f.candidate("user_1", 0.7),
        f.candidate("user_0", 0.6),
    ];
    let mut cs = original.clone();
    let d = apply(&f.conn, &mut cs, true).unwrap();
    assert_eq!(d.channel.len(), 1);
    let c = &d.channel[0];
    assert_eq!(c.location_id, original[4].id);
    assert_eq!(c.reference_support.as_ref().unwrap().seed, original[1].id);
    assert_eq!(c.reference_support.as_ref().unwrap().users, 1);
    assert!((c.addition - 0.45).abs() < 1e-12);
    assert_eq!(cs[0].id, original[4].id);
    assert_eq!(cs.len(), original.len());
    let edge = &d.edges[0];
    let source = fs::read_to_string(f.root.join(&edge.path)).unwrap();
    assert_eq!(&source[edge.bytes[0]..edge.bytes[1]], "token");
    let mut again = original;
    assert_eq!(
        serde_json::to_value(d).unwrap(),
        serde_json::to_value(apply(&f.conn, &mut again, true).unwrap()).unwrap()
    );
}

#[test]
fn entire_recorded_degree_is_used_and_high_degree_seeds_are_skipped() {
    let f = Fixture::new();
    for i in 0..64 {
        f.reference(i, "S::token", 0);
    }
    let original = vec![f.candidate("S::token", 1.0), f.candidate("user_0", 0.5)];
    let mut cs = original.clone();
    let d = apply(&f.conn, &mut cs, true).unwrap();
    assert_eq!(d.channel[0].reference_support.as_ref().unwrap().users, 64);
    assert_eq!(d.channel[0].addition, 0.0625);
    f.reference(64, "S::token", 0);
    let d = apply(&f.conn, &mut cs, true).unwrap();
    assert!(d.truncated);
    assert!(d.channel.is_empty());
    assert_eq!(d.status, "inactive");
}

#[test]
fn pool_and_seed_limits_are_applied_before_reference_support() {
    let f = Fixture::new();
    f.reference(79, "S::token", 0);
    let mut cs = vec![f.candidate("S::token", 1.0)];
    for i in 0..64 {
        cs.push(f.candidate(&format!("user_{i}"), 0.9));
    }
    cs.push(f.candidate("user_79", 0.1));
    assert!(apply(&f.conn, &mut cs, true).unwrap().channel.is_empty());
    let field = cs.remove(0);
    cs.insert(8, field);
    assert!(apply(&f.conn, &mut cs, true).unwrap().seeds.is_empty());
    cs = vec![
        f.candidate("Other::token", 1.0),
        f.candidate("user_79", 0.9),
    ];
    assert!(apply(&f.conn, &mut cs, true).unwrap().channel.is_empty());
}

#[test]
fn variants_support_recorded_references_and_calls_do_not_substitute() {
    let f = Fixture::new();
    f.reference(0, "Action::Ready", 0);
    let original = vec![
        f.candidate("Action::Ready", 1.0),
        f.candidate("user_0", 0.9),
    ];
    let mut cs = original.clone();
    assert_eq!(apply(&f.conn, &mut cs, true).unwrap().channel.len(), 1);
    f.conn
        .execute("UPDATE semantic_edges SET kind='calls'", [])
        .unwrap();
    let mut cs = original;
    assert!(apply(&f.conn, &mut cs, true).unwrap().channel.is_empty());
}

#[test]
fn exact_queries_and_unavailable_graph_preserve_scores_and_order() {
    let f = Fixture::new();
    f.reference(0, "S::token", 0);
    for exact in [false, true] {
        let mut cs = vec![f.candidate("S::token", 1.0), f.candidate("user_0", 0.9)];
        cs[0].exact_symbol_match = exact;
        let before: Vec<_> = cs.iter().map(|c| (c.id, c.score)).collect();
        let d = apply(&f.conn, &mut cs, exact).unwrap();
        assert_eq!(d.status, if exact { "exact_bypass" } else { "unavailable" });
        assert_eq!(
            before,
            cs.iter().map(|c| (c.id, c.score)).collect::<Vec<_>>()
        );
        assert!(d.edges.is_empty());
    }
}

#[test]
fn runtime_honors_scope_kind_exact_identity_and_missing_or_stale_analysis() {
    let f = Fixture::new();
    f.reference(0, "S::token", 0);
    let options = BriefOptions {
        scope: Some("inner".into()),
        ..Default::default()
    };
    let uses = f.engine(NavigationPolicy::Uses);
    let off = f.engine(NavigationPolicy::Off);
    assert!(
        f.engine(NavigationPolicy::Uses)
            .with_quantities(true)
            .brief("token access", &options)
            .unwrap_err()
            .to_string()
            .contains("quantities disabled")
    );
    let d = uses.diagnose("token access", &options).unwrap();
    assert_eq!(d.navigation.as_ref().unwrap().status, "active");
    assert!(d.candidates.iter().all(|c| c.path.starts_with("inner/")));
    assert!(
        !d.candidates
            .iter()
            .any(|c| c.symbol.as_deref() == Some("Other::token"))
    );
    for query in ["S::token", "inner/lib.rs"] {
        assert_eq!(
            serde_json::to_value(off.brief(query, &options).unwrap()).unwrap(),
            serde_json::to_value(uses.brief(query, &options).unwrap()).unwrap()
        );
    }
    let d = f
        .engine(NavigationPolicy::Uses)
        .with_kind(Some("field".into()))
        .diagnose("token access", &options)
        .unwrap();
    assert!(d.candidates.iter().all(|c| c.kind == "field"));
    assert!(d.navigation.unwrap().channel.is_empty());
    let brief = uses.brief("token access", &options).unwrap();
    assert!(brief.links.iter().any(|e| e.kind == "references"));
    fs::write(
        f.root.join("Cargo.toml"),
        "[package]\nname='changed'\nversion='0.1.0'\n",
    )
    .unwrap();
    let d = uses.diagnose("token access", &options).unwrap();
    assert_eq!(d.navigation.unwrap().status, "unavailable");
    f.conn
        .execute("DELETE FROM metadata WHERE key LIKE 'semantic_%'", [])
        .unwrap();
    let d = uses.diagnose("token access", &options).unwrap();
    assert_eq!(d.navigation.unwrap().status, "unavailable");
    assert_eq!(
        off.diagnose("token access", &options)
            .unwrap()
            .candidates
            .iter()
            .map(|c| (c.location_id, c.score))
            .collect::<Vec<_>>(),
        d.candidates
            .iter()
            .map(|c| (c.location_id, c.score))
            .collect::<Vec<_>>()
    );
}
