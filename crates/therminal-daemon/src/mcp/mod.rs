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
//! The server listens on a platform-appropriate IPC endpoint (Unix domain
//! socket on Linux/macOS, named pipe on Windows) and uses the `rmcp` crate
//! for protocol handling.
//!
//! This module is split across several files:
//! - `mod.rs` — shared types, `TherminalMcpServer` struct, `ServerHandler` trait impl
//! - `tools.rs` — 15 tool handler implementations + `tool_definitions()`
//! - `resources.rs` — MCP resource list/read/subscribe/unsubscribe logic
//! - `transport.rs` — Unix socket / Windows named pipe server lifecycle

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorCode, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResult, ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use therminal_core::config::TrustConfig;

use crate::claude_jsonl_tailer::TaggedAgentEvent;
use crate::session::SessionManager;
use crate::trust::{
    AgentIdentity, RateLimiter, TrustCheckResult, check_resource_access, check_tool_access,
};

pub mod resources;
pub mod tools;
pub mod transport;

pub use transport::start_mcp_server;

// ── Tool parameter types ────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SessionIdParam {
    /// The numeric session ID.
    pub(super) session_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct CreateSessionParam {
    /// Optional human-readable name for the session.
    pub(super) name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct WriteToPaneParam {
    /// The numeric pane ID to write to.
    pub(super) pane_id: u64,
    /// The text (or bytes as UTF-8) to send to the pane's PTY.
    pub(super) input: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct PaneIdParam {
    /// The numeric pane ID.
    pub(super) pane_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ListPanesParam {
    /// Optional session ID to filter panes by. If omitted, returns panes from all sessions.
    pub(super) session_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct QuerySemanticHistoryParam {
    /// The numeric pane ID to query.
    pub(super) pane_id: u64,
    /// Optional region type filter. One of: Prompt, Command, Output, Error, ToolCall, Thinking, Annotation.
    pub(super) region_type: Option<String>,
    /// Optional regex pattern to match against region content preview.
    pub(super) pattern: Option<String>,
    /// Maximum number of regions to return (default 20).
    pub(super) limit: Option<usize>,
    /// Only return regions starting at or after this line number.
    pub(super) since_line: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetPaneGeometryParam {
    /// The numeric pane ID.
    pub(super) pane_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct WaitForOutputParam {
    /// The numeric pane ID to watch.
    pub(super) pane_id: u64,
    /// String or regex pattern to match against output lines.
    pub(super) pattern: String,
    /// Timeout in milliseconds (default 30000, max 120000).
    #[serde(default = "default_timeout_ms")]
    pub(super) timeout_ms: u64,
    /// Only match output on or after this line number. If omitted, matches any new output.
    pub(super) since_line: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct GetHotspotsParam {
    /// The numeric pane ID to scan for hotspots.
    pub(super) pane_id: u64,
    /// Optional hotspot type filter. One of: file, url, git_ref, issue.
    pub(super) hotspot_type: Option<String>,
    /// Maximum number of hotspots to return (default 50).
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ListWorkspacesParam {
    /// Optional session ID to filter workspaces by. If omitted, returns workspaces from all sessions.
    pub(super) session_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ListAgentsParam {
    /// Optional status filter. One of: active, idle, processing, streaming, thinking, tool_use, awaiting_input.
    pub(super) status: Option<String>,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SpawnPaneParam {
    /// Session ID to create the pane in. If omitted, uses the default (first) session.
    pub(super) session_id: Option<u64>,
    /// Shell command to run. If omitted, spawns the user's default shell.
    pub(super) command: Option<String>,
    /// Working directory for the new pane. If omitted, inherits the current directory.
    pub(super) cwd: Option<String>,
    /// Split direction: "horizontal" or "vertical". Defaults to "vertical" when splitting.
    pub(super) split_direction: Option<String>,
    /// Pane ID to split from. If specified, the new pane is created as a sibling of this pane.
    pub(super) split_from: Option<u64>,
}

// ── Tool result types ───────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionListResult {
    pub(super) session_ids: Vec<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionInfoResult {
    pub(super) session_id: u64,
    pub(super) name: Option<String>,
    pub(super) created_at_secs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionCreatedResult {
    pub(super) session_id: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SessionDestroyedResult {
    pub(super) session_id: u64,
    pub(super) destroyed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct WriteToPaneResult {
    pub(super) pane_id: u64,
    pub(super) success: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PaneContentResult {
    pub(super) pane_id: u64,
    pub(super) lines: Vec<String>,
    pub(super) cursor_col: usize,
    pub(super) cursor_line: usize,
    pub(super) cols: usize,
    pub(super) rows: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct PaneInfo {
    pub(super) pane_id: u64,
    pub(super) session_id: u64,
    pub(super) cols: u16,
    pub(super) rows: u16,
    pub(super) title: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ListPanesResult {
    pub(super) panes: Vec<PaneInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SpawnPaneResult {
    pub(super) pane_id: u64,
    pub(super) session_id: u64,
    pub(super) cols: u16,
    pub(super) rows: u16,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct DestroyPaneResult {
    /// Whether the pane was successfully destroyed.
    pub(super) success: bool,
    /// Human-readable status message.
    pub(super) message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct WaitForOutputResult {
    /// Whether the pattern was matched before timeout.
    pub(super) matched: bool,
    /// The line number where the match was found (0 if not matched).
    pub(super) line_number: usize,
    /// The content of the matched line (empty if not matched).
    pub(super) line_content: String,
    /// Elapsed time in milliseconds.
    pub(super) elapsed_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct GetPaneGeometryResult {
    pub(super) pane_id: u64,
    /// Number of columns in the pane.
    pub(super) cols: u16,
    /// Number of rows in the pane.
    pub(super) rows: u16,
    /// Whether the pane can be split horizontally (top/bottom) based on minimum dimensions.
    pub(super) can_split_h: bool,
    /// Whether the pane can be split vertically (left/right) based on minimum dimensions.
    pub(super) can_split_v: bool,
}

/// Minimum columns required per pane (derived from MIN_PANE_WIDTH / typical cell width).
pub(super) const MIN_PANE_COLS: u16 = 10;

/// Minimum rows required per pane (derived from MIN_PANE_HEIGHT / typical cell height).
pub(super) const MIN_PANE_ROWS: u16 = 4;

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct HotspotInfo {
    /// The hotspot type: "file", "url", "git_ref", or "issue".
    #[serde(rename = "type")]
    pub(super) hotspot_type: String,
    /// The matched text.
    pub(super) text: String,
    /// Row number in the visible grid (0-based).
    pub(super) line: usize,
    /// First column of the match (0-based, inclusive).
    pub(super) col_start: usize,
    /// One-past-last column of the match (exclusive).
    pub(super) col_end: usize,
    /// Type-specific metadata. For files: resolved absolute path. For URLs: the URL.
    /// For git refs: the ref text. For issues: the issue reference.
    pub(super) metadata: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct GetHotspotsResult {
    pub(super) pane_id: u64,
    pub(super) hotspots: Vec<HotspotInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct WorkspaceInfoResult {
    /// Workspace slot number (1-9).
    pub(super) workspace_id: u64,
    /// Human-readable workspace name.
    pub(super) name: String,
    /// Number of panes in this workspace.
    pub(super) pane_count: usize,
    /// Whether this is the currently active workspace.
    pub(super) is_active: bool,
    /// Pane IDs assigned to this workspace.
    pub(super) pane_ids: Vec<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ListWorkspacesResult {
    pub(super) workspaces: Vec<WorkspaceInfoResult>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct AgentInfoResult {
    /// The pane ID where the agent is running.
    pub(super) pane_id: u64,
    /// Human-readable agent name (e.g. "node").
    pub(super) name: String,
    /// Agent type: claude, codex, copilot, or aider.
    pub(super) agent_type: String,
    /// Current status: active, idle, processing, streaming, thinking, tool_use, awaiting_input.
    pub(super) status: String,
    /// Current tool name if status is tool_use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_tool: Option<String>,
    /// Unix timestamp (seconds) when the agent was first detected.
    pub(super) detected_at: u64,
    /// OS process ID (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pid: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct ListAgentsResult {
    pub(super) agents: Vec<AgentInfoResult>,
}

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SemanticRegionInfo {
    /// The region type (Prompt, Command, Output, Error, ToolCall, Thinking, Annotation).
    pub(super) region_type: String,
    /// The terminal line where this region starts.
    pub(super) start_line: usize,
    /// The terminal line where this region ends, or null if still open.
    pub(super) end_line: Option<usize>,
    /// First 200 characters of the region's metadata/content preview.
    pub(super) content_preview: String,
    /// Metadata (exit_code, cwd, command, timestamp, etc.).
    pub(super) metadata: std::collections::HashMap<String, String>,
}

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct QuerySemanticHistoryResult {
    pub(super) pane_id: u64,
    pub(super) regions: Vec<SemanticRegionInfo>,
}

// ── Helper: serialize to JSON content ───────────────────────────────────

pub(super) fn json_content<T: Serialize>(value: &T) -> Result<Content, ErrorData> {
    Content::json(value)
        .map_err(|e| ErrorData::internal_error(format!("serialization error: {e}"), None))
}

pub(super) fn parse_args<T: serde::de::DeserializeOwned>(
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
pub(super) fn extract_agent_identity(context: &RequestContext<RoleServer>) -> AgentIdentity {
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
/// MCP tools and terminal resources. Each tool/resource call is gated by
/// trust tier enforcement and audit-logged.
///
/// Resources follow the `terminal://pane/{pane_id}/content` and
/// `terminal://pane/{pane_id}/output` URI scheme. Subscriptions to output
/// resources spawn a background task that forwards `DaemonEvent::PaneOutput`
/// events as MCP resource-updated notifications.
pub struct TherminalMcpServer {
    pub(super) session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    pub(super) trust_config: Arc<TrustConfig>,
    pub(super) rate_limiter: Arc<RateLimiter>,
    /// Optional broadcast sender for the Claude agent-event pipeline. Cloned
    /// per `subscribe(therminal://claude/events)` call so each MCP client gets
    /// its own broadcast::Receiver. `None` if the pipeline failed to start.
    pub(super) claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
    /// Active resource subscriptions: maps URI -> JoinHandle for the background
    /// forwarding task. Protected by a std Mutex since we only hold it briefly.
    pub(super) subscriptions: std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Per-connection ring buffer of recent Claude agent events. Filled by the
    /// background subscription task; drained by `read_resource` calls against
    /// `therminal://claude/events`. Cap is small — clients should re-read on
    /// every `notifications/resources/updated`.
    pub(super) claude_event_buffer:
        Arc<std::sync::Mutex<std::collections::VecDeque<TaggedAgentEvent>>>,
}

pub(super) const CLAUDE_EVENT_BUFFER_CAP: usize = 256;

/// URI for the global Claude agent-event stream.
pub(super) const CLAUDE_EVENTS_URI: &str = "therminal://claude/events";

impl TherminalMcpServer {
    /// Create a new MCP server backed by the given session manager and trust config.
    pub fn new(
        session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
        trust_config: Arc<TrustConfig>,
        rate_limiter: Arc<RateLimiter>,
        claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
    ) -> Self {
        Self {
            session_mgr,
            trust_config,
            rate_limiter,
            claude_events,
            subscriptions: std::sync::Mutex::new(HashMap::new()),
            claude_event_buffer: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(CLAUDE_EVENT_BUFFER_CAP),
            )),
        }
    }

    /// Enforce trust tier and rate limiting for the given tool call.
    ///
    /// Returns `Ok(())` if allowed, or an `Err(CallToolResult)` with
    /// a permission-denied error to return to the client.
    pub(super) fn enforce_trust(
        &self,
        tool_name: &str,
        agent: &AgentIdentity,
    ) -> Result<(), CallToolResult> {
        match check_tool_access(tool_name, agent, &self.trust_config, &self.rate_limiter) {
            TrustCheckResult::Allowed => Ok(()),
            TrustCheckResult::Denied(reason) => {
                Err(CallToolResult::error(vec![Content::text(reason)]))
            }
        }
    }

    /// Enforce trust tier for a resource read.
    ///
    /// All resource reads are Observer-tier (Sandboxed minimum).
    pub(super) fn enforce_resource_trust(
        &self,
        uri: &str,
        agent: &AgentIdentity,
    ) -> Result<(), ErrorData> {
        match check_resource_access(uri, agent, &self.trust_config) {
            TrustCheckResult::Allowed => Ok(()),
            TrustCheckResult::Denied(reason) => Err(ErrorData::new(
                // JSON-RPC has no standard FORBIDDEN code; use a custom application error.
                ErrorCode(-32001),
                reason,
                None,
            )),
        }
    }

    /// Parse a pane resource URI into (pane_id, resource_kind).
    ///
    /// Accepts `terminal://pane/{id}/content` and `terminal://pane/{id}/output`.
    pub(super) fn parse_pane_uri(uri: &str) -> Option<(u64, &str)> {
        let rest = uri.strip_prefix("terminal://pane/")?;
        let (id_str, kind) = rest.split_once('/')?;
        let pane_id: u64 = id_str.parse().ok()?;
        match kind {
            "content" | "output" => Some((pane_id, kind)),
            _ => None,
        }
    }
}

// ── Shared helpers used across tools/resources ──────────────────────────

/// Build a content preview string from a region's metadata (first 200 chars).
pub(super) fn build_content_preview(region: &therminal_terminal::region_index::Region) -> String {
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

/// Split a file path like `src/main.rs:42:5` into (`src/main.rs`, `:42:5`).
pub(super) fn split_file_path_parts(text: &str) -> (&str, &str) {
    if let Some(idx) = text.find(':')
        && text[idx + 1..].starts_with(|c: char| c.is_ascii_digit())
    {
        return (&text[..idx], &text[idx..]);
    }
    (text, "")
}

// ── Helpers for pane lookup ─────────────────────────────────────────────

/// Find (session_id, cols, rows) for a pane by ID across all sessions.
pub(super) fn find_pane_info(mgr: &SessionManager, pane_id: u64) -> Option<(u64, u16, u16)> {
    for (session_id, session) in mgr.iter_sessions() {
        for window in &session.windows {
            if let Some(pane) = window.pane(pane_id) {
                return Some((*session_id, pane.cols(), pane.rows()));
            }
        }
    }
    None
}

/// Find (pane_id, cols, rows) of the first pane in a session.
pub(super) fn find_first_pane_in_session(
    mgr: &SessionManager,
    session_id: u64,
) -> Option<(u64, u16, u16)> {
    for (sid, session) in mgr.iter_sessions() {
        if *sid == session_id
            && let Some(window) = session.windows.first()
            && let Some(pane) = window.panes.first()
        {
            return Some((pane.id, pane.cols(), pane.rows()));
        }
    }
    None
}

// ── ServerHandler impl ──────────────────────────────────────────────────

impl ServerHandler for TherminalMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_list_changed()
                .enable_resources_subscribe()
                .build(),
        )
        .with_instructions(
            "Therminal MCP server. Provides tools and resources to manage terminal sessions and panes. \
             Resources: terminal://pane/{id}/content (grid snapshot), terminal://pane/{id}/output (live stream).",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let agent = extract_agent_identity(&context);
        // Resources are Observer-tier; check against a representative URI.
        self.enforce_resource_trust("terminal://pane/0/content", &agent)?;
        let resources = self.build_resource_list().await;
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.list_resource_templates_impl(request, context).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        self.read_resource_impl(request, context).await
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.subscribe_impl(request, context).await
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.unsubscribe_impl(request, context).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools::tool_definitions()))
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
            "terminal.panes.create" => {
                let params: SpawnPaneParam = parse_args(args)?;
                self.handle_spawn_pane(params).await
            }
            "terminal.panes.destroy" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_destroy_pane(params).await
            }
            "terminal.panes.list" => {
                let params: ListPanesParam = parse_args(args)?;
                self.handle_list_panes(params).await
            }
            "terminal.panes.write" => {
                let params: WriteToPaneParam = parse_args(args)?;
                self.handle_write_to_pane(params).await
            }
            "terminal.panes.get_geometry" => {
                let params: GetPaneGeometryParam = parse_args(args)?;
                self.handle_get_pane_geometry(params).await
            }
            "terminal.panes.get_content" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_read_pane_content(params).await
            }
            "terminal.semantic.query_history" => {
                let params: QuerySemanticHistoryParam = parse_args(args)?;
                self.handle_query_semantic_history(params).await
            }
            "terminal.panes.wait_for_output" => {
                let params: WaitForOutputParam = parse_args(args)?;
                self.handle_wait_for_output(params).await
            }
            "terminal.semantic.get_hotspots" => {
                let params: GetHotspotsParam = parse_args(args)?;
                self.handle_get_hotspots(params).await
            }
            "terminal.workspaces.list" => {
                let params: ListWorkspacesParam = parse_args(args)?;
                self.handle_list_workspaces(params).await
            }
            "terminal.agents.list" => {
                let params: ListAgentsParam = parse_args(args)?;
                self.handle_list_agents(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}
