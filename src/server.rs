//! The proxy's own MCP surface: eight small meta-tools instead of every
//! backend schema. This is the whole point — the agent's context carries
//! ~4.3 KB no matter how many tools hide behind the gate. That number is
//! measured, not asserted by hand: see
//! `param_tests::meta_tool_surface_stays_within_its_context_budget`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result as AnyhowResult;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rmcp::{
    handler::server::router::tool::ToolRouter, handler::server::wrapper::Parameters, model::*,
    service::RequestContext, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer,
    ServerHandler,
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
    /// Whether the connected client advertised the MCP `sampling` capability
    /// at initialize. Decides whether the picker can borrow the client's own
    /// model instead of needing a configured command.
    client_supports_sampling: Arc<RwLock<bool>>,
    /// The live connection back to the client, stashed at initialize. The
    /// picker's sampling transport needs it to call `create_message`, and a
    /// `#[tool]` handler cannot reach the request context without changing
    /// every meta-tool's signature.
    peer: Arc<RwLock<Option<rmcp::Peer<RoleServer>>>>,
    /// Set once a client that *advertised* sampling refuses to actually serve
    /// it. Without this, every search for the rest of the session pays a
    /// doomed round-trip — and on clients that prompt for approval, shows the
    /// user a dialog — before falling back to the lexical order it was always
    /// going to return.
    sampling_broken: Arc<std::sync::atomic::AtomicBool>,
    /// Consecutive searches where the picker produced no usable choice.
    ///
    /// A picker that never answers is indistinguishable from one that always
    /// agrees with the lexical order: same results, just slower and more
    /// expensive. Measured against a reasoning-enabled endpoint, that was 7 of
    /// 8 searches. A streak is the only signal geco has that the feature the
    /// user turned on is doing nothing at all.
    picker_misses: Arc<std::sync::atomic::AtomicUsize>,
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
    /// Per-instance router, so the advertised surface can depend on config.
    /// `list_skills` and `read_skill` are disabled when no skill library is
    /// configured: they are 392 bytes of permanent context describing a
    /// capability that user does not have, and an invitation to call a tool
    /// that can only answer "nothing here".
    tool_router: ToolRouter<Proxy>,
    /// Always-on instruction text, built once from config. Carries the skill
    /// nudge only when there is a library to nudge towards.
    instructions: String,
}

/// The instruction text every client injects into the model's context on
/// connect. This is always-on cost exactly like the tool schemas, so it is a
/// const rather than an inline literal: the context-budget test measures it.
///
/// It must stay O(1) — a fixed policy, never a catalogue. Anything that grows
/// with the number of servers or skills belongs in a tool RESPONSE, which is
/// paid only when asked for.
const INSTRUCTIONS_BASE: &str =
    "Gateway to multiple MCP servers whose tools load on demand instead of \
     polluting your context. Flow: search (with include_schema=true to get \
     input schemas inline; kind=\"tools\" or \"skills\" to filter) → execute \
     (1–10 calls in one round-trip, cross-backend parallel), or \
     list_mcp_servers → list_tools → execute. Backends start lazily on first execution.";

/// Appended only when a skill library actually exists.
///
/// Skills are a "push" resource: nothing in a user's request names one, so the
/// model must reach for it unprompted — and measured against three local
/// models, it essentially never did. Baseline was 0-1 out of 12 runs each.
///
/// Every clause here earned its place in an A/B against those models:
///   - the COUNT is O(1); listing names would be O(N) and is what this whole
///     design refuses to do. It also measured WORSE — two of three models
///     replied `CALL code-reviewer`, treating a skill name as a tool name,
///     producing invalid calls 6/12 and 12/12 of the time.
///   - "NOT callable tools" exists to prevent exactly that failure.
///   - "ALWAYS ... before doing anything else" beat a hedged "before any
///     non-trivial task, a playbook may exist" by roughly 2x on every model.
///
/// Result: 8-12 out of 12, versus 0-1 for the base wording, and better than
/// the O(N) name list on all three. The same nudge placed in the `search`
/// tool DESCRIPTION instead scored 0-3/12 — placement matters enormously,
/// which is why this lives in the instructions.
fn skill_instructions(count: usize) -> String {
    format!(
        " {count} SKILLS (written playbooks for specialised work) are hidden behind search — \
         you cannot see them otherwise, and they are NOT callable tools. ALWAYS call search \
         first with a one-line description of the task, before doing anything else."
    )
}

