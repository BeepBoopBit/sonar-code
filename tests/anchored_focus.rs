use sonar::{BriefOptions, FocusOptions, FocusPolicy, RetrievalMode, SearchEngine};
use std::fs;

#[test]
fn anchored_focus_keeps_every_visible_match_and_default_navigation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    fs::create_dir(&root).unwrap();
    let source = format!(
        "fn dispatch() {{\n{}    if permission.denied() {{\n        report(\"permission denied\");\n        cleanup();\n        return;\n    }}\n{}\n}}\n",
        "    prepare();\n".repeat(12),
        "    finish();\n".repeat(20)
    );
    fs::write(root.join("lib.rs"), &source).unwrap();
    fs::write(root.join("guide.md"), "# Permissions\npermission denied\n").unwrap();
    fs::write(
        root.join("profiles.yaml"),
        "first:\n  error: permission denied\nsecond:\n  error: trust folder\n",
    )
    .unwrap();
    let db = temp.path().join("db");
    sonar::index_with_resources(
        &root,
        &db,
        sonar::Chunking::Lines,
        sonar::ResourcePolicy::Yaml,
    )
    .unwrap();
    let engine = |policy| {
        SearchEngine::open(&db, RetrievalMode::Lexical, None)
            .unwrap()
            .with_focus(FocusOptions {
                policy,
                max_lines: 16,
            })
    };
    let opts = BriefOptions::default();
    let q = "permission denied";
    let window = engine(FocusPolicy::Window);
    let anchored = engine(FocusPolicy::Anchored);
    let before = window.diagnose(q, &opts).unwrap();
    let after = anchored.diagnose(q, &opts).unwrap();
    assert_eq!(
        serde_json::to_value(&before.candidates).unwrap(),
        serde_json::to_value(&after.candidates).unwrap()
    );
    for f in &after.focus {
        let old = before
            .focus
            .iter()
            .find(|v| v.location_id == f.location_id)
            .unwrap();
        assert!(f.lines <= 16 && f.bytes <= 4096);
        if let Some(p) = &f.protected {
            assert_eq!(p.window, old.range);
            if let Some(r) = f.range {
                assert!(p.lines.iter().all(|l| r[0] <= *l && *l <= r[1]));
            } else {
                assert!(p.lines.is_empty());
            }
        }
        if f.path != "lib.rs" {
            assert_eq!(old.range, f.range);
        }
    }
    let mut briefs = Vec::new();
    for e in [&window, &anchored] {
        let mut b = serde_json::to_value(e.brief(q, &opts).unwrap()).unwrap();
        for f in b["files"].as_array_mut().unwrap() {
            for l in f["locations"].as_array_mut().unwrap() {
                l.as_object_mut().unwrap().remove("focus");
            }
        }
        briefs.push(b);
    }
    assert_eq!(briefs[0], briefs[1]);
    assert_eq!(
        serde_json::to_value(anchored.brief(q, &opts).unwrap()).unwrap(),
        serde_json::to_value(anchored.brief(q, &opts).unwrap()).unwrap()
    );
    fs::write(root.join("lib.rs"), "fn replacement() {}\n").unwrap();
    assert!(
        anchored
            .brief(q, &opts)
            .unwrap()
            .files
            .iter()
            .all(|f| f.path != "lib.rs")
    );
}

#[test]
fn anchored_related_focus_retains_the_verified_link_site() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("repo");
    fs::create_dir(&root).unwrap();
    let db = t.path().join("db");
    fs::write(
        root.join("guide.md"),
        format!(
            "# Guide\nworker\n{}[details](worker.yaml)\n",
            "background\n".repeat(45)
        ),
    )
    .unwrap();
    fs::write(root.join("worker.yaml"), "name: worker\nmode: worker\n").unwrap();
    sonar::index_with_resources(
        &root,
        &db,
        sonar::Chunking::Lines,
        sonar::ResourcePolicy::Yaml,
    )
    .unwrap();
    sonar::index_navigation(&db).unwrap();
    let e = SearchEngine::open(&db, RetrievalMode::Lexical, None)
        .unwrap()
        .with_navigation(sonar::NavigationPolicy::Owners)
        .with_ranking(sonar::RankingOptions {
            selection: sonar::SelectionPolicy::Ranked,
            ..Default::default()
        })
        .with_focus(FocusOptions {
            policy: FocusPolicy::Anchored,
            max_lines: 16,
        });
    let opts = BriefOptions {
        limit: 1,
        ..Default::default()
    };
    let d = e.diagnose("explain worker", &opts).unwrap();
    let f = d.focus.iter().find(|f| f.path == "guide.md").unwrap();
    let p = f.protected.as_ref().unwrap();
    assert!(p.lines.contains(&48));
    let range = f.range.unwrap();
    assert!(range[0] <= 48 && 48 <= range[1]);
    assert!(f.lines <= 16 && f.bytes <= 4096);
}
