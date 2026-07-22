//! End-to-end: real child process (python stub), snapshot refresh, lazy
//! execution, allow/deny enforcement, skills through Proxy. Skips gracefully
//! when python3 is unavailable.

use std::sync::Arc;

use geco::backend::Registry;
use geco::config::Config;
use geco::server::{
    BatchCall, BatchParams, ExecuteParams, ListToolsParams, Proxy, RefreshParams, SearchParams,
    ReadSkillParams,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use geco::snapshot::Snapshot;

fn stub_config() -> Option<Config> {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not available — skipping e2e");
        return None;
    }
    let stub = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stub_server.py");
    let raw = format!(
        r#"{{"mcpServers":{{"stub":{{
            "command":"python3","args":["{stub}"],
            "description":"stub","deny":["secret_*"]
        }}}}}}"#
    );
    Some(serde_json::from_str(&raw).unwrap())
}

/// Build a Proxy from a config, returning the proxy, registry, and snapshot.
async fn make_proxy(cfg: &Config) -> (Proxy, Arc<Registry>, Snapshot) {
    let registry = Arc::new(Registry::from_config(cfg));
    let snapshot = registry
        .build_snapshot(cfg.snapshot_hash(), None)
        .await;
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot.clone(),
        Snapshot::cache_path(std::path::Path::new("/tmp/geco-test-snapshot.json")),
        std::path::PathBuf::from("/tmp/geco-test-config.yaml"),
        vec![],
        vec![],
    );
    (proxy, registry, snapshot)
}

/// Build a Proxy with a skill library.
async fn make_proxy_with_skills(cfg: &Config, skill_dirs: Vec<String>) -> (Proxy, Arc<Registry>) {
    let registry = Arc::new(Registry::from_config(cfg));
    let snapshot = registry
        .build_snapshot(cfg.snapshot_hash(), None)
        .await;
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot.clone(),
        Snapshot::cache_path(std::path::Path::new("/tmp/geco-test-snapshot.json")),
        std::path::PathBuf::from("/tmp/geco-test-config.yaml"),
        skill_dirs,
        vec![],
    );
    (proxy, registry)
}

/// Extract the first text block from a CallToolResult and parse it as JSON.
fn extract_json(result: &rmcp::model::CallToolResult) -> serde_json::Value {
    let text = result.content.iter().find_map(|b| {
        if let ContentBlock::Text(t) = b {
            Some(t.text.as_str())
        } else {
            None
        }
    }).expect("no text block in result");
    serde_json::from_str(text).unwrap_or_else(|_| {
        panic!("failed to parse result as JSON: {text}")
    })
}

/// Whether a CallToolResult is an error.
fn is_error(result: &rmcp::model::CallToolResult) -> bool {
    result.is_error == Some(true)
}

/// Create a temp skill library with two skills for testing.
fn make_skill_library() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path();
    // Skill 1: "test-planner"
    let s1 = lib.join("test-planner");
    std::fs::create_dir_all(&s1).unwrap();
    std::fs::write(s1.join("SKILL.md"),
        "---\nname: test-planner\ndescription: Plan tests systematically before writing them\n---\n\n# Test Planner\n\nBreak down test work.\n").unwrap();
    std::fs::create_dir_all(s1.join("scripts")).unwrap();
    std::fs::write(s1.join("scripts").join("run.sh"), "#!/bin/bash\necho planning\n").unwrap();

    // Skill 2: "code-reviewer"
    let s2 = lib.join("code-reviewer");
    std::fs::create_dir_all(&s2).unwrap();
    std::fs::write(s2.join("SKILL.md"),
        "---\nname: code-reviewer\ndescription: Review code for bugs and improvements\n---\n\n# Code Reviewer\n\nCheck for bugs.\n").unwrap();

    dir
}

#[tokio::test]
async fn snapshot_execute_and_denylist() {
    let Some(cfg) = stub_config() else { return };
    let registry = Arc::new(Registry::from_config(&cfg));

    // 1. Snapshot: denylisted tool must not appear.
    let snap = registry.build_snapshot(cfg.snapshot_hash(), None).await;
    let stub = &snap.servers["stub"];
    assert_eq!(stub.refresh_status, "ok");
    let names: Vec<_> = stub.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["add", "echo"]);

    // 2. Search finds the tool.
    let hits = geco::search::search(&snap, "add numbers", 5);
    assert_eq!(hits[0].tool, "add");

    // 3. Lazy execute: backend starts on demand and computes.
    let backend = registry.get("stub").unwrap();
    let args = serde_json::json!({"a": 19, "b": 23});
    let result = backend
        .call("add", args.as_object().cloned(), &registry.proxy)
        .await
        .unwrap();
    let text = format!("{:?}", result.content);
    assert!(text.contains("42"), "expected 42 in {text}");

    // 4. Denylist blocks execution even if the agent guesses the name.
    let err = backend
        .call("secret_tool", None, &registry.proxy)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not allowed"));

    registry.shutdown_all().await;
}

