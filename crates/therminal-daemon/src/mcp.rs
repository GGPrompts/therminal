//! MCP (Model Context Protocol) server for Therminal.
//!
//! Exposes terminal session management tools via the standard MCP protocol,
//! allowing external tools (Claude Code, TUIs, dashboards) to interact with
//! daemon sessions.
//!
//! Trust enforcement is applied to every tool call: the agent's identity is
//! extracted from the MCP `initialize` handshake, looked up against the
//! `[trust]` config section, and checked against the tool's required tier.
//! Destructive tools are additionally rate-limited.
//!
//! The server listens on a dedicated Unix socket (separate from the IPC socket)
//! and uses the `rmcp` crate for protocol handling.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::handler::server::tool::schema_for_type;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use tracing::{debug, error, info, warn};

use therminal_core::config::TrustConfig;

use crate::session::SessionManager;
use crate::trust::{AgentIdentity, RateLimiter, TrustCheckResult, check_tool_access};

// ── Tool parameter types ────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionIdParam {
    /// The numeric session ID.
    session_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSessionParam {
    /// Optional human-readable name for the session.
    name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteToPaneParam {
    /// The numeric pane ID to write to.
    pane_id: u64,
    /// The text (or bytes as UTF-8) to send to the pane's PTY.
    input: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PaneIdParam {
    /// The numeric pane ID.
    pane_id: u64,
}

// ── Tool result types ───────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
struct SessionListResult {
    session_ids: Vec<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SessionInfoResult {
    session_id: u64,
    name: Option<String>,
    created_at_secs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SessionCreatedResult {
    session_id: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SessionDestroyedResult {
    session_id: u64,
    destroyed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct WriteToPaneResult {
    pane_id: u64,
    success: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PaneContentResult {
    pane_id: u64,
    lines: Vec<String>,
    cursor_col: usize,
    cursor_line: usize,
    cols: usize,
    rows: usize,
}

// ── Helper: serialize to JSON content ───────────────────────────────────

fn json_content<T: Serialize>(value: &T) -> Result<Content, ErrorData> {
    Content::json(value)
        .map_err(|e| ErrorData::internal_error(format!("serialization error: {e}"), None))
}

fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Map<String, serde_json::Value>,
) -> Result<T, ErrorData> {
    serde_json::from_value(serde_json::Value::Object(args))
        .map_err(|e| ErrorData::invalid_params(format!("invalid parameters: {e}"), None))
}

// ── Agent identity extraction ──────────────────────────────────────────

/// Extract the agent identity from the MCP connection context.
///
/// Uses the client's `Implementation.name` from the MCP `initialize`
/// handshake. Falls back to `"unknown"` if the peer info is not available
/// (e.g. before initialization completes).
fn extract_agent_identity(context: &RequestContext<RoleServer>) -> AgentIdentity {
    let name = context
        .peer
        .peer_info()
        .map(|info| info.client_info.name.clone())
        .unwrap_or_else(|| "unknown".to_string());
    AgentIdentity { name }
}

// ── MCP Server ──────────────────────────────────────────────────────────

/// Therminal MCP server handler.
///
/// Wraps a shared `SessionManager` and exposes session/pane operations as
/// MCP tools. Each tool call is gated by trust tier enforcement and
/// audit-logged.
pub struct TherminalMcpServer {
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
}

impl TherminalMcpServer {
    /// Create a new MCP server backed by the given session manager and trust config.
    pub fn new(
        session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
        trust_config: Arc<TrustConfig>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            session_mgr,
            trust_config,
            rate_limiter,
        }
    }

    /// Enforce trust tier and rate limiting for the given tool call.
    ///
    /// Returns `Ok(())` if allowed, or an `Err(CallToolResult)` with
    /// a permission-denied error to return to the client.
    fn enforce_trust(&self, tool_name: &str, agent: &AgentIdentity) -> Result<(), CallToolResult> {
        match check_tool_access(tool_name, agent, &self.trust_config, &self.rate_limiter) {
            TrustCheckResult::Allowed => Ok(()),
            TrustCheckResult::Denied(reason) => {
                Err(CallToolResult::error(vec![Content::text(reason)]))
            }
        }
    }

    async fn handle_list_sessions(&self) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let result = SessionListResult {
            session_ids: mgr.list_sessions(),
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_get_session(
        &self,
        params: SessionIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        match mgr.get_session_info(params.session_id) {
            Some((id, name, created_at_secs)) => {
                let result = SessionInfoResult {
                    session_id: id,
                    name,
                    created_at_secs,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "session not found: {}",
                params.session_id
            ))])),
        }
    }

    async fn handle_create_session(
        &self,
        params: CreateSessionParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        match mgr.create_session(params.name) {
            Ok(session_id) => {
                let result = SessionCreatedResult { session_id };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "failed to create session: {e}"
            ))])),
        }
    }

    async fn handle_destroy_session(
        &self,
        params: SessionIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        let destroyed = mgr.destroy_session(params.session_id);
        let result = SessionDestroyedResult {
            session_id: params.session_id,
            destroyed,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_write_to_pane(
        &self,
        params: WriteToPaneParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        match mgr.send_keys_to_pane(params.pane_id, params.input.as_bytes()) {
            Ok(()) => {
                let result = WriteToPaneResult {
                    pane_id: params.pane_id,
                    success: true,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "write_to_pane failed: {e}"
            ))])),
        }
    }

    async fn handle_read_pane_content(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        match mgr.capture_pane(params.pane_id) {
            Ok(snap) => {
                let lines: Vec<String> = snap
                    .grid
                    .iter()
                    .map(|row| row.iter().map(|(ch, _)| ch).collect())
                    .collect();
                let result = PaneContentResult {
                    pane_id: snap.pane_id,
                    lines,
                    cursor_col: snap.cursor_col,
                    cursor_line: snap.cursor_line,
                    cols: snap.cols,
                    rows: snap.rows,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "read_pane_content failed: {e}"
            ))])),
        }
    }
}

