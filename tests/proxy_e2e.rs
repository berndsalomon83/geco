//! End-to-end: real child process (python stub), snapshot refresh, lazy
//! execution, allow/deny enforcement, skills through Proxy. Skips gracefully
//! when no Python interpreter is available — unless GECO_REQUIRE_E2E is set,
//! in which case a missing interpreter is a hard failure (see `python_bin`).

use std::sync::Arc;

use geco::backend::Registry;
use geco::config::Config;
use geco::server::{
    ExecuteCall, ExecuteParams, ListToolsParams, Proxy, ReadSkillParams, SearchParams,
};
use geco::snapshot::Snapshot;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ContentBlock;

/// The interpreter to run the stub MCP server with, or None if there isn't
/// one. Probes `python3` then `python`: Windows ships `python.exe` but
/// usually not `python3`, so probing only the former made the ENTIRE e2e
/// suite silently skip itself there — a green run that tested nothing.
///
/// Set `GECO_REQUIRE_E2E=1` (CI does) to turn "no interpreter" into a hard
/// failure instead of a skip. A run that quietly covered nothing is worse
/// than a red one, because it looks like coverage.
fn python_bin() -> Option<String> {
    isolate_health();
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

/// Redirect health writes into a temp file. `Proxy::publish_health()` writes
/// unconditionally, so without this the suite overwrites the developer's real
/// health.json — clobbering the state a running geco is reporting through.
fn isolate_health() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("geco-test-health-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("GECO_HEALTH", dir.join("health.json"));
    });
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
    let snapshot = registry.build_snapshot(cfg.snapshot_hash(), None).await;
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
    let snapshot = registry.build_snapshot(cfg.snapshot_hash(), None).await;
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
    let text = result
        .content
        .iter()
        .find_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .expect("no text block in result");
    serde_json::from_str(text).unwrap_or_else(|_| panic!("failed to parse result as JSON: {text}"))
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
    std::fs::write(
        s1.join("scripts").join("run.sh"),
        "#!/bin/bash\necho planning\n",
    )
    .unwrap();

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
async fn health_reports_backend_failures() {
    // Verify a backend failure reaches the agent and that publish_health +
    // all_issues agree (the surfaces: list_mcp_servers' `issues`,
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
    assert_ne!(
        broken.refresh_status, "ok",
        "broken server should have failed"
    );

    // publish_health writes to health.json — verify it doesn't deadlock.
    proxy.publish_health().await;

    // list_mcp_servers should report the broken server as an issue. This is
    // the agent's only in-band view of gateway health now that `health` is a
    // CLI command, so it is the one that must not silently stop working.
    let result = proxy.list_mcp_servers().await.unwrap();
    let json = extract_json(&result);
    let issues = json["issues"]
        .as_array()
        .unwrap_or_else(|| panic!("expected an `issues` array when a backend is down: {json}"));
    assert!(
        issues.iter().any(|i| i["subject"] == "broken"),
        "issues should mention the broken server: {json}"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn list_mcp_servers_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    let result = proxy.list_mcp_servers().await.unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let servers = json["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["server"], "stub");
    assert_eq!(servers[0]["tools"], 2);
    assert_eq!(servers[0]["last_refresh"], "ok");
    // Tool preview should include both tools (add + echo, sorted alphabetically).
    let preview = servers[0]["tool_preview"].as_array().unwrap();
    assert_eq!(preview.len(), 2, "expected 2 tools in preview: {json}");
    let names: Vec<_> = preview
        .iter()
        .map(|t| t["tool"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"add"),
        "expected 'add' in preview: {names:?}"
    );
    assert!(
        names.contains(&"echo"),
        "expected 'echo' in preview: {names:?}"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn list_tools_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // List all tools on the stub server.
    let result = proxy
        .list_tools(Parameters(ListToolsParams {
            server: "stub".into(),
            tool: None,
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let tools = json["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|t| t["tool"].as_str().unwrap()).collect();
    assert!(names.contains(&"add"), "expected 'add' in {names:?}");
    assert!(names.contains(&"echo"), "expected 'echo' in {names:?}");
    // secret_tool must be hidden by the denylist.
    assert!(
        !names.contains(&"secret_tool"),
        "secret_tool should be hidden"
    );

    // Get the full input schema for a specific tool.
    let result = proxy
        .list_tools(Parameters(ListToolsParams {
            server: "stub".into(),
            tool: Some("add".into()),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert_eq!(json["tool"], "add");
    assert!(
        json["input_schema"]["properties"]["a"].is_object(),
        "expected input schema with 'a' property: {json}"
    );

    // Unknown server returns an error.
    let result = proxy
        .list_tools(Parameters(ListToolsParams {
            server: "nope".into(),
            tool: None,
        }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for unknown server");
    let json = extract_json(&result);
    assert_eq!(
        json["error"]["code"], "unknown_server",
        "expected unknown_server: {json}"
    );

    // Unknown tool returns an error.
    let result = proxy
        .list_tools(Parameters(ListToolsParams {
            server: "stub".into(),
            tool: Some("nope".into()),
        }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for unknown tool");
    let json = extract_json(&result);
    assert_eq!(
        json["error"]["code"], "unknown_tool",
        "expected unknown_tool: {json}"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn search_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Search for "add numbers" with kind="tools" should find the 'add' tool.
    let result = proxy
        .search(Parameters(SearchParams {
            query: "add numbers".into(),
            limit: None,
            include_schema: None,
            kind: Some("tools".into()),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json["results"].as_array().unwrap();
    assert!(!hits.is_empty(), "expected at least one hit");
    assert_eq!(
        json["count"],
        hits.len(),
        "count must match results length: {json}"
    );
    assert_eq!(hits[0]["kind"], "tool");
    assert_eq!(hits[0]["name"], "add");
    assert_eq!(hits[0]["server"], "stub");
    assert!(
        hits[0].get("input_schema").is_none() || hits[0]["input_schema"].is_null(),
        "input_schema should be absent without include_schema: {json}"
    );

    // Search with limit.
    let result = proxy
        .search(Parameters(SearchParams {
            query: "add".into(),
            limit: Some(1),
            include_schema: None,
            kind: None,
        }))
        .await
        .unwrap();
    let json = extract_json(&result);
    let hits = json["results"].as_array().unwrap();
    // Tools are limited to 1, skills may also appear (but no skills configured here).
    let tool_hits: Vec<_> = hits.iter().filter(|h| h["kind"] == "tool").collect();
    assert_eq!(
        tool_hits.len(),
        1,
        "expected exactly 1 tool hit with limit=1: {json}"
    );

    // Search with include_schema=true: should include the input schema inline.
    let result = proxy
        .search(Parameters(SearchParams {
            query: "add numbers".into(),
            limit: None,
            include_schema: Some(true),
            kind: Some("tools".into()),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json["results"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"], "add");
    assert!(
        hits[0]["input_schema"]["properties"]["a"].is_object(),
        "expected input schema with 'a' property: {json}"
    );
    assert!(
        hits[0]["input_schema"]["properties"]["b"].is_object(),
        "expected input schema with 'b' property: {json}"
    );

    // No matches is an empty result, NOT an error — an is_error response here
    // told the model the call itself had failed.
    let result = proxy
        .search(Parameters(SearchParams {
            query: "xyzzy_nothing".into(),
            limit: None,
            include_schema: None,
            kind: None,
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "no matches must not be an error");
    let json = extract_json(&result);
    assert_eq!(json["count"], 0, "expected count 0: {json}");
    assert!(
        json["results"].as_array().unwrap().is_empty(),
        "expected empty results: {json}"
    );
    assert!(
        json["hint"].is_string(),
        "expected a hint on empty results: {json}"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_through_proxy() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Single call: add(17 + 25 = 42).
    let args = serde_json::json!({"a": 17, "b": 25})
        .as_object()
        .unwrap()
        .clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "add".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "execute should succeed");
    let json = extract_json(&result);
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["ok"], true);
    assert!(
        results[0]["result"].as_str().unwrap_or("").contains("42"),
        "expected 42: {json}"
    );

    // Single call: echo.
    let args = serde_json::json!({"text": "hello geco"})
        .as_object()
        .unwrap()
        .clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "echo".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(
        json[0]["result"]
            .as_str()
            .unwrap_or("")
            .contains("hello geco"),
        "expected echo: {json}"
    );

    // Denylist: secret_tool is blocked at execute time even if guessed.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "secret_tool".into(),
                arguments: None,
            }],
        }))
        .await
        .unwrap();
    let json = extract_json(&result);
    assert!(json[0]["ok"] == false, "denied tool should fail: {json}");
    assert_eq!(
        json[0]["error"]["code"], "not_allowed",
        "expected not_allowed: {json}"
    );

    // Unknown server.
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
    let json = extract_json(&result);
    assert!(json[0]["ok"] == false, "unknown server should fail: {json}");
    assert_eq!(
        json[0]["error"]["code"], "unknown_server",
        "expected unknown_server: {json}"
    );

    // Unknown tool on a valid server.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "nonexistent".into(),
                arguments: None,
            }],
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
    let (proxy, registry) =
        make_proxy_with_skills(&cfg, vec![lib.path().display().to_string()]).await;

    // list_skills: should show both skills.
    let result = proxy.list_skills().await.unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let skills = json["skills"].as_array().unwrap();
    assert_eq!(json["count"], 2, "expected 2 skills: {json}");
    let names: Vec<_> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"test-planner"),
        "expected test-planner in {names:?}"
    );
    assert!(
        names.contains(&"code-reviewer"),
        "expected code-reviewer in {names:?}"
    );
    // test-planner has a version in frontmatter; code-reviewer doesn't.
    let planner = skills.iter().find(|s| s["name"] == "test-planner").unwrap();
    assert_eq!(
        planner["version"], "1.2.0",
        "expected version 1.2.0: {planner}"
    );
    let reviewer = skills
        .iter()
        .find(|s| s["name"] == "code-reviewer")
        .unwrap();
    assert!(
        reviewer.get("version").is_none() || reviewer["version"].is_null(),
        "code-reviewer should have no version: {reviewer}"
    );

    // search with kind="skills": "review" should find code-reviewer.
    let result = proxy
        .search(Parameters(SearchParams {
            query: "review".into(),
            limit: None,
            include_schema: None,
            kind: Some("skills".into()),
        }))
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
        .search(Parameters(SearchParams {
            query: "plan tests".into(),
            limit: None,
            include_schema: None,
            kind: Some("skills".into()),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let hits = json["results"].as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["name"], "test-planner");

    // search with kind="skills": no match is an empty result, not an error.
    let result = proxy
        .search(Parameters(SearchParams {
            query: "xyzzy_nothing".into(),
            limit: None,
            include_schema: None,
            kind: Some("skills".into()),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "no skill matches must not be an error");
    let json = extract_json(&result);
    assert_eq!(json["count"], 0, "expected count 0: {json}");
    assert!(
        json["hint"].is_string(),
        "expected a hint on empty results: {json}"
    );

    // search with kind="skills" + no skills configured returns error.
    // (already tested above — the make_proxy_with_skills has skills)

    // read_skill: load the full SKILL.md + file inventory.
    let result = proxy
        .read_skill(Parameters(ReadSkillParams {
            name: "test-planner".into(),
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let text = format!("{:?}", result.content);
    assert!(
        text.contains("Test Planner"),
        "expected SKILL.md body in result"
    );
    assert!(
        text.contains("run.sh"),
        "expected bundled script in file inventory"
    );

    // read_skill: unknown skill returns an error.
    let result = proxy
        .read_skill(Parameters(ReadSkillParams {
            name: "nonexistent".into(),
        }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for unknown skill");
    let json = extract_json(&result);
    assert_eq!(
        json["error"]["code"], "unknown_skill",
        "expected unknown_skill: {json}"
    );

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
    let (proxy, registry) =
        make_proxy_with_skills(&cfg, vec![lib.path().display().to_string()]).await;

    // Initially 2 skills.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(json["count"], 2, "expected 2 skills initially: {json}");

    // Drop a third skill into the library dir.
    let s3 = lib.path().join("third-skill");
    std::fs::create_dir_all(&s3).unwrap();
    std::fs::write(
        s3.join("SKILL.md"),
        "---\nname: third-skill\ndescription: A skill added after startup\n---\n\n# Third Skill\n",
    )
    .unwrap();

    // rescan_skills (what spawn_skill_reload calls on a filesystem event).
    proxy.rescan_skills().await;

    // Now 3 skills, including the new one.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(json["count"], 3, "expected 3 skills after rescan: {json}");
    let skills = json["skills"].as_array().unwrap();
    let names: Vec<_> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"third-skill"),
        "expected third-skill in {names:?}"
    );

    // read_skill works on the newly-added skill.
    let result = proxy
        .read_skill(Parameters(ReadSkillParams {
            name: "third-skill".into(),
        }))
        .await
        .unwrap();
    assert!(
        !is_error(&result),
        "expected read_skill to find third-skill"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn rescan_skills_picks_up_edited_description() {
    // Editing a SKILL.md frontmatter should update the description
    // returned by list_skills after a rescan.
    let Some(cfg) = stub_config() else { return };
    let lib = make_skill_library();
    let (proxy, registry) =
        make_proxy_with_skills(&cfg, vec![lib.path().display().to_string()]).await;

    // Initial description for code-reviewer.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    let reviewer = json["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "code-reviewer")
        .unwrap();
    assert_eq!(
        reviewer["description"],
        "Review code for bugs and improvements"
    );

    // Edit the SKILL.md frontmatter description.
    let s2 = lib.path().join("code-reviewer");
    std::fs::write(s2.join("SKILL.md"),
        "---\nname: code-reviewer\ndescription: UPDATED description after edit\n---\n\n# Code Reviewer\n").unwrap();

    proxy.rescan_skills().await;

    // Description should now reflect the edit.
    let result = proxy.list_skills().await.unwrap();
    let json = extract_json(&result);
    let reviewer = json["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "code-reviewer")
        .unwrap();
    assert_eq!(
        reviewer["description"], "UPDATED description after edit",
        "expected updated description: {reviewer}"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn rescan_skills_drops_removed_skill() {
    // Removing a skill dir should make it disappear from list_skills
    // after a rescan.
    let Some(cfg) = stub_config() else { return };
    let lib = make_skill_library();
    let (proxy, registry) =
        make_proxy_with_skills(&cfg, vec![lib.path().display().to_string()]).await;

    assert_eq!(
        extract_json(&proxy.list_skills().await.unwrap())["count"],
        2
    );

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
        .read_skill(Parameters(ReadSkillParams {
            name: "code-reviewer".into(),
        }))
        .await
        .unwrap();
    assert!(is_error(&result), "expected error for removed skill");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn config_reload_failure_surfaces_to_the_agent() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // `issues` is absent, not empty, when there is nothing wrong — that is
    // the whole point of folding it into this response instead of paying for
    // a `health` tool in every session's context.
    let config_issues = |json: &serde_json::Value| -> usize {
        json["issues"]
            .as_array()
            .map(|a| a.iter().filter(|i| i["kind"] == "config").count())
            .unwrap_or(0)
    };

    let result = proxy.list_mcp_servers().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(config_issues(&json), 0, "expected none initially: {json}");

    // Simulate a config-reload failure (what the hot-reload watcher does on
    // a broken config file).
    proxy
        .set_config_issue(Some(geco::health::Issue {
            kind: "config".into(),
            subject: "reload".into(),
            detail: "parse failed (keeping old config): invalid YAML".into(),
            severity: "error".into(),
        }))
        .await;

    // It should now reach the agent without a dedicated health tool.
    let result = proxy.list_mcp_servers().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(config_issues(&json), 1, "expected 1 config issue: {json}");
    let detail = json["issues"]
        .as_array()
        .and_then(|a| a.iter().find(|i| i["kind"] == "config"))
        .and_then(|i| i["detail"].as_str())
        .unwrap_or_default();
    assert!(
        detail.contains("parse failed"),
        "expected the reason, not just the fact: {detail:?}"
    );

    // Clear the issue (what a successful reload does).
    proxy.set_config_issue(None).await;
    let result = proxy.list_mcp_servers().await.unwrap();
    let json = extract_json(&result);
    assert_eq!(config_issues(&json), 0, "expected none after clear: {json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_on_warm_backend_reuses_connection() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // First call: cold start.
    let args = serde_json::json!({"a": 1, "b": 2})
        .as_object()
        .unwrap()
        .clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "add".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(
        json[0]["result"].as_str().unwrap_or("").contains("3"),
        "expected 3: {json}"
    );

    // Second call: warm — should reuse connection.
    let args = serde_json::json!({"a": 40, "b": 2})
        .as_object()
        .unwrap()
        .clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "add".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    assert!(
        json[0]["result"].as_str().unwrap_or("").contains("42"),
        "expected 42: {json}"
    );

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
    let servers = json["servers"].as_array().unwrap();
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
    let servers = json["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["description"], "updated desc");

    // Execute through the reloaded proxy — should still work.
    let args = serde_json::json!({"a": 10, "b": 5})
        .as_object()
        .unwrap()
        .clone();
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "add".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "execute should work after reload");
    let json = extract_json(&result);
    assert!(
        json[0]["result"].as_str().unwrap_or("").contains("15"),
        "expected 15: {json}"
    );

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
    assert!(
        backend.check_health().await,
        "cold backend should be healthy"
    );

    // Start the backend by calling a tool.
    let args = serde_json::json!({"a": 1, "b": 1})
        .as_object()
        .unwrap()
        .clone();
    let _ = backend
        .call("add", Some(args), &registry.proxy)
        .await
        .unwrap();
    assert!(matches!(
        backend.status().await,
        geco::backend::Status::Up { .. }
    ));

    // Warm backend: check_health returns true.
    assert!(
        backend.check_health().await,
        "warm backend should be healthy"
    );

    // Shut down — simulates the child process dying.
    backend.shutdown().await;
    assert!(matches!(
        backend.status().await,
        geco::backend::Status::Cold
    ));

    // After shutdown, conn is None — check_health is a no-op (returns true).
    assert!(
        backend.check_health().await,
        "after shutdown, check_health is a no-op"
    );

    // Reconnect: call should work again (new connection).
    let args = serde_json::json!({"a": 2, "b": 3})
        .as_object()
        .unwrap()
        .clone();
    let result = backend
        .call("add", Some(args), &registry.proxy)
        .await
        .unwrap();
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
                    arguments: Some(
                        serde_json::json!({"a": 1, "b": 2})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                },
                ExecuteCall {
                    server: "stub".into(),
                    tool: "echo".into(),
                    arguments: Some(
                        serde_json::json!({"text": "hello batch"})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                },
                ExecuteCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(
                        serde_json::json!({"a": 40, "b": 2})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
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
    assert!(
        results[0]["result"].as_str().unwrap_or("").contains("3"),
        "expected 3: {json}"
    );
    assert_eq!(results[1]["tool"], "echo");
    assert!(
        results[1]["result"]
            .as_str()
            .unwrap_or("")
            .contains("hello batch"),
        "expected echo: {json}"
    );
    assert_eq!(results[2]["tool"], "add");
    assert!(
        results[2]["result"].as_str().unwrap_or("").contains("42"),
        "expected 42: {json}"
    );
    assert!(
        results.iter().all(|r| r["ok"] == true),
        "all calls should succeed: {json}"
    );

    // Mixed success/error (unknown tool) — the other call should still succeed.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![
                ExecuteCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(
                        serde_json::json!({"a": 10, "b": 5})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
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
    assert!(
        results[0]["ok"] == true,
        "first call should succeed: {json}"
    );
    assert!(results[1]["ok"] == false, "second call should fail: {json}");
    assert!(
        results[1]["result"]
            .as_str()
            .unwrap_or("")
            .contains("unknown tool"),
        "expected 'unknown tool' in result: {json}"
    );

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
    assert_eq!(
        json["error"]["code"], "empty_calls",
        "expected empty_calls: {json}"
    );

    // Too many calls (>10) returns an error.
    let calls: Vec<_> = (0..11)
        .map(|_| ExecuteCall {
            server: "stub".into(),
            tool: "echo".into(),
            arguments: Some(
                serde_json::json!({"text": "x"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        })
        .collect();
    let result = proxy
        .execute(Parameters(ExecuteParams { calls }))
        .await
        .unwrap();
    assert!(is_error(&result), ">10 calls should error");
    let json = extract_json(&result);
    assert_eq!(
        json["error"]["code"], "too_many_calls",
        "expected too_many_calls: {json}"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn execute_records_metrics() {
    let Some(cfg) = stub_config() else { return };
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // Make a couple of calls.
    let args = serde_json::json!({"a": 1, "b": 2})
        .as_object()
        .unwrap()
        .clone();
    let _ = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "add".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();

    let args = serde_json::json!({"a": 3, "b": 4})
        .as_object()
        .unwrap()
        .clone();
    let _ = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "add".into(),
                arguments: Some(args),
            }],
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
async fn execute_batches_across_backends() {
    // `execute`'s description promises cross-backend calls run in parallel,
    // but every batch test so far used a single backend. This covers the
    // claim: two servers, one batch, results in INPUT order regardless of
    // which finishes first.
    let Some(py) = python_bin() else { return };
    let cfg = config_from(serde_json::json!({
        "stub":  { "command": py, "args": [stub_path()], "description": "stub" },
        "stub2": { "command": py, "args": [stub_path()], "description": "second stub" },
    }));
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![
                ExecuteCall {
                    server: "stub2".into(),
                    tool: "echo".into(),
                    arguments: Some(
                        serde_json::json!({"text": "from stub2"})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                },
                ExecuteCall {
                    server: "stub".into(),
                    tool: "add".into(),
                    arguments: Some(
                        serde_json::json!({"a": 20, "b": 22})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                },
            ],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result));
    let json = extract_json(&result);
    let results = json.as_array().unwrap();
    assert_eq!(results.len(), 2, "{json}");
    // Input order, not completion order.
    assert_eq!(
        results[0]["server"], "stub2",
        "results must keep input order: {json}"
    );
    assert_eq!(
        results[1]["server"], "stub",
        "results must keep input order: {json}"
    );
    assert!(
        results.iter().all(|r| r["ok"] == true),
        "both should succeed: {json}"
    );
    assert!(
        results[0]["result"]
            .as_str()
            .unwrap_or("")
            .contains("from stub2"),
        "{json}"
    );
    assert!(
        results[1]["result"].as_str().unwrap_or("").contains("42"),
        "{json}"
    );

    // Both backends should now be warm — proof they really both ran.
    assert!(matches!(
        registry.get("stub").unwrap().status().await,
        geco::backend::Status::Up { .. }
    ));
    assert!(matches!(
        registry.get("stub2").unwrap().status().await,
        geco::backend::Status::Up { .. }
    ));

    registry.shutdown_all().await;
}

#[tokio::test]
async fn allowlist_filters_snapshot_and_execution() {
    // Only `deny` was covered end to end. An allowlist is the stricter,
    // more security-relevant direction: everything not named is invisible
    // AND unrunnable, even if the agent guesses the name.
    let Some(py) = python_bin() else { return };
    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "description": "stub", "allow": ["add"],
        }
    }));
    let (proxy, registry, snapshot) = make_proxy(&cfg).await;

    // Snapshot: only the allowed tool survives.
    let names: Vec<_> = snapshot.servers["stub"]
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(names, vec!["add"], "allowlist should leave only `add`");

    // list_tools agrees.
    let result = proxy
        .list_tools(Parameters(ListToolsParams {
            server: "stub".into(),
            tool: None,
        }))
        .await
        .unwrap();
    let json = extract_json(&result);
    let listed: Vec<_> = json["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tool"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["add"], "{json}");

    // And a guessed, non-allowlisted tool is refused at execution time.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "echo".into(),
                arguments: Some(
                    serde_json::json!({"text": "hi"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            }],
        }))
        .await
        .unwrap();
    let json = extract_json(&result);
    assert_eq!(
        json[0]["ok"], false,
        "non-allowlisted tool must not run: {json}"
    );
    assert_eq!(json[0]["error"]["code"], "not_allowed", "{json}");

    registry.shutdown_all().await;
}

#[tokio::test]
async fn backend_death_mid_call_is_detected_and_recovers() {
    // check_health_detects_dead_backend only covers an orderly shutdown().
    // This is the real failure mode: the child dies mid-call without
    // replying, and the cached connection must be reset rather than reused.
    let Some(py) = python_bin() else { return };
    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "env": { "STUB_EXTRA_TOOLS": "1" },
        }
    }));
    let registry = Arc::new(Registry::from_config(&cfg));
    let backend = registry.get("stub").unwrap();

    // Warm it up.
    let args = serde_json::json!({"a": 1, "b": 1})
        .as_object()
        .unwrap()
        .clone();
    backend
        .call("add", Some(args), &registry.proxy)
        .await
        .unwrap();
    assert!(matches!(
        backend.status().await,
        geco::backend::Status::Up { .. }
    ));

    // Kill it mid-call. Bounded by our own timeout so a hang fails fast
    // rather than sitting on geco's 120s call_timeout.
    let crashed = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        backend.call("crash", None, &registry.proxy),
    )
    .await
    .expect("crash call should return promptly, not hang until call_timeout");
    let err_msg = match crashed {
        Err(e) => format!("{e:#}"),
        Ok(r) => panic!("a backend that exits mid-call must surface an error, got {r:?}"),
    };

    // The dead connection must have been dropped, not left cached.
    let status = backend.status().await;
    assert!(
        matches!(status, geco::backend::Status::Failed { .. }),
        "a died-mid-call backend should be Failed, got {status:?}\n\
         underlying transport error was: {err_msg}"
    );

    // ...and the next call must transparently reconnect.
    let args = serde_json::json!({"a": 2, "b": 3})
        .as_object()
        .unwrap()
        .clone();
    let res = backend
        .call("add", Some(args), &registry.proxy)
        .await
        .expect("next call should reconnect to a fresh child");
    assert!(
        format!("{:?}", res.content).contains("5"),
        "expected 5 after reconnect"
    );
    assert!(matches!(
        backend.status().await,
        geco::backend::Status::Up { .. }
    ));

    registry.shutdown_all().await;
}

/// Settles the question the code review left open: does dropping the
/// connection actually reap the child process, or does every backend leak
/// one? Unix-only — it needs `ps` to check liveness. The Windows answer
/// still rests on rmcp's kill-on-drop behaviour and is unverified here.
#[cfg(unix)]
#[tokio::test]
async fn backend_child_is_reaped_on_shutdown() {
    let Some(py) = python_bin() else { return };
    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "env": { "STUB_EXTRA_TOOLS": "1" },
        }
    }));
    let registry = Arc::new(Registry::from_config(&cfg));
    let backend = registry.get("stub").unwrap();

    // Ask the child for its own PID.
    let res = backend.call("pid", None, &registry.proxy).await.unwrap();
    let text = res
        .content
        .iter()
        .find_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .expect("pid tool should return text");
    let pid: u32 = text
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("bad pid {text:?}: {e}"));

    // `ps -o stat=` prints nothing once the process is gone; a leading Z
    // means it exited but hasn't been reaped, which counts as dead here.
    let is_alive = |pid: u32| -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output();
        match out {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                !s.is_empty() && !s.starts_with('Z')
            }
            Err(_) => false,
        }
    };
    assert!(is_alive(pid), "child {pid} should be running while warm");

    registry.shutdown_all().await;

    // Reaping is asynchronous — give it a moment before declaring a leak.
    let mut alive = true;
    for _ in 0..50 {
        if !is_alive(pid) {
            alive = false;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        !alive,
        "backend child {pid} still running 5s after shutdown_all() — \
         dropping the connection does NOT reap the child, so every backend \
         leaks a process on reload and on exit"
    );
}

/// Read an env var as the spawned child sees it.
async fn backend_getenv(
    backend: &Arc<geco::backend::Backend>,
    registry: &Registry,
    name: &str,
) -> String {
    let args = serde_json::json!({ "name": name })
        .as_object()
        .unwrap()
        .clone();
    let res = backend
        .call("getenv", Some(args), &registry.proxy)
        .await
        .unwrap();
    res.content
        .iter()
        .find_map(|b| {
            if let ContentBlock::Text(t) = b {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn backend_env_is_isolated_by_default() {
    // The whole point of the isolation work: a secret exported in geco's
    // shell for ONE server must not be handed to every other server.
    let Some(py) = python_bin() else { return };
    std::env::set_var("GECO_AMBIENT_SECRET", "must-not-leak");

    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "env": { "STUB_EXTRA_TOOLS": "1" },
        }
    }));
    let registry = Arc::new(Registry::from_config(&cfg));
    let backend = registry.get("stub").unwrap();

    assert_eq!(
        backend_getenv(&backend, &registry, "GECO_AMBIENT_SECRET").await,
        "<unset>",
        "an undeclared ambient variable must not reach the backend"
    );
    // ...while the things a server genuinely needs still arrive.
    assert_eq!(
        backend_getenv(&backend, &registry, "STUB_EXTRA_TOOLS").await,
        "1",
        "declared env must reach the backend"
    );
    assert_ne!(
        backend_getenv(&backend, &registry, "PATH").await,
        "<unset>",
        "PATH is in the baseline — without it nothing runs"
    );

    std::env::remove_var("GECO_AMBIENT_SECRET");
    registry.shutdown_all().await;
}

#[tokio::test]
async fn pass_env_now_actually_forwards_a_variable() {
    // Before 0.7 this test could not fail: the child inherited everything,
    // so pass_env was indistinguishable from doing nothing.
    let Some(py) = python_bin() else { return };
    std::env::set_var("GECO_OPTED_IN_SECRET", "forwarded");
    std::env::set_var("GECO_NOT_OPTED_IN", "withheld");

    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "env": { "STUB_EXTRA_TOOLS": "1" },
            "pass_env": ["GECO_OPTED_IN_SECRET"],
        }
    }));
    let registry = Arc::new(Registry::from_config(&cfg));
    let backend = registry.get("stub").unwrap();

    assert_eq!(
        backend_getenv(&backend, &registry, "GECO_OPTED_IN_SECRET").await,
        "forwarded",
        "pass_env must forward the variable it names"
    );
    assert_eq!(
        backend_getenv(&backend, &registry, "GECO_NOT_OPTED_IN").await,
        "<unset>",
        "a sibling variable must stay behind — that's the whole point"
    );

    std::env::remove_var("GECO_OPTED_IN_SECRET");
    std::env::remove_var("GECO_NOT_OPTED_IN");
    registry.shutdown_all().await;
}

#[tokio::test]
async fn env_passthrough_globs_and_inherit_escape_hatch() {
    let Some(py) = python_bin() else { return };
    std::env::set_var("GECO_GLOB_ONE", "one");
    std::env::set_var("GECO_OTHER", "other");

    // Glob passthrough lets a whole family through, nothing else.
    let cfg: Config = serde_json::from_value(serde_json::json!({
        "proxy": { "env_passthrough": ["GECO_GLOB_*"] },
        "mcpServers": {
            "stub": {
                "command": py, "args": [stub_path()],
                "env": { "STUB_EXTRA_TOOLS": "1" },
            }
        }
    }))
    .unwrap();
    let registry = Arc::new(Registry::from_config(&cfg));
    let backend = registry.get("stub").unwrap();
    assert_eq!(
        backend_getenv(&backend, &registry, "GECO_GLOB_ONE").await,
        "one"
    );
    assert_eq!(
        backend_getenv(&backend, &registry, "GECO_OTHER").await,
        "<unset>"
    );
    registry.shutdown_all().await;

    // inherit_env: true restores the pre-0.7 behaviour wholesale.
    let cfg: Config = serde_json::from_value(serde_json::json!({
        "proxy": { "inherit_env": true },
        "mcpServers": {
            "stub": {
                "command": py, "args": [stub_path()],
                "env": { "STUB_EXTRA_TOOLS": "1" },
            }
        }
    }))
    .unwrap();
    let registry = Arc::new(Registry::from_config(&cfg));
    let backend = registry.get("stub").unwrap();
    assert_eq!(
        backend_getenv(&backend, &registry, "GECO_OTHER").await,
        "other",
        "inherit_env: true must hand the child everything again"
    );
    registry.shutdown_all().await;

    std::env::remove_var("GECO_GLOB_ONE");
    std::env::remove_var("GECO_OTHER");
}

#[tokio::test]
async fn tools_list_changed_refreshes_the_snapshot_unprompted() {
    // The drift this closes: a backend that gains or loses tools at runtime
    // stayed wrong in the snapshot until someone happened to run
    // a manual refresh, with nothing reporting that anything had changed.
    let Some(py) = python_bin() else { return };
    let cfg = config_from(serde_json::json!({
        "stub": {
            "command": py, "args": [stub_path()],
            "description": "stub",
            "env": { "STUB_EXTRA_TOOLS": "1" },
        }
    }));
    let (proxy, registry, _snap) = make_proxy(&cfg).await;
    proxy.clone().spawn_tool_change_listener();

    let tool_names = |p: Proxy| async move {
        let r = p
            .list_tools(Parameters(ListToolsParams {
                server: "stub".into(),
                tool: None,
            }))
            .await
            .unwrap();
        let json = extract_json(&r);
        json["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["tool"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };

    let before = tool_names(proxy.clone()).await;
    assert!(
        !before.contains(&"brand_new_tool".to_string()),
        "precondition: the tool should not exist yet, got {before:?}"
    );

    // Ask the backend to grow its toolset. It replies, then sends
    // notifications/tools/list_changed — no manual refresh anywhere.
    let result = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "grow_toolset".into(),
                arguments: None,
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&result), "grow_toolset should succeed");

    // The listener debounces ~200ms, then re-scans. Poll rather than sleep a
    // fixed amount, so this isn't slower or flakier than it needs to be.
    let mut after = Vec::new();
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        after = tool_names(proxy.clone()).await;
        if after.contains(&"brand_new_tool".to_string()) {
            break;
        }
    }
    assert!(
        after.contains(&"brand_new_tool".to_string()),
        "snapshot should have picked up the new tool unprompted, got {after:?}"
    );

    proxy.shutdown_backends().await;
    registry.shutdown_all().await;
}

#[tokio::test]
async fn search_effectiveness_is_measured() {
    // Tool-call counters can't answer the question that matters for a
    // gateway: is discovery working? These counters can.
    let Some(py) = python_bin() else { return };
    let cfg = config_from(serde_json::json!({
        "stub": { "command": py, "args": [stub_path()], "description": "stub" }
    }));
    let (proxy, registry, _snap) = make_proxy(&cfg).await;

    // A search that finds something.
    let r = proxy
        .search(Parameters(SearchParams {
            query: "add numbers".into(),
            limit: None,
            include_schema: None,
            kind: Some("tools".into()),
        }))
        .await
        .unwrap();
    assert!(!is_error(&r));

    // A search that finds nothing.
    let r = proxy
        .search(Parameters(SearchParams {
            query: "xyzzy_no_such_thing".into(),
            limit: None,
            include_schema: None,
            kind: Some("tools".into()),
        }))
        .await
        .unwrap();
    assert_eq!(extract_json(&r)["count"], 0);

    let m = proxy.metrics_snapshot().await;
    assert_eq!(m.searches, 2, "both searches counted");
    assert_eq!(m.searches_no_match, 1, "one found nothing");
    assert_eq!(
        m.recent_no_match,
        vec!["xyzzy_no_such_thing".to_string()],
        "the miss is recorded so it can be acted on"
    );
    assert_eq!(m.search_to_execute, 0, "nothing executed yet");

    // Executing a tool that search surfaced counts as search leading somewhere.
    let args = serde_json::json!({"a": 1, "b": 2})
        .as_object()
        .unwrap()
        .clone();
    let r = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "add".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();
    assert!(!is_error(&r));

    let m = proxy.metrics_snapshot().await;
    assert_eq!(
        m.search_to_execute, 1,
        "the executed tool came from a search hit"
    );

    // A tool nobody searched for does not count.
    let args = serde_json::json!({"text": "hi"})
        .as_object()
        .unwrap()
        .clone();
    let _ = proxy
        .execute(Parameters(ExecuteParams {
            calls: vec![ExecuteCall {
                server: "stub".into(),
                tool: "echo".into(),
                arguments: Some(args),
            }],
        }))
        .await
        .unwrap();
    let m = proxy.metrics_snapshot().await;
    assert_eq!(
        m.search_to_execute, 1,
        "echo was never surfaced by a search"
    );

    registry.shutdown_all().await;
}

#[tokio::test]
async fn refresh_concurrency_is_capped() {
    // Six backends, cap of 2. Without a cap all six children start at once;
    // with it, at most two are alive at any instant. The stub reports its own
    // PID, but simpler and more direct: count how many the OS is running.
    let Some(py) = python_bin() else { return };
    let mut servers = serde_json::Map::new();
    for i in 0..6 {
        servers.insert(
            format!("stub{i}"),
            serde_json::json!({ "command": py, "args": [stub_path()] }),
        );
    }
    let cfg: Config = serde_json::from_value(serde_json::json!({
        "proxy": { "refresh_concurrency": 2 },
        "mcpServers": servers,
    }))
    .unwrap();

    let registry = Arc::new(Registry::from_config(&cfg));
    let snap = registry.build_snapshot(cfg.snapshot_hash(), None).await;

    // All six still get scanned — the cap paces them, it doesn't drop any.
    assert_eq!(snap.servers.len(), 6, "every server must still be probed");
    for (name, s) in &snap.servers {
        assert_eq!(s.refresh_status, "ok", "{name} should have refreshed");
        // 3: add, echo, secret_tool — no denylist is configured here.
        assert_eq!(s.tools.len(), 3, "{name} should report its tools");
    }

    registry.shutdown_all().await;
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
    let (proxy, registry) =
        make_proxy_with_skills(&cfg, vec![lib.path().display().to_string()]).await;

    let roots = proxy.skill_watch_roots().await;
    let expected = std::path::PathBuf::from(lib.path().display().to_string());
    assert_eq!(
        roots.len(),
        1,
        "expected exactly the configured library: {roots:?}"
    );
    assert!(
        roots.contains(&expected),
        "expected {} in {roots:?}",
        expected.display()
    );

    // Run it from a spawned task too — that is where the panic actually fired.
    let proxy2 = proxy.clone();
    tokio::spawn(async move { proxy2.skill_watch_roots().await })
        .await
        .expect("resolving watch roots inside a spawned task must not panic");

    registry.shutdown_all().await;
}

// ---------------------------------------------------------------------------
// The background watchers.
//
// spawn_hot_reload, spawn_skill_reload and spawn_health_check were called by
// exactly one place — main.rs — and by no test at all. Coverage put src/
// server.rs at 70% with ~291 uncovered lines, and this is most of them.
//
// That gap has already cost real money once: the skill watcher resolved its
// paths with a blocking lock read inside an async task, which panics on the
// spot, so hot-reload never ran for anyone. Nothing failed, nothing logged,
// the feature was simply absent. A test that only calls reload_from_config
// directly — as the existing ones do, deliberately — cannot see that, because
// the bug is in the wiring, not the work.
//
// These drive the real thing: touch a file, wait, assert the proxy noticed.
// Filesystem events are asynchronous and platform-dependent, so they poll
// with a generous ceiling rather than sleeping a fixed amount, and they fail
// loudly rather than skipping when nothing happens.
// ---------------------------------------------------------------------------

/// Poll `check` until it returns true, or fail after `secs`.
///
/// The tests that use this depend on OS filesystem-notification and
/// process-death timing, which `notify` and the kernel deliver on their own
/// schedule and differently across Linux, macOS and Windows. If one of them
/// ever goes flaky in CI, the cause is almost always that timing — not a
/// logic error in the watcher or the health loop. Raise the `secs` ceiling
/// here before suspecting the code under test; these have run green on all
/// three platforms with the current values, so a *new* flake means the
/// environment got slower, not that the assertion got wrong.
async fn wait_until<F, Fut>(secs: u64, what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("timed out after {secs}s waiting for: {what}");
}

#[tokio::test]
async fn config_watcher_rebuilds_the_snapshot_on_a_real_file_edit() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.yaml");
    let write_config = |desc: &str| {
        std::fs::write(
            &cfg_path,
            format!(
                "mcpServers:\n  s:\n    command: [\"/nonexistent/binary\"]\n    description: \"{desc}\"\n"
            ),
        )
        .unwrap();
    };

    // The backend cannot spawn, which is fine and even useful: the server's
    // *description* comes from the config, so it proves the config was
    // re-read without needing a working MCP server on the test machine.
    write_config("before");
    let cfg = Config::load(&cfg_path).unwrap();
    let registry = Registry::from_config(&cfg);
    let snapshot = registry.build_snapshot(cfg.snapshot_hash(), None).await;
    let proxy = Proxy::new(
        registry,
        snapshot,
        dir.path().join("snapshot.json"),
        cfg_path.clone(),
        vec![],
        vec![],
    );

    proxy.clone().spawn_hot_reload();
    // Give the watcher time to arm before touching the file, or the event is
    // missed and the test fails for the wrong reason.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // The watcher gates on mtime, and some filesystems have coarse
    // granularity — make sure the timestamp genuinely differs.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    write_config("after");

    // Asserted through list_mcp_servers rather than the private snapshot
    // field: what matters is that the AGENT sees the new config, which is a
    // stronger claim than an internal struct having been updated.
    wait_until(30, "the config watcher to rebuild the snapshot", || async {
        let json = extract_json(&proxy.list_mcp_servers().await.unwrap());
        json["servers"]
            .as_array()
            .and_then(|a| a.first())
            .map(|s| s["description"] == "after")
            .unwrap_or(false)
    })
    .await;

    proxy.set_shutdown();
}

#[tokio::test]
async fn skill_watcher_picks_up_a_skill_added_on_disk() {
    let lib = make_skill_library();
    let lib_path = lib.path().to_string_lossy().to_string();

    let cfg = config_from(serde_json::json!({}));
    let (proxy, _registry) = make_proxy_with_skills(&cfg, vec![lib_path.clone()]).await;

    let skill_count = |p: &Proxy| {
        let p = p.clone();
        async move {
            extract_json(&p.list_skills().await.unwrap())["count"]
                .as_u64()
                .unwrap_or(0)
        }
    };
    assert_eq!(
        skill_count(&proxy).await,
        2,
        "fixture should start with two skills"
    );

    proxy.clone().spawn_skill_reload();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Add a third skill the way a user would — by dropping a directory in.
    let new_skill = lib.path().join("db-migrator");
    std::fs::create_dir_all(&new_skill).unwrap();
    std::fs::write(
        new_skill.join("SKILL.md"),
        "---\nname: db-migrator\ndescription: Migrate database schemas safely\n---\n\n# DB Migrator\n",
    )
    .unwrap();

    // If the watcher task panicked on startup — the real bug this guards —
    // nothing here ever changes and this fails rather than passing silently.
    wait_until(30, "the skill watcher to notice a new skill", || async {
        extract_json(&proxy.list_skills().await.unwrap())["skills"]
            .as_array()
            .map(|a| a.iter().any(|s| s["name"] == "db-migrator"))
            .unwrap_or(false)
    })
    .await;

    proxy.set_shutdown();
}

/// Kill a process by PID the way an OOM killer or `kill -9` would — no chance
/// for the child to shut its transport down politely.
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status();
}

/// The health-check loop, exercised against the case it exists for: a warm
/// backend whose child dies while nobody is calling it.
///
/// This test could not pass before the active probe landed. `is_closed()`
/// stays false for an idle rmcp client whose child died — measured, ten
/// seconds after a confirmed kill it still reported healthy — so the loop was
/// blind to exactly the failure the README promised it caught. The fix probes
/// with `list_tools`; this proves the loop now notices with no call to help
/// it, and the mutation check below proves the probe is what does it.
#[tokio::test]
async fn health_check_loop_notices_a_backend_killed_while_idle() {
    let Some(py) = python_bin() else { return };
    isolate_health();

    let cfg: Config = serde_json::from_value(serde_json::json!({
        "mcpServers": {
            "stub": {
                "command": py,
                "args": [stub_path()],
                "env": { "STUB_EXTRA_TOOLS": "1" },
            }
        },
        // The loop sleeps this long between sweeps; the 30s default would
        // outlast the test's own ceiling.
        "proxy": { "health_check_secs": 1 }
    }))
    .expect("test config must deserialize");

    let (proxy, registry, _snap) = make_proxy(&cfg).await;
    let backend = registry.get("stub").unwrap();

    // Warm it and learn the child's real PID in the same call. Read the text
    // block properly — scraping digits from a Debug-formatted content array
    // picks up unrelated struct fields and yields a plausible non-PID.
    let result = backend
        .call("pid", None, &registry.proxy)
        .await
        .expect("pid call should succeed");
    let text = result
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .expect("pid tool should return text");
    let pid: u32 = text
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("bad pid {text:?}: {e}"));
    assert!(matches!(
        backend.status().await,
        geco::backend::Status::Up { .. }
    ));

    proxy.clone().spawn_health_check();

    // Kill the child behind geco's back. No call goes through the connection,
    // so nothing but the health loop can notice.
    kill_pid(pid);

    wait_until(
        60,
        "the health-check loop to notice a backend killed while idle",
        || async { matches!(backend.status().await, geco::backend::Status::Failed { .. }) },
    )
    .await;

    proxy.set_shutdown();
    registry.shutdown_all().await;
}

/// The health probe must not kill a backend that is merely BUSY.
///
/// check_health probes with list_tools; a serial MCP server processing a long
/// tool call cannot answer the probe until the call finishes, so the probe
/// times out. An earlier version of the fix treated that timeout as death and
/// reset the connection — which would tear down a live backend mid-call and
/// spawn a duplicate child on every sweep that overlaps a slow call. A dead
/// child fails the probe promptly with a transport error instead, so only
/// that resets. This proves a busy backend survives a sweep and its call
/// still returns.
#[tokio::test]
async fn health_check_leaves_a_busy_backend_alone() {
    let Some(py) = python_bin() else { return };
    isolate_health();

    let cfg: Config = serde_json::from_value(serde_json::json!({
        "mcpServers": {
            "stub": { "command": py, "args": [stub_path()],
                      "env": { "STUB_EXTRA_TOOLS": "1" } }
        },
        // Sweep once a second, with the default 5s probe timeout: several
        // sweeps land during the 7s call, each probe queuing behind it.
        "proxy": { "health_check_secs": 1 }
    }))
    .expect("test config must deserialize");

    let (proxy, registry, _snap) = make_proxy(&cfg).await;
    let backend = registry.get("stub").unwrap();

    // Warm it.
    backend
        .call("pid", None, &registry.proxy)
        .await
        .expect("warm-up call");

    proxy.clone().spawn_health_check();

    // Start a 7s call and, while it runs, let the health loop sweep across it.
    let call_backend = registry.get("stub").unwrap();
    let proxy_cfg = registry.proxy.clone();
    let slow = tokio::spawn(async move {
        let args = serde_json::json!({ "ms": 7000 })
            .as_object()
            .unwrap()
            .clone();
        call_backend.call("sleep", Some(args), &proxy_cfg).await
    });

    // Halfway through, the backend must still be Up — not reset by a probe
    // that timed out behind the in-flight call.
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert!(
        matches!(backend.status().await, geco::backend::Status::Up { .. }),
        "a busy backend must not be marked Failed by the health probe: {:?}",
        backend.status().await
    );

    // And the call itself must complete successfully.
    let result = slow
        .await
        .expect("call task panicked")
        .expect("the slow call must succeed, not be killed by a spurious reset");
    let text = format!("{:?}", result.content);
    assert!(text.contains("slept"), "expected the sleep reply: {text}");
    assert!(
        matches!(backend.status().await, geco::backend::Status::Up { .. }),
        "still Up after the call returns"
    );

    proxy.set_shutdown();
    registry.shutdown_all().await;
}
