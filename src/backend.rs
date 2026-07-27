//! Backend lifecycle: each configured MCP server stays COLD (no process)
//! until the first `execute` touches it. Snapshot refreshes use a
//! short-lived connection that is torn down immediately afterwards.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::{Config, EnvPolicy, ProxyConfig, ServerConfig};
use crate::snapshot::{ServerSnapshot, Snapshot, ToolSnapshot};

type Conn = Arc<RunningService<RoleClient, BackendClient>>;

/// How long a health probe waits before giving up on a reply. A timeout is
/// treated as "alive but busy", not dead (see check_health), so this bounds
/// how long a sweep can take, not how patient it is with a dying backend —
/// a dead child fails the probe promptly with a transport error regardless.
const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The client half of a backend connection.
///
/// Exists so geco can react to `notifications/tools/list_changed`. The unit
/// type `()` also implements `ClientHandler`, which is what this used to
/// connect with — and its default hooks accept the notification and drop it
/// on the floor. A backend that gains or loses tools at runtime therefore
/// stayed wrong in the snapshot until someone happened to run `geco refresh`,
/// with nothing anywhere reporting the drift.
#[derive(Clone)]
pub struct BackendClient {
    name: String,
    /// Where to report a tool-list change. `None` for short-lived connections
    /// (snapshot builds, the `refresh` CLI) that nobody is listening for.
    notify: Option<tokio::sync::mpsc::Sender<String>>,
}

