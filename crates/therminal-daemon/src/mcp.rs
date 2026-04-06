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

#[derive(Debug, Deserialize, JsonSchema)]
struct ListPanesParam {
    /// Optional session ID to filter panes by. If omitted, returns panes from all sessions.
    session_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QuerySemanticHistoryParam {
    /// The numeric pane ID to query.
    pane_id: u64,
    /// Optional region type filter. One of: Prompt, Command, Output, Error, ToolCall, Thinking, Annotation.
    region_type: Option<String>,
    /// Optional regex pattern to match against region content preview.
    pattern: Option<String>,
    /// Maximum number of regions to return (default 20).
    limit: Option<usize>,
    /// Only return regions starting at or after this line number.
    since_line: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetPaneGeometryParam {
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

#[derive(Debug, Serialize, JsonSchema)]
struct PaneInfo {
    pane_id: u64,
    session_id: u64,
    cols: u16,
    rows: u16,
    title: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListPanesResult {
    panes: Vec<PaneInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetPaneGeometryResult {
    pane_id: u64,
    /// Number of columns in the pane.
    cols: u16,
    /// Number of rows in the pane.
    rows: u16,
    /// Whether the pane can be split horizontally (top/bottom) based on minimum dimensions.
    can_split_h: bool,
    /// Whether the pane can be split vertically (left/right) based on minimum dimensions.
    can_split_v: bool,
}

/// Minimum columns required per pane (derived from MIN_PANE_WIDTH / typical cell width).
const MIN_PANE_COLS: u16 = 10;

/// Minimum rows required per pane (derived from MIN_PANE_HEIGHT / typical cell height).
const MIN_PANE_ROWS: u16 = 4;

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
struct SemanticRegionInfo {
    /// The region type (Prompt, Command, Output, Error, ToolCall, Thinking, Annotation).
    region_type: String,
    /// The terminal line where this region starts.
    start_line: usize,
    /// The terminal line where this region ends, or null if still open.
    end_line: Option<usize>,
    /// First 200 characters of the region's metadata/content preview.
    content_preview: String,
    /// Metadata (exit_code, cwd, command, timestamp, etc.).
    metadata: std::collections::HashMap<String, String>,
}

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
struct QuerySemanticHistoryResult {
    pane_id: u64,
    regions: Vec<SemanticRegionInfo>,
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

    async fn handle_list_panes(&self, params: ListPanesParam) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let mut panes = Vec::new();
        for (session_id, session) in mgr.iter_sessions() {
            if let Some(filter_id) = params.session_id
                && *session_id != filter_id
            {
                continue;
            }
            for window in &session.windows {
                for pane in &window.panes {
                    panes.push(PaneInfo {
                        pane_id: pane.id,
                        session_id: *session_id,
                        cols: pane.cols(),
                        rows: pane.rows(),
                        title: String::new(),
                    });
                }
            }
        }
        let result = ListPanesResult { panes };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_get_pane_geometry(
        &self,
        params: GetPaneGeometryParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        for session in mgr.iter_sessions().map(|(_, s)| s) {
            for window in &session.windows {
                if let Some(pane) = window.pane(params.pane_id) {
                    let cols = pane.cols();
                    let rows = pane.rows();
                    // A horizontal split divides rows; each half must meet MIN_PANE_ROWS.
                    let can_split_h = rows >= MIN_PANE_ROWS * 2;
                    // A vertical split divides cols; each half must meet MIN_PANE_COLS.
                    let can_split_v = cols >= MIN_PANE_COLS * 2;
                    let result = GetPaneGeometryResult {
                        pane_id: params.pane_id,
                        cols,
                        rows,
                        can_split_h,
                        can_split_v,
                    };
                    return Ok(CallToolResult::success(vec![json_content(&result)?]));
                }
            }
        }
        Ok(CallToolResult::error(vec![Content::text(format!(
            "pane not found: {}",
            params.pane_id
        ))]))
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
    async fn handle_query_semantic_history(
        &self,
        params: QuerySemanticHistoryParam,
    ) -> Result<CallToolResult, ErrorData> {
        use therminal_terminal::region_index::RegionKind;

        // Parse optional region_type filter.
        let kind_filter: Option<RegionKind> = match params.region_type.as_deref() {
            None => None,
            Some(s) => {
                let kind = match s {
                    "Prompt" => RegionKind::Prompt,
                    "Command" => RegionKind::Command,
                    "Output" => RegionKind::Output,
                    "Error" => RegionKind::Error,
                    "ToolCall" => RegionKind::ToolCall,
                    "Thinking" => RegionKind::Thinking,
                    "Annotation" => RegionKind::Annotation,
                    other => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "unknown region_type: {other}. Valid values: Prompt, Command, Output, Error, ToolCall, Thinking, Annotation"
                        ))]));
                    }
                };
                Some(kind)
            }
        };

        // Compile optional regex pattern.
        let regex = match &params.pattern {
            None => None,
            Some(pat) => match regex::Regex::new(pat) {
                Ok(r) => Some(r),
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "invalid regex pattern: {e}"
                    ))]));
                }
            },
        };

        let limit = params.limit.unwrap_or(20);
        let since_line = params.since_line.unwrap_or(0);

        // Get the region index for this pane.
        let region_index = {
            let mgr = self.session_mgr.lock().await;
            match mgr.pane_region_index(params.pane_id) {
                Ok(idx) => idx,
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "query_semantic_history failed: {e}"
                    ))]));
                }
            }
        };

        let idx = region_index
            .lock()
            .map_err(|e| ErrorData::internal_error(format!("lock poisoned: {e}"), None))?;

        let kind_name = |k: RegionKind| -> &'static str {
            match k {
                RegionKind::Prompt => "Prompt",
                RegionKind::Command => "Command",
                RegionKind::Output => "Output",
                RegionKind::Error => "Error",
                RegionKind::ToolCall => "ToolCall",
                RegionKind::Thinking => "Thinking",
                RegionKind::Annotation => "Annotation",
            }
        };

        let mut regions = Vec::new();
        for region in idx.regions() {
            // Apply since_line filter.
            if region.start_line < since_line {
                continue;
            }

            // Apply kind filter.
            if let Some(filter_kind) = kind_filter
                && region.kind != filter_kind
            {
                continue;
            }

            // Build content preview from metadata (first 200 chars).
            let content_preview = build_content_preview(region);

            // Apply regex filter on content preview.
            if let Some(ref re) = regex
                && !re.is_match(&content_preview)
            {
                continue;
            }

            regions.push(SemanticRegionInfo {
                region_type: kind_name(region.kind).to_string(),
                start_line: region.start_line,
                end_line: region.end_line,
                content_preview,
                metadata: region.metadata.clone(),
            });

            if regions.len() >= limit {
                break;
            }
        }

        let result = QuerySemanticHistoryResult {
            pane_id: params.pane_id,
            regions,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }
}

