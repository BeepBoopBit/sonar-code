use super::*;
use protobuf::Message;
use scip::types::{Document, Index, Metadata, Occurrence as ScipOccurrence, PositionEncoding};
use std::{fs, path::PathBuf};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}
impl Fixture {
    // Explicit reference identities exercise the production importer without guessing by field spelling.
    fn new(source: &str, refs: &[(&str, &str, &str, usize, usize)]) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("lib.rs"), source).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='fields'\nversion='0.1.0'\n",
        )
        .unwrap();
        let db = temp.path().join("index.sqlite");
        let indexed = crate::index(&root, &db).unwrap();
        let conn = crate::store::open(&db, false).unwrap();
        let mut occurrences = vec![];
        let position = |start: usize, end: usize| {
            let prefix = &source[..start];
            let row = prefix.bytes().filter(|b| *b == b'\n').count();
            let column = prefix
                .rsplit_once('\n')
                .map_or(prefix.len(), |(_, s)| s.len());
            vec![row as i32, column as i32, (column + end - start) as i32]
        };
        for row in conn.prepare("SELECT symbol,kind,name_start,name_end FROM locations WHERE kind IN ('field','function')").unwrap().query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u32>(2)? as usize,r.get::<_,u32>(3)? as usize))).unwrap() {
            let (symbol,kind,start,end) = row.unwrap();
            occurrences.push(ScipOccurrence { symbol:format!("fixture {kind} {symbol}"), range:position(start,end), symbol_roles:1, ..Default::default() });
        }
        for &(symbol, kind, needle, offset, len) in refs {
            assert_eq!(
                source.matches(needle).count(),
                1,
                "ambiguous fixture reference {needle}"
            );
            let start = source.find(needle).unwrap() + offset;
            occurrences.push(ScipOccurrence {
                symbol: format!("fixture {kind} {symbol}"),
                range: position(start, start + len),
                ..Default::default()
            });
        }
        let bytes = Index {
            metadata: Some(Metadata {
                project_root: format!("file://{}", root.display()),
                ..Default::default()
            })
            .into(),
            documents: vec![Document {
                relative_path: "lib.rs".into(),
                language: "rust".into(),
                position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                occurrences,
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();
        let manifest = crate::SemanticManifest {
            root: root.to_str().unwrap().into(),
            snapshot: indexed.snapshot,
            inputs: crate::languages::rust::inputs::semantic_inputs(&root).unwrap(),
            analyzer: "fixture".into(),
            toolchain: "fixture".into(),
            config: serde_json::json!({}),
            scip_hash: blake3::hash(&bytes).to_hex().to_string(),
        };
        let scip = temp.path().join("index.scip");
        let mp = temp.path().join("manifest.json");
        fs::write(&scip, bytes).unwrap();
        fs::write(&mp, serde_json::to_vec(&manifest).unwrap()).unwrap();
        crate::import_scip(&db, &scip, &mp).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
    fn handle(&self, symbol: &str, kind: &str) -> String {
        let conn = crate::store::open(&self.db, false).unwrap();
        let id = conn
            .query_row(
                "SELECT id FROM locations WHERE symbol=?1 AND kind=?2",
                params![symbol, kind],
                |r| r.get(0),
            )
            .unwrap();
        explore::handle(&conn, &explore::generation(&conn).unwrap(), id).unwrap()
    }
    fn query(&self, symbol: &str, kind: &str) -> FieldUseResponse {
        field_uses(
            &self.db,
            &self.handle(symbol, kind),
            &FieldUseOptions::default(),
        )
        .unwrap()
    }
}
fn target<'a>(r: &'a FieldUseResponse, handle: &str) -> &'a NavigationLocation {
    if r.anchor.handle == handle {
        &r.anchor
    } else {
        r.locations.iter().find(|l| l.handle == handle).unwrap()
    }
}

