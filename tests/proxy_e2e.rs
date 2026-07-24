//! End-to-end: real child process (python stub), snapshot refresh, lazy
//! execution, allow/deny enforcement, skills through Proxy. Skips gracefully
//! when no Python interpreter is available — unless GECO_REQUIRE_E2E is set,
//! in which case a missing interpreter is a hard failure (see `python_bin`).

use std::sync::Arc;

use geco::backend::Registry;
use geco::config::Config;
use geco::server::{
    ExecuteCall, ExecuteParams, ListToolsParams, Proxy, RefreshParams, SearchParams,
    ReadSkillParams,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;
use geco::snapshot::Snapshot;

/// The interpreter to run the stub MCP server with, or None if there isn't
/// one. Probes `python3` then `python`: Windows ships `python.exe` but
/// usually not `python3`, so probing only the former made the ENTIRE e2e
/// suite silently skip itself there — a green run that tested nothing.
///
/// Set `GECO_REQUIRE_E2E=1` (CI does) to turn "no interpreter" into a hard
/// failure instead of a skip. A run that quietly covered nothing is worse
/// than a red one, because it looks like coverage.
fn python_bin() -> Option<String> {
    for bin in ["python3", "python"] {
        // Check the exit status, not just that spawning worked: Windows'
        // App Execution Alias stub for `python` runs fine and exits 9009.
        let runs = std::process::Command::new(bin)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if runs {
            return Some(bin.to_string());
        }
    }
    assert!(
        std::env::var_os("GECO_REQUIRE_E2E").is_none(),
        "GECO_REQUIRE_E2E is set but neither `python3` nor `python` runs — \
         the e2e suite would have skipped itself and reported success"
    );
    eprintln!("no python3/python available — skipping e2e");
    None
}

fn stub_path() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stub_server.py")
}

/// Build a Config from an `mcpServers` map.
///
/// Deliberately `json!` + `from_value` rather than `format!` into a JSON
/// string literal: on Windows CARGO_MANIFEST_DIR is `C:\Users\...`, and
/// backslashes are escape characters inside a JSON string — `\U` and `\g`
/// are not valid escapes, so every format!-built config in this file failed
/// to parse there. It never surfaced only because the python3 probe above
/// bailed out first.
fn config_from(servers: serde_json::Value) -> Config {
    serde_json::from_value(serde_json::json!({ "mcpServers": servers }))
        .expect("test config must deserialize")
}

fn stub_config() -> Option<Config> {
    let py = python_bin()?;
    Some(config_from(serde_json::json!({
        "stub": {
            "command": py,
            "args": [stub_path()],
            "description": "stub",
            "deny": ["secret_*"],
        }
    })))
}

/// Build a Proxy from a config, returning the proxy, registry, and snapshot.
/// Uses a unique temp dir per call to avoid parallel-test file races.
async fn make_proxy(cfg: &Config) -> (Proxy, Arc<Registry>, Snapshot) {
    let registry = Arc::new(Registry::from_config(cfg));
    let snapshot = registry
        .build_snapshot(cfg.snapshot_hash(), None)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let snap_path = dir.path().join("snapshot.json");
    let cfg_path = dir.path().join("config.yaml");
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot.clone(),
        snap_path,
        cfg_path,
        vec![],
        vec![],
    );
    std::mem::forget(dir); // keep temp dir alive for test duration
    (proxy, registry, snapshot)
}