impl rmcp::handler::client::ClientHandler for BackendClient {
    async fn on_tool_list_changed(&self, _context: rmcp::service::NotificationContext<RoleClient>) {
        let Some(tx) = &self.notify else { return };
        tracing::info!("backend '{}' reported tools/list_changed", self.name);
        // try_send, not send: a full queue means a refresh is already pending,
        // and one refresh subsumes any number of notifications. Awaiting here
        // would also block the connection's read loop.
        if let Err(e) = tx.try_send(self.name.clone()) {
            tracing::debug!(
                "backend '{}': tool-change signal not queued ({e}) — a refresh is already pending",
                self.name
            );
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum Status {
    /// Not running — will start on first use.
    Cold,
    Up {
        since: String,
    },
    Failed {
        error: String,
        at: String,
    },
}

pub struct Backend {
    pub name: String,
    pub cfg: ServerConfig,
    /// How much of geco's environment this backend's child may see.
    /// Held here rather than threaded through every call path, since it is
    /// fixed for the lifetime of the Backend (a config change rebuilds the
    /// whole Registry).
    env_policy: EnvPolicy,
    /// Set once by the Proxy so long-lived connections can report
    /// `tools/list_changed`. Absent for CLI and snapshot-only use.
    notify: std::sync::OnceLock<tokio::sync::mpsc::Sender<String>>,
    conn: Mutex<Option<Conn>>,
    status: Mutex<Status>,
    /// Single-flight for `connect_raw`, holding the last attempt's error.
    ///
    /// Deliberately NOT `conn`: this is held for the length of a spawn plus an
    /// MCP handshake (up to `startup_timeout_secs`), and `check_health` and
    /// `shutdown` must not queue behind that. It exists because a batched
    /// `execute` used to have every call miss the cache at once and start its
    /// own child — eight `npx` processes for one eight-call batch, seven of
    /// them discarded seconds later. For a backend holding an exclusive
    /// resource (a browser profile lock, a fixed port, a single-writer DB)
    /// the winner could then fail *because* of the losers, and the user saw an
    /// intermittent `backend_error` that vanished on retry.
    connect_gate: Mutex<Option<String>>,
    /// Bumped once per completed connect attempt, win or lose. A caller that
    /// queued on the gate compares this against what it saw on arrival to tell
    /// "nobody has tried yet" from "somebody just tried and failed" — without
    /// which the queue would spawn N sequential doomed children instead of N
    /// parallel ones, which is slower rather than better.
    connect_epoch: std::sync::atomic::AtomicU64,
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl Backend {
    pub fn new(name: String, cfg: ServerConfig, env_policy: EnvPolicy) -> Self {
        Self {
            name,
            cfg,
            env_policy,
            notify: std::sync::OnceLock::new(),
            conn: Mutex::new(None),
            status: Mutex::new(Status::Cold),
            connect_gate: Mutex::new(None),
            connect_epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Route this backend's `tools/list_changed` notifications to `tx`.
    /// Takes effect on the next connect; already-warm connections keep the
    /// handler they were built with.
    pub fn set_tool_change_notifier(&self, tx: tokio::sync::mpsc::Sender<String>) {
        let _ = self.notify.set(tx);
    }

    pub async fn status(&self) -> Status {
        self.status.lock().await.clone()
    }

    /// Resolve command + env off the async thread — `env_expanded` may run a
    /// keychain subprocess (`security`/`secret-tool`), which is blocking I/O
    /// that shouldn't stall a tokio worker. Building the `TokioChildProcess`
    /// itself is cheap and stays on the async side.
    async fn resolve_and_spawn(&self) -> Result<TokioChildProcess> {
        let cfg = self.cfg.clone();
        let name = self.name.clone();
        let policy = self.env_policy.clone();
        let (bin, argv, clear_env, envs) = tokio::task::spawn_blocking(move || -> Result<_> {
            let (bin, argv) = cfg.command_line()?;
            let (clear_env, envs) = cfg.env_for_spawn(&policy)?;
            Ok((bin, argv, clear_env, envs))
        })
        .await??;
        if clear_env {
            tracing::debug!(
                "backend '{}': isolated environment, {} vars passed",
                name,
                envs.len()
            );
        }
        TokioChildProcess::new(Command::new(&bin).configure(|c| {
            c.args(&argv);
            // Order matters: clear first, then apply. `envs` already contains
            // the baseline plus whatever the server declared.
            if clear_env {
                c.env_clear();
            }
            c.envs(&envs);
        }))
        .map_err(|e| {
            // "Command not found" is the most common first-run stumble, and a
            // bare "cannot spawn (npx)" sends people hunting the wrong thing.
            // Decided from the error TYPE, not its text, so it survives locale
            // and platform differences. The PATH hint matters because geco
            // isolates each backend's environment: a tool installed via nvm /
            // fnm / volta, or only on a shell's PATH, is invisible unless geco
            // itself was launched from that shell (a GUI-launched agent won't
            // have it).
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow!(
                    "backend '{name}': command '{bin}' not found. Is it installed \
                     and on the PATH geco was started with? geco runs backends in \
                     an isolated environment, so a tool installed via a version \
                     manager (nvm/fnm/volta) or only on your shell's PATH may be \
                     invisible unless geco itself is launched from that shell."
                )
            } else {
                anyhow::Error::new(e).context(format!("cannot spawn backend '{name}' ({bin})"))
            }
        })
    }

    async fn connect_raw(&self, startup: Duration) -> Result<Conn> {
        let transport = self.resolve_and_spawn().await?;
        let client = BackendClient {
            name: self.name.clone(),
            notify: self.notify.get().cloned(),
        };
        let svc = tokio::time::timeout(startup, client.serve(transport))
            .await
            .map_err(|_| {
                anyhow!(
                    "backend '{}' did not initialize within {startup:?}",
                    self.name
                )
            })?
            .with_context(|| format!("backend '{}' failed MCP initialize", self.name))?;
        Ok(Arc::new(svc))
    }

    /// Long-lived connection for tool execution (lazy, cached, self-healing).
    ///
    /// Single-flight: concurrent callers against a cold backend produce
    /// exactly one child process, and every one of them gets that child's
    /// connection — or, if it could not start, that child's error.
    async fn connection(&self, startup: Duration) -> Result<Conn> {
        use std::sync::atomic::Ordering;

        // Warm path, and the only one that matters for throughput: no gate, no
        // contention, just the cached handle. The `conn` lock is never held
        // across a connect, so `check_health` and `shutdown` stay responsive
        // while a cold start is in flight.
        {
            let guard = self.conn.lock().await;
            if let Some(c) = guard.as_ref() {
                return Ok(c.clone());
            }
        }

        // Read the epoch BEFORE queueing, so that after the wait it can be
        // compared against what actually happened while we were queued.
        let seen = self.connect_epoch.load(Ordering::SeqCst);
        let mut gate = self.connect_gate.lock().await;

        // Someone connected while we waited — take theirs. This is the good
        // case, and the reason nothing else spawns a child.
        {
            let guard = self.conn.lock().await;
            if let Some(c) = guard.as_ref() {
                return Ok(c.clone());
            }
        }
        // Someone tried while we waited and left no connection behind, so they
        // failed. Report their error rather than starting an identical child
        // to fail the same way: for a batch of ten calls against a backend
        // whose command is not installed, retrying in turn would serialise ten
        // full startup timeouts where the answer was known after the first.
        if self.connect_epoch.load(Ordering::SeqCst) != seen {
            if let Some(err) = gate.as_ref() {
                tracing::debug!(
                    "backend '{}': a concurrent connect just failed — reusing its error",
                    self.name
                );
                return Err(anyhow::anyhow!(err.clone()));
            }
        }

        let result = self.connect_raw(startup).await;
        // Bumped for win and loss alike: what a queued caller needs to know is
        // that an attempt *completed*, not how it went.
        self.connect_epoch.fetch_add(1, Ordering::SeqCst);
        match result {
            Ok(c) => {
                *gate = None;
                *self.conn.lock().await = Some(c.clone());
                *self.status.lock().await = Status::Up { since: now() };
                Ok(c)
            }
            Err(e) => {
                *gate = Some(e.to_string());
                *self.status.lock().await = Status::Failed {
                    error: e.to_string(),
                    at: now(),
                };
                Err(e)
            }
        }
    }

    /// Execute a tool, lazily starting the backend. A transport-level error
    /// resets the cached connection so the next call reconnects fresh.
    pub async fn call(
        &self,
        tool: &str,
        arguments: Option<serde_json::Map<String, serde_json::Value>>,
        proxy: &ProxyConfig,
    ) -> Result<CallToolResult> {
        if !self.cfg.tool_allowed(tool) {
            bail!(
                "DENY: tool '{tool}' is not allowed on server '{}' (allow/deny config)",
                self.name
            );
        }
        let conn = self.connection(self.cfg.startup_timeout(proxy)).await?;
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let call_timeout = self.cfg.call_timeout(proxy);
        let res = tokio::time::timeout(call_timeout, conn.call_tool(params)).await;
        match res {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => {
                // Transport/protocol error. If the connection is dead (child
                // exited, transport closed), reset so the next call reconnects
                // fresh instead of reusing a dead conn for up to 30s.
                //
                // Decided from the error TYPE, never its text. This used to
                // substring-match "stream closed"/"broken pipe"/"connection
                // reset", which was wrong in both directions: rmcp actually
                // reports a dead child as "Transport closed" — matching none
                // of those, so a crashed backend stayed cached as Up until the
                // 30s health check noticed — while a healthy HTTP/browser
                // backend reporting "connection reset by peer" about ITS
                // upstream would get its child torn down for nothing.
                //
                // TransportClosed/TransportSend mean our pipe to the child is
                // gone. McpError means the child answered, with an error — it
                // is alive, and its message is its own text to write.
                let is_dead = matches!(
                    e,
                    ServiceError::TransportClosed | ServiceError::TransportSend(_)
                ) || conn.is_closed();
                let msg = format!("{e:#}");
                if is_dead {
                    *self.conn.lock().await = None;
                    *self.status.lock().await = Status::Failed {
                        error: format!("transport error: {msg}"),
                        at: now(),
                    };
                }
                Err(anyhow!("backend '{}' call failed: {e}", self.name))
            }
            Err(_) => {
                // Timeout: connection state unknown — reset so we reconnect.
                *self.conn.lock().await = None;
                *self.status.lock().await = Status::Failed {
                    error: format!("call to '{tool}' timed out"),
                    at: now(),
                };
                Err(anyhow!(
                    "backend '{}' timed out after {}s calling '{tool}'",
                    self.name,
                    call_timeout.as_secs()
                ))
            }
        }
    }

    /// List tools for the snapshot. Reuses a live connection if present,
    /// otherwise connects briefly and tears down again (stays lazy).
    pub async fn snapshot_tools(&self, proxy: &ProxyConfig) -> Result<Vec<ToolSnapshot>> {
        let startup = self.cfg.startup_timeout(proxy);
        let list_timeout = self.cfg.call_timeout(proxy);
        let (conn, ephemeral) = {
            let guard = self.conn.lock().await;
            match guard.as_ref() {
                Some(c) => (Some(c.clone()), false),
                None => (None, true),
            }
        };
        let conn = if let Some(c) = conn {
            c
        } else {
            self.connect_raw(startup).await?
        };
        let listed = tokio::time::timeout(list_timeout, conn.list_all_tools())
            .await
            .map_err(|_| anyhow!("backend '{}' tools/list timed out", self.name))?
            .with_context(|| format!("backend '{}' tools/list failed", self.name))?;
        if ephemeral {
            // Tear down: we only wanted the catalogue. Dropping the last Arc
            // kills the child process via the transport.
            drop(conn);
        }
        let mut tools: Vec<ToolSnapshot> = listed
            .into_iter()
            .filter(|t| self.cfg.tool_allowed(&t.name))
            .map(|t| ToolSnapshot {
                name: t.name.to_string(),
                description: t.description.as_deref().unwrap_or_default().to_string(),
                input_schema: serde_json::Value::Object((*t.input_schema).clone()),
            })
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tools)
    }

    pub async fn shutdown(&self) {
        let mut guard = self.conn.lock().await;
        *guard = None;
        *self.status.lock().await = Status::Cold;
    }

    /// Is a warm backend's connection actually alive? A cold backend has
    /// nothing to check and passes.
    ///
    /// This ACTIVELY PROBES, because `is_closed()` alone is not enough.
    /// `is_closed()` reports whether *our* side tore the service down — it
    /// flips when the peer read loop sees EOF while a request is in flight,
    /// but an rmcp client only reads the transport when it has an outstanding
    /// request. A child that dies while IDLE therefore leaves `is_closed()`
    /// false indefinitely: measured, the connection still reported healthy 10
    /// seconds after the child was killed, while a real call failed at once.
    /// So the periodic checker — whose entire job is to catch a silently-dead
    /// idle backend before the agent does — was blind to exactly that case.
    ///
    /// The probe is `list_tools`, not the spec's `ping`, on purpose. Ping is
    /// a MUST that real servers nonetheless skip, and a live backend that
    /// ignored ping would be reset on every sweep — worse than the bug this
    /// fixes. Every backend geco can proxy answers `list_tools`, because geco
    /// calls it to build the snapshot in the first place; a server that
    /// didn't could not be used at all. It travels the same transport a real
    /// call would, so a dead child fails it.
    pub async fn check_health(&self) -> bool {
        // Snapshot the connection, then release the lock: the probe can take
        // up to HEALTH_PROBE_TIMEOUT, and holding the lock across it would
        // stall every concurrent call and shutdown for that long.
        let conn = {
            let guard = self.conn.lock().await;
            match guard.as_ref() {
                Some(c) => c.clone(),
                None => return true, // cold — nothing to check
            }
        };

        let dead = if conn.is_closed() {
            true // definitively torn down; no need to probe
        } else {
            match tokio::time::timeout(HEALTH_PROBE_TIMEOUT, conn.list_all_tools()).await {
                Ok(Ok(_)) => false, // answered — alive
                Ok(Err(_)) => true, // transport/protocol error — the pipe is gone: dead
                // A TIMEOUT is not death. A dead child fails the probe
                // *promptly* with a transport error (the pipe is closed), so
                // it lands in the arm above. A timeout means the child is
                // there but did not answer in time — and the most common
                // reason is that it is BUSY: many MCP servers process
                // requests serially, so a probe sent while a long tool call
                // is running waits behind it. Resetting on timeout would tear
                // down a live, working backend mid-call and spawn a duplicate
                // child every health sweep that overlaps a slow call — a
                // self-inflicted outage on exactly the backends people run
                // (browsers, databases). A genuinely wedged-but-alive backend
                // is left for the reactive path, where the call's own timeout
                // handles it. So: do not treat a timeout as dead.
                Err(_) => false,
            }
        };

        if !dead {
            return true;
        }

        // Reset — but only if the connection we probed is still the current
        // one. A concurrent `call` may have already detected the death and
        // reconnected, or a `shutdown` may have cleared it; clobbering either
        // would be its own bug. If it was replaced, the death we saw belongs
        // to a connection nobody uses anymore, so report healthy.
        let mut guard = self.conn.lock().await;
        let same = guard
            .as_ref()
            .map(|c| Arc::ptr_eq(c, &conn))
            .unwrap_or(false);
        if !same {
            return true;
        }
        tracing::warn!(
            "backend '{}' failed its health probe — resetting",
            self.name
        );
        *guard = None;
        drop(guard);
        *self.status.lock().await = Status::Failed {
            error: "connection lost (health probe failed)".into(),
            at: now(),
        };
        false
    }
}

#[derive(Clone)]
pub struct Registry {
    pub proxy: ProxyConfig,
    pub backends: BTreeMap<String, Arc<Backend>>,
}

impl Registry {
    pub fn from_config(cfg: &Config) -> Self {
        let policy = cfg.proxy.env_policy();
        let backends = cfg
            .servers
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(name, s)| {
                (
                    name.clone(),
                    Arc::new(Backend::new(name.clone(), s.clone(), policy.clone())),
                )
            })
            .collect();
        Self {
            proxy: cfg.proxy.clone(),
            backends,
        }
    }

    pub fn get(&self, name: &str) -> Result<Arc<Backend>> {
        self.backends
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown server '{name}' — see list_mcp_servers"))
    }

    /// Build a fresh snapshot by asking every backend (in parallel) for its
    /// tools. Failures are recorded per server, never fatal for the proxy.
    pub async fn build_snapshot(&self, config_hash: String, only: Option<&str>) -> Snapshot {
        let mut snap = Snapshot {
            config_hash,
            generated_at: now(),
            ..Default::default()
        };
        // Cap how many backends start at once. Every one of these spawns a
        // child process, so an uncapped refresh of a large config is a
        // process storm at exactly the wrong moment — agent startup.
        // 0 means "unlimited" (documented). It must NOT be usize::MAX:
        // tokio's semaphore panics above MAX_PERMITS (usize::MAX >> 3), so
        // the documented way to disable the cap used to crash the refresh
        // task outright. MAX_PERMITS is unlimited for any real config.
        let limit = if self.proxy.refresh_concurrency == 0 {
            tokio::sync::Semaphore::MAX_PERMITS
        } else {
            self.proxy
                .refresh_concurrency
                .min(tokio::sync::Semaphore::MAX_PERMITS)
        };
        let permits = Arc::new(tokio::sync::Semaphore::new(limit));

        let mut join = tokio::task::JoinSet::new();
        for (name, backend) in &self.backends {
            if let Some(filter) = only {
                if filter != name {
                    continue;
                }
            }
            let backend = backend.clone();
            let proxy = self.proxy.clone();
            let name = name.clone();
            let permits = permits.clone();
            join.spawn(async move {
                // Held until this backend is done being probed. A closed
                // semaphore can't happen here (nothing closes it), so failing
                // to acquire just means proceeding uncapped rather than
                // silently dropping the server from the snapshot.
                let _permit = permits.acquire_owned().await.ok();
                let result = backend.snapshot_tools(&proxy).await;
                (name, backend.cfg.description.clone(), result)
            });
        }
        while let Some(joined) = join.join_next().await {
            let (name, description, result) = match joined {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("build_snapshot: task panicked: {e}");
                    continue;
                }
            };
            let entry = match result {
                Ok(tools) => ServerSnapshot {
                    description: description.unwrap_or_default(),
                    refresh_status: "ok".into(),
                    tools,
                },
                Err(e) => {
                    tracing::warn!("snapshot refresh failed for '{name}': {e:#}");
                    ServerSnapshot {
                        description: description.unwrap_or_default(),
                        refresh_status: format!("error: {e}"),
                        tools: Vec::new(),
                    }
                }
            };
            snap.servers.insert(name, entry);
        }
        snap
    }

    pub async fn shutdown_all(&self) {
        for backend in self.backends.values() {
            backend.shutdown().await;
        }
    }

    /// Route every backend's `tools/list_changed` notifications to `tx`.
    /// Called by the Proxy; the CLI paths (`refresh`, `inspect`) leave it
    /// unset, so their short-lived connections stay silent.
    pub fn set_tool_change_notifier(&self, tx: tokio::sync::mpsc::Sender<String>) {
        for backend in self.backends.values() {
            backend.set_tool_change_notifier(tx.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config from a JSON literal, so tests read like the file a user
    /// writes rather than like struct construction.
    fn config(v: serde_json::Value) -> Config {
        serde_json::from_value(v).expect("test config must deserialize")
    }

    fn registry(v: serde_json::Value) -> Registry {
        Registry::from_config(&config(v))
    }

    /// `enabled: false` has to be honoured at *construction*, not filtered
    /// later. A disabled server that still exists in the registry is one
    /// stray `get()` away from being spawned.
    #[test]
    fn disabled_servers_never_enter_the_registry() {
        let r = registry(serde_json::json!({
            "mcpServers": {
                "on":  { "command": ["true"] },
                "off": { "command": ["true"], "enabled": false },
            }
        }));
        assert!(r.backends.contains_key("on"));
        assert!(
            !r.backends.contains_key("off"),
            "a disabled server must not be constructed at all, got {:?}",
            r.backends.keys().collect::<Vec<_>>()
        );
        assert!(r.get("off").is_err(), "and must not be reachable by name");
    }

    /// The error a user sees when the agent invents a server name. It should
    /// say what to do next, not just fail.
    #[test]
    fn unknown_server_error_points_somewhere_useful() {
        let r = registry(serde_json::json!({ "mcpServers": { "a": { "command": ["true"] } } }));
        // `Backend` is not Debug, so unwrap_err() is unavailable on the Ok
        // side of this Result — match instead.
        let err = match r.get("nope") {
            Ok(_) => panic!("an unknown server must not resolve"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("nope"), "should name the server: {err}");
        assert!(
            err.contains("list_mcp_servers"),
            "should say how to find the real name: {err}"
        );
    }

    /// Backends are lazy: constructing a registry must not start anything.
    /// If this ever regresses, every `geco inspect` and every proxy startup
    /// becomes a process storm.
    #[tokio::test]
    async fn backends_start_cold() {
        let r = registry(serde_json::json!({
            "mcpServers": { "a": { "command": ["/nonexistent/binary"] } }
        }));
        assert!(
            matches!(r.get("a").unwrap().status().await, Status::Cold),
            "a freshly built backend must be Cold, not started"
        );
    }

    /// deny is enforced at EXECUTION, not merely by hiding tools from
    /// listings. This is the security-relevant half: hiding a tool is
    /// cosmetic if calling it by name still works.
    ///
    /// It also must be decided BEFORE connecting — the command here does not
    /// exist, so if the check happened after `connection()` this would fail
    /// with a spawn error instead of DENY.
    #[tokio::test]
    async fn denied_tools_are_refused_without_ever_spawning() {
        let cfg = config(serde_json::json!({
            "mcpServers": {
                "a": { "command": ["/nonexistent/binary"], "deny": ["secret_*"] }
            }
        }));
        let r = Registry::from_config(&cfg);
        let backend = r.get("a").unwrap();

        let err = format!(
            "{:#}",
            backend
                .call("secret_thing", None, &cfg.proxy)
                .await
                .expect_err("a denied tool must not be callable")
        );
        assert!(
            err.starts_with("DENY:"),
            "denial must be reported as DENY (execute maps this to \
             not_allowed), got: {err}"
        );
        assert!(
            matches!(backend.status().await, Status::Cold),
            "a denied call must not have started the backend"
        );
    }

    /// A backend that cannot spawn is recorded as a per-server failure, and
    /// the snapshot still comes back. One broken server must never take the
    /// gateway down with it.
    #[tokio::test]
    async fn a_backend_that_cannot_spawn_fails_only_itself() {
        let r = registry(serde_json::json!({
            "mcpServers": {
                "broken": { "command": ["/nonexistent/binary"], "description": "d" }
            }
        }));
        let snap = r.build_snapshot("hash".into(), None).await;

        let entry = snap.servers.get("broken").expect(
            "a failing server must still appear in the snapshot, \
                    or it vanishes from list_mcp_servers with no explanation",
        );
        assert!(
            entry.refresh_status.starts_with("error:"),
            "status should carry the reason: {:?}",
            entry.refresh_status
        );
        // A missing command is the #1 first-run stumble (dogfooding hit it as
        // "cannot spawn (npx)" with no clue). The message must name the missing
        // command and point at PATH, not just say "spawn failed".
        let msg = entry.refresh_status.to_lowercase();
        assert!(
            msg.contains("not found") && msg.contains("path"),
            "a not-found command must explain itself and mention PATH: {:?}",
            entry.refresh_status
        );
        assert!(entry.tools.is_empty());
        assert_eq!(snap.config_hash, "hash");
    }

    /// `only` restricts the refresh to one server. The bug this guards is
    /// subtle and was hit for real: a single-server refresh must not produce
    /// a snapshot that *omits* everything else, because callers merge on the
    /// assumption that absence means "unchanged", not "gone".
    #[tokio::test]
    async fn a_filtered_refresh_touches_only_that_server() {
        let r = registry(serde_json::json!({
            "mcpServers": {
                "a": { "command": ["/nonexistent/binary"] },
                "b": { "command": ["/nonexistent/binary"] },
            }
        }));
        let snap = r.build_snapshot("h".into(), Some("a")).await;
        assert_eq!(
            snap.servers.keys().collect::<Vec<_>>(),
            vec!["a"],
            "a filtered refresh must contain exactly the filtered server"
        );

        // A filter naming nothing must yield an empty snapshot rather than
        // silently falling back to refreshing everything.
        let none = r.build_snapshot("h".into(), Some("does-not-exist")).await;
        assert!(
            none.servers.is_empty(),
            "an unmatched filter must not refresh everything: {:?}",
            none.servers.keys().collect::<Vec<_>>()
        );
    }

    /// refresh_concurrency = 0 means "uncapped", not "no permits" — a
    /// semaphore built with 0 would deadlock every backend forever.
    #[tokio::test]
    async fn zero_refresh_concurrency_means_uncapped_not_stalled() {
        let r = registry(serde_json::json!({
            "mcpServers": {
                "a": { "command": ["/nonexistent/binary"] },
                "b": { "command": ["/nonexistent/binary"] },
            },
            "proxy": { "refresh_concurrency": 0 }
        }));
        let snap = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            r.build_snapshot("h".into(), None),
        )
        .await
        .expect("refresh_concurrency=0 must not hang");
        assert_eq!(snap.servers.len(), 2);
    }

    /// Shutting down backends that were never started must be a no-op, not a
    /// hang or a panic. This runs on every clean exit.
    #[tokio::test]
    async fn shutting_down_cold_backends_is_harmless() {
        let r = registry(serde_json::json!({
            "mcpServers": { "a": { "command": ["true"] }, "b": { "command": ["true"] } }
        }));
        tokio::time::timeout(std::time::Duration::from_secs(10), r.shutdown_all())
            .await
            .expect("shutdown_all must not hang on cold backends");
    }

    /// `check_health` answers "is this connection known-dead?", NOT "is this
    /// backend running?". A cold backend has no connection, so nothing is
    /// wrong and it passes — the periodic checker must not tear down or mark
    /// Failed every backend that simply hasn't been used yet.
    ///
    /// Worth stating because the name invites the opposite reading, and the
    /// opposite reading is what this test originally asserted.
    #[tokio::test]
    async fn health_check_passes_a_cold_backend_because_nothing_is_wrong() {
        let r = registry(serde_json::json!({
            "mcpServers": { "a": { "command": ["/nonexistent/binary"] } }
        }));
        let backend = r.get("a").unwrap();
        assert!(
            backend.check_health().await,
            "a cold backend has no dead connection, so the check must pass"
        );
        assert!(
            matches!(backend.status().await, Status::Cold),
            "and the check must not have changed its status"
        );
    }
}
