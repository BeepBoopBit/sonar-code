use sonar::{
    Direction,
    analysis::{DependencyOptions, DependencyResponse, dependencies},
};
use std::{collections::BTreeSet, fs, path::PathBuf};
struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        for (path, text) in files {
            let file = root.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        let db = temp.path().join("index.sqlite");
        sonar::index(&root, &db).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
    fn query(&self, path: &str) -> DependencyResponse {
        let response = dependencies(&self.db, path, &Default::default()).unwrap();
        for group in &response.relationships {
            for kind in &group.kinds {
                for ev in &kind.evidence {
                    let raw = fs::read(self.root.join(&ev.path)).unwrap();
                    let [a, b] = ev.bytes;
                    assert!(a < b && b <= raw.len());
                    assert_eq!(
                        raw[..a].iter().filter(|c| **c == b'\n').count() + 1,
                        ev.line
                    );
                    assert_eq!(ev.provider, "python-dependencies-v1");
                }
            }
        }
        for dep in &response.unresolved {
            let raw = fs::read(self.root.join(&dep.evidence.path)).unwrap();
            let [a, b] = dep.evidence.bytes;
            assert!(a < b && b <= raw.len());
            assert_eq!(
                raw[..a].iter().filter(|c| **c == b'\n').count() + 1,
                dep.evidence.line
            );
        }
        response
    }
}
fn targets(response: &DependencyResponse) -> BTreeSet<&str> {
    response
        .relationships
        .iter()
        .map(|g| g.to.as_str())
        .collect()
}

