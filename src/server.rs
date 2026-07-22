//! The proxy's own MCP surface: nine small meta-tools instead of every
//! backend schema. This is the whole point — the agent's context carries
//! ~3.6 KB no matter how many tools hide behind the gate.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
pub struct GetToolsParams {
    /// Server name from list_servers
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
    /// Max results per kind (default 8 for tools, 5 for skills)
    #[serde(default, deserialize_with = "opt_u32_lenient")]
    #[schemars(with = "u32")]
    pub limit: Option<u32>,
    /// Include the full input schema for each tool hit so you can call
    /// execute_tool directly without a separate get_tools call.
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
pub struct ExecuteParams {
    /// Server name from list_servers / search
    pub server: String,
    /// Tool name on that server
    pub tool: String,
    /// Arguments for the tool as a JSON object, matching its input schema
    #[serde(default, deserialize_with = "opt_args_lenient")]
    #[schemars(with = "JsonObject")]
    pub arguments: Option<JsonObject>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchCall {
    /// Server name from list_servers / search
    pub server: String,
    /// Tool name on that server
    pub tool: String,
    /// Arguments for the tool as a JSON object
    #[serde(default, deserialize_with = "opt_args_lenient")]
    #[schemars(with = "JsonObject")]
    pub arguments: Option<JsonObject>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchParams {
    /// Up to 10 tool calls to execute in parallel across backends.
    /// Same-backend calls serialize naturally; cross-backend calls run
    /// concurrently. Results are returned in the same order as the input.
    pub calls: Vec<BatchCall>,
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
    skill_dirs: Vec<String>,
    absorb_dirs: Vec<String>,
    absorb_issues: Arc<RwLock<Vec<Issue>>>,
    client: Arc<RwLock<Option<String>>>,
}

fn text_result(v: serde_json::Value) -> CallToolResult {
    let text = serde_json::to_string_pretty(&v).unwrap_or_else(|e| format!("serialize error: {e}"));
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn err(msg: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg)])
}

#[tool_router]
impl Proxy {
    pub fn new(registry: Registry, snapshot: Snapshot, snapshot_path: PathBuf,
               config_path: PathBuf, skill_dirs: Vec<String>, absorb_dirs: Vec<String>) -> Self {
        let absorb_issues = Self::absorb(&skill_dirs, &absorb_dirs);
        let skills = SkillIndex::scan(&skill_dirs);
        Self {
            registry: Arc::new(RwLock::new(registry)),
            snapshot: Arc::new(RwLock::new(snapshot)),
            snapshot_path,
            config_path,
            skills: Arc::new(RwLock::new(skills)),
            skill_dirs,
            absorb_dirs,
            absorb_issues: Arc::new(RwLock::new(absorb_issues)),
            client: Arc::new(RwLock::new(None)),
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
        let watch_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
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
            let mut last_fire: Option<Instant> = None;
            loop {
                if rx.recv().await.is_none() {
                    break;
                }
                // Debounce: ignore events within 500ms of the last one.
                let now = Instant::now();
                if let Some(t) = last_fire {
                    if now.duration_since(t) < Duration::from_millis(500) {
                        continue;
                    }
                }
                last_fire = Some(now);

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
    /// exited), the connection is reset so the next `execute_tool` reconnects
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
                    break;
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

        // Identify and shut down removed backends.
        {
            let old_registry = self.registry.read().await;
            for (name, backend) in &old_registry.backends {
                if !new_registry.backends.contains_key(name) {
                    backend.shutdown().await;
                    tracing::info!("hot-reload: shut down removed backend '{name}'");
                }
            }
        }

        *self.registry.write().await = new_registry;

        // Rebuild snapshot from the new registry.
        let registry = self.registry.read().await;
        let snap = registry.build_snapshot(new_hash, None).await;
        drop(registry);
        tracing::info!(
            "hot-reload: config changed, refreshed {} servers, {} tools",
            snap.servers.len(),
            snap.tool_count()
        );

        *self.snapshot.write().await = snap.clone();
        if let Err(e) = snap.save(&self.snapshot_path) {
            tracing::warn!("hot-reload: cannot persist snapshot: {e:#}");
        }

        let fresh_absorb = Self::absorb(&self.skill_dirs, &self.absorb_dirs);
        *self.absorb_issues.write().await = fresh_absorb;
        *self.skills.write().await = SkillIndex::scan(&self.skill_dirs);

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
        health::write(client.as_deref(), issues);
    }

    /// The current full issue set (backends + absorb) — used by the
    /// `health` meta-tool and `publish_health` so they always agree.
    async fn all_issues(&self) -> Vec<Issue> {
        let snap = self.snapshot.read().await;
        let absorb = self.absorb_issues.read().await;
        let mut issues = absorb.clone();
        for (name, s) in &snap.servers {
            if s.refresh_status != "ok" {
                issues.push(Issue {
                    kind: "backend".into(),
                    subject: name.clone(),
                    detail: s.refresh_status.clone(),
                    severity: "error".into(),
                });
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

    #[tool(
        description = "List the MCP servers behind this proxy: name, what it's for, tool count, running state. Start here."
    )]
    pub async fn list_servers(&self) -> Result<CallToolResult, McpError> {
        let snap = self.snapshot.read().await;
        let registry = self.registry.read().await;
        let mut out = Vec::new();
        for (name, s) in &snap.servers {
            let status = match registry.get(name) {
                Ok(b) => serde_json::to_value(b.status().await).unwrap_or(json!("unknown")),
                Err(_) => json!("removed from config"),
            };
            out.push(json!({
                "server": name,
                "description": s.description,
                "tools": s.tools.len(),
                "status": status,
                "last_refresh": s.refresh_status,
            }));
        }
        Ok(text_result(json!(out)))
    }

    #[tool(
        description = "List a server's tools (name + one-line description). Pass `tool` to get that tool's full input schema before executing it."
    )]
    pub async fn get_tools(
        &self,
        Parameters(p): Parameters<GetToolsParams>,
    ) -> Result<CallToolResult, McpError> {
        let snap = self.snapshot.read().await;
        let Some(server) = snap.servers.get(&p.server) else {
            return Ok(err(format!(
                "unknown server '{}' — see list_servers",
                p.server
            )));
        };
        match p.tool {
            Some(tool_name) => match snap.find_tool(&p.server, &tool_name) {
                Some(t) => Ok(text_result(json!({
                    "server": p.server,
                    "tool": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }))),
                None => Ok(err(format!(
                    "no tool '{tool_name}' on '{}' — call get_tools without `tool` to list",
                    p.server
                ))),
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
                    "hint": "get_tools with `tool` gives the input schema; execute_tool runs it",
                })))
            }
        }
    }

    #[tool(
        description = "Find tools AND skills by keywords in one call. Returns hits tagged by kind (\"tool\" or \"skill\"), best match first. For tool hits, set include_schema=true to get each tool's full input schema inline — saves a get_tools call. Use kind=\"tools\" or kind=\"skills\" to search only one surface."
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
            return Ok(err(format!(
                "no matches for '{}'{} — try different keywords",
                p.query,
                if kind.is_empty() { String::new() } else { format!(" (kind={kind})") }
            )));
        }
        Ok(text_result(json!(results)))
    }

    #[tool(
        description = "Execute a tool on a backend server (started on demand). Get the schema via get_tools first if unsure about arguments."
    )]
    pub async fn execute_tool(
        &self,
        Parameters(p): Parameters<ExecuteParams>,
    ) -> Result<CallToolResult, McpError> {
        let registry = self.registry.read().await;
        let backend = match registry.get(&p.server) {
            Ok(b) => b,
            Err(e) => return Ok(err(e.to_string())),
        };
        match backend.call(&p.tool, p.arguments, &registry.proxy).await {
            Ok(result) => Ok(result),
            Err(e) => Ok(err(format!("{e:#}"))),
        }
    }

    #[tool(
        description = "Execute multiple tool calls in one round-trip. Calls to different backends run in parallel; same-backend calls serialize. Results are returned in input order. Max 10 calls."
    )]
    pub async fn execute_batch(
        &self,
        Parameters(p): Parameters<BatchParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.calls.is_empty() {
            return Ok(err("no calls in batch".into()));
        }
        if p.calls.len() > 10 {
            return Ok(err(format!("batch too large: {} calls (max 10)", p.calls.len())));
        }
        let registry = self.registry.read().await;
        let proxy_cfg = registry.proxy.clone();

        // Spawn one task per call. Same-backend calls share the same
        // Arc<Backend> and its conn Mutex, so they serialize naturally;
        // cross-backend calls run concurrently.
        let mut join = tokio::task::JoinSet::new();
        for (i, call) in p.calls.into_iter().enumerate() {
            let registry = registry.clone();
            let proxy_cfg = proxy_cfg.clone();
            join.spawn(async move {
                let backend = match registry.get(&call.server) {
                    Ok(b) => b,
                    Err(e) => {
                        return (i, json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": false,
                            "error": e.to_string(),
                        }));
                    }
                };
                match backend.call(&call.tool, call.arguments, &proxy_cfg).await {
                    Ok(result) => {
                        let text = result.content.iter().find_map(|b| {
                            if let ContentBlock::Text(t) = b { Some(t.text.as_str()) }
                            else { None }
                        }).unwrap_or("");
                        (i, json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": !result.is_error.unwrap_or(false),
                            "result": text,
                        }))
                    }
                    Err(e) => {
                        (i, json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": false,
                            "error": format!("{e:#}"),
                        }))
                    }
                }
            });
        }

        let mut results = Vec::new();
        while let Some(joined) = join.join_next().await {
            if let Ok((i, result)) = joined {
                results.push((i, result));
            }
        }
        results.sort_by_key(|(i, _)| *i);
        let results: Vec<_> = results.into_iter().map(|(_, r)| r).collect();
        Ok(text_result(json!(results)))
    }

    #[tool(
        description = "List every available agent skill (name + one-line description). Use when asked what skills exist; use search with kind=\"skills\" for targeted lookup."
    )]
    pub async fn list_skills(&self) -> Result<CallToolResult, McpError> {
        let idx = self.skills.read().await;
        if idx.skills.is_empty() {
            return Ok(err("no skill libraries configured (config `skills.dirs`)".into()));
        }
        let out: Vec<_> = idx
            .skills
            .iter()
            .map(|s| json!({"name": s.name, "description": short_desc(&s.description, 100)}))
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
            return Ok(err(format!("unknown skill '{}' — use search with kind=\"skills\"", p.name)));
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
            Err(e) => Ok(err(format!("cannot read skill '{}': {e}", p.name))),
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
        description = "Re-scan backend servers for their current tools (all, or one via `server`). Use after installing or reconfiguring a backend."
    )]
    pub async fn refresh_tools(
        &self,
        Parameters(p): Parameters<RefreshParams>,
    ) -> Result<CallToolResult, McpError> {
        let hash = { self.snapshot.read().await.config_hash.clone() };
        let fresh = {
            let registry = self.registry.read().await;
            registry.build_snapshot(hash, p.server.as_deref()).await
        };
        let mut snap = self.snapshot.write().await;
        match p.server {
            Some(ref name) => {
                if let Some(entry) = fresh.servers.get(name) {
                    snap.servers.insert(name.clone(), entry.clone());
                    snap.generated_at = fresh.generated_at;
                } else if snap.servers.contains_key(name) {
                    // Server no longer in config — evict the stale entry so
                    // a single-server refresh can clean up a removal without
                    // needing a full refresh.
                    snap.servers.remove(name);
                    snap.generated_at = fresh.generated_at;
                    tracing::info!("refresh_tools: '{}' removed from config, evicted from snapshot", name);
                } else {
                    // Neither in config nor in the snapshot — typo, not a
                    // removal. Surface it instead of silently doing nothing.
                    return Ok(err(format!(
                        "unknown server '{name}' — see list_servers"
                    )));
                }
            }
            None => {
                snap.servers = fresh.servers;
                snap.generated_at = fresh.generated_at;
            }
        }
        if let Err(e) = snap.save(&self.snapshot_path) {
            tracing::warn!("cannot persist snapshot: {e:#}");
        }
        let summary: Vec<_> = snap
            .servers
            .iter()
            .map(|(n, s)| json!({"server": n, "tools": s.tools.len(), "status": s.refresh_status}))
            .collect();
        drop(snap); // release write lock before publish_health (which reads)
        let fresh_absorb = Self::absorb(&self.skill_dirs, &self.absorb_dirs);
        *self.absorb_issues.write().await = fresh_absorb;
        *self.skills.write().await = SkillIndex::scan(&self.skill_dirs);
        self.publish_health().await;
        Ok(text_result(json!(summary)))
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
                 input schemas inline; kind=\"tools\" or \"skills\" to filter) → execute_tool \
                 (or execute_batch for multiple independent calls), or list_servers → \
                 get_tools → execute_tool. Backends start lazily on first execution."
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
            serde_json::from_str(r#"{"server":"s","tool":"t","arguments":{"q":"hi"}}"#).unwrap();
        assert_eq!(p.arguments.unwrap()["q"], "hi");
        let p: ExecuteParams =
            serde_json::from_str(r#"{"server":"s","tool":"t","arguments":"{\"q\": \"hi\"}"}"#)
                .unwrap();
        assert_eq!(p.arguments.unwrap()["q"], "hi");
        let p: ExecuteParams = serde_json::from_str(r#"{"server":"s","tool":"t"}"#).unwrap();
        assert!(p.arguments.is_none());
    }

    #[test]
    fn stringified_non_object_arguments_rejected() {
        let r = serde_json::from_str::<ExecuteParams>(r#"{"server":"s","tool":"t","arguments":"[1,2]"}"#);
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
        assert_eq!(schema["properties"]["arguments"]["type"], "object");
        let schema = serde_json::to_value(schemars::schema_for!(GetToolsParams)).unwrap();
        assert_eq!(schema["properties"]["tool"]["type"], "string");
    }
}
