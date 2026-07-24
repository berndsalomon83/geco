//! The proxy's own MCP surface: eight small meta-tools instead of every
//! backend schema. This is the whole point — the agent's context carries
//! ~4.7 KB no matter how many tools hide behind the gate. That number is
//! measured, not asserted by hand: see
//! `param_tests::meta_tool_surface_stays_within_its_context_budget`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result as AnyhowResult;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rmcp::{
    handler::server::wrapper::Parameters, model::*, service::RequestContext, tool, tool_handler,
    tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use serde_json::json;
use tokio::sync::RwLock;

use crate::backend::Registry;
use crate::health::{self, Issue};
use crate::search;
use crate::skills::{read_skill, SkillIndex};
use crate::snapshot::{short_desc, Snapshot};

// NOTE on the schema annotations below: schemars renders `Option<T>` as a
// `["T","null"]` union type, which some client bridges silently drop —
// leaving the field UNTYPED in the schema the model sees. Untyped fields
// invite stringly-typed guesses ("3" for a count, JSON-in-a-string for an
// object). So every optional field pins a concrete schema type via
// `#[schemars(with = ...)]` (optionality is conveyed by `required` alone),
// and the deserializers are lenient about the guesses clients make anyway.

fn opt_u32_lenient<'de, D>(d: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        N(u32),
        S(String),
    }
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        None => None,
        Some(NumOrStr::N(n)) => Some(n),
        Some(NumOrStr::S(s)) => Some(s.trim().parse().map_err(serde::de::Error::custom)?),
    })
}