/// Build a Proxy with a skill library.
async fn make_proxy_with_skills(cfg: &Config, skill_dirs: Vec<String>) -> (Proxy, Arc<Registry>) {
    let registry = Arc::new(Registry::from_config(cfg));
    let snapshot = registry
        .build_snapshot(cfg.snapshot_hash(), None)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let snap_path = dir.path().join("snapshot.json");
    let cfg_path = dir.path().join("config.yaml");
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot.clone(),
        snap_path,
        cfg_path,
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
        "---\nname: test-planner\ndescription: Plan tests systematically before writing them\nversion: 1.2.0\n---\n\n# Test Planner\n\nBreak down test work.\n").unwrap();
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
    assert!(err.to_string().contains("DENY:"));

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
    assert!(is_error(&result3), "unknown server should error");
    let json = extract_json(&result3);
    assert_eq!(json["error"]["code"], "unknown_server", "expected unknown_server code: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn health_reports_backend_failures() {
    // Verify the health meta-tool reports backend failures and that
    // publish_health + all_issues agree (the three surfaces: health tool,
    // health.json, and the internal issue list).
    let Some(py) = python_bin() else { return };

    // Add a deliberately broken server to the config.
    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "description": "stub", "deny": ["secret_*"],
        },
        "broken": { "command": "/nonexistent/binary", "description": "broken" },
    }));

    let registry = Arc::new(Registry::from_config(&cfg));
    let snapshot = registry.build_snapshot(cfg.snapshot_hash(), None).await;
    let dir = tempfile::tempdir().unwrap();
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot.clone(),
        dir.path().join("snapshot.json"),
        dir.path().join("config.yaml"),
        vec![],
        vec![],
    );
    std::mem::forget(dir);

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
    // Tool preview should include both tools (add + echo, sorted alphabetically).
    let preview = servers[0]["tool_preview"].as_array().unwrap();
    assert_eq!(preview.len(), 2, "expected 2 tools in preview: {json}");
    let names: Vec<_> = preview.iter().map(|t| t["tool"].as_str().unwrap()).collect();
    assert!(names.contains(&"add"), "expected 'add' in preview: {names:?}");
    assert!(names.contains(&"echo"), "expected 'echo' in preview: {names:?}");

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
    let json = extract_json(&result);
    assert_eq!(json["error"]["code"], "unknown_server", "expected unknown_server: {json}");

    // Unknown tool returns an error.
    let result = proxy
        .list_tools(Parameters(ListToolsParams { server: "stub".into(), tool: Some("nope".into()) }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for unknown tool");
    let json = extract_json(&result);
    assert_eq!(json["error"]["code"], "unknown_tool", "expected unknown_tool: {json}");

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
    let hits = json["results"].as_array().unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(json["count"], hits.len(), "count must match results length: {json}");
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
    let hits = json["results"].as_array().unwrap();
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
    let hits = json["results"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"], "add");
    assert!(hits[0]["input_schema"]["properties"]["a"].is_object(),
            "expected input schema with 'a' property: {json}");
    assert!(hits[0]["input_schema"]["properties"]["b"].is_object(),
            "expected input schema with 'b' property: {json}");

    // No matches is an empty result, NOT an error — an is_error response here
    // told the model the call itself had failed.
    let result = proxy
        .search(Parameters(SearchParams { query: "xyzzy_nothing".into(), limit: None, include_schema: None, kind: None }))
        .await
        .unwrap();
    assert!(!is_error(&result), "no matches must not be an error");
    let json = extract_json(&result);
    assert_eq!(json["count"], 0, "expected count 0: {json}");
    assert!(json["results"].as_array().unwrap().is_empty(), "expected empty results: {json}");
    assert!(json["hint"].is_string(), "expected a hint on empty results: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Single call: add(17 + 25 = 42).
    let args = serde_json::json!({"a": 17, "b": 25}).as_object().unwrap().clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "add".into(), arguments: Some(args) }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "execute should succeed");
    let json = extract_json(&result);
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], true);
    assert!(results[0]["result"].as_str().unwrap_or("").contains("42"), "expected 42: {json}");

    // Single call: echo.
    let args = serde_json::json!({"text": "hello geco"}).as_object().unwrap().clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "echo".into(), arguments: Some(args) }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(json[0]["result"].as_str().unwrap_or("").contains("hello geco"), "expected echo: {json}");

    // Denylist: secret_tool is blocked at execute time even if guessed.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "secret_tool".into(), arguments: None }],
        }))
        .await
        .unwrap();
    let json = extract_json(&result);
    assert!(json[0]["ok"] == false, "denied tool should fail: {json}");
    assert_eq!(json[0]["error"]["code"], "not_allowed", "expected not_allowed: {json}");

    // Unknown server.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "nope".into(), tool: "add".into(), arguments: None }],
        }))
        .await
        .unwrap();
    let json = extract_json(&result);
    assert!(json[0]["ok"] == false, "unknown server should fail: {json}");
    assert_eq!(json[0]["error"]["code"], "unknown_server", "expected unknown_server: {json}");

    // Unknown tool on a valid server.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "nonexistent".into(), arguments: None }],
        }))
        .await
        .unwrap();
    let json = extract_json(&result);
    assert!(json[0]["ok"] == false, "unknown tool should fail: {json}");

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
    // test-planner has a version in frontmatter; code-reviewer doesn't.
    let planner = skills.iter().find(|s| s["name"] == "test-planner").unwrap();
    assert_eq!(planner["version"], "1.2.0", "expected version 1.2.0: {planner}");
    let reviewer = skills.iter().find(|s| s["name"] == "code-reviewer").unwrap();
    assert!(reviewer.get("version").is_none() || reviewer["version"].is_null(),
            "code-reviewer should have no version: {reviewer}");

    // search with kind="skills": "review" should find code-reviewer.
    let result = proxy
        .search(Parameters(SearchParams { query: "review".into(), limit: None, include_schema: None, kind: Some("skills".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json["results"].as_array().unwrap();
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
    let hits = json["results"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"], "test-planner");

    // search with kind="skills": no match is an empty result, not an error.
    let result = proxy
        .search(Parameters(SearchParams { query: "xyzzy_nothing".into(), limit: None, include_schema: None, kind: Some("skills".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result), "no skill matches must not be an error");
    let json = extract_json(&result);
    assert_eq!(json["count"], 0, "expected count 0: {json}");
    assert!(json["hint"].is_string(), "expected a hint on empty results: {json}");

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
    let json = extract_json(&result);
    assert_eq!(json["error"]["code"], "unknown_skill", "expected unknown_skill: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn rescan_skills_picks_up_new_skill() {
    // Test rescan_skills directly (not the file watcher, which is
    // OS-dependent and hard to test reliably). After dropping a new skill
    // dir into the library and calling rescan_skills, the new skill should
    // appear in list_skills without touching the config.
    let Some(cfg) = stub_config() else { return };
    let lib = make_skill_library();
    let (proxy, registry) = make_proxy_with_skills(
        &cfg,
        vec![lib.path().display().to_string()],
    )
    .await;

    // Initially 2 skills.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(json["count"], 2, "expected 2 skills initially: {json}");

    // Drop a third skill into the library dir.
    let s3 = lib.path().join("third-skill");
    std::fs::create_dir_all(&s3).unwrap();
    std::fs::write(s3.join("SKILL.md"),
        "---\nname: third-skill\ndescription: A skill added after startup\n---\n\n# Third Skill\n").unwrap();

    // rescan_skills (what spawn_skill_reload calls on a filesystem event).
    proxy.rescan_skills().await;

    // Now 3 skills, including the new one.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(json["count"], 3, "expected 3 skills after rescan: {json}");
    let skills = json["skills"].as_array().unwrap();
    let names: Vec<_> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"third-skill"), "expected third-skill in {names:?}");

    // read_skill works on the newly-added skill.
    let result = proxy
        .read_skill(Parameters(ReadSkillParams { name: "third-skill".into() }))
        .await
        .unwrap();
    assert!(!is_error(&result), "expected read_skill to find third-skill");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn rescan_skills_picks_up_edited_description() {
    // Editing a SKILL.md frontmatter should update the description
    // returned by list_skills after a rescan.
    let Some(cfg) = stub_config() else { return };
    let lib = make_skill_library();
    let (proxy, registry) = make_proxy_with_skills(
        &cfg,
        vec![lib.path().display().to_string()],
    )
    .await;

    // Initial description for code-reviewer.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    let reviewer = json["skills"].as_array().unwrap()
        .iter().find(|s| s["name"] == "code-reviewer").unwrap();
    assert_eq!(reviewer["description"], "Review code for bugs and improvements");

    // Edit the SKILL.md frontmatter description.
    let s2 = lib.path().join("code-reviewer");
    std::fs::write(s2.join("SKILL.md"),
        "---\nname: code-reviewer\ndescription: UPDATED description after edit\n---\n\n# Code Reviewer\n").unwrap();

    proxy.rescan_skills().await;

    // Description should now reflect the edit.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    let reviewer = json["skills"].as_array().unwrap()
        .iter().find(|s| s["name"] == "code-reviewer").unwrap();
    assert_eq!(reviewer["description"], "UPDATED description after edit",
        "expected updated description: {reviewer}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn rescan_skills_drops_removed_skill() {
    // Removing a skill dir should make it disappear from list_skills
    // after a rescan.
    let Some(cfg) = stub_config() else { return };
    let lib = make_skill_library();
    let (proxy, registry) = make_proxy_with_skills(
        &cfg,
        vec![lib.path().display().to_string()],
    )
    .await;

    assert_eq!(extract_json(&proxy.list_skills().await.unwrap())["count"], 2);

    // Remove the code-reviewer skill entirely.
    std::fs::remove_dir_all(lib.path().join("code-reviewer")).unwrap();

    proxy.rescan_skills().await;

    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(json["count"], 1, "expected 1 skill after removal: {json}");
    let skills = json["skills"].as_array().unwrap();
    assert_eq!(skills[0]["name"], "test-planner");

    // read_skill should now error for the removed skill.
    let result = proxy
        .read_skill(Parameters(ReadSkillParams { name: "code-reviewer".into() }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for removed skill");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn config_reload_failure_surfaces_in_health() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Initially healthy (no config issues).
    let result = proxy.health().await.unwrap();
    let json = extract_json(&result);
    assert!(json["issues"].as_array().unwrap().iter().all(|i| i["kind"] != "config"),
            "expected no config issues initially: {json}");

    // Simulate a config-reload failure (what the hot-reload watcher does on
    // a broken config file).
    proxy.set_config_issue(Some(geco::health::Issue {
        kind: "config".into(),
        subject: "reload".into(),
        detail: "parse failed (keeping old config): invalid YAML".into(),
        severity: "error".into(),
    })).await;

    // Health should now surface the config issue.
    let result = proxy.health().await.unwrap();
    let json = extract_json(&result);
    let config_issues: Vec<_> = json["issues"].as_array().unwrap().iter()
        .filter(|i| i["kind"] == "config").collect();
    assert_eq!(config_issues.len(), 1, "expected 1 config issue: {json}");
    assert!(config_issues[0]["detail"].as_str().unwrap().contains("parse failed"),
            "expected parse failed in detail: {config_issues:?}");

    // Clear the issue (what a successful reload does).
    proxy.set_config_issue(None).await;
    let result = proxy.health().await.unwrap();
    let json = extract_json(&result);
    assert!(json["issues"].as_array().unwrap().iter().all(|i| i["kind"] != "config"),
            "expected no config issues after clear: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_on_warm_backend_reuses_connection() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // First call: cold start.
    let args = serde_json::json!({"a": 1, "b": 2}).as_object().unwrap().clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "add".into(), arguments: Some(args) }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(json[0]["result"].as_str().unwrap_or("").contains("3"), "expected 3: {json}");

    // Second call: warm — should reuse connection.
    let args = serde_json::json!({"a": 40, "b": 2}).as_object().unwrap().clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "add".into(), arguments: Some(args) }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(json[0]["result"].as_str().unwrap_or("").contains("42"), "expected 42: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn hot_reload_updates_snapshot() {
    // Test reload_from_config directly (not the file watcher, which is
    // OS-dependent and hard to test reliably). Verify that after a reload:
    // 1. New servers appear in the snapshot
    // 2. Removed servers disappear
    // 3. list_mcp_servers reflects the new state
    // 4. execute works on the new server
    let Some(py) = python_bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let snap_path = dir.path().join("snapshot.json");
    let cfg_path = dir.path().join("config.yaml");

    // Initial config: one server ("stub").
    let cfg1 = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "description": "stub", "deny": ["secret_*"],
        }
    }));
    let registry = Arc::new(Registry::from_config(&cfg1));
    let snapshot = registry.build_snapshot(cfg1.snapshot_hash(), None).await;
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot,
        snap_path,
        cfg_path,
        vec![],
        vec![],
    );
    std::mem::forget(dir);

    // Verify initial state: 1 server.
    let result = proxy.list_mcp_servers().await.unwrap();
    let json = extract_json(&result);
    let servers = json.as_array().unwrap();
    assert_eq!(servers.len(), 1, "expected 1 server initially: {json}");
    assert_eq!(servers[0]["server"], "stub");

    // Reload with a config that has a different description for "stub"
    // (proving the snapshot hash changes and tools are re-fetched).
    let cfg2 = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "description": "updated desc", "deny": ["secret_*"],
        }
    }));
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
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "add".into(), arguments: Some(args) }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "execute should work after reload");
    let json = extract_json(&result);
    assert!(json[0]["result"].as_str().unwrap_or("").contains("15"), "expected 15: {json}");

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
    // Test multi-call execution: multiple calls in one round-trip, results
    // in input order, errors don't abort the whole batch.
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // 3 calls to the same backend (serializes naturally):
    // add(1+2), echo("hello"), add(40+2).
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![
                ExecuteCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(serde_json::json!({"a": 1, "b": 2}).as_object().unwrap().clone()),
                },
                ExecuteCall {
                    server: "stub".into(),
                    tool: "echo".into(),
                    arguments: Some(serde_json::json!({"text": "hello batch"}).as_object().unwrap().clone()),
                },
                ExecuteCall {
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

    // Mixed success/error (unknown tool) — the other call should still succeed.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![
                ExecuteCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(serde_json::json!({"a": 10, "b": 5}).as_object().unwrap().clone()),
                },
                ExecuteCall {
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
    assert!(results[1]["result"].as_str().unwrap_or("").contains("unknown tool"),
            "expected 'unknown tool' in result: {json}");

    // Unknown server — error, not a crash.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "nope".into(),
                tool: "add".into(),
                arguments: None,
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(json[0]["ok"] == false, "unknown server should fail: {json}");

    // Empty calls returns an error.
    let result = proxy
        .execute(Parameters(ExecuteParams { calls: vec![] }))
        .await
        .unwrap();
    assert!(is_error(&result), "empty calls should error");
    let json = extract_json(&result);
    assert_eq!(json["error"]["code"], "empty_calls", "expected empty_calls: {json}");

    // Too many calls (>10) returns an error.
    let calls: Vec<_> = (0..11).map(|_| ExecuteCall {
        server: "stub".into(),
        tool: "echo".into(),
        arguments: Some(serde_json::json!({"text": "x"}).as_object().unwrap().clone()),
    }).collect();
    let result = proxy
        .execute(Parameters(ExecuteParams { calls }))
        .await
        .unwrap();
    assert!(is_error(&result), ">10 calls should error");
    let json = extract_json(&result);
    assert_eq!(json["error"]["code"], "too_many_calls", "expected too_many_calls: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_records_metrics() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Make a couple of calls.
    let args = serde_json::json!({"a": 1, "b": 2}).as_object().unwrap().clone();
    let _ = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "add".into(), arguments: Some(args) }],
        }))
        .await
        .unwrap();

    let args = serde_json::json!({"a": 3, "b": 4}).as_object().unwrap().clone();
    let _ = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall { server: "stub".into(), tool: "add".into(), arguments: Some(args) }],
        }))
        .await
        .unwrap();

    // Check metrics.
    let metrics = proxy.metrics_snapshot().await;
    assert_eq!(metrics.total_calls, 2, "expected 2 total calls");
    assert_eq!(metrics.total_errors, 0, "expected 0 errors");
    let stub = &metrics.per_backend["stub"];
    assert_eq!(stub.calls, 2, "expected 2 stub calls");
    assert!(stub.cold_starts >= 1, "expected at least 1 cold start");
    assert!(stub.last_call_at.is_some(), "last_call_at should be set");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn refresh_tools_returns_diff() {
    // Verify refresh_tools returns a diff: after the first refresh, all
    // tools are "unchanged". After a second refresh with no changes, the
    // diff should show 0 added, 0 removed, 0 changed.
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // First full refresh — everything is either added or unchanged
    // (depends on whether the snapshot already had the tools).
    let result = proxy
        .refresh_tools(Parameters(RefreshParams { server: None }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(json["diff"].is_object(), "expected diff in response: {json}");
    // The stub has 2 tools (add, echo). On the first refresh after snapshot
    // build, they should be unchanged (same snapshot data).
    let diff = &json["diff"];
    assert!(diff["unchanged"].as_u64().unwrap_or(0) >= 2,
            "expected at least 2 unchanged tools: {diff}");

    // Second refresh — nothing changed.
    let result = proxy
        .refresh_tools(Parameters(RefreshParams { server: None }))
        .await
        .unwrap();
    let json = extract_json(&result);
    let diff = &json["diff"];
    assert_eq!(diff["added"].as_array().unwrap().len(), 0, "expected 0 added: {diff}");
    assert_eq!(diff["removed"].as_array().unwrap().len(), 0, "expected 0 removed: {diff}");
    assert_eq!(diff["changed"].as_array().unwrap().len(), 0, "expected 0 changed: {diff}");
    assert!(diff["unchanged"].as_u64().unwrap_or(0) >= 2,
            "expected at least 2 unchanged: {diff}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn refresh_tools_diff_shows_added_server() {
    // Regression: compute_diff missed newly added servers in a full refresh
    // because the iterator only covered servers in both old and new.
    let Some(py) = python_bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let snap_path = dir.path().join("snapshot.json");
    let cfg_path = dir.path().join("config.yaml");
    std::mem::forget(dir);

    // Config with two servers, but initial snapshot only has one.
    // (Simulates: user added a server to config, then calls refresh_tools
    // manually without hot-reload having fired yet.)
    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "description": "stub", "deny": ["secret_*"],
        },
        "stub2": {
            "command": py, "args": [stub_path()],
            "description": "second stub",
        },
    }));
    let registry = Arc::new(Registry::from_config(&cfg));

    // Build a snapshot with only "stub" (not stub2).
    let snapshot = registry.build_snapshot(cfg.snapshot_hash(), Some("stub")).await;
    // build_snapshot with a filter only builds that one server.
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot,
        snap_path,
        cfg_path,
        vec![],
        vec![],
    );

    // Now refresh_tools (full) — should build both servers and diff
    // against the old snapshot (which only had stub).
    let result = proxy
        .refresh_tools(Parameters(RefreshParams { server: None }))
        .await
        .unwrap();
    let json = extract_json(&result);
    let diff = &json["diff"];
    let added = diff["added"].as_array().unwrap();
    assert!(!added.is_empty(), "expected added tools from new server: {diff}");
    assert!(added.iter().any(|a| a["server"] == "stub2"),
            "expected stub2 tools in added: {added:?}");

    proxy.shutdown_backends().await;
}