#[test]
fn exact_uses_distinguish_access_construction_and_opaque_contexts() {
    let source = "struct S { value: i32 }\nfn change(mut s: S) {\n s.value = 1;\n s.value += 2;\n let a = s.value;\n let b = &s.value;\n let c = &mut s.value;\n s.value.method();\n let d = s.value.child;\n s.value[0] = 2;\n let x = S { value: 3 };\n let S { value: bound } = s;\n let deferred = || s.value;\n assert!(s.value == 1);\n}\n";
    let patterns = [
        "s.value = 1;",
        "s.value +=",
        "s.value;\n let b",
        "s.value;\n let c",
        "s.value;\n s.value.method",
        "s.value.method",
        "s.value.child",
        "s.value[0]",
        "value: 3",
        "value: bound",
        "s.value;\n assert",
        "s.value ==",
    ];
    let refs: Vec<_> = patterns
        .iter()
        .map(|p| {
            (
                "S::value",
                "field",
                *p,
                if p.starts_with("s.") { 2 } else { 0 },
                5,
            )
        })
        .collect();
    let f = Fixture::new(source, &refs);
    let r = f.query("S::value", "field");
    assert_eq!(
        r.uses.iter().map(|u| u.access).collect::<Vec<_>>(),
        vec![
            FieldAccess::Write,
            FieldAccess::ReadWrite,
            FieldAccess::Read,
            FieldAccess::Borrow,
            FieldAccess::BorrowMut,
            FieldAccess::Receiver,
            FieldAccess::Projection,
            FieldAccess::Projection,
            FieldAccess::Initialize,
            FieldAccess::Pattern,
            FieldAccess::Read,
            FieldAccess::Reference
        ]
    );
    assert!(r.uses[10].limitations.contains(&"deferred_body".into()));
    assert!(r.uses[11].limitations.contains(&"opaque_macro".into()));
    let context = r.uses[11].context.as_ref().unwrap();
    assert_eq!(
        &source[context.start_byte..context.end_byte],
        "assert!(s.value == 1)"
    );
    for u in &r.uses {
        let s = &u.evidence.span;
        assert_eq!(&source[s.start_byte..s.end_byte], "value");
        assert_eq!(target(&r, &u.owner).symbol.as_deref(), Some("change"));
    }
}

#[test]
fn immutable_local_connects_distinct_compiler_fields_and_callee_in_both_directions() {
    let source = "struct Input { value: i32 }\nstruct Output { value: i32 }\nfn convert(v:i32)->i32 {v}\nfn bridge(input:Input)->Output {\n // π🙂\n let amount = convert(input.value);\n Output { value: amount }\n}\n";
    let f = Fixture::new(
        source,
        &[
            ("Input::value", "field", "input.value", 6, 5),
            ("Output::value", "field", "value: amount", 0, 5),
            ("convert", "function", "convert(input", 0, 7),
        ],
    );
    let input = f.query("Input::value", "field");
    assert!(
        input.uses[0]
            .associations
            .iter()
            .any(|a| a.relation == "local_binding"
                && target(&input, &a.target).symbol.as_deref() == Some("Output::value"))
    );
    assert!(
        input.uses[0]
            .associations
            .iter()
            .any(|a| a.relation == "call"
                && target(&input, &a.target).symbol.as_deref() == Some("convert"))
    );
    let output = f.query("Output::value", "field");
    assert_eq!(output.uses[0].access, FieldAccess::Initialize);
    assert!(
        output.uses[0]
            .associations
            .iter()
            .any(|a| a.relation == "local_binding"
                && target(&output, &a.target).symbol.as_deref() == Some("Input::value"))
    );
    assert_eq!(f.query("bridge", "function").uses.len(), 2);
    for r in [&input, &output] {
        for a in &r.uses[0].associations {
            if let Some(b) = &a.binding {
                assert_eq!(
                    &source[b.span.start_byte..b.span.end_byte],
                    "let amount = convert(input.value);"
                );
            }
        }
    }
}

#[test]
fn mutation_shadowing_qualified_names_and_deferred_bodies_do_not_make_binding_links() {
    for middle in [
        "let mut amount = input.value;",
        "let amount = input.value; let amount = 0;",
        "let amount = input.value; let later = || Output { value: amount };",
        "let amount = input.value; let later = Output { value: module::amount };",
        "let amount = input.value; macro_call!(Output { value: amount });",
    ] {
        let tail = if middle.contains("later") || middle.contains("macro_call") {
            ""
        } else {
            "Output { value: amount };"
        };
        let source = format!(
            "struct Input {{ value:i32 }}\nstruct Output {{ value:i32 }}\nfn bridge(input:Input) {{ {middle} {tail} }}\n"
        );
        let f = Fixture::new(
            &source,
            &[
                ("Input::value", "field", "input.value", 6, 5),
                ("Output::value", "field", "Output { value: ", 9, 5),
            ],
        );
        let r = f.query("Input::value", "field");
        assert!(
            !r.uses[0]
                .associations
                .iter()
                .any(|a| a.relation == "local_binding"),
            "{middle}"
        );
    }
}

#[test]
fn shorthand_initialization_and_same_expression_keep_compiler_identity() {
    let source = "struct A { value:i32 }\nstruct B { value:i32 }\nfn direct(a:A)->B { B { value:a.value } }\nfn short(value:i32)->B { B { value } }\n";
    let f = Fixture::new(
        source,
        &[
            ("A::value", "field", "a.value", 2, 5),
            ("B::value", "field", "value:a.value", 0, 5),
            ("B::value", "field", "B { value }", 4, 5),
        ],
    );
    let r = f.query("B::value", "field");
    assert_eq!(r.uses.len(), 2);
    assert!(r.uses.iter().all(|u| u.access == FieldAccess::Initialize));
    assert!(
        r.uses[0]
            .associations
            .iter()
            .any(|a| a.relation == "same_expression"
                && target(&r, &a.target).symbol.as_deref() == Some("A::value"))
    );
    assert_eq!(f.query("A::value", "field").uses.len(), 1);
}