/// Accept a JSON object — or the same object serialized into a string,
/// which LLM clients produce constantly.
fn opt_args_lenient<'de, D>(d: D) -> Result<Option<JsonObject>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum ObjOrStr {
        O(JsonObject),
        S(String),
    }
    Ok(match Option::<ObjOrStr>::deserialize(d)? {
        None => None,
        Some(ObjOrStr::O(o)) => Some(o),
        Some(ObjOrStr::S(s)) => {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(serde_json::Value::Object(o)) => Some(o),
                    Ok(other) => {
                        return Err(serde::de::Error::custom(format!(
                            "arguments must be a JSON object, got {other}"
                        )))
                    }
                    Err(e) => {
                        return Err(serde::de::Error::custom(format!(
                            "arguments is a string but not valid JSON: {e}"
                        )))
                    }
                }
            }
        }
    })
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListToolsParams {
    /// Server name from list_mcp_servers
    pub server: String,
    /// Optional: a tool name to get the full input schema for
    #[serde(default)]
    #[schemars(with = "String")]
    pub tool: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// What you're looking for, e.g. "store memory", "create issue", "plan a project"
    pub query: String,
    /// Max results per kind (default 8; skills capped at 20)
    #[serde(default, deserialize_with = "opt_u32_lenient")]
    #[schemars(with = "u32")]
    pub limit: Option<u32>,
    /// Include the full input schema for each tool hit so you can call
    /// execute directly without a separate list_tools call.
    /// Default false — schemas are verbose, so only request when you
    /// plan to execute immediately. (Skills don't have input schemas.)
    #[serde(default)]
    #[schemars(with = "bool")]
    pub include_schema: Option<bool>,
    /// Filter to one kind: "tools", "skills", or omit for both (default).
    #[serde(default)]
    #[schemars(with = "String")]
    pub kind: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExecuteCall {
    /// Server name from list_mcp_servers / search
    pub server: String,
    /// Tool name on that server
    pub tool: String,
    /// Arguments for the tool as a JSON object
    #[serde(default, deserialize_with = "opt_args_lenient")]
    #[schemars(with = "JsonObject")]
    pub arguments: Option<JsonObject>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExecuteParams {
    /// 1–10 tool calls to execute. Cross-backend calls run in parallel.
    /// Results are returned in input order as {server, tool, ok, result/error}.
    /// A single call is just `execute({calls: [{server, tool, arguments}]})`.
    pub calls: Vec<ExecuteCall>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadSkillParams {
    /// Skill name from search (kind="skills")
    pub name: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RefreshParams {
    /// Optional: refresh only this server
    #[serde(default)]
    #[schemars(with = "String")]
    pub server: Option<String>,
}

#[derive(Clone)]
pub struct Proxy {
    registry: Arc<RwLock<Registry>>,
    snapshot: Arc<RwLock<Snapshot>>,
    snapshot_path: PathBuf,
    config_path: PathBuf,
    skills: Arc<RwLock<SkillIndex>>,
    skill_dirs: Arc<RwLock<Vec<String>>>,
    absorb_dirs: Arc<RwLock<Vec<String>>>,
    absorb_issues: Arc<RwLock<Vec<Issue>>>,
    config_issues: Arc<RwLock<Vec<Issue>>>,
    client: Arc<RwLock<Option<String>>>,
    metrics: Arc<RwLock<crate::health::Metrics>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// (server, tool) pairs recently surfaced by `search`, so `execute` can
    /// tell whether search actually led anywhere. Bounded ring buffer — this
    /// is a coarse signal, not an audit trail.
    recent_search_hits: Arc<RwLock<std::collections::VecDeque<(String, String)>>>,
    /// Backends report `tools/list_changed` here. Kept so reload_from_config
    /// can wire up the registry it builds.
    tool_change_tx: tokio::sync::mpsc::Sender<String>,
    /// Receiver, parked until `spawn_tool_change_listener` takes it. Proxy is
    /// Clone and a Receiver is not, hence the Option-in-a-Mutex.
    tool_change_rx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<String>>>>,
}

/// The instruction text every client injects into the model's context on
/// connect. This is always-on cost exactly like the tool schemas, so it is a
/// const rather than an inline literal: the context-budget test measures it.
///
/// It must stay O(1) — a fixed policy, never a catalogue. Anything that grows
/// with the number of servers or skills belongs in a tool RESPONSE, which is
/// paid only when asked for.
const INSTRUCTIONS: &str = "Gateway to multiple MCP servers whose tools load on demand instead of \
     polluting your context. Flow: search (with include_schema=true to get \
     input schemas inline; kind=\"tools\" or \"skills\" to filter) → execute \
     (1–10 calls in one round-trip, cross-backend parallel), or \
     list_mcp_servers → list_tools → execute. Backends start lazily on first execution.";

/// How many (server, tool) hits to remember for the search→execute signal.
const RECENT_HITS_CAP: usize = 64;
/// How many zero-result queries to keep in metrics. Bounded because this ends
/// up in health.json on disk, and it's a diagnostic aid, not a query log.
const RECENT_NO_MATCH_CAP: usize = 20;

fn text_result(v: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|e| format!("serialize error: {e}"));
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Structured error: returns JSON the agent can parse programmatically
/// instead of pattern-matching on prose. Always `CallToolResult::error`
/// with `is_error: true` so the model knows it failed.
fn structured_err(code: &str, message: &str) -> CallToolResult {
    let body = serde_json::to_string_pretty(&json!({
        "error": { "code": code, "message": message }
    }))
    .unwrap_or_else(|e| format!("serialize error: {e}"));
    CallToolResult::error(vec![ContentBlock::text(body)])
}

/// Structured error with extra context fields (server, tool, etc.).
fn structured_err_ctx(code: &str, message: &str, context: serde_json::Value) -> CallToolResult {
    let mut error = json!({ "code": code, "message": message });
    if let serde_json::Value::Object(ctx) = context {
        if let serde_json::Value::Object(ref mut e) = error {
            for (k, v) in ctx {
                e.insert(k, v);
            }
        }
    }
    let body = serde_json::to_string_pretty(&json!({ "error": error }))
        .unwrap_or_else(|e| format!("serialize error: {e}"));
    CallToolResult::error(vec![ContentBlock::text(body)])
}

/// Sync a watcher against the desired set of roots: watch what's new, unwatch
/// what's gone. `desired` is resolved by the caller (it needs async locks —
/// see `Proxy::skill_watch_roots`), so this stays a plain sync function.
fn sync_watches(
    watcher: &mut RecommendedWatcher,
    watched: &mut std::collections::HashSet<PathBuf>,
    desired: &std::collections::HashSet<PathBuf>,
) {
    let to_add: Vec<PathBuf> = desired.difference(watched).cloned().collect();
    for root in to_add {
        match watcher.watch(&root, RecursiveMode::Recursive) {
            Ok(()) => {
                watched.insert(root);
            }
            Err(e) => tracing::warn!("skill watcher: cannot watch {}: {e}", root.display()),
        }
    }
    let to_remove: Vec<PathBuf> = watched.difference(desired).cloned().collect();
    for root in &to_remove {
        let _ = watcher.unwatch(root);
    }
    watched.retain(|d| desired.contains(d));
}

/// Compute the diff between the old and new snapshots for the refresh_tools
/// response. If `server_filter` is set, only that server is diffed.
fn compute_diff(
    old: &Snapshot,
    new: &Snapshot,
    server_filter: &Option<String>,
) -> serde_json::Value {
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = 0u64;

    let servers: Box<
        dyn Iterator<
            Item = (
                &String,
                &crate::snapshot::ServerSnapshot,
                &crate::snapshot::ServerSnapshot,
            ),
        >,
    > = if let Some(name) = server_filter {
        match (old.servers.get(name), new.servers.get(name)) {
            (Some(o), Some(n)) => Box::new(std::iter::once((name, o, n))),
            (None, Some(n)) => {
                // New server — all tools are "added".
                for t in &n.tools {
                    added.push(json!({"server": name, "tool": t.name}));
                }
                return json!({"added": added, "removed": removed, "changed": changed, "unchanged": 0});
            }
            (Some(o), None) => {
                // Removed server — all tools are "removed".
                for t in &o.tools {
                    removed.push(json!({"server": name, "tool": t.name}));
                }
                return json!({"added": added, "removed": removed, "changed": changed, "unchanged": 0});
            }
            (None, None) => {
                return json!({"added": [], "removed": [], "changed": [], "unchanged": 0})
            }
        }
    } else {
        Box::new(
            new.servers
                .iter()
                .filter_map(|(name, n)| old.servers.get(name).map(|o| (name, o, n))),
        )
    };

    for (name, old_entry, new_entry) in servers {
        let old_tools: std::collections::HashMap<&str, &crate::snapshot::ToolSnapshot> = old_entry
            .tools
            .iter()
            .map(|t| (t.name.as_str(), t))
            .collect();
        let new_tools: std::collections::HashMap<&str, &crate::snapshot::ToolSnapshot> = new_entry
            .tools
            .iter()
            .map(|t| (t.name.as_str(), t))
            .collect();

        // Tools in new but not old = added.
        for tname in new_tools.keys() {
            if !old_tools.contains_key(tname) {
                added.push(json!({"server": name, "tool": tname}));
            }
        }
        // Tools in old but not new = removed.
        for tname in old_tools.keys() {
            if !new_tools.contains_key(tname) {
                removed.push(json!({"server": name, "tool": tname}));
            }
        }
        // Tools in both = check for changes.
        for (tname, old_t) in &old_tools {
            if let Some(new_t) = new_tools.get(tname) {
                if old_t.description != new_t.description
                    || old_t.input_schema != new_t.input_schema
                {
                    changed.push(json!({"server": name, "tool": tname}));
                } else {
                    unchanged += 1;
                }
            }
        }
    }

    // For a full refresh, also check for removed and added servers.
    if server_filter.is_none() {
        for (name, old_entry) in &old.servers {
            if !new.servers.contains_key(name) {
                for t in &old_entry.tools {
                    removed.push(json!({"server": name, "tool": t.name}));
                }
            }
        }
        for (name, new_entry) in &new.servers {
            if !old.servers.contains_key(name) {
                for t in &new_entry.tools {
                    added.push(json!({"server": name, "tool": t.name}));
                }
            }
        }
    }

    json!({
        "added": added,
        "removed": removed,
        "changed": changed,
        "unchanged": unchanged,
    })
}

#[tool_router]
impl Proxy {
    pub fn new(
        registry: Registry,
        snapshot: Snapshot,
        snapshot_path: PathBuf,
        config_path: PathBuf,
        skill_dirs: Vec<String>,
        absorb_dirs: Vec<String>,
    ) -> Self {
        let (skills, absorb_issues) = Self::absorb_and_scan(&skill_dirs, &absorb_dirs);
        let metrics = crate::health::Metrics {
            started_at: crate::backend::now(),
            ..Default::default()
        };
        // Wire tool-change notifications before the registry disappears behind
        // a lock. Small queue on purpose: notifications coalesce, because one
        // refresh subsumes any number of them.
        let (tool_change_tx, tool_change_rx) = tokio::sync::mpsc::channel::<String>(16);
        registry.set_tool_change_notifier(tool_change_tx.clone());
        Self {
            registry: Arc::new(RwLock::new(registry)),
            snapshot: Arc::new(RwLock::new(snapshot)),
            snapshot_path,
            config_path,
            skills: Arc::new(RwLock::new(skills)),
            skill_dirs: Arc::new(RwLock::new(skill_dirs)),
            absorb_dirs: Arc::new(RwLock::new(absorb_dirs)),
            absorb_issues: Arc::new(RwLock::new(absorb_issues)),
            config_issues: Arc::new(RwLock::new(Vec::new())),
            client: Arc::new(RwLock::new(None)),
            metrics: Arc::new(RwLock::new(metrics)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            recent_search_hits: Arc::new(RwLock::new(std::collections::VecDeque::new())),
            tool_change_tx,
            tool_change_rx: Arc::new(std::sync::Mutex::new(Some(tool_change_rx))),
        }
    }

    /// Watch for backends reporting `tools/list_changed` and re-scan just that
    /// server. Without this the snapshot silently drifts: a backend that adds
    /// or removes a tool at runtime stays wrong until someone runs
    /// `refresh_tools`, and nothing anywhere reports that it happened.
    pub fn spawn_tool_change_listener(self) {
        let Some(mut rx) = self
            .tool_change_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            tracing::warn!("tool-change listener already started; ignoring");
            return;
        };
        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                if self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                // Coalesce a burst: a server that rebuilds its tool list may
                // notify several times, and re-scanning once per notification
                // would spawn a connection each time.
                let mut pending: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                pending.insert(first);
                tokio::time::sleep(Duration::from_millis(200)).await;
                while let Ok(more) = rx.try_recv() {
                    pending.insert(more);
                }
                for server in pending {
                    self.refresh_server(&server).await;
                }
            }
        });
    }

    /// Re-scan one backend's tools and merge the result into the snapshot.
    /// Used by the tool-change listener; `refresh_tools` has its own path
    /// because it also reports a diff.
    async fn refresh_server(&self, name: &str) {
        let hash = { self.snapshot.read().await.config_hash.clone() };
        let fresh = {
            let registry = self.registry.read().await.clone();
            registry.build_snapshot(hash, Some(name)).await
        };
        let Some(entry) = fresh.servers.get(name).cloned() else {
            // Server vanished from the registry between notification and
            // refresh (a hot-reload removed it) — nothing to update.
            return;
        };
        let (before, after) = {
            let mut snap = self.snapshot.write().await;
            let before = snap.servers.get(name).map(|s| s.tools.len()).unwrap_or(0);
            let after = entry.tools.len();
            snap.generated_at = fresh.generated_at.clone();
            snap.servers.insert(name.to_string(), entry);
            (before, after)
        };
        tracing::info!("tools/list_changed: re-scanned '{name}', {before} -> {after} tools");
        let to_save = self.snapshot.read().await.clone();
        if let Err(e) = to_save.save(&self.snapshot_path) {
            tracing::warn!("cannot persist snapshot after tool change: {e:#}");
        }
        self.publish_health().await;
    }

    /// Spawn a background task that watches the config file for changes and
    /// auto-reloads: rebuilds the Registry, recomputes the snapshot, and
    /// publishes health. Debounced to 500ms to coalesce editor write patterns
    /// (temp-file + rename fires multiple events).
    pub fn spawn_hot_reload(self) {
        let config_path = self.config_path.clone();
        let proxy = self.clone();
        let shutdown = self.shutdown.clone();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);
        let tx_clone = tx.clone();

        let mut watcher = match RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                if let Ok(event) = res {
                    if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                        let _ = tx_clone.blocking_send(());
                    }
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("config watcher disabled: {e}");
                return;
            }
        };

        // Watch the parent directory (editors write temp files then rename,
        // which shows up as events on the dir, not the file itself).
        let mut watch_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        if watch_dir.as_os_str().is_empty() {
            watch_dir = PathBuf::from(".");
        }
        if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
            tracing::warn!("config watcher disabled: {e}");
            return;
        }

        // Leak the watcher so it lives for the process lifetime — the proxy
        // runs until the agent disconnects, and dropping the watcher would
        // stop watching. This is a single long-lived watcher per process.
        std::mem::forget(watcher);

        tokio::spawn(async move {
            // Seed from the config's current mtime. Starting at None would
            // make the very first event pass the "did it actually change?"
            // check unconditionally — and since we watch the whole parent
            // directory (which also holds geco.schema.json, editor swap
            // files, …), an unrelated write there would trigger a full
            // reload and re-spawn every backend exactly once per process.
            let mut last_mtime: Option<std::time::SystemTime> = std::fs::metadata(&config_path)
                .and_then(|m| m.modified())
                .ok();
            loop {
                if proxy.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                if rx.recv().await.is_none() {
                    break;
                }
                // Drain any burst of events (editors write temp + rename,
                // firing multiple events in quick succession).
                while rx.try_recv().is_ok() {}

                // Verify the config file actually changed (mtime check filters
                // out events on sibling files in the watched directory).
                let mtime = match std::fs::metadata(&config_path).and_then(|m| m.modified()) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if last_mtime == Some(mtime) {
                    continue;
                }
                last_mtime = Some(mtime);

                // Reload config, rebuild registry, rebuild snapshot.
                let cfg = match crate::config::Config::load(&config_path) {
                    Ok(c) => c,
                    Err(e) => {
                        let msg = format!("{e:#}");
                        tracing::warn!("config reload failed (keeping old config): {msg}");
                        *proxy.config_issues.write().await = vec![Issue {
                            kind: "config".into(),
                            subject: "reload".into(),
                            detail: format!("parse failed (keeping old config): {msg}"),
                            severity: "error".into(),
                        }];
                        proxy.publish_health().await;
                        continue;
                    }
                };
                if let Err(e) = proxy.reload_from_config(cfg).await {
                    let msg = format!("{e:#}");
                    tracing::warn!("hot-reload: reload failed: {msg}");
                    *proxy.config_issues.write().await = vec![Issue {
                        kind: "config".into(),
                        subject: "reload".into(),
                        detail: format!("reload failed (keeping old config): {msg}"),
                        severity: "error".into(),
                    }];
                    proxy.publish_health().await;
                } else {
                    // Successful reload — reload_from_config already cleared
                    // config_issues and published health via rescan_skills.
                }
            }
        });
    }

    /// Spawn a background task that periodically checks if warm backends
    /// are still alive. When a dead connection is detected (child process
    /// exited), the connection is reset so the next `execute` reconnects
    /// fresh instead of erroring. Interval is `proxy.health_check_secs`;
    /// 0 disables the check entirely.
    pub fn spawn_health_check(self) {
        let proxy = self.clone();
        tokio::spawn(async move {
            loop {
                if proxy.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let interval = {
                    let registry = proxy.registry.read().await;
                    registry.proxy.health_check_secs
                };
                if interval == 0 {
                    // Disabled — sleep and re-check so hot-reload can re-enable.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;

                let registry = proxy.registry.read().await;
                let backends: Vec<_> = registry.backends.values().cloned().collect();
                drop(registry);

                let mut dead = Vec::new();
                for backend in &backends {
                    if !backend.check_health().await {
                        dead.push(backend.name.clone());
                    }
                }
                if !dead.is_empty() {
                    proxy.publish_health().await;
                    tracing::info!(
                        "health check: reset {} dead backend(s): {:?}",
                        dead.len(),
                        dead
                    );
                }
            }
        });
    }

    /// Spawn a background task that watches the skill directories and absorb
    /// directories for changes. When a SKILL.md is added, edited, or removed
    /// (or a new skill is dropped into an absorb dir), re-runs absorb + scan
    /// so the gateway picks it up without a config touch or a manual
    /// `refresh_tools`. Watches recursively; editor temp-file + rename bursts
    /// are drained (debounced) before the rescan.
    ///
    /// Re-evaluates watch roots after each event cycle so that new dirs added
    /// to `skills.dirs` / `skills.absorb` via hot-reload are picked up without
    /// restarting the proxy.
    pub fn spawn_skill_reload(self) {
        let proxy = self.clone();
        let shutdown = self.shutdown.clone();

        tokio::spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);
            let tx_clone = tx.clone();
            let mut watcher = match RecommendedWatcher::new(
                move |res: notify::Result<notify::Event>| {
                    if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(event) = res {
                        if matches!(
                            event.kind,
                            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                        ) {
                            let _ = tx_clone.blocking_send(());
                        }
                    }
                },
                notify::Config::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("skill watcher disabled: {e}");
                    return;
                }
            };

            // Track which roots we're currently watching so we can add new
            // ones and remove stale ones when hot-reload changes the config.
            // The desired set is resolved via `skill_watch_roots().await` —
            // never with a blocking lock read, which panics inside a task.
            use std::collections::HashSet;
            let mut watched: HashSet<PathBuf> = HashSet::new();
            sync_watches(&mut watcher, &mut watched, &proxy.skill_watch_roots().await);

            // Don't leak the watcher — keep it in the task scope so we can
            // reconfigure it. The task lives for the process lifetime.
            loop {
                if proxy.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }

                // If we're not watching anything, poll for new dirs periodically
                // instead of blocking on recv (which would never fire).
                if watched.is_empty() {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    sync_watches(&mut watcher, &mut watched, &proxy.skill_watch_roots().await);
                    continue;
                }

                if rx.recv().await.is_none() {
                    break;
                }
                // Drain any burst of events (editors write temp + rename,
                // firing multiple events in quick succession).
                while rx.try_recv().is_ok() {}

                // Small debounce window so a multi-file editor operation
                // (e.g. git checkout touching several skills at once) coalesces
                // into a single rescan.
                tokio::time::sleep(Duration::from_millis(200)).await;
                while rx.try_recv().is_ok() {}

                proxy.rescan_skills().await;
                tracing::info!("skill watcher: rescan complete");

                // Re-evaluate watch roots in case hot-reload changed the config.
                sync_watches(&mut watcher, &mut watched, &proxy.skill_watch_roots().await);
            }
        });
    }

    /// The directories the skill watcher should currently be watching:
    /// every configured skill/absorb dir that exists on disk. Async because
    /// the dir lists live behind the same locks hot-reload writes to.
    pub async fn skill_watch_roots(&self) -> std::collections::HashSet<PathBuf> {
        let skill_dirs = self.skill_dirs.read().await.clone();
        let absorb_dirs = self.absorb_dirs.read().await.clone();
        skill_dirs
            .iter()
            .chain(absorb_dirs.iter())
            .map(|d| crate::skills::expand_tilde(d))
            .filter(|d| d.exists())
            .collect()
    }

    /// Reload config: rebuild registry, snapshot, skills, and health.
    /// Called by `spawn_hot_reload` when the config file changes, but also
    /// testable in isolation.
    pub async fn reload_from_config(&self, cfg: crate::config::Config) -> AnyhowResult<()> {
        let new_hash = cfg.snapshot_hash();
        let new_registry = crate::backend::Registry::from_config(&cfg);
        // Fresh Backends start with no notifier — rewire them, or tool-change
        // notifications stop working silently after the first hot-reload.
        new_registry.set_tool_change_notifier(self.tool_change_tx.clone());

        // Collect removed backends' Arcs before swapping the registry.
        // We shut them down after the swap to avoid a TOCTOU window where
        // execute could re-obtain a just-shut-down backend from the old registry.
        let removed_backends: Vec<(String, std::sync::Arc<crate::backend::Backend>)> = {
            let old_registry = self.registry.read().await;
            old_registry
                .backends
                .iter()
                .filter(|(name, _)| !new_registry.backends.contains_key(*name))
                .map(|(n, b)| (n.clone(), b.clone()))
                .collect()
        };

        // Update skill dirs from the new config so hot-reload picks up
        // changes to skills.dirs / skills.absorb.
        *self.skill_dirs.write().await = cfg.skills.dirs.clone();
        *self.absorb_dirs.write().await = cfg.skills.absorb.clone();

        // Swap the registry atomically.
        *self.registry.write().await = new_registry;

        // Shut down removed backends after the swap. In-flight execute calls
        // holding old Arcs will reconnect on their next call (conn was reset).
        for (name, backend) in &removed_backends {
            backend.shutdown().await;
            tracing::info!("hot-reload: shut down removed backend '{name}'");
        }

        // Rebuild snapshot — clone the registry and drop the read lock so
        // execute / list_mcp_servers aren't blocked during the build.
        let snap = {
            let registry = self.registry.read().await.clone();
            registry.build_snapshot(new_hash, None).await
        };
        tracing::info!(
            "hot-reload: config changed, refreshed {} servers, {} tools",
            snap.servers.len(),
            snap.tool_count()
        );

        *self.snapshot.write().await = snap.clone();
        if let Err(e) = snap.save(&self.snapshot_path) {
            tracing::warn!("hot-reload: cannot persist snapshot: {e:#}");
        }

        // Clear any prior config-reload issue BEFORE rescan_skills, which
        // calls publish_health() — otherwise the health file is written
        // with the stale error still present.
        let mut issues = self.config_issues.write().await;
        let had_issues = !issues.is_empty();
        if had_issues {
            issues.clear();
        }
        drop(issues);

        self.rescan_skills().await;
        Ok(())
    }

    /// Mark the proxy as shutting down. Background watchers (hot-reload,
    /// health-check, skill-reload) check this flag and exit their loops.
    pub fn set_shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the config-reload issue (used by the hot-reload watcher and
    /// exposed for tests so the health surface can be verified without
    /// triggering the OS-dependent file watcher).
    pub async fn set_config_issue(&self, issue: Option<Issue>) {
        *self.config_issues.write().await = issue.map(|i| vec![i]).unwrap_or_default();
    }

    /// Shut down all backend processes. Called when the proxy is stopping.
    pub async fn shutdown_backends(&self) {
        self.set_shutdown();
        let registry = self.registry.read().await;
        registry.shutdown_all().await;
    }

    /// Recompute the full problem set and publish it to the health file.
    pub async fn publish_health(&self) {
        let issues = self.all_issues().await;
        let client = self.client.read().await.clone();
        let metrics = self.metrics.read().await.clone();
        health::write(client.as_deref(), issues, Some(metrics));
    }

    /// Snapshot of the current metrics (for testing).
    pub async fn metrics_snapshot(&self) -> crate::health::Metrics {
        self.metrics.read().await.clone()
    }

    /// The current full issue set (backends + absorb) — used by the
    /// `health` meta-tool and `publish_health` so they always agree.
    /// Checks both the snapshot's refresh_status AND the live backend
    /// status (a warm backend that crashed after a successful snapshot
    /// has refresh_status="ok" but Status::Failed).
    async fn all_issues(&self) -> Vec<Issue> {
        // Clone what we need and drop locks before awaiting backend
        // status() — holding the registry read lock across async status
        // checks would block hot-reload (which needs a write lock).
        let (servers, absorb, config) = {
            let snap = self.snapshot.read().await;
            let absorb = self.absorb_issues.read().await.clone();
            let config = self.config_issues.read().await.clone();
            let servers: Vec<_> = snap
                .servers
                .iter()
                .map(|(n, s)| (n.clone(), s.refresh_status.clone()))
                .collect();
            (servers, absorb, config)
        };
        let registry = {
            let guard = self.registry.read().await;
            guard.clone()
        };
        let mut issues = absorb;
        issues.extend(config);
        for (name, refresh_status) in &servers {
            let snapshot_ok = refresh_status == "ok";
            if !snapshot_ok {
                issues.push(Issue {
                    kind: "backend".into(),
                    subject: name.clone(),
                    detail: refresh_status.clone(),
                    severity: "error".into(),
                });
            }
            if snapshot_ok {
                if let Ok(backend) = registry.get(name) {
                    if matches!(
                        backend.status().await,
                        crate::backend::Status::Failed { .. }
                    ) {
                        issues.push(Issue {
                            kind: "backend".into(),
                            subject: name.clone(),
                            detail: "connection lost (child process exited)".into(),
                            severity: "error".into(),
                        });
                    }
                }
            }
        }
        issues
    }

    fn absorb(skill_dirs: &[String], absorb_dirs: &[String]) -> Vec<Issue> {
        if absorb_dirs.is_empty() {
            return Vec::new();
        }
        let Some(library) = skill_dirs.first() else {
            tracing::warn!(
                "skills.absorb configured but skills.dirs is empty — nothing to absorb into"
            );
            return Vec::new();
        };
        let (moved, errors) = crate::skills::absorb_into(library, absorb_dirs);
        if !moved.is_empty() {
            tracing::info!(
                "absorbed {} stray skill(s) into the library: {}",
                moved.len(),
                moved.join(", ")
            );
        }
        errors
            .into_iter()
            .map(|e| {
                tracing::warn!("skill absorb: {e}");
                Issue {
                    kind: "skills".into(),
                    subject: "absorb".into(),
                    detail: e,
                    severity: "warning".into(),
                }
            })
            .collect()
    }

    /// Run absorb + skill scan and return (SkillIndex, issues).
    /// Used by new(), reload_from_config(), and refresh_tools() to avoid
    /// duplicating the absorb-then-scan-then-collect-errors pattern.
    fn absorb_and_scan(skill_dirs: &[String], absorb_dirs: &[String]) -> (SkillIndex, Vec<Issue>) {
        let mut issues = Self::absorb(skill_dirs, absorb_dirs);
        let scanned = SkillIndex::scan(skill_dirs);
        for e in &scanned.errors {
            tracing::warn!("skill scan error: {e}");
            issues.push(Issue {
                kind: "skills".into(),
                subject: "scan".into(),
                detail: e.clone(),
                severity: "warning".into(),
            });
        }
        (scanned, issues)
    }

    /// Re-run absorb + scan and swap the results into `self.skills` /
    /// `self.absorb_issues`, then publish health. Used by reload_from_config,
    /// refresh_tools, and spawn_skill_reload so the three callers never drift.
    /// The absorb + scan is sync filesystem I/O, so it runs on a blocking
    /// thread to avoid stalling the async runtime.
    pub async fn rescan_skills(&self) {
        let skill_dirs = self.skill_dirs.read().await.clone();
        let absorb_dirs = self.absorb_dirs.read().await.clone();
        let (scanned, issues) =
            tokio::task::spawn_blocking(move || Self::absorb_and_scan(&skill_dirs, &absorb_dirs))
                .await
                .unwrap_or_else(|e| {
                    tracing::error!("rescan_skills: blocking task panicked: {e}");
                    (SkillIndex::default(), Vec::new())
                });
        *self.absorb_issues.write().await = issues;
        *self.skills.write().await = scanned;
        self.publish_health().await;
    }

    #[tool(
        description = "List the MCP servers behind this proxy: name, what it's for, tool count, running state, and a preview of up to 5 tool names. Start here."
    )]
    pub async fn list_mcp_servers(&self) -> Result<CallToolResult, McpError> {
        let servers: Vec<_> = {
            let snap = self.snapshot.read().await;
            snap.servers.iter()
                .map(|(name, s)| {
                    let tool_preview: Vec<_> = s.tools.iter().take(5)
                        .map(|t| json!({"tool": t.name, "description": short_desc(&t.description, 80)}))
                        .collect();
                    (name.clone(), s.description.clone(), s.tools.len(), s.refresh_status.clone(), tool_preview)
                })
                .collect()
        };
        let registry = {
            let guard = self.registry.read().await;
            guard.clone()
        };
        let mut out = Vec::new();
        for (name, desc, tools, last_refresh, tool_preview) in &servers {
            let status = match registry.get(name) {
                Ok(b) => serde_json::to_value(b.status().await).unwrap_or(json!("unknown")),
                Err(_) => json!("removed from config"),
            };
            out.push(json!({
                "server": name,
                "description": desc,
                "tools": tools,
                "tool_preview": tool_preview,
                "status": status,
                "last_refresh": last_refresh,
            }));
        }
        Ok(text_result(json!(out)))
    }

    #[tool(
        description = "List a server's tools (name + one-line description). Pass `tool` to get that tool's full input schema before executing it."
    )]
    pub async fn list_tools(
        &self,
        Parameters(p): Parameters<ListToolsParams>,
    ) -> Result<CallToolResult, McpError> {
        let snap = self.snapshot.read().await;
        let Some(server) = snap.servers.get(&p.server) else {
            return Ok(structured_err_ctx(
                "unknown_server",
                &format!("server '{}' not found", p.server),
                json!({ "server": p.server }),
            ));
        };
        match p.tool {
            Some(tool_name) => match snap.find_tool(&p.server, &tool_name) {
                Some(t) => Ok(text_result(json!({
                    "server": p.server,
                    "tool": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }))),
                None => Ok(structured_err_ctx(
                    "unknown_tool",
                    &format!("tool '{tool_name}' not found on server '{}'", p.server),
                    json!({ "server": p.server, "tool": tool_name }),
                )),
            },
            None => {
                let tools: Vec<_> = server
                    .tools
                    .iter()
                    .map(
                        |t| json!({"tool": t.name, "description": short_desc(&t.description, 140)}),
                    )
                    .collect();
                Ok(text_result(json!({
                    "server": p.server,
                    "tools": tools,
                    "hint": "list_tools with `tool` gives the input schema; execute runs it",
                })))
            }
        }
    }

    #[tool(
        description = "Find tools AND skills by keywords in one call. Returns {results, count} where each result is tagged by kind (\"tool\" or \"skill\"), best match first; count 0 means nothing matched (not an error). For tool hits, set include_schema=true to get each tool's full input schema inline — saves a list_tools call. Use kind=\"tools\" or kind=\"skills\" to search only one surface."
    )]
    pub async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = p.kind.as_deref().unwrap_or("");
        if !kind.is_empty() && kind != "tools" && kind != "skills" {
            return Ok(structured_err_ctx(
                "invalid_kind",
                &format!(
                    "kind must be \"tools\", \"skills\", or omitted (got \"{}\")",
                    kind
                ),
                json!({ "kind": kind }),
            ));
        }
        let limit = p.limit.unwrap_or(8).clamp(1, 25) as usize;
        let include_schema = p.include_schema.unwrap_or(false);
        let mut results = Vec::new();

        if kind != "skills" {
            let snap = self.snapshot.read().await;
            let tool_hits = search::search(&snap, &p.query, limit);
            for h in &tool_hits {
                let mut entry = json!({
                    "kind": "tool",
                    "server": h.server,
                    "name": h.tool,
                    "description": h.description,
                });
                if include_schema {
                    if let Some(t) = snap.find_tool(&h.server, &h.tool) {
                        entry["input_schema"] = t.input_schema.clone();
                    }
                }
                results.push(entry);
            }
            drop(snap);
        }

        let mut skills_configured = true;
        if kind != "tools" {
            let idx = self.skills.read().await;
            skills_configured = !idx.skills.is_empty();
            if skills_configured {
                let skill_limit = if limit > 20 { 20 } else { limit };
                let skill_hits = idx.search(&p.query, skill_limit);
                for s in &skill_hits {
                    results.push(json!({
                        "kind": "skill",
                        "name": s.name,
                        "description": short_desc(&s.description, 160),
                    }));
                }
            }
        }

        // Record what search did, so its effectiveness is measurable in
        // production and not only against the eval corpus.
        let surfaced_skill = results.iter().any(|r| r["kind"] == "skill");
        {
            let mut m = self.metrics.write().await;
            m.searches += 1;
            if surfaced_skill {
                m.skill_surfaces += 1;
            }
            if results.is_empty() {
                m.searches_no_match += 1;
                // Keep the misses: each is either a tool that doesn't exist
                // or one whose description doesn't say what it's for.
                m.recent_no_match.push(p.query.clone());
                let overflow = m.recent_no_match.len().saturating_sub(RECENT_NO_MATCH_CAP);
                if overflow > 0 {
                    m.recent_no_match.drain(0..overflow);
                }
            }
        }
        if !results.is_empty() {
            let mut recent = self.recent_search_hits.write().await;
            for r in &results {
                if r["kind"] == "tool" {
                    if let (Some(s), Some(t)) = (r["server"].as_str(), r["name"].as_str()) {
                        recent.push_back((s.to_string(), t.to_string()));
                    }
                }
            }
            while recent.len() > RECENT_HITS_CAP {
                recent.pop_front();
            }
        }

        // Finding nothing is a valid answer, not a failure — this used to
        // return is_error, which tells the model the call itself broke.
        if results.is_empty() {
            let hint = if !skills_configured && kind == "skills" {
                "no skill libraries are configured — set `skills.dirs` in the geco config"
                    .to_string()
            } else {
                format!(
                    "nothing matched '{}' — try fewer or more general keywords, \
                     or list_mcp_servers to browse what's available",
                    p.query
                )
            };
            return Ok(text_result(
                json!({ "results": [], "count": 0, "hint": hint }),
            ));
        }
        let count = results.len();
        Ok(text_result(json!({ "results": results, "count": count })))
    }

    #[tool(
        description = "Execute 1–10 tool calls. Cross-backend calls run in parallel. Results returned in input order as {server, tool, ok, result/error}. A single call: execute({calls: [{server, tool, arguments}]})."
    )]
    pub async fn execute(
        &self,
        Parameters(p): Parameters<ExecuteParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.calls.is_empty() {
            return Ok(structured_err(
                "empty_calls",
                "no calls — pass at least one {server, tool, arguments}",
            ));
        }
        if p.calls.len() > 10 {
            return Ok(structured_err(
                "too_many_calls",
                &format!("{} calls (max 10)", p.calls.len()),
            ));
        }
        let n = p.calls.len();
        let call_names: Vec<(String, String)> = p
            .calls
            .iter()
            .map(|c| (c.server.clone(), c.tool.clone()))
            .collect();
        let (registry, proxy_cfg) = {
            let guard = self.registry.read().await;
            (guard.clone(), guard.proxy.clone())
        };
        // Did search lead here? Snapshot the recent hits before running, so a
        // concurrent search can't retroactively take credit.
        let from_search: std::collections::HashSet<(String, String)> = {
            let recent = self.recent_search_hits.read().await;
            let wanted: std::collections::HashSet<(String, String)> =
                call_names.iter().cloned().collect();
            recent
                .iter()
                .filter(|p| wanted.contains(*p))
                .cloned()
                .collect()
        };

        let mut join = tokio::task::JoinSet::new();
        for (i, call) in p.calls.into_iter().enumerate() {
            let registry = registry.clone();
            let proxy_cfg = proxy_cfg.clone();
            join.spawn(async move {
                let start = std::time::Instant::now();
                let backend = match registry.get(&call.server) {
                    Ok(b) => b,
                    Err(_) => {
                        return (i, json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": false,
                            "error": { "code": "unknown_server", "message": format!("server '{}' not found", call.server) },
                        }), start.elapsed().as_millis() as u64, false);
                    }
                };
                let was_down = !matches!(backend.status().await, crate::backend::Status::Up { .. });
                match backend.call(&call.tool, call.arguments, &proxy_cfg).await {
                    Ok(result) => {
                        let elapsed = start.elapsed().as_millis() as u64;
                        // Flatten text blocks for the convenience "result" field,
                        // but also include the full structured "content" array so
                        // non-text blocks (images, audio, resources) survive —
                        // e.g. Playwright screenshots return ImageContent.
                        let text = result.content.iter()
                            .filter_map(|b| {
                                if let ContentBlock::Text(t) = b { Some(t.text.as_str()) }
                                else { None }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let has_non_text = result.content.iter().any(|b| {
                            !matches!(b, ContentBlock::Text(_))
                        });
                        let ok = !result.is_error.unwrap_or(false);
                        let mut entry = json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": ok,
                            "result": text,
                        });
                        // Pass through the full content array when non-text
                        // blocks are present so images/resources aren't lost.
                        if has_non_text {
                            entry["content"] = serde_json::to_value(&result.content)
                                .unwrap_or(json!([]));
                        }
                        if !ok {
                            entry["error"] = json!({
                                "code": "tool_error",
                                "message": text,
                            });
                        }
                        (i, entry, elapsed, was_down)
                    }
                    Err(e) => {
                        let elapsed = start.elapsed().as_millis() as u64;
                        let msg = format!("{e:#}");
                        let code = if msg.starts_with("DENY:") { "not_allowed" }
                                   else if msg.contains("timed out") { "timeout" }
                                   else { "backend_error" };
                        (i, json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": false,
                            "error": { "code": code, "message": msg },
                        }), elapsed, was_down)
                    }
                }
            });
        }

        let mut results: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                let (server, tool) = &call_names[i];
                json!({
                    "server": server,
                    "tool": tool,
                    "ok": false,
                    "error": {"code": "internal_error", "message": "task panicked"}
                })
            })
            .collect();
        let mut needs_health_update = false;
        while let Some(joined) = join.join_next().await {
            match joined {
                Ok((i, result, latency_ms, was_down)) => {
                    results[i] = result;
                    let server = results[i]["server"].as_str().unwrap_or("").to_string();
                    let ok = results[i]["ok"].as_bool().unwrap_or(false);
                    let code = results[i]["error"]["code"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    // If a previously-down backend's status changed (success
                    // or failure), update health so the TUI plugin stays in sync.
                    if was_down {
                        needs_health_update = true;
                    }
                    // A timeout or transport error on a previously-up backend
                    // flips it to Status::Failed — surface that to health too,
                    // otherwise the TUI toast misses the most common failure mode.
                    if !ok && matches!(code.as_str(), "timeout" | "backend_error") {
                        needs_health_update = true;
                    }
                    let tool = results[i]["tool"].as_str().unwrap_or("").to_string();
                    let mut metrics = self.metrics.write().await;
                    metrics.total_calls += 1;
                    if !ok {
                        metrics.total_errors += 1;
                    }
                    if from_search.contains(&(server.clone(), tool)) {
                        metrics.search_to_execute += 1;
                    }
                    // Don't create a per-backend row for a server that doesn't
                    // exist — a typo'd name would otherwise show up as a
                    // permanent phantom backend in `geco stats` forever.
                    if code.as_str() != "unknown_server" {
                        let entry = metrics.per_backend.entry(server.clone()).or_default();
                        entry.calls += 1;
                        if !ok {
                            entry.errors += 1;
                        }
                        if was_down {
                            entry.cold_starts += 1;
                        }
                        entry.total_latency_ms = entry.total_latency_ms.saturating_add(latency_ms);
                        entry.last_call_at = Some(crate::backend::now());
                    }
                }
                Err(e) => {
                    tracing::error!("execute: task panicked: {e}");
                }
            }
        }
        if needs_health_update {
            self.publish_health().await;
        }
        Ok(text_result(json!(results)))
    }

    #[tool(
        description = "List every available agent skill (name + one-line description). Use when asked what skills exist; use search with kind=\"skills\" for targeted lookup."
    )]
    pub async fn list_skills(&self) -> Result<CallToolResult, McpError> {
        let idx = self.skills.read().await;
        if idx.skills.is_empty() {
            return Ok(structured_err(
                "no_skills",
                "no skill libraries configured (configure `skills.dirs`)",
            ));
        }
        let out: Vec<_> = idx
            .skills
            .iter()
            .map(|s| {
                let mut entry =
                    json!({"name": s.name, "description": short_desc(&s.description, 100)});
                if let Some(v) = &s.version {
                    entry["version"] = json!(v);
                }
                entry
            })
            .collect();
        Ok(text_result(json!({"skills": out, "count": out.len()})))
    }

    #[tool(
        description = "Load a skill's full instructions (SKILL.md) and its file inventory. Follow the instructions; run bundled scripts via your shell at the listed absolute paths."
    )]
    pub async fn read_skill(
        &self,
        Parameters(p): Parameters<ReadSkillParams>,
    ) -> Result<CallToolResult, McpError> {
        let meta = {
            let idx = self.skills.read().await;
            idx.get(&p.name).cloned()
        };
        let Some(meta) = meta else {
            return Ok(structured_err_ctx(
                "unknown_skill",
                &format!("skill '{}' not found", p.name),
                json!({ "name": p.name }),
            ));
        };
        // read_skill does sync filesystem I/O (read_to_string + recursive
        // collect_files) — run it on a blocking thread.
        let result = tokio::task::spawn_blocking(move || read_skill(&meta))
            .await
            .map_err(|e| McpError::internal_error(format!("read_skill task failed: {e}"), None))?;
        match result {
            Ok((body, files)) => {
                self.metrics.write().await.skill_reads += 1;
                let tree = if files.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n\n## Bundled files (absolute paths)\n{}",
                        files.join("\n")
                    )
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "{body}{tree}"
                ))]))
            }
            Err(e) => Ok(structured_err_ctx(
                "read_error",
                &format!("cannot read skill '{}': {e}", p.name),
                json!({ "name": p.name }),
            )),
        }
    }

    #[tool(
        description = "Report the gateway's own health: which backends failed their last refresh and why. Use if tools seem missing or a server is misbehaving."
    )]
    pub async fn health(&self) -> Result<CallToolResult, McpError> {
        let snap = self.snapshot.read().await;
        let server_count = snap.servers.len();
        let tool_count = snap.tool_count();
        drop(snap);
        let issues = self.all_issues().await;
        let issue_view: Vec<_> = issues
            .iter()
            .map(|i| json!({"kind": i.kind, "subject": i.subject, "detail": i.detail, "severity": i.severity}))
            .collect();
        Ok(text_result(json!({
            "healthy": issues.is_empty(),
            "servers": server_count,
            "tools": tool_count,
            "issues": issue_view,
        })))
    }

    #[tool(
        description = "Re-scan backend servers for their current tools (all, or one via `server`). Returns a diff (added/removed/changed/unchanged) so you can see what's new without comparing against a previous call. Use after installing or reconfiguring a backend."
    )]
    pub async fn refresh_tools(
        &self,
        Parameters(p): Parameters<RefreshParams>,
    ) -> Result<CallToolResult, McpError> {
        let hash = { self.snapshot.read().await.config_hash.clone() };
        let fresh = {
            let registry = self.registry.read().await.clone();
            registry.build_snapshot(hash, p.server.as_deref()).await
        };
        let mut snap = self.snapshot.write().await;
        let old_snap = snap.clone();
        let fresh_for_diff = fresh.clone();
        let fresh_generated_at = fresh.generated_at.clone();
        match p.server {
            Some(ref name) => {
                if let Some(entry) = fresh.servers.get(name) {
                    snap.servers.insert(name.clone(), entry.clone());
                    snap.generated_at = fresh_generated_at.clone();
                } else if snap.servers.contains_key(name) {
                    snap.servers.remove(name);
                    snap.generated_at = fresh_generated_at.clone();
                    tracing::info!(
                        "refresh_tools: '{}' removed from config, evicted from snapshot",
                        name
                    );
                } else {
                    return Ok(structured_err_ctx(
                        "unknown_server",
                        &format!("server '{}' not found", name),
                        json!({ "server": name }),
                    ));
                }
            }
            None => {
                snap.servers = fresh.servers;
                snap.generated_at = fresh_generated_at;
            }
        }
        let diff = compute_diff(&old_snap, &fresh_for_diff, &p.server);
        let summary: Vec<_> = snap
            .servers
            .iter()
            .map(|(n, s)| json!({"server": n, "tools": s.tools.len(), "status": s.refresh_status}))
            .collect();
        let snapshot_to_save = snap.clone();
        drop(snap); // release write lock before disk I/O and publish_health
        if let Err(e) = snapshot_to_save.save(&self.snapshot_path) {
            tracing::warn!("cannot persist snapshot: {e:#}");
        }

        // NOTE: refresh_tools deliberately shuts down nothing. It re-reads
        // backends' tool catalogues; it does not re-read the config, so the
        // registry it started with is still the current one. An earlier
        // version shut down "backends missing from the snapshot", which is a
        // different set entirely: a single-server refresh leaves every other
        // server out of `fresh`, so refreshing one backend killed all the
        // others whenever the snapshot was partial (empty cache with
        // refresh_on_start=false, or a filtered build). Backend lifecycle
        // belongs to reload_from_config, which compares old and new *configs*.
        self.rescan_skills().await;
        Ok(text_result(json!({
            "summary": summary,
            "diff": diff,
        })))
    }
}

