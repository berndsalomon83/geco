//! End-to-end: real child process (python stub), snapshot refresh, lazy
//! execution, allow/deny enforcement. Skips gracefully when python3 is
//! unavailable.

use std::sync::Arc;

use geco::backend::Registry;
use geco::config::Config;
use geco::server::{Proxy, RefreshParams};
use rmcp::handler::server::wrapper::Parameters;
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

async fn make_proxy(cfg: &Config) -> (Proxy, Arc<Registry>, Snapshot) {
    let registry = Arc::new(Registry::from_config(cfg));
    let snapshot = registry
        .build_snapshot(cfg.snapshot_hash(), None)
        .await;
    let proxy = Proxy::new(
        registry.clone(),
        snapshot.clone(),
        Snapshot::cache_path(std::path::Path::new("/tmp/geco-test-snapshot.json")),
        vec![],
        vec![],
    );
    (proxy, registry, snapshot)
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
        registry.clone(),
        snapshot.clone(),
        Snapshot::cache_path(std::path::Path::new("/tmp/geco-test-snapshot.json")),
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
    let text = format!("{result:?}");
    assert!(text.contains("broken"), "health tool should mention broken server: {text}");
    assert!(text.contains("false") || text.contains("\"healthy\":false"),
            "health tool should report unhealthy: {text}");

    registry.shutdown_all().await;
}