// ── Tool definitions ────────────────────────────────────────────────────

fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            "terminal.sessions.list",
            "List all active terminal session IDs",
            schema_for_type::<()>(),
        ),
        Tool::new(
            "terminal.sessions.get",
            "Get details about a specific terminal session (name, creation time)",
            schema_for_type::<SessionIdParam>(),
        ),
        Tool::new(
            "terminal.sessions.create",
            "Create a new terminal session with a shell and PTY. Returns the new session ID.",
            schema_for_type::<CreateSessionParam>(),
        ),
        Tool::new(
            "terminal.sessions.destroy",
            "Destroy a terminal session and all its panes",
            schema_for_type::<SessionIdParam>(),
        ),
        Tool::new(
            "terminal.panes.write",
            "Write text input to a pane's PTY (send keystrokes or commands to the terminal)",
            schema_for_type::<WriteToPaneParam>(),
        ),
        Tool::new(
            "terminal.panes.get_content",
            "Read the current visible content of a terminal pane (grid snapshot with cursor position)",
            schema_for_type::<PaneIdParam>(),
        ),
    ]
}

// ── ServerHandler impl ──────────────────────────────────────────────────

impl ServerHandler for TherminalMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(
            "Therminal MCP server. Provides tools to manage terminal sessions and panes.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.as_ref();
        let args = request.arguments.unwrap_or_default();

        // Extract agent identity from the MCP connection context.
        let agent = extract_agent_identity(&context);

        // Enforce trust tier and rate limiting.
        if let Err(denied_result) = self.enforce_trust(name, &agent) {
            return Ok(denied_result);
        }

        match name {
            "terminal.sessions.list" => self.handle_list_sessions().await,
            "terminal.sessions.get" => {
                let params: SessionIdParam = parse_args(args)?;
                self.handle_get_session(params).await
            }
            "terminal.sessions.create" => {
                let params: CreateSessionParam = parse_args(args)?;
                self.handle_create_session(params).await
            }
            "terminal.sessions.destroy" => {
                let params: SessionIdParam = parse_args(args)?;
                self.handle_destroy_session(params).await
            }
            "terminal.panes.write" => {
                let params: WriteToPaneParam = parse_args(args)?;
                self.handle_write_to_pane(params).await
            }
            "terminal.panes.get_content" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_read_pane_content(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

// ── Server lifecycle ────────────────────────────────────────────────────

/// Start the MCP server, accepting connections on the given Unix socket.
///
/// Each accepted connection is served independently. The server runs until
/// the `shutdown` notify is triggered. Trust enforcement uses the provided
/// `TrustConfig` to gate tool access per agent.
pub async fn start_mcp_server(
    config: therminal_core::config::McpConfig,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<()> {
    if !config.enabled {
        info!("MCP server disabled by config");
        return Ok(());
    }

    let socket_path = config.resolved_socket_path();

    // Clean stale socket
    match std::fs::remove_file(&socket_path) {
        Ok(()) => debug!(path = %socket_path.display(), "removed stale MCP socket"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to remove stale MCP socket {}: {e}",
                socket_path.display()
            ));
        }
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind MCP socket: {}", socket_path.display()))?;

    // Set socket permissions on Unix (owner-only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&socket_path, perms).ok();
    }

    info!(path = %socket_path.display(), "MCP server listening");

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        let sm = Arc::clone(&session_mgr);
                        let tc = Arc::clone(&trust_config);
                        let rl = Arc::clone(&rate_limiter);
                        tokio::spawn(async move {
                            let server = TherminalMcpServer::new(sm, tc, rl);
                            let (reader, writer) = stream.into_split();
                            match server.serve((reader, writer)).await {
                                Ok(running) => {
                                    if let Err(e) = running.waiting().await {
                                        debug!(error = %e, "MCP connection task ended");
                                    }
                                }
                                Err(e) => {
                                    debug!(error = %e, "MCP connection init failed");
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "MCP accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = shutdown.notified() => {
                info!("MCP server shutting down");
                break;
            }
        }
    }

    // Clean up socket
    cleanup_socket(&socket_path);
    Ok(())
}

/// Remove the MCP socket file.
fn cleanup_socket(path: &Path) {
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            warn!(error = %e, path = %path.display(), "failed to remove MCP socket on cleanup");
        } else {
            debug!(path = %path.display(), "MCP socket cleaned up");
        }
    }
}