#[test]
fn pagination_scope_and_freshness_are_explicit_and_cursor_bound() {
    let source = "struct S { value:i32 }\nfn a(s:S) { let x=s.value; let y=s.value+1; }\n";
    let f = Fixture::new(
        source,
        &[
            ("S::value", "field", "s.value;", 2, 5),
            ("S::value", "field", "s.value+", 2, 5),
        ],
    );
    let handle = f.handle("S::value", "field");
    let mut options = FieldUseOptions {
        limit: 1,
        ..Default::default()
    };
    let first = field_uses(&f.db, &handle, &options).unwrap();
    assert_eq!(first.uses.len(), 1);
    options.cursor = first.next_cursor;
    assert!(field_uses(&f.db, &f.handle("a", "function"), &options).is_err());
    let second = field_uses(&f.db, &handle, &options).unwrap();
    assert_eq!(second.uses.len(), 1);
    assert!(second.next_cursor.is_none());
    assert_ne!(
        first.uses[0].evidence.span.start_byte,
        second.uses[0].evidence.span.start_byte
    );
    options.scope = Some("elsewhere".into());
    assert!(field_uses(&f.db, &handle, &options).is_err());
    options.cursor = None;
    assert!(
        field_uses(&f.db, &handle, &options)
            .unwrap()
            .uses
            .is_empty()
    );
    options.scope = None;
    options.limit = 0;
    assert!(field_uses(&f.db, &handle, &options).is_err());
    options.limit = 20;
    let other = Fixture::new(source, &[("S::value", "field", "s.value;", 2, 5)]);
    assert!(field_uses(&other.db, &handle, &options).is_err());
    fs::write(
        f.root.join("Cargo.toml"),
        "[package]\nname='changed'\nversion='0.1.0'\n",
    )
    .unwrap();
    let stale = field_uses(&f.db, &handle, &options).unwrap();
    assert_eq!(stale.index.semantic_freshness, "stale");
    assert!(stale.uses.is_empty());
    assert_eq!(stale.notices[0].code, "graph_unavailable");
    fs::write(f.root.join("lib.rs"), format!("{source}\n// changed\n")).unwrap();
    assert!(field_uses(&f.db, &handle, &options).is_err());
}

#[test]
fn large_contexts_keep_exact_occurrences_and_report_omitted_associations() {
    let source = format!(
        "struct S {{ value:i32 }}\nfn read(s:S) {{ consume(s.value, \"{}\"); }}\n",
        "π".repeat(2200)
    );
    let f = Fixture::new(&source, &[("S::value", "field", "s.value", 2, 5)]);
    let r = f.query("S::value", "field");
    let u = &r.uses[0];
    assert!(u.context.is_none());
    assert!(u.associations.is_empty());
    assert!(u.limitations.contains(&"context_budget".into()));
    assert!(u.limitations.contains(&"association_unit_budget".into()));
    assert_eq!(
        &source[u.evidence.span.start_byte..u.evidence.span.end_byte],
        "value"
    );
}

#[test]
fn a_field_used_as_an_index_is_read_and_a_callable_field_is_a_receiver() {
    let source = "struct S { value:i32 }\nfn read(s:S) { let x=items[s.value]; s.value(); }\n";
    let f = Fixture::new(
        source,
        &[
            ("S::value", "field", "s.value]", 2, 5),
            ("S::value", "field", "s.value()", 2, 5),
        ],
    );
    let r = f.query("S::value", "field");
    assert_eq!(r.uses[0].access, FieldAccess::Read);
    assert_eq!(r.uses[1].access, FieldAccess::Receiver);
}

#[test]
fn destructuring_assignments_are_writes_and_pattern_fields_not_construction() {
    let source = "struct S { value:i32 }\nfn change(mut s:S) { (s.value,) = (1,); S { value:s.value } = s; let next=S { value:s.value+1 }; }\n";
    let f = Fixture::new(
        source,
        &[
            ("S::value", "field", "s.value,", 2, 5),
            ("S::value", "field", "value:s.value }", 0, 5),
            ("S::value", "field", "s.value } =", 2, 5),
            ("S::value", "field", "value:s.value+", 0, 5),
            ("S::value", "field", "s.value+1", 2, 5),
        ],
    );
    let r = f.query("S::value", "field");
    assert_eq!(
        r.uses.iter().map(|u| u.access).collect::<Vec<_>>(),
        vec![
            FieldAccess::Write,
            FieldAccess::Pattern,
            FieldAccess::Write,
            FieldAccess::Initialize,
            FieldAccess::Read
        ]
    );
}