/// How many (server, tool) hits to remember for the search→execute signal.
const RECENT_HITS_CAP: usize = 64;
/// How many zero-result queries to keep in metrics. Bounded because this ends
/// up in health.json on disk, and it's a diagnostic aid, not a query log.
const RECENT_NO_MATCH_CAP: usize = 20;

/// How many consecutive empty picks before geco says the picker is not working.
/// Three, because one is noise and a misconfigured picker fails every time —
/// the observed rate on a reasoning-enabled endpoint was 7 in 8.
const PICKER_MISS_STREAK: usize = 3;

/// One thing the picker may choose, from either surface, owned so it outlives
/// the locks the two catalogues are read under.
#[derive(Clone)]
struct PickCandidate {
    /// "tool" or "skill". Explicit rather than inferred from an empty server:
    /// a config can name a server `""`, and a tool mistaken for a skill would
    /// be hoisted into the wrong list.
    kind: &'static str,
    server: String,
    name: String,
    description: String,
}

impl PickCandidate {
    /// Roughly what this entry costs in `Picker::catalogue_block`.
    fn chars(&self) -> usize {
        self.server.len() + self.name.len() + self.description.len() + 8
    }
}

/// Every tool in the snapshot as picker candidates — when they fit.
///
/// `None` means "does not fit, use the lexical shortlist", which is also what
/// a `budget_chars` of 0 means. Measured, the fitting case is worth 94.8%
/// rank-1 against the shortlist's 74.6%: a lexical shortlist keeps exactly the
/// candidates that share vocabulary with the query — the plausible wrong
/// answers — and discards the ones a model would reject at a glance.
///
/// Counted in characters against a characters-per-token estimate rather than
/// tokenised properly. geco has no tokenizer and will not grow one for this:
/// both branches work, so a 20% error picks a slightly different mode and
/// nothing else. Bails as soon as the budget is passed, so an enormous
/// catalogue costs a partial walk rather than a full build.
fn whole_catalogue(snap: &Snapshot, budget_chars: usize) -> Option<Vec<PickCandidate>> {
    if budget_chars == 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut chars = 0usize;
    for (server, s) in &snap.servers {
        for t in &s.tools {
            // 140 to match what `search_with` puts in a Hit, so both modes
            // describe a tool identically. `catalogue_block` caps at 160.
            let c = PickCandidate {
                kind: "tool",
                server: server.clone(),
                name: t.name.clone(),
                description: short_desc(&t.description, 140),
            };
            chars += c.chars();
            if chars > budget_chars {
                return None;
            }
            out.push(c);
        }
    }
    Some(out)
}

/// The same, for the skill library. Separate function rather than a generic
/// one because the two sources share no shape, and a library is small enough
/// that it nearly always fits whatever the tool catalogue left over.
fn whole_skill_library(idx: &SkillIndex, budget_chars: usize) -> Option<Vec<PickCandidate>> {
    if budget_chars == 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut chars = 0usize;
    for s in &idx.skills {
        let c = PickCandidate {
            kind: "skill",
            server: String::new(),
            name: s.name.clone(),
            description: short_desc(&s.description, 140),
        };
        chars += c.chars();
        if chars > budget_chars {
            return None;
        }
        out.push(c);
    }
    Some(out)
}