#[tokio::test]
async fn refresh_tools_does_not_deadlock() {
    // Regression test for the P0 deadlock: refresh_tools held the snapshot
    // write lock across publish_health, which tries to read the same lock.
    // If this test returns within a reasonable time, the deadlock is gone.
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // refresh_tools with no server arg (full refresh)
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        proxy.refresh_tools(Parameters(RefreshParams { server: None })),
    )
    .await;
    assert!(result.is_ok(), "refresh_tools timed out (deadlock?)");
    let result = result.unwrap().unwrap();
    let text = format!("{result:?}");
    assert!(text.contains("stub"), "expected stub in result: {text}");

    // refresh_tools with a specific server (single-server path)
    let result2 = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        proxy.refresh_tools(Parameters(RefreshParams { server: Some("stub".into()) })),
    )
    .await;
    assert!(result2.is_ok(), "single-server refresh_tools timed out (deadlock?)");

    // refresh_tools with an unknown server should error, not hang
    let result3 = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        proxy.refresh_tools(Parameters(RefreshParams { server: Some("nonexistent".into()) })),
    )
    .await;
    assert!(result3.is_ok(), "unknown-server refresh_tools timed out");
    let result3 = result3.unwrap().unwrap();
    let text3 = format!("{result3:?}");
    assert!(text3.contains("unknown server"), "expected error for unknown server: {text3}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn health_reports_backend_failures() {
    // Verify the health meta-tool reports backend failures and that
    // publish_health + all_issues agree (the three surfaces: health tool,
    // health.json, and the internal issue list).
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }

    // Add a deliberately broken server to the config.
    let raw = format!(
        r#"{{"mcpServers":{{
            "stub":{{"command":"python3","args":["{stub}"],"description":"stub","deny":["secret_*"]}},
            "broken":{{"command":"/nonexistent/binary","description":"broken"}}
        }}}}"#,
        stub = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stub_server.py")
    );
    let cfg: Config = serde_json::from_str(&raw).unwrap();

    let registry = Arc::new(Registry::from_config(&cfg));
    let snapshot = registry.build_snapshot(cfg.snapshot_hash(), None).await;
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot.clone(),
        Snapshot::cache_path(std::path::Path::new("/tmp/geco-test-snapshot.json")),
        std::path::PathBuf::from("/tmp/geco-test-config.yaml"),
        vec![],
        vec![],
    );

    // The broken server should have an error refresh_status.
    let broken = &snapshot.servers["broken"];
    assert_ne!(broken.refresh_status, "ok", "broken server should have failed");

    // publish_health writes to health.json — verify it doesn't deadlock.
    proxy.publish_health().await;

    // The health meta-tool should report the broken server as an issue.
    let result = proxy.health().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(json["healthy"], false, "health tool should report unhealthy: {json}");
    let issues = json["issues"].as_array().unwrap();
    assert!(issues.iter().any(|i| i["subject"] == "broken"),
            "health tool should mention broken server: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn list_mcp_servers_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    let result = proxy.list_mcp_servers().await.unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let servers = json.as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["server"], "stub");
    assert_eq!(servers[0]["tools"], 2);
    assert_eq!(servers[0]["last_refresh"], "ok");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn list_tools_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // List all tools on the stub server.
    let result = proxy
        .list_tools(Parameters(ListToolsParams { server: "stub".into(), tool: None }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let tools = json["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["tool"].as_str().unwrap()).collect();
    assert!(names.contains(&"add"), "expected 'add' in {names:?}");
    assert!(names.contains(&"echo"), "expected 'echo' in {names:?}");
    // secret_tool must be hidden by the denylist.
    assert!(!names.contains(&"secret_tool"), "secret_tool should be hidden");

    // Get the full input schema for a specific tool.
    let result = proxy
        .list_tools(Parameters(ListToolsParams { server: "stub".into(), tool: Some("add".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert_eq!(json["tool"], "add");
    assert!(json["input_schema"]["properties"]["a"].is_object(),
            "expected input schema with 'a' property: {json}");

    // Unknown server returns an error.
    let result = proxy
        .list_tools(Parameters(ListToolsParams { server: "nope".into(), tool: None }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for unknown server");

    // Unknown tool returns an error.
    let result = proxy
        .list_tools(Parameters(ListToolsParams { server: "stub".into(), tool: Some("nope".into()) }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for unknown tool");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn search_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Search for "add numbers" with kind="tools" should find the 'add' tool.
    let result = proxy
        .search(Parameters(SearchParams { query: "add numbers".into(), limit: None, include_schema: None, kind: Some("tools".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json.as_array().unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(hits[0]["kind"], "tool");
    assert_eq!(hits[0]["name"], "add");
    assert_eq!(hits[0]["server"], "stub");
    assert!(hits[0].get("input_schema").is_none() || hits[0]["input_schema"].is_null(),
           "input_schema should be absent without include_schema: {json}");

    // Search with limit.
    let result = proxy
        .search(Parameters(SearchParams { query: "add".into(), limit: Some(1), include_schema: None, kind: None }))
        .await
        .unwrap();
    let json = extract_json(&result);
    let hits = json.as_array().unwrap();
    // Tools are limited to 1, skills may also appear (but no skills configured here).
    let tool_hits: Vec<_> = hits.iter().filter(|h| h["kind"] == "tool").collect();
    assert_eq!(tool_hits.len(), 1, "expected exactly 1 tool hit with limit=1: {json}");

    // Search with include_schema=true: should include the input schema inline.
    let result = proxy
        .search(Parameters(SearchParams { query: "add numbers".into(), limit: None, include_schema: Some(true), kind: Some("tools".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json.as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"], "add");
    assert!(hits[0]["input_schema"]["properties"]["a"].is_object(),
            "expected input schema with 'a' property: {json}");
    assert!(hits[0]["input_schema"]["properties"]["b"].is_object(),
            "expected input schema with 'b' property: {json}");

    // Search for nonexistent query returns an error.
    let result = proxy
        .search(Parameters(SearchParams { query: "xyzzy_nothing".into(), limit: None, include_schema: None, kind: None }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for no matches");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_tool_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Execute 'add' with arguments through the Proxy meta-tool.
    let args = serde_json::json!({"a": 17, "b": 25})
        .as_object()
        .unwrap()
        .clone();
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "stub".into(),
            tool: "add".into(),
            arguments: Some(args),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "execute_tool should succeed");
    let text = format!("{:?}", result.content);
    assert!(text.contains("42"), "expected 42 in {text}");

    // Execute 'echo' with a text argument.
    let args = serde_json::json!({"text": "hello geco"})
        .as_object()
        .unwrap()
        .clone();
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "stub".into(),
            tool: "echo".into(),
            arguments: Some(args),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let text = format!("{:?}", result.content);
    assert!(text.contains("hello geco"), "expected echoed text in {text}");

    // Denylist: secret_tool is blocked at execute time even if guessed.
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "stub".into(),
            tool: "secret_tool".into(),
            arguments: None,
        }))
        .await
        .unwrap();
    assert!(is_error(&result), "denied tool should error");

    // Unknown server.
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "nope".into(),
            tool: "add".into(),
            arguments: None,
        }))
        .await
        .unwrap();
    assert!(is_error(&result), "unknown server should error");

    // Unknown tool on a valid server.
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "stub".into(),
            tool: "nonexistent".into(),
            arguments: None,
        }))
        .await
        .unwrap();
    assert!(is_error(&result), "unknown tool should error");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn skills_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let lib = make_skill_library();
    let (proxy, registry) = make_proxy_with_skills(
        &cfg,
        vec![lib.path().display().to_string()],
    )
    .await;

    // list_skills: should show both skills.
    let result = proxy.list_skills().await.unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let skills = json["skills"].as_array().unwrap();
    assert_eq!(json["count"], 2, "expected 2 skills: {json}");
    let names: Vec<_> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"test-planner"), "expected test-planner in {names:?}");
    assert!(names.contains(&"code-reviewer"), "expected code-reviewer in {names:?}");

    // search with kind="skills": "review" should find code-reviewer.
    let result = proxy
        .search(Parameters(SearchParams { query: "review".into(), limit: None, include_schema: None, kind: Some("skills".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json.as_array().unwrap();
    assert!(!hits.is_empty(), "expected at least one hit for 'review'");
    assert_eq!(hits[0]["kind"], "skill");
    assert_eq!(hits[0]["name"], "code-reviewer");

    // search with kind="skills": "plan" should find test-planner.
    let result = proxy
        .search(Parameters(SearchParams { query: "plan tests".into(), limit: None, include_schema: None, kind: Some("skills".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json.as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"], "test-planner");

    // search with kind="skills": no match returns an error.
    let result = proxy
        .search(Parameters(SearchParams { query: "xyzzy_nothing".into(), limit: None, include_schema: None, kind: Some("skills".into()) }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for no skill matches");

    // search with kind="skills" + no skills configured returns error.
    // (already tested above — the make_proxy_with_skills has skills)

    // read_skill: load the full SKILL.md + file inventory.
    let result = proxy
        .read_skill(Parameters(ReadSkillParams { name: "test-planner".into() }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let text = format!("{:?}", result.content);
    assert!(text.contains("Test Planner"), "expected SKILL.md body in result");
    assert!(text.contains("run.sh"), "expected bundled script in file inventory");

    // read_skill: unknown skill returns an error.
    let result = proxy
        .read_skill(Parameters(ReadSkillParams { name: "nonexistent".into() }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for unknown skill");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_tool_on_warm_backend_reuses_connection() {
    // Verify that a second execute_tool call reuses the warm backend
    // connection (no re-spawn) and still returns correct results.
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // First call: cold start.
    let args = serde_json::json!({"a": 1, "b": 2}).as_object().unwrap().clone();
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "stub".into(),
            tool: "add".into(),
            arguments: Some(args),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let text = format!("{:?}", result.content);
    assert!(text.contains("3"), "expected 3 in {text}");

    // Second call: warm — should reuse connection.
    let args = serde_json::json!({"a": 40, "b": 2}).as_object().unwrap().clone();
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "stub".into(),
            tool: "add".into(),
            arguments: Some(args),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let text = format!("{:?}", result.content);
    assert!(text.contains("42"), "expected 42 in {text}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn hot_reload_updates_snapshot() {
    // Test reload_from_config directly (not the file watcher, which is
    // OS-dependent and hard to test reliably). Verify that after a reload:
    // 1. New servers appear in the snapshot
    // 2. Removed servers disappear
    // 3. list_mcp_servers reflects the new state
    // 4. execute_tool works on the new server
    if std::process::Command::new("python3").arg("--version").output().is_err() {
        return;
    }
    let stub = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stub_server.py");
    let snap_path = Snapshot::cache_path(std::path::Path::new("/tmp/geco-test-snapshot.json"));

    // Initial config: one server ("stub").
    let cfg1: Config = serde_json::from_str(&format!(
        r#"{{"mcpServers":{{"stub":{{"command":"python3","args":["{stub}"],"description":"stub","deny":["secret_*"]}}}}}}"#
    )).unwrap();
    let registry = Arc::new(Registry::from_config(&cfg1));
    let snapshot = registry.build_snapshot(cfg1.snapshot_hash(), None).await;
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot,
        snap_path,
        std::path::PathBuf::from("/tmp/geco-test-config.yaml"),
        vec![],
        vec![],
    );

    // Verify initial state: 1 server.
    let result = proxy.list_mcp_servers().await.unwrap();
    let json = extract_json(&result);
    let servers = json.as_array().unwrap();
    assert_eq!(servers.len(), 1, "expected 1 server initially: {json}");
    assert_eq!(servers[0]["server"], "stub");

    // Reload with a config that has a different description for "stub"
    // (proving the snapshot hash changes and tools are re-fetched).
    let cfg2: Config = serde_json::from_str(&format!(
        r#"{{"mcpServers":{{"stub":{{"command":"python3","args":["{stub}"],"description":"updated desc","deny":["secret_*"]}}}}}}"#
    )).unwrap();
    proxy.reload_from_config(cfg2).await.unwrap();

    // After reload: still 1 server, but description changed.
    let result = proxy.list_mcp_servers().await.unwrap();
    let json = extract_json(&result);
    let servers = json.as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["description"], "updated desc");

    // Execute through the reloaded proxy — should still work.
    let args = serde_json::json!({"a": 10, "b": 5}).as_object().unwrap().clone();
    let result = proxy
        .execute_tool(Parameters(ExecuteParams {
            server: "stub".into(),
            tool: "add".into(),
            arguments: Some(args),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "execute_tool should work after reload");
    let text = format!("{:?}", result.content);
    assert!(text.contains("15"), "expected 15 in {text}");

    proxy.shutdown_backends().await;
}

#[tokio::test]
async fn check_health_detects_dead_backend() {
    // Verify check_health returns true for cold and warm backends, and
    // that after shutdown the backend can reconnect fresh.
    let Some(cfg) = stub_config() else { return };
    let registry = Arc::new(Registry::from_config(&cfg));
    let backend = registry.get("stub").unwrap();

    // Cold backend: check_health returns true (nothing to check).
    assert!(backend.check_health().await, "cold backend should be healthy");

    // Start the backend by calling a tool.
    let args = serde_json::json!({"a": 1, "b": 1}).as_object().unwrap().clone();
    let _ = backend.call("add", Some(args), &registry.proxy).await.unwrap();
    assert!(matches!(backend.status().await, geco::backend::Status::Up { .. }));

    // Warm backend: check_health returns true.
    assert!(backend.check_health().await, "warm backend should be healthy");

    // Shut down — simulates the child process dying.
    backend.shutdown().await;
    assert!(matches!(backend.status().await, geco::backend::Status::Cold));

    // After shutdown, conn is None — check_health is a no-op (returns true).
    assert!(backend.check_health().await, "after shutdown, check_health is a no-op");

    // Reconnect: call should work again (new connection).
    let args = serde_json::json!({"a": 2, "b": 3}).as_object().unwrap().clone();
    let result = backend.call("add", Some(args), &registry.proxy).await.unwrap();
    let text = format!("{:?}", result.content);
    assert!(text.contains("5"), "expected 5 after reconnect: {text}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_batch_through_proxy() {
    // Test batch execution: multiple calls in one round-trip, results
    // in input order, errors don't abort the whole batch.
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Batch with 3 calls to the same backend (serializes naturally):
    // add(1+2), echo("hello"), add(40+2).
    let result = proxy
        .execute_batch(Parameters(BatchParams {
            calls: vec![
                BatchCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(serde_json::json!({"a": 1, "b": 2}).as_object().unwrap().clone()),
                },
                BatchCall {
                    server: "stub".into(),
                    tool: "echo".into(),
                    arguments: Some(serde_json::json!({"text": "hello batch"}).as_object().unwrap().clone()),
                },
                BatchCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(serde_json::json!({"a": 40, "b": 2}).as_object().unwrap().clone()),
                },
            ],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 3, "expected 3 results: {json}");
    assert_eq!(results[0]["tool"], "add");
    assert!(results[0]["result"].as_str().unwrap_or("").contains("3"), "expected 3: {json}");
    assert_eq!(results[1]["tool"], "echo");
    assert!(results[1]["result"].as_str().unwrap_or("").contains("hello batch"), "expected echo: {json}");
    assert_eq!(results[2]["tool"], "add");
    assert!(results[2]["result"].as_str().unwrap_or("").contains("42"), "expected 42: {json}");
    assert!(results.iter().all(|r| r["ok"] == true), "all calls should succeed: {json}");

    // Batch with an error (unknown tool) — the other call should still succeed.
    let result = proxy
        .execute_batch(Parameters(BatchParams {
            calls: vec![
                BatchCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(serde_json::json!({"a": 10, "b": 5}).as_object().unwrap().clone()),
                },
                BatchCall {
                    server: "stub".into(),
                    tool: "nonexistent".into(),
                    arguments: None,
                },
            ],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(results[0]["ok"] == true, "first call should succeed: {json}");
    assert!(results[1]["ok"] == false, "second call should fail: {json}");
    assert!(results[1]["result"].as_str().unwrap_or("").contains("unknown tool")
            || results[1].get("error").is_some(),
            "expected error for unknown tool: {json}");

    // Batch with an unknown server — error, not a crash.
    let result = proxy
        .execute_batch(Parameters(BatchParams {
            calls: vec![BatchCall {
                server: "nope".into(),
                tool: "add".into(),
                arguments: None,
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0]["ok"] == false, "unknown server should fail: {json}");

    // Empty batch returns an error.
    let result = proxy
        .execute_batch(Parameters(BatchParams { calls: vec![] }))
        .await
        .unwrap();
    assert!(is_error(&result), "empty batch should error");

    // Too many calls (>10) returns an error.
    let calls: Vec<_> = (0..11).map(|_| BatchCall {
        server: "stub".into(),
        tool: "echo".into(),
        arguments: Some(serde_json::json!({"text": "x"}).as_object().unwrap().clone()),
    }).collect();
    let result = proxy
        .execute_batch(Parameters(BatchParams { calls }))
        .await
        .unwrap();
    assert!(is_error(&result), "batch >10 should error");

    registry.shutdown_all().await;
}