/// Build a content preview string from a region's metadata (first 200 chars).
fn build_content_preview(region: &therminal_terminal::region_index::Region) -> String {
    // Prefer "command" metadata, then "cwd", then concatenate all metadata values.
    let raw = if let Some(cmd) = region.metadata.get("command") {
        cmd.clone()
    } else if let Some(cwd) = region.metadata.get("cwd") {
        cwd.clone()
    } else if region.metadata.is_empty() {
        String::new()
    } else {
        region
            .metadata
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if raw.len() > 200 {
        format!("{}...", &raw[..197])
    } else {
        raw
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
            "terminal.panes.list",
            "List all panes with their dimensions, session membership, and title. Optionally filter by session ID.",
            schema_for_type::<ListPanesParam>(),
        ),
        Tool::new(
            "terminal.panes.write",
            "Write text input to a pane's PTY (send keystrokes or commands to the terminal)",
            schema_for_type::<WriteToPaneParam>(),
        ),
        Tool::new(
            "terminal.panes.get_geometry",
            "Get a pane's grid dimensions (cols, rows) and whether it can be split horizontally or vertically based on minimum pane size constraints",
            schema_for_type::<GetPaneGeometryParam>(),
        ),
        Tool::new(
            "terminal.panes.get_content",
            "Read the current visible content of a terminal pane (grid snapshot with cursor position)",
            schema_for_type::<PaneIdParam>(),
        ),
        Tool::new(
            "terminal.semantic.query_history",
            "Query the semantic region index for a pane. Returns typed regions (Prompt, Command, Output, Error, etc.) with metadata. Supports filtering by region type, regex pattern, and scroll position.",
            schema_for_type::<QuerySemanticHistoryParam>(),
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
            "terminal.panes.list" => {
                let params: ListPanesParam = parse_args(args)?;
                self.handle_list_panes(params).await
            }
            "terminal.panes.write" => {
                let params: WriteToPaneParam = parse_args(args)?;
                self.handle_write_to_pane(params).await
            }
            "terminal.panes.get_content" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_read_pane_content(params).await
            }
            "terminal.semantic.query_history" => {
                let params: QuerySemanticHistoryParam = parse_args(args)?;
                self.handle_query_semantic_history(params).await
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