/// A few tokens describing a content block, for the JSON summary, so the model
/// knows something arrived without the bytes being spelled out to it.
///
/// `bytes` is the base64 length, which is ~4/3 of the real payload — close
/// enough to answer the only question the model has ("is this big?"), and
/// cheaper than decoding to find out.
fn describe_block(b: &ContentBlock) -> serde_json::Value {
    match b {
        ContentBlock::Image(i) => {
            json!({"type": "image", "mime_type": i.mime_type, "bytes": i.data.len()})
        }
        ContentBlock::Audio(a) => {
            json!({"type": "audio", "mime_type": a.mime_type, "bytes": a.data.len()})
        }
        ContentBlock::Resource(_) => json!({"type": "resource"}),
        ContentBlock::ResourceLink(r) => json!({"type": "resource_link", "uri": r.uri}),
        // Text never reaches here — it is flattened into `result` — but the
        // match has to be total, and a silent drop would be the bug this
        // whole function exists to fix.
        ContentBlock::Text(_) => json!({"type": "text"}),
        // `ContentBlock` is `#[non_exhaustive]`: a future MCP block type
        // still gets attached and still gets announced, just without a
        // description geco is not yet able to write.
        _ => json!({"type": "unknown"}),
    }
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

        // Advertise the skill meta-tools only when there is a library behind
        // them. Disabled routes are hidden from tools/list AND rejected by
        // call, so a client cannot reach a capability that isn't configured.
        // Note this is decided once, at construction: adding skills.dirs to a
        // running proxy needs a restart before the two tools appear.
        let mut tool_router = Self::tool_router();
        if skill_dirs.is_empty() {
            tool_router.disable_route("list_skills");
            tool_router.disable_route("read_skill");
        }

        // Only claim skills exist when some were actually found. A count of
        // zero would be a lie, and a nudge towards an empty library wastes a
        // call on every task.
        let instructions = match skills.skills.len() {
            0 => INSTRUCTIONS_BASE.to_string(),
            n => format!("{INSTRUCTIONS_BASE}{}", skill_instructions(n)),
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
            config_issues: Arc::new(RwLock::new(Vec::new())),
            client: Arc::new(RwLock::new(None)),
            client_supports_sampling: Arc::new(RwLock::new(false)),
            peer: Arc::new(RwLock::new(None)),
            sampling_broken: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            picker_misses: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            metrics: Arc::new(RwLock::new(metrics)),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            recent_search_hits: Arc::new(RwLock::new(std::collections::VecDeque::new())),
            tool_change_tx,
            tool_change_rx: Arc::new(std::sync::Mutex::new(Some(tool_change_rx))),
            tool_router,
            instructions,
        }
    }

    /// Watch for backends reporting `tools/list_changed` and re-scan just that
    /// server. Without this the snapshot silently drifts: a backend that adds
    /// or removes a tool at runtime stays wrong until someone runs
    /// a manual refresh, and nothing anywhere reports that it happened.
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
    /// Used by the tool-change listener; config hot-reload has its own path
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
    /// a restart. Watches recursively; editor temp-file + rename bursts
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
    /// Used by new() and reload_from_config() to avoid
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
    /// and spawn_skill_reload so the callers never drift.
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
        description = "List the MCP servers behind this proxy: name, what it's for, tool count, running state, and a preview of up to 5 tool names. Reports gateway problems in `issues` when there are any. Start here."
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

        // Gateway-level problems (a config that failed to reload, a skill
        // directory that wouldn't scan) ride along here rather than in a
        // meta-tool of their own. A dedicated `health` tool cost 218 bytes of
        // every session's context to answer a question that is only ever
        // asked when something is already broken — and the per-server half of
        // its answer was already in this response. Carrying it in the payload
        // costs nothing when things work, which is nearly always.
        //
        // The `issues` key is omitted when empty — a healthy gateway should
        // not spend tokens saying so on every call — but the envelope itself
        // is unconditional. A response that changes shape between healthy and
        // broken is a trap for anything parsing it, and "broken" is exactly
        // when you least want a surprise.
        let issues = self.all_issues().await;
        let mut body = json!({ "servers": out });
        if !issues.is_empty() {
            let issue_view: Vec<_> = issues
                .iter()
                .map(|i| json!({"kind": i.kind, "subject": i.subject, "detail": i.detail, "severity": i.severity}))
                .collect();
            body["issues"] = json!(issue_view);
            body["hint"] = json!("run `geco health` for the full gateway report");
        }
        Ok(text_result(body))
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

        // User-provided synonym groups (proxy.search_synonyms). Cloned once so
        // the registry lock isn't held across the searches below.
        let (user_synonyms, picker_cfg) = {
            let g = self.registry.read().await;
            (g.proxy.search_synonyms.clone(), g.proxy.picker.clone())
        };
        // A client that advertised sampling and then refused to serve it is
        // treated, from the second search on, exactly like one that never
        // advertised it — which lets `mode: auto` fall through to a configured
        // command instead of being stuck on a transport that does not work.
        let sampling_usable = *self.client_supports_sampling.read().await
            && !self
                .sampling_broken
                .load(std::sync::atomic::Ordering::Relaxed);
        let picker = crate::picker::Picker::from_config(&picker_cfg, sampling_usable);

        // With a picker enabled, retrieve a WIDER net than the caller asked
        // for: the model can only choose from what it is handed, and recall@50
        // is 44% against recall@5's 22% (docs/search-ranking.md).
        let retrieve = if picker.is_enabled() {
            picker.candidates.max(limit)
        } else {
            limit
        };
        let skill_limit = if limit > 20 { 20 } else { limit };
        let budget = picker.whole_catalogue_budget_chars();

        // Both surfaces are ranked lexically FIRST, before any model
        // round-trip, and each lock is released as soon as its list is copied
        // out. That matters more than it looks: a pick can take 20 seconds
        // (300 if the timeout is raised), and holding a read guard that long
        // lets the next writer — a `tools/list_changed` rescan, a config
        // hot-reload — queue, after which every subsequent reader queues
        // behind *it*. The one a user feels is `execute`, whose trailing
        // `publish_health` reads the snapshot: a tool call that already
        // succeeded sits there until the picker returns, and the whole gateway
        // looks hung on an unrelated request.
        let (mut tool_hits, tool_pool) = if kind != "skills" {
            let snap = self.snapshot.read().await;
            let hits = search::search_with(&snap, &p.query, retrieve, &user_synonyms);
            let pool = if picker.is_enabled() {
                whole_catalogue(&snap, budget)
            } else {
                None
            };
            (hits, pool)
        } else {
            (Vec::new(), None)
        };

        let mut skills_configured = true;
        let (mut skill_hits, skill_pool) = if kind != "tools" {
            let idx = self.skills.read().await;
            skills_configured = !idx.skills.is_empty();
            // A wider net for the picker here too, for the same reason.
            let take = if picker.is_enabled() {
                retrieve.max(skill_limit)
            } else {
                skill_limit
            };
            let hits: Vec<(String, String)> = idx
                .search_with(&p.query, take, &user_synonyms)
                .into_iter()
                .map(|s| (s.name.clone(), short_desc(&s.description, 160)))
                .collect();
            // Whatever the tool catalogue left. A library is small enough that
            // this is nearly always the whole of it.
            let spent: usize = tool_pool
                .as_ref()
                .map(|p| p.iter().map(PickCandidate::chars).sum())
                .unwrap_or(0);
            let pool = if picker.is_enabled() {
                whole_skill_library(&idx, budget.saturating_sub(spent))
            } else {
                None
            };
            (hits, pool)
        } else {
            (Vec::new(), None)
        };

        // ONE pick across both surfaces, not one per surface. Two independent
        // picks would force a choice on whichever surface the request has
        // nothing to do with — the prompt has no "none of these" escape, so a
        // model asked "which skill best fits?" about a pure tool request will
        // name one anyway, and geco would promote that noise to rank 1. Asking
        // once lets the model answer the question that was actually asked:
        // what is the best capability here, of either kind.
        // A query that scored NOTHING on either surface gets no pick at all.
        //
        // The prompt has no "none of these" answer, so a model handed the whole
        // catalogue for "send an SMS to my mum" against a filesystem and a
        // github server will still name something — and geco would return it as
        // `count: 1`, the best match, to a request this gateway cannot serve.
        // Finding nothing is a legitimate answer that `search` is careful to
        // give (it deliberately does not return is_error for it), and
        // whole-catalogue mode would otherwise make it unreachable: `count` 0
        // could never happen again, which also silences `searches_no_match` and
        // the `recent_no_match` log — the documented signal for a tool whose
        // description does not say what it is for.
        //
        // This does not cost the case the feature exists for. A target that
        // scores zero is reached whenever OTHER tools scored, which on any real
        // catalogue is nearly always; the guard only fires when the request has
        // no lexical footing anywhere at all.
        let any_lexical_signal = !tool_hits.is_empty() || !skill_hits.is_empty();
        let pool: Vec<PickCandidate> = if picker.is_enabled() && any_lexical_signal {
            let mut pool = tool_pool.unwrap_or_else(|| {
                tool_hits
                    .iter()
                    .map(|h| PickCandidate {
                        kind: "tool",
                        server: h.server.clone(),
                        name: h.tool.clone(),
                        description: h.description.clone(),
                    })
                    .collect()
            });
            pool.extend(skill_pool.unwrap_or_else(|| {
                skill_hits
                    .iter()
                    .map(|(name, description)| PickCandidate {
                        kind: "skill",
                        server: String::new(),
                        name: name.clone(),
                        description: description.clone(),
                    })
                    .collect()
            }));
            pool
        } else {
            Vec::new()
        };

        let mut chosen: Option<PickCandidate> = None;
        if pool.len() > 1 {
            let cands: Vec<crate::picker::Candidate<'_>> = pool
                .iter()
                .map(|c| crate::picker::Candidate {
                    kind: c.kind,
                    server: &c.server,
                    tool: &c.name,
                    description: &c.description,
                })
                .collect();
            // Cloned rather than borrowed: holding the read guard across the
            // pick would block every reconnect for the length of a model
            // round-trip, and `Peer` is an Arc-backed handle.
            let peer = self.peer.read().await.clone();
            // A pick hoists that entry to rank 1 of its own list and leaves
            // the lexical order beneath it. No pick (unsupported, timeout,
            // garbage reply) simply leaves both orders alone.
            match picker.pick(&p.query, &cands, peer.as_ref()).await {
                crate::picker::Pick::Choice(idx) => {
                    let c = &pool[idx];
                    tracing::debug!("picker chose {} {}::{}", c.kind, c.server, c.name);
                    chosen = Some(c.clone());
                    self.picker_misses
                        .store(0, std::sync::atomic::Ordering::Relaxed);
                }
                crate::picker::Pick::NoChoice => {
                    // Logged at the streak threshold, not per miss: one bad
                    // reply is noise, and a line per search would bury the
                    // signal in the noise it is trying to report. A later
                    // success resets the count, so this stays quiet for a
                    // picker that merely disagrees occasionally.
                    let streak = self
                        .picker_misses
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    if streak == PICKER_MISS_STREAK {
                        tracing::warn!(
                            "picker: {PICKER_MISS_STREAK} consecutive searches produced no \
                             usable choice, so every one paid for a model round-trip and \
                             returned the lexical order anyway. Check the picker's model is \
                             reachable, answers with JSON, and is NOT in reasoning mode."
                        );
                    }
                }
                // Said once, then never attempted again this session. The
                // cause was already logged where it was seen; this line is
                // the durable consequence.
                crate::picker::Pick::TransportDead => {
                    self.sampling_broken
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!(
                        "picker: sampling is not usable on this connection — \
                         lexical ranking for the rest of the session"
                    );
                }
            }
        }

        // Apply the pick to whichever LEXICAL list it belongs to — that is
        // what the caller gets back. The other list is left alone, which is
        // the correct outcome: the model said the best answer is over here.
        if let Some(c) = chosen {
            if c.kind == "skill" {
                match skill_hits.iter().position(|(name, _)| *name == c.name) {
                    Some(pos) => {
                        let hit = skill_hits.remove(pos);
                        skill_hits.insert(0, hit);
                    }
                    None => skill_hits.insert(0, (c.name, c.description)),
                }
            } else {
                match tool_hits
                    .iter()
                    .position(|h| h.server == c.server && h.tool == c.name)
                {
                    Some(pos) => {
                        let hit = tool_hits.remove(pos);
                        tool_hits.insert(0, hit);
                    }
                    // The model reached past the lexical shortlist entirely —
                    // which is the entire point of whole-catalogue mode, and
                    // happens on roughly one search in five. Without this the
                    // best answer would be found and then thrown away.
                    None => tool_hits.insert(
                        0,
                        search::Hit {
                            server: c.server,
                            tool: c.name,
                            description: c.description,
                            score: 0.0,
                            coverage: 0.0,
                        },
                    ),
                }
            }
        }

        tool_hits.truncate(limit);
        skill_hits.truncate(skill_limit);

        if !tool_hits.is_empty() {
            // Re-taken only for the schemas, and only when asked for. A
            // refresh that landed during the pick means the caller gets the
            // newer schema for a tool ranked against the older snapshot —
            // which is the right way round: a stale schema is what gets
            // executed and fails, a stale ranking is merely a stale ranking.
            let snap = include_schema.then_some(()).map(|_| self.snapshot.clone());
            let snap = match &snap {
                Some(s) => Some(s.read().await),
                None => None,
            };
            for h in &tool_hits {
                let mut entry = json!({
                    "kind": "tool",
                    "server": h.server,
                    "name": h.tool,
                    "description": h.description,
                });
                if let Some(snap) = &snap {
                    if let Some(t) = snap.find_tool(&h.server, &h.tool) {
                        entry["input_schema"] = t.input_schema.clone();
                    }
                }
                results.push(entry);
            }
        }

        for (name, description) in &skill_hits {
            results.push(json!({
                "kind": "skill",
                "name": name,
                "description": description,
            }));
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
        description = "Execute 1–10 tool calls. Cross-backend calls run in parallel. Results returned in input order as {server, tool, ok, result/error}. A single call: execute({calls: [{server, tool, arguments}]}). A tool that returns an image, audio or a resource lists it under `attachments` and the block itself follows the JSON."
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
                        }), Vec::new(), start.elapsed().as_millis() as u64, false);
                    }
                };
                let was_down = !matches!(backend.status().await, crate::backend::Status::Up { .. });
                match backend.call(&call.tool, call.arguments, &proxy_cfg).await {
                    Ok(result) => {
                        let elapsed = start.elapsed().as_millis() as u64;
                        // Text is flattened into the convenience `result`
                        // field. Everything else — a screenshot, audio, an
                        // embedded resource — is carried out of here as REAL
                        // content blocks and re-attached to geco's own reply.
                        //
                        // It used to be serialized into the same JSON blob,
                        // which was the worst of both worlds: a 200 KB PNG
                        // became ~270 KB of base64 inside a *text* block, so
                        // no client could render it and the model paid ~70k
                        // tokens to not see an image. Meanwhile `result` read
                        // "" — and `result` is the field the tool description
                        // documents, so a model following the contract
                        // concluded the screenshot returned nothing.
                        let text = result.content.iter()
                            .filter_map(|b| {
                                if let ContentBlock::Text(t) = b { Some(t.text.as_str()) }
                                else { None }
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let attachments: Vec<ContentBlock> = result.content.iter()
                            .filter(|b| !matches!(b, ContentBlock::Text(_)))
                            .cloned()
                            .collect();
                        let ok = !result.is_error.unwrap_or(false);
                        let mut entry = json!({
                            "server": call.server,
                            "tool": call.tool,
                            "ok": ok,
                            "result": text,
                        });
                        // A descriptor, not the payload: this tells the model
                        // something arrived and what it is, in a handful of
                        // tokens, while the bytes ride along as content blocks
                        // the client can actually render.
                        if !attachments.is_empty() {
                            entry["attachments"] = json!(attachments.iter()
                                .map(describe_block)
                                .collect::<Vec<_>>());
                        }
                        if !ok {
                            entry["error"] = json!({
                                "code": "tool_error",
                                "message": text,
                            });
                        }
                        (i, entry, attachments, elapsed, was_down)
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
                        }), Vec::new(), elapsed, was_down)
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
        // Non-text blocks, kept per call index so they can be re-attached in
        // the caller's order rather than in whatever order the tasks finish.
        let mut attachments: Vec<Vec<ContentBlock>> = vec![Vec::new(); n];
        let mut needs_health_update = false;
        while let Some(joined) = join.join_next().await {
            match joined {
                Ok((i, result, blocks, latency_ms, was_down)) => {
                    results[i] = result;
                    attachments[i] = blocks;
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
        // The JSON summary first — it is what the model reads and what the
        // tool description promises — then the raw blocks, in call order, so a
        // client renders the screenshot it asked for instead of receiving a
        // page of base64 it has to pretend is prose.
        let mut content = vec![ContentBlock::text(
            serde_json::to_string_pretty(&json!(results))
                .unwrap_or_else(|e| format!("serialize error: {e}")),
        )];
        content.extend(attachments.into_iter().flatten());
        Ok(CallToolResult::success(content))
    }

    #[tool(
        description = "List every available agent skill (name + one-line description). Use when asked what skills exist; use search with kind=\"skills\" for targeted lookup."
    )]
    pub async fn list_skills(&self) -> Result<CallToolResult, McpError> {
        let idx = self.skills.read().await;
        if idx.skills.is_empty() {
            // This tool is only advertised when `skills.dirs` is non-empty, so
            // reaching here means the library IS configured and simply empty —
            // the one state in which "configure `skills.dirs`" is guaranteed
            // to be wrong advice. Every fresh install is in it: both
            // installers scaffold `geco.example.yaml`, whose `dirs` points at
            // a path most new users have never had, and geco then creates it
            // empty. The old message sent the user to edit a setting that was
            // never the problem.
            //
            // Nor is an empty library an error. `search` already returns
            // count 0 rather than `is_error` for exactly this reason: a valid
            // empty answer marked as a failed call tells the model the call
            // itself broke.
            let dirs = self.skill_dirs.read().await.clone();
            return Ok(text_result(json!({
                "skills": [],
                "count": 0,
                "searched": dirs,
                "hint": "no skills found — the library is configured but empty. \
                         A skill is a directory containing SKILL.md.",
            })));
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
}

// Per-instance router rather than the static one, so the advertised tool
// surface can depend on configuration.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for Proxy {
    async fn initialize(
        &self,
        params: InitializeRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        // Record who we're serving ("opencode", "claude-ai", …) so the health
        // file is tagged and the companion plugin only toasts for its client.
        *self.client.write().await = Some(params.client_info.name.clone());
        // Does this client let a server borrow its model? That decides whether
        // the picker can use MCP sampling (zero config, the user's own model)
        // or needs a configured command. Most clients do not implement it yet,
        // hence the other transports.
        let sampling = params.capabilities.sampling.is_some();
        *self.client_supports_sampling.write().await = sampling;
        // Keep the connection itself: `create_message` is a request *back* to
        // the client, and this handler is the only place the peer is handed
        // to us. A reconnect re-runs initialize, so a stale peer is replaced
        // rather than accumulated — and a client that failed sampling on the
        // previous connection gets a clean slate on the new one.
        *self.peer.write().await = Some(ctx.peer.clone());
        self.sampling_broken
            .store(false, std::sync::atomic::Ordering::Relaxed);
        tracing::debug!(
            "client '{}' sampling capability: {sampling}",
            params.client_info.name
        );
        // Re-publish health now that we know who we serve, so the file is
        // tagged for the companion plugin's client filter.
        self.publish_health().await;
        Ok(self.get_info())
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(self.instructions.clone())
    }
}

#[cfg(test)]
mod param_tests {
    use super::*;

    /// A Proxy with no backends and the given skill dirs, for surface tests.
    /// `Proxy::new` is sync and touches the filesystem only through
    /// absorb/scan, which are no-ops for empty or missing dirs.
    fn surface_probe(skill_dirs: Vec<String>) -> Proxy {
        let cfg: crate::config::Config =
            serde_json::from_value(json!({"mcpServers": {"s": {"command": ["true"]}}})).unwrap();
        Proxy::new(
            crate::backend::Registry::from_config(&cfg),
            Snapshot::default(),
            PathBuf::from("geco-test-snapshot.json"),
            PathBuf::from("geco-test-config.yaml"),
            skill_dirs,
            vec![],
        )
    }

    /// Two of the six meta-tools exist only to serve a skill library. If
    /// none is configured they are permanent context describing a capability
    /// the user doesn't have — and an invitation to call something that can
    /// only answer "nothing here".
    #[test]
    fn skill_meta_tools_are_hidden_without_a_library() {
        let with = surface_probe(vec!["/nonexistent/skills".into()]);
        let without = surface_probe(vec![]);

        let names = |p: &Proxy| -> Vec<String> {
            p.tool_router
                .list_all()
                .iter()
                .map(|t| t.name.to_string())
                .collect()
        };
        let (a, b) = (names(&with), names(&without));

        assert_eq!(a.len(), 6, "a configured library advertises all 6: {a:?}");
        assert!(a.contains(&"list_skills".to_string()));
        assert!(a.contains(&"read_skill".to_string()));

        assert_eq!(b.len(), 4, "no library should advertise 4: {b:?}");
        assert!(!b.contains(&"list_skills".to_string()), "got {b:?}");
        assert!(!b.contains(&"read_skill".to_string()), "got {b:?}");
        // search survives — it still finds tools, and explains itself when
        // asked for skills that aren't there.
        assert!(b.contains(&"search".to_string()), "got {b:?}");

        let cost = |p: &Proxy| {
            serde_json::to_string(&p.tool_router.list_all())
                .unwrap()
                .len()
        };
        let (ca, cb) = (cost(&with), cost(&without));
        eprintln!(
            "surface: with skills {ca} B, without {cb} B (saves {} B)",
            ca - cb
        );
        assert!(cb < ca, "hiding two tools must cost fewer bytes");
    }

    /// The whole product claim: the agent's context carries a small, FIXED
    /// meta-tool surface no matter how many backend tools hide behind it.
    /// Nothing asserted that, so a verbose description could quietly inflate
    /// every session's cost. The ceiling is deliberately loose — this guards
    /// against ballooning, not against ordinary wording edits.
    #[test]
    fn meta_tool_surface_stays_within_its_context_budget() {
        let tools = Proxy::tool_router().list_all();
        assert_eq!(tools.len(), 6, "expected 6 meta-tools, got {}", tools.len());

        let wire = serde_json::to_string(&tools).expect("tools must serialize");
        // The instructions ride in the initialize result and stay in context
        // for the whole session, exactly like the schemas do. Omitting them
        // understated the headline number and — worse — left an unguarded
        // channel where always-on cost could grow without any test noticing.
        // Worst case: base instructions plus the skill nudge. A 3-digit count
        // is as long as the sentence ever gets, since it carries a number
        // rather than names.
        let instr = format!("{INSTRUCTIONS_BASE}{}", skill_instructions(999));
        let bytes = wire.len() + instr.len();
        // Visible with `cargo test -- --nocapture` when you want the number.
        eprintln!(
            "meta-tool surface: {} tools, {} B schema + {} B instructions = {bytes} B",
            tools.len(),
            wire.len(),
            instr.len()
        );
        // Measured at 3780 + 596 bytes; the budget leaves ~27% headroom.
        // Schema was 4305 with eight tools. `health` (218 B) and
        // `refresh_tools` (480 B) both became CLI commands, and describing
        // list_mcp_servers' new `issues` field spent 56 B back. A further
        // 118 B went on telling `execute`'s caller that a tool returning an
        // image lists it under `attachments` — bought deliberately, because
        // an undocumented field is a field the model does not use.
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
