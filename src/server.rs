//! The proxy's own MCP surface: five tiny meta-tools instead of every
//! backend schema. This is the whole point — the agent's context carries
//! ~2 KB no matter how many tools hide behind the gate.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters, model::*, tool, tool_handler, tool_router,
    ErrorData as McpError, ServerHandler,
};
use serde_json::json;
use tokio::sync::RwLock;

use crate::backend::Registry;
use crate::search;
use crate::snapshot::{short_desc, Snapshot};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetToolsParams {
    /// Server name from list_servers
    pub server: String,
    /// Optional: a tool name to get the full input schema for
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// What you're looking for, e.g. "store memory" or "create issue"
    pub query: String,
    /// Max results (default 8)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ExecuteParams {
    /// Server name from list_servers / search_tools
    pub server: String,
    /// Tool name on that server
    pub tool: String,
    /// Arguments for the tool, matching its input schema
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<JsonObject>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RefreshParams {
    /// Optional: refresh only this server
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
}

#[derive(Clone)]
pub struct Proxy {
    registry: Arc<Registry>,
    snapshot: Arc<RwLock<Snapshot>>,
    snapshot_path: PathBuf,
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
    pub fn new(registry: Arc<Registry>, snapshot: Snapshot, snapshot_path: PathBuf) -> Self {
        Self {
            registry,
            snapshot: Arc::new(RwLock::new(snapshot)),
            snapshot_path,
        }
    }

    #[tool(
        description = "List the MCP servers behind this proxy: name, what it's for, tool count, running state. Start here."
    )]
    pub async fn list_servers(&self) -> Result<CallToolResult, McpError> {
        let snap = self.snapshot.read().await;
        let mut out = Vec::new();
        for (name, s) in &snap.servers {
            let status = match self.registry.get(name) {
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
        description = "Find tools across all servers by keywords in one call. Returns server + tool + description, best match first."
    )]
    pub async fn search_tools(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        let snap = self.snapshot.read().await;
        let limit = p.limit.unwrap_or(8).clamp(1, 25);
        let hits = search::search(&snap, &p.query, limit);
        if hits.is_empty() {
            return Ok(err(format!(
                "no tools match '{}' — try list_servers or refresh_tools",
                p.query
            )));
        }
        Ok(text_result(json!(hits
            .iter()
            .map(|h| json!({"server": h.server, "tool": h.tool, "description": h.description}))
            .collect::<Vec<_>>())))
    }

    #[tool(
        description = "Execute a tool on a backend server (started on demand). Get the schema via get_tools first if unsure about arguments."
    )]
    pub async fn execute_tool(
        &self,
        Parameters(p): Parameters<ExecuteParams>,
    ) -> Result<CallToolResult, McpError> {
        let backend = match self.registry.get(&p.server) {
            Ok(b) => b,
            Err(e) => return Ok(err(e.to_string())),
        };
        match backend.call(&p.tool, p.arguments, &self.registry.proxy).await {
            Ok(result) => Ok(result),
            Err(e) => Ok(err(format!("{e:#}"))),
        }
    }

    #[tool(
        description = "Re-scan backend servers for their current tools (all, or one via `server`). Use after installing or reconfiguring a backend."
    )]
    pub async fn refresh_tools(
        &self,
        Parameters(p): Parameters<RefreshParams>,
    ) -> Result<CallToolResult, McpError> {
        let hash = { self.snapshot.read().await.config_hash.clone() };
        let fresh = self
            .registry
            .build_snapshot(hash, p.server.as_deref())
            .await;
        let mut snap = self.snapshot.write().await;
        match p.server {
            Some(ref name) => {
                if let Some(entry) = fresh.servers.get(name) {
                    snap.servers.insert(name.clone(), entry.clone());
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
        Ok(text_result(json!(summary)))
    }
}

#[tool_handler]
impl ServerHandler for Proxy {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Gateway to multiple MCP servers whose tools load on demand instead of \
                 polluting your context. Flow: search_tools (or list_servers → get_tools) \
                 to discover, get_tools with `tool` for the schema, execute_tool to run it. \
                 Backends start lazily on first execution."
                    .to_string(),
            )
    }
}
