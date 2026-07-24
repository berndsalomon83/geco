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

use crate::config::{Config, ProxyConfig, ServerConfig};
use crate::snapshot::{ServerSnapshot, Snapshot, ToolSnapshot};

type Conn = Arc<RunningService<RoleClient, ()>>;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "lowercase", tag = "state")]
pub enum Status {
    /// Not running — will start on first use.
    Cold,
    Up { since: String },
    Failed { error: String, at: String },
}

pub struct Backend {
    pub name: String,
    pub cfg: ServerConfig,
    conn: Mutex<Option<Conn>>,
    status: Mutex<Status>,
}

pub(crate) fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

impl Backend {
    pub fn new(name: String, cfg: ServerConfig) -> Self {
        Self {
            name,
            cfg,
            conn: Mutex::new(None),
            status: Mutex::new(Status::Cold),
        }
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
        let (bin, argv, envs) = tokio::task::spawn_blocking(move || -> Result<_> {
            let (bin, argv) = cfg.command_line()?;
            let envs = cfg.env_expanded()?;
            Ok((bin, argv, envs))
        })
        .await??;
        TokioChildProcess::new(Command::new(&bin).configure(|c| {
            c.args(&argv);
            c.envs(&envs);
        }))
        .with_context(|| format!("cannot spawn backend '{}' ({bin})", name))
    }

    async fn connect_raw(&self, startup: Duration) -> Result<Conn> {
        let transport = self.resolve_and_spawn().await?;
        let svc = tokio::time::timeout(startup, ().serve(transport))
            .await
            .map_err(|_| anyhow!("backend '{}' did not initialize within {startup:?}", self.name))?
            .with_context(|| format!("backend '{}' failed MCP initialize", self.name))?;
        Ok(Arc::new(svc))
    }

    /// Long-lived connection for tool execution (lazy, cached, self-healing).
    async fn connection(&self, startup: Duration) -> Result<Conn> {
        // Check for a live connection under the lock, but don't hold it
        // across connect_raw — that can take up to startup_timeout_secs and
        // would block check_health, shutdown, and concurrent calls.
        {
            let guard = self.conn.lock().await;
            if let Some(c) = guard.as_ref() {
                return Ok(c.clone());
            }
        }
        // Connect without holding the lock.
        match self.connect_raw(startup).await {
            Ok(c) => {
                let mut guard = self.conn.lock().await;
                if let Some(existing) = guard.as_ref() {
                    // Another task won the race — our child process will be
                    // dropped (killed by Drop on the Conn). Log it.
                    tracing::info!(
                        "backend '{}': concurrent connect race — discarding our connection, using winner's",
                        self.name
                    );
                    return Ok(existing.clone());
                }
                *guard = Some(c.clone());
                drop(guard);
                *self.status.lock().await = Status::Up { since: now() };
                Ok(c)
            }
            Err(e) => {
                let guard = self.conn.lock().await;
                if let Some(existing) = guard.as_ref() {
                    // Another task connected successfully while we failed.
                    // Return their connection instead of the error — the
                    // agent sees success, not a spurious backend_error.
                    tracing::info!(
                        "backend '{}': concurrent connect won after our failure — using winner's connection",
                        self.name
                    );
                    return Ok(existing.clone());
                }
                drop(guard);
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
            bail!("DENY: tool '{tool}' is not allowed on server '{}' (allow/deny config)", self.name);
        }
        let conn = self
            .connection(Duration::from_secs(proxy.startup_timeout_secs))
            .await?;
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }
        let res = tokio::time::timeout(
            Duration::from_secs(proxy.call_timeout_secs),
            conn.call_tool(params),
        )
        .await;
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
                    proxy.call_timeout_secs
                ))
            }
        }
    }

    /// List tools for the snapshot. Reuses a live connection if present,
    /// otherwise connects briefly and tears down again (stays lazy).
    pub async fn snapshot_tools(&self, proxy: &ProxyConfig) -> Result<Vec<ToolSnapshot>> {
        let startup = Duration::from_secs(proxy.startup_timeout_secs);
        let list_timeout = Duration::from_secs(proxy.call_timeout_secs);
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

    /// Check if a warm backend's connection is still alive. If the child
    /// process has died, `RunningService::is_closed()` returns true and we
    /// reset the connection so the next `call` reconnects fresh.
    pub async fn check_health(&self) -> bool {
        let mut guard = self.conn.lock().await;
        let is_dead = guard.as_ref().map(|c| c.is_closed()).unwrap_or(false);
        if is_dead {
            tracing::warn!("backend '{}' connection is dead — resetting", self.name);
            *guard = None;
            drop(guard);
            *self.status.lock().await = Status::Failed {
                error: "connection lost (child process exited)".into(),
                at: now(),
            };
            return false;
        }
        true
    }
}

#[derive(Clone)]
pub struct Registry {
    pub proxy: ProxyConfig,
    pub backends: BTreeMap<String, Arc<Backend>>,
}

impl Registry {
    pub fn from_config(cfg: &Config) -> Self {
        let backends = cfg
            .servers
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(name, s)| {
                (
                    name.clone(),
                    Arc::new(Backend::new(name.clone(), s.clone())),
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
            join.spawn(async move {
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
}
