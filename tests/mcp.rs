use std::{
    collections::BTreeSet, fs, future::Future, path::PathBuf, process::Stdio, time::Duration,
};

use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("lib.rs"),
            "struct Settings { count: usize }\nfn policy() { helper(); }\nfn helper() {}\n",
        )
        .unwrap();
        fs::write(
            root.join("guide.md"),
            "# Policy\n[implementation](lib.rs#L2)\n",
        )
        .unwrap();
        let db = temp.path().join("index.sqlite");
        sonar::index(&root, &db).unwrap();
        sonar::index_navigation(&db).unwrap();
        Self {
            _temp: temp,
            root,
            db,
        }
    }
}

fn run(future: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future);
}

struct Client {
    child: Child,
    input: Option<ChildStdin>,
    output: Lines<BufReader<ChildStdout>>,
    modern: bool,
}

impl Client {
    async fn start(f: &Fixture, modern: bool) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sonar"))
            .arg("--db")
            .arg(&f.db)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut client = Self {
            child,
            input,
            output,
            modern,
        };
        if modern {
            let discover = client.request(0, "server/discover", json!({})).await;
            assert!(
                discover["result"]["supportedVersions"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("2026-07-28"))
            );
            assert!(discover["result"]["capabilities"]["tools"].is_object());
        } else {
            let init = client.request(0, "initialize", json!({"protocolVersion":"2025-11-25", "clientInfo":{"name":"sonar-tests", "version":"1"},"capabilities":{}})).await;
            assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
            assert_eq!(init["result"]["serverInfo"]["name"], "sonar");
            client
                .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await;
        }
        client
    }

    fn frame(&self, id: u64, method: &str, mut params: Value) -> Value {
        if self.modern {
            params["_meta"] = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28", "io.modelcontextprotocol/clientInfo":{"name":"sonar-tests","version":"1"}, "io.modelcontextprotocol/clientCapabilities":{}});
        }
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    }

    async fn send(&mut self, value: Value) {
        let input = self.input.as_mut().unwrap();
        input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        input.flush().await.unwrap();
    }

    async fn receive(&mut self) -> Value {
        let line = timeout(Duration::from_secs(10), self.output.next_line())
            .await
            .expect("MCP response timed out")
            .unwrap()
            .expect("server closed stdout");
        serde_json::from_str(&line).expect("stdout must contain only MCP JSON frames")
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(self.frame(id, method, params)).await;
        let response = self.receive().await;
        assert_eq!(response["id"], id, "{response}");
        response
    }

    async fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(id, "tools/call", json!({"name":name,"arguments":arguments}))
            .await
    }

    async fn close(mut self) {
        drop(self.input.take());
        let status = timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("server did not exit after stdin EOF")
            .unwrap();
        assert!(status.success(), "server exit: {status}");
    }
}

fn content(response: &Value) -> Value {
    assert!(response.get("error").is_none(), "{response}");
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{response}");
    let text: Value = serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(text, result["structuredContent"]);
    text
}

