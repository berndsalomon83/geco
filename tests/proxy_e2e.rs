//! End-to-end: real child process (python stub), snapshot refresh, lazy
//! execution, allow/deny enforcement. Skips gracefully when python3 is
//! unavailable.

use std::sync::Arc;

use ondemand_mcp::backend::Registry;
use ondemand_mcp::config::Config;

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
    let hits = ondemand_mcp::search::search(&snap, "add numbers", 5);
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
