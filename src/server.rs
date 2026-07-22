//! The proxy's own MCP surface: eight small meta-tools instead of every
//! backend schema. This is the whole point — the agent's context carries
//! ~3.4 KB no matter how many tools hide behind the gate.

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
    client: Arc<RwLock<Option<String>>>,
    metrics: Arc<RwLock<crate::health::Metrics>>,
}

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

    let servers: Box<dyn Iterator<Item = (&String, &crate::snapshot::ServerSnapshot, &crate::snapshot::ServerSnapshot)>> =
        if let Some(name) = server_filter {
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
                (None, None) => return json!({"added": [], "removed": [], "changed": [], "unchanged": 0}),
            }
        } else {
            Box::new(new.servers.iter().filter_map(|(name, n)| {
                old.servers.get(name).map(|o| (name, o, n))
            }))
        };

    for (name, old_entry, new_entry) in servers {
        let old_tools: std::collections::HashMap<&str, &crate::snapshot::ToolSnapshot> =
            old_entry.tools.iter().map(|t| (t.name.as_str(), t)).collect();
        let new_tools: std::collections::HashMap<&str, &crate::snapshot::ToolSnapshot> =
            new_entry.tools.iter().map(|t| (t.name.as_str(), t)).collect();

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
                if old_t.description != new_t.description || old_t.input_schema != new_t.input_schema {
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
    pub fn new(registry: Registry, snapshot: Snapshot, snapshot_path: PathBuf,
               config_path: PathBuf, skill_dirs: Vec<String>, absorb_dirs: Vec<String>) -> Self {
        let (skills, absorb_issues) = Self::absorb_and_scan(&skill_dirs, &absorb_dirs);
        let metrics = crate::health::Metrics {
            started_at: crate::backend::now(),
            ..Default::default()
        };
        Self {
            registry: Arc::new(RwLock::new(registry)),
            snapshot: Arc::new(RwLock::new(snapshot)),
            snapshot_path,
            config_path,
            skills: Arc::new(RwLock::new(skills)),
            skill_dirs: Arc::new(RwLock::new(skill_dirs)),
            absorb_dirs: Arc::new(RwLock::new(absorb_dirs)),
            absorb_issues: Arc::new(RwLock::new(absorb_issues)),
            client: Arc::new(RwLock::new(None)),
            metrics: Arc::new(RwLock::new(metrics)),
        }
    }

    /// Spawn a background task that watches the config file for changes and
    /// auto-reloads: rebuilds the Registry, recomputes the snapshot, and
    /// publishes health. Debounced to 500ms to coalesce editor write patterns
    /// (temp-file + rename fires multiple events).
    pub fn spawn_hot_reload(self) {
        let config_path = self.config_path.clone();
        let proxy = self.clone();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(16);
        let tx_clone = tx.clone();

        // File watcher → notify on channel.
        let mut watcher = match RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
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
            let mut last_mtime: Option<std::time::SystemTime> = None;
            loop {
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
                        tracing::warn!("config reload failed (keeping old config): {e:#}");
                        continue;
                    }
                };
                if let Err(e) = proxy.reload_from_config(cfg).await {
                    tracing::warn!("hot-reload: reload failed: {e:#}");
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
                    tracing::info!("health check: reset {} dead backend(s): {:?}", dead.len(), dead);
                }
            }
        });
    }

    /// Reload config: rebuild registry, snapshot, skills, and health.
    /// Called by `spawn_hot_reload` when the config file changes, but also
    /// testable in isolation.
    pub async fn reload_from_config(&self, cfg: crate::config::Config) -> AnyhowResult<()> {
        let new_hash = cfg.snapshot_hash();
        let new_registry = crate::backend::Registry::from_config(&cfg);

        // Collect removed backends' Arcs before swapping the registry.
        // We shut them down after the swap to avoid a TOCTOU window where
        // execute could re-obtain a just-shut-down backend from the old registry.
        let removed_backends: Vec<(String, std::sync::Arc<crate::backend::Backend>)> = {
            let old_registry = self.registry.read().await;
            old_registry.backends.iter()
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

        let skill_dirs = self.skill_dirs.read().await.clone();
        let absorb_dirs = self.absorb_dirs.read().await.clone();
        let (scanned, issues) = Self::absorb_and_scan(&skill_dirs, &absorb_dirs);
        *self.absorb_issues.write().await = issues;
        *self.skills.write().await = scanned;

        self.publish_health().await;
        Ok(())
    }

    /// Shut down all backend processes. Called when the proxy is stopping.
    pub async fn shutdown_backends(&self) {
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
        let (servers, absorb) = {
            let snap = self.snapshot.read().await;
            let absorb = self.absorb_issues.read().await.clone();
            let servers: Vec<_> = snap.servers.iter()
                .map(|(n, s)| (n.clone(), s.refresh_status.clone()))
                .collect();
            (servers, absorb)
        };
        let registry = {
            let guard = self.registry.read().await;
            guard.clone()
        };
        let mut issues = absorb;
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
                    if matches!(backend.status().await, crate::backend::Status::Failed { .. }) {
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
            tracing::warn!("skills.absorb configured but skills.dirs is empty — nothing to absorb into");
            return Vec::new();
        };
        let (moved, errors) = crate::skills::absorb_into(library, absorb_dirs);
        if !moved.is_empty() {
            tracing::info!("absorbed {} stray skill(s) into the library: {}",
                           moved.len(), moved.join(", "));
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
                    .map(|t| json!({"tool": t.name, "description": short_desc(&t.description, 140)}))
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
        description = "Find tools AND skills by keywords in one call. Returns hits tagged by kind (\"tool\" or \"skill\"), best match first. For tool hits, set include_schema=true to get each tool's full input schema inline — saves a list_tools call. Use kind=\"tools\" or kind=\"skills\" to search only one surface."
    )]
    pub async fn search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = p.kind.as_deref().unwrap_or("");
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

        if kind != "tools" {
            let idx = self.skills.read().await;
            if !idx.skills.is_empty() {
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

        if results.is_empty() {
            return Ok(structured_err_ctx(
                "no_matches",
                &format!("no matches for '{}'", p.query),
                json!({ "query": p.query, "kind": kind }),
            ));
        }
        Ok(text_result(json!(results)))
    }

    #[tool(
        description = "Execute 1–10 tool calls. Cross-backend calls run in parallel. Results returned in input order as {server, tool, ok, result/error}. A single call: execute({calls: [{server, tool, arguments}]})."
    )]
    pub async fn execute(
        &self,
        Parameters(p): Parameters<ExecuteParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.calls.is_empty() {
            return Ok(structured_err("empty_calls", "no calls — pass at least one {server, tool, arguments}"));
        }
        if p.calls.len() > 10 {
            return Ok(structured_err("too_many_calls", &format!("{} calls (max 10)", p.calls.len())));
        }
        let n = p.calls.len();
        let call_names: Vec<(String, String)> = p.calls.iter()
            .map(|c| (c.server.clone(), c.tool.clone()))
            .collect();
        let (registry, proxy_cfg) = {
            let guard = self.registry.read().await;
            (guard.clone(), guard.proxy.clone())
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
                let was_cold = matches!(backend.status().await, crate::backend::Status::Cold);
                match backend.call(&call.tool, call.arguments, &proxy_cfg).await {
                    Ok(result) => {
                        let elapsed = start.elapsed().as_millis() as u64;
                        // Concatenate all text blocks; note non-text blocks.
                        let mut text = result.content.iter()
                            .filter_map(|b| {
                                if let ContentBlock::Text(t) = b { Some(t.text.as_str()) }
                                else { None }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let has_non_text = result.content.iter().any(|b| {
                            !matches!(b, ContentBlock::Text(_))
                        });
                        if has_non_text && !text.is_empty() {
                            text.push_str("\n[non-text content omitted]");
                        } else if has_non_text {
                            text = "[non-text content omitted]".to_string();
                        }
                        let ok = !result.is_error.unwrap_or(false);
                        let mut entry = json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": ok,
                            "result": text,
                        });
                        if !ok {
                            entry["error"] = json!({
                                "code": "tool_error",
                                "message": text,
                            });
                        }
                        (i, entry, elapsed, was_cold)
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
                        }), elapsed, was_cold)
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
                Ok((i, result, latency_ms, was_cold)) => {
                    if i < n {
                        results[i] = result;
                    }
                    let server = results[i]["server"].as_str().unwrap_or("").to_string();
                    let ok = results[i]["ok"].as_bool().unwrap_or(false);
                    // If a previously-failed/cold backend succeeded, update health
                    // so the TUI plugin clears the stale "connection lost" warning.
                    if ok && was_cold {
                        needs_health_update = true;
                    }
                    let mut metrics = self.metrics.write().await;
                    metrics.total_calls += 1;
                    if !ok { metrics.total_errors += 1; }
                    let entry = metrics.per_backend.entry(server.clone())
                        .or_default();
                    entry.calls += 1;
                    if !ok { entry.errors += 1; }
                    if was_cold { entry.cold_starts += 1; }
                    entry.total_latency_ms += latency_ms;
                    entry.last_call_at = Some(crate::backend::now());
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
            return Ok(structured_err("no_skills", "no skill libraries configured (configure `skills.dirs`)"));
        }
        let out: Vec<_> = idx
            .skills
            .iter()
            .map(|s| {
                let mut entry = json!({"name": s.name, "description": short_desc(&s.description, 100)});
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
        let idx = self.skills.read().await;
        let Some(meta) = idx.get(&p.name) else {
            return Ok(structured_err_ctx(
                "unknown_skill",
                &format!("skill '{}' not found", p.name),
                json!({ "name": p.name }),
            ));
        };
        match read_skill(meta) {
            Ok((body, files)) => {
                let tree = if files.is_empty() {
                    String::new()
                } else {
                    format!("\n\n## Bundled files (absolute paths)\n{}", files.join("\n"))
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
                    tracing::info!("refresh_tools: '{}' removed from config, evicted from snapshot", name);
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
        let skill_dirs = self.skill_dirs.read().await.clone();
        let absorb_dirs = self.absorb_dirs.read().await.clone();
        let (scanned, issues) = Self::absorb_and_scan(&skill_dirs, &absorb_dirs);
        *self.absorb_issues.write().await = issues;
        *self.skills.write().await = scanned;
        self.publish_health().await;
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
            .with_instructions(
                "Gateway to multiple MCP servers whose tools load on demand instead of \
                 polluting your context. Flow: search (with include_schema=true to get \
                 input schemas inline; kind=\"tools\" or \"skills\" to filter) → execute \
                 (1–10 calls in one round-trip, cross-backend parallel), or \
                 list_mcp_servers → list_tools → execute. Backends start lazily on first execution."
                    .to_string(),
            )
    }
}

#[cfg(test)]
mod param_tests {
    use super::*;

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
        let p: SearchParams = serde_json::from_str(r#"{"query":"x","include_schema":true}"#).unwrap();
        assert_eq!(p.include_schema, Some(true));
        let p: SearchParams = serde_json::from_str(r#"{"query":"x","include_schema":false}"#).unwrap();
        assert_eq!(p.include_schema, Some(false));
    }

    #[test]
    fn arguments_accept_object_and_stringified_object() {
        let p: ExecuteParams =
            serde_json::from_str(r#"{"calls":[{"server":"s","tool":"t","arguments":{"q":"hi"}}]}"#).unwrap();
        assert_eq!(p.calls[0].arguments.clone().unwrap()["q"], "hi");
        let p: ExecuteParams =
            serde_json::from_str(r#"{"calls":[{"server":"s","tool":"t","arguments":"{\"q\": \"hi\"}"}]}"#)
                .unwrap();
        assert_eq!(p.calls[0].arguments.clone().unwrap()["q"], "hi");
        let p: ExecuteParams = serde_json::from_str(r#"{"calls":[{"server":"s","tool":"t"}]}"#).unwrap();
        assert!(p.calls[0].arguments.is_none());
    }

    #[test]
    fn stringified_non_object_arguments_rejected() {
        let r = serde_json::from_str::<ExecuteParams>(r#"{"calls":[{"server":"s","tool":"t","arguments":"[1,2]"}]}"#);
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