#[test]
fn python_navigation_and_language_coverage_are_available_over_the_same_mcp_tools() {
    run(async {
        let f = Fixture::new();
        fs::write(f.root.join("client.py"),
            "class Client:\n    def paginate(self, cursor):\n        \"\"\"Reject expired pagination cursors.\"\"\"\n        return cursor\n\ndef shared_token(): pass\n").unwrap();
        fs::write(f.root.join("same.rs"), "fn shared_token() {}\n").unwrap();
        fs::write(
            f.root.join("same.ts"),
            "export const shared_token = () => 1\n",
        )
        .unwrap();
        sonar::index(&f.root, &f.db).unwrap();
        sonar::index_navigation(&f.db).unwrap();
        let mut c = Client::start(&f, true).await;
        let status = content(&c.call(1, "sonar_status", json!({})).await);
        let languages = status["index"]["languages"].as_array().unwrap();
        assert_eq!(languages.len(), 3);
        assert!(
            languages
                .iter()
                .any(|l| l["language"] == "python" && l["semantic_freshness"] == "unsupported")
        );
        let args = json!({"query":"expired pagination cursors","scope":"client.py"});
        let brief = content(&c.call(2, "sonar_brief", args.clone()).await);
        assert!(brief.to_string().contains("Client.paginate"));
        let investigation = content(&c.call(3, "sonar_investigate", args).await);
        let expected = sonar::Investigator::open(&f.db, None)
            .unwrap()
            .investigate(
                "expired pagination cursors",
                &sonar::InvestigationOptions {
                    scope: Some("client.py".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(investigation, serde_json::to_value(expected).unwrap());
        let located = content(
            &c.call(4, "sonar_locate", json!({"path":"client.py","line":4}))
                .await,
        );
        let handle = &located["locations"][0]["handle"];
        let followed = content(
            &c.call(5, "sonar_follow", json!({"handle":handle,"query":"cursor"}))
                .await,
        );
        assert!(followed["matches"].as_array().unwrap().is_empty());
        assert!(
            followed["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["code"] == "language_analysis_unavailable")
        );
        let outline = content(
            &c.call(6, "sonar_outline", json!({"path":"client.py"}))
                .await,
        );
        assert!(
            outline["locations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["kind"] == "class")
        );
        assert!(
            outline["locations"]
                .as_array()
                .unwrap()
                .iter()
                .all(|l| l["language"] == "python")
        );
        let mixed = content(
            &c.call(7, "sonar_brief", json!({"query":"shared_token"}))
                .await,
        );
        let starts = mixed["starts"].as_array().unwrap();
        let languages: BTreeSet<_> = mixed["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| {
                f["locations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|l| starts.contains(&l["id"]))
            })
            .map(|f| f["language"].as_str().unwrap())
            .collect();
        assert_eq!(
            languages,
            ["python", "rust", "typescript"].into_iter().collect()
        );
        c.close().await;
    });
}

#[test]
fn typescript_tsx_navigation_uses_the_existing_mcp_contract() {
    run(async {
        let f = Fixture::new();
        fs::write(f.root.join("client.tsx"), "/** Render an expired pagination cursor. */\nexport const Badge = ({cursor}: {cursor: string}) => <span>{cursor}</span>\n").unwrap();
        fs::write(
            f.root.join("guide.md"),
            "# Cursor\n[render](client.tsx#L2)\n",
        )
        .unwrap();
        sonar::index(&f.root, &f.db).unwrap();
        sonar::index_navigation(&f.db).unwrap();
        let before = fs::read(&f.db).unwrap();
        let mut c = Client::start(&f, true).await;
        let status = content(&c.call(1, "sonar_status", json!({})).await);
        assert!(
            status["index"]["languages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["language"] == "typescript"
                    && l["syntax_freshness"] == "current"
                    && l["semantic_freshness"] == "unavailable")
        );
        let brief = content(&c.call(2, "sonar_brief", json!({"query":"Badge"})).await);
        assert!(brief.to_string().contains("Badge"));
        let located = content(
            &c.call(3, "sonar_locate", json!({"path":"client.tsx","line":2}))
                .await,
        );
        let handle = &located["locations"][0]["handle"];
        assert_eq!(located["locations"][0]["symbol"], "Badge");
        assert_eq!(located["locations"][0]["language"], "typescript");
        let outline = content(
            &c.call(4, "sonar_outline", json!({"path":"client.tsx"}))
                .await,
        );
        assert_eq!(
            outline["locations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|l| l["kind"] == "function")
                .count(),
            1
        );
        let args = json!({"query":"expired pagination cursor","from":handle});
        let context = content(&c.call(5, "sonar_investigate", args).await);
        let expected = sonar::Investigator::open(&f.db, None)
            .unwrap()
            .investigate(
                "expired pagination cursor",
                &sonar::InvestigationOptions {
                    from: Some(handle.as_str().unwrap().into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(context, serde_json::to_value(expected).unwrap());
        assert!(
            context["links"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["kind"] == "documents")
        );
        for (id, tool, args) in [
            (6, "sonar_neighbors", json!({"handle":handle})),
            (7, "sonar_follow", json!({"handle":handle,"query":"cursor"})),
            (8, "sonar_field_uses", json!({"handle":handle})),
        ] {
            let response = content(&c.call(id, tool, args).await);
            assert!(
                response["notices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["code"] == "graph_unavailable"),
                "{response}"
            );
        }
        c.close().await;
        assert_eq!(fs::read(&f.db).unwrap(), before);
    });
}

#[test]
fn mcp_status_and_briefs_detect_languages_omitted_by_a_legacy_index() {
    run(async {
        let f = Fixture::new();
        let conn = rusqlite::Connection::open(&f.db).unwrap();
        conn.execute("DELETE FROM metadata WHERE key='languages'", [])
            .unwrap();
        fs::write(f.root.join("new.py"), "def policy(): pass\n").unwrap();
        let before = fs::read(&f.db).unwrap();
        let mut c = Client::start(&f, true).await;
        for (id, tool, args) in [
            (1, "sonar_status", json!({})),
            (2, "sonar_brief", json!({"query":"policy"})),
        ] {
            let response = content(&c.call(id, tool, args).await);
            let python = response["index"]["languages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["language"] == "python")
                .unwrap();
            assert_eq!(python["files"], 0);
            assert_eq!(python["detected_files"], 1);
            assert_eq!(python["unindexed_files"], 1);
            assert_eq!(python["syntax_freshness"], "unindexed");
            assert!(
                response["notices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["code"] == "unindexed_language")
            );
        }
        c.close().await;
        assert_eq!(fs::read(&f.db).unwrap(), before);
    });
}

#[test]
fn legacy_stdio_exposes_read_only_schemas_and_matches_navigation_api() {
    run(async {
        let f = Fixture::new();
        let original_db = fs::read(&f.db).unwrap();
        let mut c = Client::start(&f, false).await;
        let list = c.request(1, "tools/list", json!({})).await;
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 8);
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert_eq!(tool["annotations"]["destructiveHint"], false);
            assert_eq!(tool["annotations"]["openWorldHint"], false);
        }
        let status = content(&c.call(2, "sonar_status", json!({})).await);
        assert_eq!(status["root"], f.root.to_str().unwrap());
        assert_eq!(status["retrieval"], "lexical");
        assert_eq!(status["index"]["syntax_freshness"], "current");
        assert_eq!(status["index"]["semantic_freshness"], "unavailable");
        let brief = content(
            &c.call(
                3,
                "sonar_brief",
                json!({"query":"policy","exclude":["guide.md"]}),
            )
            .await,
        );
        let expected = sonar::SearchEngine::open_navigation(&f.db, None)
            .unwrap()
            .with_exclusions(vec!["guide.md".into()])
            .brief("policy", &Default::default())
            .unwrap();
        assert_eq!(brief, serde_json::to_value(expected).unwrap());
        let investigation = content(
            &c.call(4, "sonar_investigate", json!({"query":"explain policy"}))
                .await,
        );
        assert_eq!(
            investigation,
            serde_json::to_value(
                sonar::Investigator::open(&f.db, None)
                    .unwrap()
                    .investigate("explain policy", &Default::default())
                    .unwrap()
            )
            .unwrap()
        );
        let located = content(
            &c.call(5, "sonar_locate", json!({"path":"lib.rs","line":2}))
                .await,
        );
        assert_eq!(
            located,
            serde_json::to_value(sonar::locate(&f.db, "lib.rs", 2, None).unwrap()).unwrap()
        );
        let handle = located["locations"][0]["handle"].as_str().unwrap();
        let neighbors = content(
            &c.call(
                6,
                "sonar_neighbors",
                json!({"handle":handle,"relation":"documents"}),
            )
            .await,
        );
        assert_eq!(neighbors["links"].as_array().unwrap().len(), 1);
        let follow = content(
            &c.call(7, "sonar_follow", json!({"handle":handle,"query":"helper"}))
                .await,
        );
        assert!(
            follow["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["code"] == "graph_unavailable")
        );
        let field = sonar::locate(&f.db, "lib.rs", 1, Some(19))
            .unwrap()
            .locations
            .remove(0)
            .handle;
        let uses = content(&c.call(8, "sonar_field_uses", json!({"handle":field})).await);
        assert!(
            uses["notices"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["code"] == "graph_unavailable")
        );
        let chosen = content(
            &c.call(
                9,
                "sonar_investigate",
                json!({"query":"policy","from":handle}),
            )
            .await,
        );
        assert_eq!(chosen["links"].as_array().unwrap().len(), 1);
        assert_eq!(chosen["links"][0]["kind"], "documents");
        c.close().await;
        assert_eq!(fs::read(&f.db).unwrap(), original_db);
    });
}

#[test]
fn modern_stdio_supports_discovery_pagination_and_recovery_after_invalid_calls() {
    run(async {
        let f = Fixture::new();
        let mut c = Client::start(&f, true).await;
        let first = content(
            &c.call(1, "sonar_outline", json!({"path":"lib.rs","limit":1}))
                .await,
        );
        let cursor = first["next_cursor"].as_str().unwrap();
        let next = content(
            &c.call(
                2,
                "sonar_outline",
                json!({"path":"lib.rs","limit":1,"cursor":cursor}),
            )
            .await,
        );
        assert_ne!(
            next["locations"][0]["handle"],
            first["locations"][0]["handle"]
        );
        for (id, name, args) in [
            (
                3,
                "sonar_outline",
                json!({"path":"guide.md","cursor":cursor}),
            ),
            (4, "sonar_brief", json!({"query":"policy","limit":0})),
            (5, "sonar_locate", json!({"path":"../escape.rs","line":1})),
            (
                6,
                "sonar_investigate",
                json!({"query":"policy","exclude":["../escape"]}),
            ),
        ] {
            let error = c.call(id, name, args).await;
            assert_eq!(error["result"]["isError"], true, "{error}");
            assert!(
                !error["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .is_empty()
            );
        }
        for (id, name, args) in [
            (7, "does_not_exist", json!({})),
            (8, "sonar_brief", json!({"query":"policy","limit":"wrong"})),
            (9, "sonar_status", json!({"unexpected":true})),
            (
                10,
                "sonar_neighbors",
                json!({"handle":"invalid","relation":"invented"}),
            ),
        ] {
            let error = c.call(id, name, args).await;
            assert!(
                error.get("error").is_some() || error["result"]["isError"] == true,
                "{error}"
            );
        }
        let list = c.request(11, "tools/list", json!({})).await;
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 8);
        assert_eq!(
            content(&c.call(12, "sonar_status", json!({})).await)["index"]["syntax_freshness"],
            "current"
        );
        c.close().await;
    });
}

#[test]
fn running_server_observes_source_edits_reindexing_and_stale_handles() {
    run(async {
        let f = Fixture::new();
        let mut c = Client::start(&f, false).await;
        let old = content(
            &c.call(1, "sonar_locate", json!({"path":"lib.rs","line":2}))
                .await,
        );
        fs::write(f.root.join("lib.rs"), "fn changed_policy() {}\n").unwrap();
        fs::write(f.root.join("added.md"), "# Added\nnew policy\n").unwrap();
        let status = content(&c.call(2, "sonar_status", json!({})).await);
        assert_eq!(status["index"]["syntax_freshness"], "stale");
        assert_eq!(status["changed_or_deleted_files"], 1);
        assert_eq!(status["added_files"], 1);
        let stale = c
            .call(3, "sonar_locate", json!({"path":"lib.rs","line":1}))
            .await;
        assert_eq!(stale["result"]["isError"], true);
        sonar::index(&f.root, &f.db).unwrap();
        let stale = c
            .call(
                4,
                "sonar_neighbors",
                json!({"handle":old["locations"][0]["handle"]}),
            )
            .await;
        assert_eq!(stale["result"]["isError"], true);
        let fresh = content(
            &c.call(5, "sonar_locate", json!({"path":"lib.rs","line":1}))
                .await,
        );
        assert_eq!(fresh["locations"][0]["symbol"], "changed_policy");
        assert_eq!(
            content(&c.call(6, "sonar_status", json!({})).await)["literal_navigation_current"],
            false
        );
        sonar::index_navigation(&f.db).unwrap();
        assert_eq!(
            content(&c.call(7, "sonar_status", json!({})).await)["literal_navigation_current"],
            true
        );
        c.close().await;
    });
}

#[test]
fn protocol_remains_responsive_under_backpressure_and_cancelled_work_has_no_reply() {
    run(async {
        let f = Fixture::new();
        let mut c = Client::start(&f, true).await;
        let lock = rusqlite::Connection::open(&f.db).unwrap();
        lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        c.send(c.frame(
            1,
            "tools/call",
            json!({"name":"sonar_status","arguments":{}}),
        ))
        .await;
        // Hold the index writer lock so the worker cannot finish before cancellation.
        tokio::time::sleep(Duration::from_millis(100)).await;
        c.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1,"reason":"test cancellation"}})).await;
        for id in 10..30 {
            c.send(c.frame(
                id,
                "tools/call",
                json!({"name":"sonar_status","arguments":{}}),
            ))
            .await;
        }
        c.send(c.frame(99, "tools/list", json!({}))).await;
        let mut received = BTreeSet::new();
        let mut busy = 0;
        loop {
            let response = c.receive().await;
            let id = response["id"].as_u64().unwrap();
            assert_ne!(id, 1, "cancelled request received a reply");
            if id == 99 {
                assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 8);
                break;
            }
            assert_eq!(
                response["result"]["structuredContent"]["error"]["code"], "busy",
                "{response}"
            );
            assert!(received.insert(id));
            busy += 1;
        }
        lock.execute_batch("COMMIT;").unwrap();
        while received.len() < 20 {
            let response = c.receive().await;
            let id = response["id"].as_u64().unwrap();
            assert!(
                (10..30).contains(&id) && received.insert(id),
                "unexpected reply {response}"
            );
            if response["result"]["isError"] == true {
                assert_eq!(
                    response["result"]["structuredContent"]["error"]["code"],
                    "busy"
                );
                busy += 1;
            } else {
                content(&response);
            }
        }
        assert!(
            busy >= 11,
            "queue accepted more than one active plus eight waiting requests"
        );
        assert_eq!(
            content(&c.call(100, "sonar_status", json!({})).await)["read_only"],
            true
        );
        c.close().await;
    });
}

#[test]
fn mcp_role_filter_matches_the_api_and_rejects_unknown_roles() {
    run(async {
        let f = Fixture::new();
        fs::write(
            f.root.join("test_policy.py"),
            "def test_policy():\n    assert True\n",
        )
        .unwrap();
        sonar::index(&f.root, &f.db).unwrap();
        let mut client = Client::start(&f, true).await;
        let result = content(&client.call(1, "sonar_brief", json!({"query":"policy", "role":"implementation", "kind":"function", "related":0})).await);
        let expected = sonar::SearchEngine::open_navigation(&f.db, None)
            .unwrap()
            .with_source_role(Some(sonar::SourceRole::Implementation))
            .with_kind(Some("function".into()))
            .brief(
                "policy",
                &sonar::BriefOptions {
                    related: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result, serde_json::to_value(expected).unwrap());
        assert!(!result["starts"].as_array().unwrap().is_empty());
        let invalid = client
            .call(
                2,
                "sonar_brief",
                json!({"query":"policy", "role":"guessed_owner"}),
            )
            .await;
        assert!(invalid.get("error").is_some() || invalid["result"]["isError"] == true);
        assert!(
            !content(
                &client
                    .call(
                        3,
                        "sonar_brief",
                        json!({"query":"policy", "role":"test", "kind":"function", "related":0})
                    )
                    .await
            )["starts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        client.close().await;
    });
}

#[test]
fn official_sdk_client_can_discover_and_call_the_stdio_server() {
    run(async {
        let f = Fixture::new();
        let mut command = Command::new(env!("CARGO_BIN_EXE_sonar"));
        command.arg("--db").arg(&f.db).arg("mcp");
        let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
        let tools = client.list_tools(None).await.unwrap();
        assert_eq!(tools.tools.len(), 8);
        let result = client
            .call_tool(
                CallToolRequestParams::new("sonar_brief")
                    .with_arguments(json!({"query":"policy"}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));
        assert!(
            !result.structured_content.unwrap()["starts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        client.cancel().await.unwrap();
    });
}

#[test]
fn missing_index_fails_without_polluting_protocol_stdout() {
    let temp = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_sonar"))
        .arg("--db")
        .arg(temp.path().join("missing.sqlite"))
        .arg("mcp")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("index not found")
    );
}