#[test]
fn local_imports_and_aliases_link_modules_and_initializers_with_exact_evidence() {
    let f = Fixture::new(&[
        (
            "main.py",
            "# 雪\nimport settings as cfg, pkg.child as kid\nfrom settings import VALUE as V, OTHER\n",
        ),
        ("settings.py", "VALUE = 1\nOTHER = 2\n"),
        ("pkg/__init__.py", ""),
        ("pkg/child.py", ""),
    ]);
    let before = fs::read(&f.db).unwrap();
    let r = f.query("main.py");
    assert_eq!(
        targets(&r),
        BTreeSet::from(["settings.py", "pkg/__init__.py", "pkg/child.py"])
    );
    assert_eq!(r.unresolved_total, 0);
    let group = r
        .relationships
        .iter()
        .find(|g| g.to == "settings.py")
        .unwrap();
    assert_eq!(group.kinds[0].occurrences, 2);
    let source = fs::read_to_string(f.root.join("main.py")).unwrap();
    for ev in &group.kinds[0].evidence {
        assert_eq!(&source[ev.bytes[0]..ev.bytes[1]], "settings");
    }
    let reverse = dependencies(
        &f.db,
        "settings.py",
        &DependencyOptions {
            direction: Direction::Incoming,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(reverse.relationships.iter().any(|g| g.from == "main.py"));
    assert_eq!(fs::read(&f.db).unwrap(), before);
}

#[test]
fn src_layout_and_explicit_relative_imports_use_regular_package_ancestry() {
    let f = Fixture::new(&[
        ("app.py", "import pkg.sub.service\n"),
        ("src/pkg/__init__.py", ""),
        ("src/pkg/core.py", "VALUE = 1\n"),
        ("src/pkg/sub/__init__.py", ""),
        (
            "src/pkg/sub/service.py",
            "from ..core import VALUE\nfrom . import helper as local\n",
        ),
        ("src/pkg/sub/helper.py", ""),
    ]);
    let absolute = f.query("app.py");
    assert!(targets(&absolute).contains("src/pkg/sub/service.py"));
    let relative = f.query("src/pkg/sub/service.py");
    assert!(targets(&relative).contains("src/pkg/core.py"));
    assert!(targets(&relative).contains("src/pkg/sub/helper.py"));
    assert_eq!(relative.unresolved_total, 0);
}

#[test]
fn from_package_imports_do_not_guess_attributes_or_dynamic_exports() {
    let f = Fixture::new(&[
        (
            "app.py",
            "from empty import child as mod\nfrom bound import child\nfrom hooks import child\n",
        ),
        ("empty/__init__.py", "\"Package docs.\"\n"),
        ("empty/child.py", ""),
        ("bound/__init__.py", "child = 7\n"),
        ("bound/child.py", ""),
        (
            "hooks/__init__.py",
            "def __getattr__(name):\n    return 3\n",
        ),
        ("hooks/child.py", ""),
    ]);
    let r = f.query("app.py");
    assert!(targets(&r).contains("empty/child.py"), "{r:#?}");
    assert!(targets(&r).contains("bound/__init__.py"));
    assert!(!targets(&r).contains("bound/child.py"));
    assert!(!targets(&r).contains("hooks/child.py"));
    assert_eq!(
        r.unresolved
            .iter()
            .filter(|d| d.reason == "package_attribute_or_submodule")
            .count(),
        2
    );
}

#[test]
fn ambiguous_roots_layouts_namespace_stubs_and_invalid_relatives_are_explicit() {
    let f = Fixture::new(&[
        (
            "app.py",
            "import duplicate, conflict, ns.child, stubs, missing\nfrom . import sibling\n",
        ),
        ("duplicate.py", ""),
        ("src/duplicate.py", ""),
        ("conflict.py", ""),
        ("conflict/__init__.py", ""),
        ("ns/child.py", ""),
        ("stubs.pyi", "VALUE: int\n"),
        ("pkg/__init__.py", ""),
        ("pkg/over.py", "from .. import missing\n"),
    ]);
    let r = f.query("app.py");
    assert!(r.relationships.is_empty());
    let reasons = r
        .unresolved
        .iter()
        .map(|d| d.reason.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        reasons,
        BTreeSet::from([
            "ambiguous_import_roots",
            "ambiguous_module_target",
            "namespace_package_layout_unsupported",
            "stub_only_target",
            "external_or_not_indexed",
            "relative_package_context_unavailable"
        ])
    );
    assert!(
        f.query("pkg/over.py")
            .unresolved
            .iter()
            .any(|d| d.reason == "relative_import_beyond_package")
    );
}

#[test]
fn wildcards_and_dynamic_calls_are_gaps_and_comments_strings_are_not_imports() {
    let f = Fixture::new(&[
        (
            "app.py",
            "from __future__ import annotations\nfrom settings import *\nimport importlib as loader\nfrom importlib import import_module as load\nloader.import_module('settings')\nload('settings')\n__import__('settings')\n# import false_module\ntext = 'import fake'\n",
        ),
        ("settings.py", "VALUE = 1\n"),
    ]);
    let r = f.query("app.py");
    assert_eq!(targets(&r), BTreeSet::from(["settings.py"]));
    assert_eq!(
        r.unresolved
            .iter()
            .filter(|d| d.reason == "dynamic_import_unsupported")
            .count(),
        3
    );
    assert_eq!(
        r.unresolved
            .iter()
            .filter(|d| d.reason == "wildcard_expansion_unavailable")
            .count(),
        1
    );
    assert!(r.unresolved.iter().all(|d| !d.target.contains("fake")
        && !d.target.contains("false_module")
        && !d.target.contains("__future__")));
}

#[test]
fn parse_errors_stale_sources_and_provider_upgrades_require_explicit_refresh() {
    let f = Fixture::new(&[
        ("app.py", "from settings import VALUE\n"),
        ("settings.py", "VALUE = 1\n"),
        ("broken.py", "def (:\nimport settings"),
    ]);
    let r = f.query("app.py");
    let coverage = r.coverage.iter().find(|c| c.language == "python").unwrap();
    assert_eq!(coverage.skipped_files, vec!["broken.py"]);
    assert!(
        !r.notices
            .iter()
            .any(|n| n.code == "syntax_dependencies_unavailable" && n.message.contains("python"))
    );
    let conn = rusqlite::Connection::open(&f.db).unwrap();
    conn.execute(
        "DELETE FROM analysis_batches WHERE provider='python-dependencies-v1'",
        [],
    )
    .unwrap();
    drop(conn);
    assert!(
        dependencies(&f.db, "app.py", &Default::default())
            .unwrap_err()
            .to_string()
            .contains("providers changed")
    );
    assert_eq!(sonar::index(&f.root, &f.db).unwrap().changed, 0);
    assert_eq!(f.query("app.py").relationships.len(), 1);
    fs::remove_file(f.root.join("settings.py")).unwrap();
    assert!(dependencies(&f.db, "app.py", &Default::default()).is_err());
    sonar::index(&f.root, &f.db).unwrap();
    assert!(
        f.query("app.py")
            .unresolved
            .iter()
            .any(|d| d.reason == "external_or_not_indexed")
    );
}

#[test]
fn root_shadowing_and_stub_companions_never_invent_a_runtime_package() {
    let f = Fixture::new(&[
        ("app.py", "import pkg.child\nimport real\n"),
        ("pkg.py", ""),
        ("src/pkg/__init__.py", ""),
        ("src/pkg/child.py", ""),
        ("real.py", "VALUE = 1\n"),
        ("real.pyi", "VALUE: int\n"),
    ]);
    let r = f.query("app.py");
    assert_eq!(targets(&r), BTreeSet::from(["real.py"]));
    assert!(
        r.unresolved
            .iter()
            .any(|d| d.reason == "ambiguous_import_roots")
    );
}

#[test]
fn multiline_unicode_and_conditional_imports_keep_original_module_spans() {
    let f = Fixture::new(&[
        (
            "app.py",
            "from café import (\n    VALUE as v,\n    OTHER,\n)\nif enabled:\n    import pkg . child as loaded\ndef lazy():\n    from café import VALUE\n",
        ),
        ("café.py", "VALUE = 1\nOTHER = 2\n"),
        ("pkg/__init__.py", ""),
        ("pkg/child.py", ""),
    ]);
    let r = f.query("app.py");
    assert_eq!(r.unresolved_total, 0);
    assert_eq!(
        targets(&r),
        BTreeSet::from(["café.py", "pkg/__init__.py", "pkg/child.py"])
    );
    assert_eq!(
        r.relationships
            .iter()
            .find(|g| g.to == "café.py")
            .unwrap()
            .kinds[0]
            .occurrences,
        2
    );
}