#[tool_handler]
impl ServerHandler for Proxy {
    async fn initialize(
        &self,
        params: InitializeRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        // Record who we're serving ("opencode", "claude-ai", …) so the health
        // file is tagged and the companion plugin only toasts for its client.
        *self.client.write().await = Some(params.client_info.name.clone());
        // Re-publish health now that we know who we serve, so the file is
        // tagged for the companion plugin's client filter.
        self.publish_health().await;
        Ok(self.get_info())
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

#[cfg(test)]
mod param_tests {
    use super::*;
    use crate::snapshot::{ServerSnapshot, ToolSnapshot};

    /// A snapshot from (server, [tool]) pairs, for diff tests.
    fn snap_of(servers: &[(&str, &[&str])]) -> Snapshot {
        let mut s = Snapshot::default();
        for (name, tools) in servers {
            s.servers.insert(
                (*name).into(),
                ServerSnapshot {
                    tools: tools
                        .iter()
                        .map(|t| ToolSnapshot {
                            name: (*t).into(),
                            description: "d".into(),
                            input_schema: json!({"type": "object"}),
                        })
                        .collect(),
                    ..Default::default()
                },
            );
        }
        s
    }

    #[test]
    fn compute_diff_reports_a_removed_server() {
        // The added-server path has a regression test; this is its mirror.
        // A full refresh must report the tools of a server that vanished,
        // not silently drop them from the diff.
        let old = snap_of(&[("a", &["t1"]), ("b", &["t2", "t3"])]);
        let new = snap_of(&[("a", &["t1"])]);

        let diff = compute_diff(&old, &new, &None);
        let removed = diff["removed"].as_array().unwrap();
        assert_eq!(
            removed.len(),
            2,
            "both of b's tools should be removed: {diff}"
        );
        assert!(
            removed.iter().all(|r| r["server"] == "b"),
            "removals should be attributed to b: {diff}"
        );
        assert!(
            diff["added"].as_array().unwrap().is_empty(),
            "nothing added: {diff}"
        );
        assert_eq!(diff["unchanged"], 1, "a's tool is unchanged: {diff}");
    }

    #[test]
    fn compute_diff_single_server_filter_reports_removal() {
        // Same, through the `server`-filtered path.
        let old = snap_of(&[("a", &["t1"]), ("b", &["t2"])]);
        let new = snap_of(&[("a", &["t1"])]);
        let diff = compute_diff(&old, &new, &Some("b".to_string()));
        let removed = diff["removed"].as_array().unwrap();
        assert_eq!(removed.len(), 1, "{diff}");
        assert_eq!(removed[0]["server"], "b");
        assert_eq!(removed[0]["tool"], "t2");
    }

    #[test]
    fn compute_diff_flags_a_changed_schema() {
        let old = snap_of(&[("a", &["t1"])]);
        let mut new = snap_of(&[("a", &["t1"])]);
        new.servers.get_mut("a").unwrap().tools[0].input_schema =
            json!({"type": "object", "properties": {"x": {"type": "string"}}});
        let diff = compute_diff(&old, &new, &None);
        assert_eq!(diff["changed"].as_array().unwrap().len(), 1, "{diff}");
        assert_eq!(diff["unchanged"], 0, "{diff}");
    }

    /// The whole product claim: the agent's context carries a small, FIXED
    /// meta-tool surface no matter how many backend tools hide behind it.
    /// Nothing asserted that, so a verbose description could quietly inflate
    /// every session's cost. The ceiling is deliberately loose — this guards
    /// against ballooning, not against ordinary wording edits.
    #[test]
    fn meta_tool_surface_stays_within_its_context_budget() {
        let tools = Proxy::tool_router().list_all();
        assert_eq!(tools.len(), 8, "expected 8 meta-tools, got {}", tools.len());

        let wire = serde_json::to_string(&tools).expect("tools must serialize");
        // The instructions ride in the initialize result and stay in context
        // for the whole session, exactly like the schemas do. Omitting them
        // understated the headline number and — worse — left an unguarded
        // channel where always-on cost could grow without any test noticing.
        let bytes = wire.len() + INSTRUCTIONS.len();
        // Visible with `cargo test -- --nocapture` when you want the number.
        eprintln!(
            "meta-tool surface: {} tools, {} B schema + {} B instructions = {bytes} B",
            tools.len(),
            wire.len(),
            INSTRUCTIONS.len()
        );
        // Measured at 4305 + 370 bytes; the budget leaves ~28% headroom.
        assert!(
            bytes <= 6000,
            "meta-tool surface grew to {bytes} bytes (budget 6000). This is the \
             always-on context cost of every session — trim a description or \
             raise the budget deliberately."
        );
        assert!(
            bytes > 1000,
            "surface is suspiciously small ({bytes} bytes) — are the tool \
             descriptions still attached?"
        );
    }

    #[test]
    fn limit_accepts_number_and_string() {
        let p: SearchParams = serde_json::from_str(r#"{"query":"x","limit":3}"#).unwrap();
        assert_eq!(p.limit, Some(3));
        let p: SearchParams = serde_json::from_str(r#"{"query":"x","limit":"3"}"#).unwrap();
        assert_eq!(p.limit, Some(3));
        let p: SearchParams = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!(p.limit, None);
    }

    #[test]
    fn include_schema_defaults_none() {
        let p: SearchParams = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!(p.include_schema, None);
        let p: SearchParams =
            serde_json::from_str(r#"{"query":"x","include_schema":true}"#).unwrap();
        assert_eq!(p.include_schema, Some(true));
        let p: SearchParams =
            serde_json::from_str(r#"{"query":"x","include_schema":false}"#).unwrap();
        assert_eq!(p.include_schema, Some(false));
    }

    #[test]
    fn arguments_accept_object_and_stringified_object() {
        let p: ExecuteParams =
            serde_json::from_str(r#"{"calls":[{"server":"s","tool":"t","arguments":{"q":"hi"}}]}"#)
                .unwrap();
        assert_eq!(p.calls[0].arguments.clone().unwrap()["q"], "hi");
        let p: ExecuteParams = serde_json::from_str(
            r#"{"calls":[{"server":"s","tool":"t","arguments":"{\"q\": \"hi\"}"}]}"#,
        )
        .unwrap();
        assert_eq!(p.calls[0].arguments.clone().unwrap()["q"], "hi");
        let p: ExecuteParams =
            serde_json::from_str(r#"{"calls":[{"server":"s","tool":"t"}]}"#).unwrap();
        assert!(p.calls[0].arguments.is_none());
    }

    #[test]
    fn stringified_non_object_arguments_rejected() {
        let r = serde_json::from_str::<ExecuteParams>(
            r#"{"calls":[{"server":"s","tool":"t","arguments":"[1,2]"}]}"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn optional_fields_have_concrete_schema_types() {
        // The regression that bit us live: Option<T> fields rendered as
        // type unions that client bridges drop. Pin concrete types.
        let schema = serde_json::to_value(schemars::schema_for!(SearchParams)).unwrap();
        assert_eq!(schema["properties"]["limit"]["type"], "integer");
        assert_eq!(schema["properties"]["include_schema"]["type"], "boolean");
        let schema = serde_json::to_value(schemars::schema_for!(ExecuteParams)).unwrap();
        assert_eq!(schema["properties"]["calls"]["type"], "array");
        let schema = serde_json::to_value(schemars::schema_for!(ListToolsParams)).unwrap();
        assert_eq!(schema["properties"]["tool"]["type"], "string");
    }
}