#[tokio::test]
async fn partial_refresh_leaves_other_backends_running() {
    // Regression: refresh_tools shut down every registry backend that was
    // missing from the post-refresh snapshot. A single-server refresh leaves
    // every OTHER server out of the fresh build, so whenever the snapshot was
    // partial (empty cache with refresh_on_start=false, or a filtered build),
    // refreshing one backend killed all the others.
    let Some(py) = python_bin() else { return };
    let dir = tempfile::tempdir().unwrap();
    let snap_path = dir.path().join("snapshot.json");
    let cfg_path = dir.path().join("config.yaml");
    std::mem::forget(dir);

    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "description": "stub",
        },
        "stub2": {
            "command": py, "args": [stub_path()],
            "description": "second stub",
        },
    }));
    let registry = Arc::new(Registry::from_config(&cfg));

    // Snapshot deliberately covers only "stub" — "stub2" is configured and
    // live, but absent from the snapshot.
    let snapshot = registry.build_snapshot(cfg.snapshot_hash(), Some("stub")).await;
    let proxy = Proxy::new(
        (*registry).clone(),
        snapshot,
        snap_path,
        cfg_path,
        vec![],
        vec![],
    );

    // Warm up stub2 so it has a real child process to lose.
    let stub2 = registry.get("stub2").unwrap();
    let args = serde_json::json!({"a": 1, "b": 1}).as_object().unwrap().clone();
    stub2.call("add", Some(args), &registry.proxy).await.unwrap();
    assert!(matches!(stub2.status().await, geco::backend::Status::Up { .. }),
            "stub2 should be warm before the refresh");

    // Refresh only "stub".
    let result = proxy
        .refresh_tools(Parameters(RefreshParams { server: Some("stub".into()) }))
        .await
        .unwrap();
    assert!(!is_error(&result), "single-server refresh should succeed");

    let status = stub2.status().await;
    assert!(matches!(status, geco::backend::Status::Up { .. }),
            "partial refresh must not shut down unrelated backends, got: {status:?}");

    proxy.shutdown_backends().await;
}

#[tokio::test]
async fn skill_watch_roots_resolves_inside_a_task() {
    // Regression: the skill watcher resolved its watch roots with
    // RwLock::blocking_read from inside a tokio task. That panics on the spot
    // ("Cannot block the current thread from within a runtime"), so the
    // watcher died at startup and skills silently never hot-reloaded. This
    // exercises the resolver from an async context, where the old code blew up.
    let Some(cfg) = stub_config() else { return };
    let lib = make_skill_library();
    let (proxy, registry) = make_proxy_with_skills(
        &cfg,
        vec![lib.path().display().to_string()],
    )
    .await;

    let roots = proxy.skill_watch_roots().await;
    let expected = std::path::PathBuf::from(lib.path().display().to_string());
    assert_eq!(roots.len(), 1, "expected exactly the configured library: {roots:?}");
    assert!(roots.contains(&expected), "expected {} in {roots:?}", expected.display());

    // Run it from a spawned task too — that is where the panic actually fired.
    let proxy2 = proxy.clone();
    tokio::spawn(async move { proxy2.skill_watch_roots().await })
        .await
        .expect("resolving watch roots inside a spawned task must not panic");

    registry.shutdown_all().await;
}
