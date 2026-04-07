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
    /// Current working directory, from OSC 7 or initial spawn. `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) cwd: Option<String>,
    /// Exit code of the most recently finished command (from OSC 633 D marks
    /// via the region index). `None` when no command has finished yet or the
    /// shell integration isn't reporting exit codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_exit_code: Option<i32>,
    /// Name of the AI agent detected in this pane (from the daemon's
    /// `AgentRegistry`). `None` when no agent is detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) agent_name: Option<String>,
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

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use therminal_core::config::{AgentTrust, TrustConfig, TrustTier};
    use tokio::sync::broadcast;

    use crate::session::SessionManager;
    use crate::trust::{AgentIdentity, RateLimiter};

    use super::TherminalMcpServer;
    use super::tools::tool_definitions;

    // ── Fixture helpers ─────────────────────────────────────────────────

    /// Build a `TherminalMcpServer` with a real (empty) `SessionManager` and
    /// the given `TrustConfig`. No PTY is spawned — the session manager is
    /// empty so any tool that looks up panes/sessions will return "not found"
    /// rather than doing any real work.
    pub(crate) fn make_server(trust_config: TrustConfig) -> TherminalMcpServer {
        let (event_tx, _) = broadcast::channel(16);
        let session_mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new(event_tx)));
        let trust_config = Arc::new(trust_config);
        let rate_limiter = Arc::new(RateLimiter::new(100));
        TherminalMcpServer::new(session_mgr, trust_config, rate_limiter, None)
    }

    /// Build an `AgentIdentity` with the given name.
    fn agent(name: &str) -> AgentIdentity {
        AgentIdentity {
            name: name.to_string(),
        }
    }

    /// `TrustConfig` where the default tier is `Sandboxed` (most restrictive).
    fn sandboxed_config() -> TrustConfig {
        TrustConfig {
            default_tier: TrustTier::Sandboxed,
            ..TrustConfig::default()
        }
    }

    /// `TrustConfig` where the default tier is `Supervised` (can call Writer tools).
    fn supervised_config() -> TrustConfig {
        TrustConfig {
            default_tier: TrustTier::Supervised,
            ..TrustConfig::default()
        }
    }

    /// `TrustConfig` where the default tier is `Trusted` (full Admin access).
    fn trusted_config() -> TrustConfig {
        TrustConfig {
            default_tier: TrustTier::Trusted,
            ..TrustConfig::default()
        }
    }

    // ── parse_pane_uri ──────────────────────────────────────────────────

    #[test]
    fn parse_pane_uri_content() {
        let result = TherminalMcpServer::parse_pane_uri("terminal://pane/42/content");
        assert_eq!(result, Some((42, "content")));
    }

    #[test]
    fn parse_pane_uri_output() {
        let result = TherminalMcpServer::parse_pane_uri("terminal://pane/7/output");
        assert_eq!(result, Some((7, "output")));
    }

    #[test]
    fn parse_pane_uri_rejects_unknown_kind() {
        assert!(TherminalMcpServer::parse_pane_uri("terminal://pane/1/hotspot").is_none());
    }

    #[test]
    fn parse_pane_uri_rejects_malformed() {
        assert!(TherminalMcpServer::parse_pane_uri("terminal://pane/notanumber/content").is_none());
        assert!(TherminalMcpServer::parse_pane_uri("http://example.com").is_none());
        assert!(TherminalMcpServer::parse_pane_uri("").is_none());
    }

    // ── split_file_path_parts ───────────────────────────────────────────

    #[test]
    fn split_file_path_parts_with_line_col() {
        let (path, suffix) = super::split_file_path_parts("src/main.rs:42:5");
        assert_eq!(path, "src/main.rs");
        assert_eq!(suffix, ":42:5");
    }

    #[test]
    fn split_file_path_parts_no_suffix() {
        let (path, suffix) = super::split_file_path_parts("src/main.rs");
        assert_eq!(path, "src/main.rs");
        assert_eq!(suffix, "");
    }

    // ── build_content_preview ───────────────────────────────────────────

    #[test]
    fn build_content_preview_uses_command_field() {
        use std::time::Instant;
        use therminal_terminal::region_index::{Region, RegionKind};
        let mut region = Region {
            kind: RegionKind::Command,
            start_line: 0,
            end_line: None,
            metadata: std::collections::HashMap::new(),
            timestamp: Instant::now(),
        };
        region
            .metadata
            .insert("command".to_string(), "cargo build".to_string());
        region
            .metadata
            .insert("cwd".to_string(), "/home/user".to_string());
        let preview = super::build_content_preview(&region);
        assert_eq!(preview, "cargo build");
    }

    #[test]
    fn build_content_preview_falls_back_to_cwd() {
        use std::time::Instant;
        use therminal_terminal::region_index::{Region, RegionKind};
        let mut region = Region {
            kind: RegionKind::Prompt,
            start_line: 0,
            end_line: None,
            metadata: std::collections::HashMap::new(),
            timestamp: Instant::now(),
        };
        region
            .metadata
            .insert("cwd".to_string(), "/home/user/projects".to_string());
        let preview = super::build_content_preview(&region);
        assert_eq!(preview, "/home/user/projects");
    }

    #[test]
    fn build_content_preview_truncates_long_content() {
        use std::time::Instant;
        use therminal_terminal::region_index::{Region, RegionKind};
        let mut region = Region {
            kind: RegionKind::Output,
            start_line: 0,
            end_line: None,
            metadata: std::collections::HashMap::new(),
            timestamp: Instant::now(),
        };
        let long = "x".repeat(300);
        region.metadata.insert("command".to_string(), long);
        let preview = super::build_content_preview(&region);
        assert_eq!(preview.len(), 200);
        assert!(preview.ends_with("..."));
    }

    // ── enforce_trust (Observer tools) ─────────────────────────────────

    /// All 10 Observer tools must be accessible to a Sandboxed agent.
    #[test]
    fn sandboxed_agent_can_call_all_observer_tools() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        let observer_tools = [
            "terminal.sessions.list",
            "terminal.sessions.get",
            "terminal.panes.list",
            "terminal.panes.get_geometry",
            "terminal.panes.get_content",
            "terminal.panes.wait_for_output",
            "terminal.semantic.query_history",
            "terminal.semantic.get_hotspots",
            "terminal.workspaces.list",
            "terminal.agents.list",
        ];
        for tool in &observer_tools {
            assert!(
                server.enforce_trust(tool, &agent).is_ok(),
                "expected Sandboxed to be allowed for Observer tool: {tool}"
            );
        }
    }

    // ── enforce_trust (Writer tools) ────────────────────────────────────

    /// A Sandboxed agent must be denied all Writer tools.
    #[test]
    fn sandboxed_agent_denied_writer_tools() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        let writer_tools = [
            "terminal.sessions.create",
            "terminal.panes.write",
            "terminal.panes.create",
        ];
        for tool in &writer_tools {
            let result = server.enforce_trust(tool, &agent);
            assert!(
                result.is_err(),
                "expected Sandboxed to be denied for Writer tool: {tool}"
            );
        }
    }

    /// A Supervised agent must be allowed all Writer tools.
    #[test]
    fn supervised_agent_can_call_writer_tools() {
        let server = make_server(supervised_config());
        let agent = agent("supervised-bot");
        let writer_tools = [
            "terminal.sessions.create",
            "terminal.panes.write",
            "terminal.panes.create",
        ];
        for tool in &writer_tools {
            assert!(
                server.enforce_trust(tool, &agent).is_ok(),
                "expected Supervised to be allowed for Writer tool: {tool}"
            );
        }
    }

    // ── enforce_trust (Admin tools) ─────────────────────────────────────

    /// A Sandboxed agent must be denied all Admin tools.
    #[test]
    fn sandboxed_agent_denied_admin_tools() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        for tool in &["terminal.sessions.destroy", "terminal.panes.destroy"] {
            let result = server.enforce_trust(tool, &agent);
            assert!(
                result.is_err(),
                "expected Sandboxed to be denied for Admin tool: {tool}"
            );
        }
    }

    /// A Supervised agent must be denied Admin tools.
    #[test]
    fn supervised_agent_denied_admin_tools() {
        let server = make_server(supervised_config());
        let agent = agent("supervised-bot");
        for tool in &["terminal.sessions.destroy", "terminal.panes.destroy"] {
            let result = server.enforce_trust(tool, &agent);
            assert!(
                result.is_err(),
                "expected Supervised to be denied for Admin tool: {tool}"
            );
        }
    }

    /// A Trusted agent must be allowed all Admin tools.
    #[test]
    fn trusted_agent_can_call_admin_tools() {
        let server = make_server(trusted_config());
        let agent = agent("trusted-bot");
        for tool in &["terminal.sessions.destroy", "terminal.panes.destroy"] {
            assert!(
                server.enforce_trust(tool, &agent).is_ok(),
                "expected Trusted to be allowed for Admin tool: {tool}"
            );
        }
    }

    // ── enforce_trust (all 15 tools parameterized) ──────────────────────

    /// Exhaustive: every tool has a known category — Trusted can call all 15.
    #[test]
    fn trusted_agent_can_call_all_15_tools() {
        let server = make_server(trusted_config());
        let agent = agent("trusted-bot");
        let all_tools = [
            // Observer (10)
            "terminal.sessions.list",
            "terminal.sessions.get",
            "terminal.panes.list",
            "terminal.panes.get_geometry",
            "terminal.panes.get_content",
            "terminal.panes.wait_for_output",
            "terminal.semantic.query_history",
            "terminal.semantic.get_hotspots",
            "terminal.workspaces.list",
            "terminal.agents.list",
            // Writer (3)
            "terminal.sessions.create",
            "terminal.panes.write",
            "terminal.panes.create",
            // Admin (2)
            "terminal.sessions.destroy",
            "terminal.panes.destroy",
        ];
        assert_eq!(all_tools.len(), 15, "expected exactly 15 tools");
        for tool in &all_tools {
            assert!(
                server.enforce_trust(tool, &agent).is_ok(),
                "expected Trusted to be allowed for: {tool}"
            );
        }
    }

    // ── enforce_trust (per-agent allowlist) ─────────────────────────────

    /// Trust bypass regression: an agent with per-agent `allowed_tools` must
    /// not gain access to tools not in the list, even if the tier would
    /// otherwise permit them.
    #[test]
    fn per_agent_allowlist_restricts_trusted_agent() {
        let mut config = trusted_config();
        config.agents.insert(
            "restricted-claude".to_string(),
            AgentTrust {
                tier: TrustTier::Trusted,
                allowed_tools: Some(vec!["terminal.sessions.list".to_string()]),
            },
        );
        let server = make_server(config);
        let agent = agent("restricted-claude");

        // Allowed tool must pass.
        assert!(
            server
                .enforce_trust("terminal.sessions.list", &agent)
                .is_ok()
        );

        // All other tools must be denied regardless of tier.
        for tool in &[
            "terminal.sessions.get",
            "terminal.sessions.create",
            "terminal.sessions.destroy",
            "terminal.panes.list",
            "terminal.panes.write",
            "terminal.panes.destroy",
        ] {
            assert!(
                server.enforce_trust(tool, &agent).is_err(),
                "expected allowlist to deny: {tool}"
            );
        }
    }

    /// An empty allowlist must deny every tool call, even for a Trusted agent.
    #[test]
    fn per_agent_empty_allowlist_denies_all_tools() {
        let mut config = trusted_config();
        config.agents.insert(
            "locked-agent".to_string(),
            AgentTrust {
                tier: TrustTier::Trusted,
                allowed_tools: Some(vec![]),
            },
        );
        let server = make_server(config);
        let agent = agent("locked-agent");
        for tool in &[
            "terminal.sessions.list",
            "terminal.sessions.create",
            "terminal.sessions.destroy",
        ] {
            assert!(
                server.enforce_trust(tool, &agent).is_err(),
                "expected empty allowlist to deny: {tool}"
            );
        }
    }

    // ── enforce_trust (rate limiting) ───────────────────────────────────

    /// Trusted agents must be rate-limited on Admin tools when the limiter cap is hit.
    #[test]
    fn admin_tools_rate_limited_for_trusted_agent() {
        let (event_tx, _) = broadcast::channel(16);
        let session_mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new(event_tx)));
        let trust_config = Arc::new(trusted_config());
        // Allow only 1 destructive call per minute.
        let rate_limiter = Arc::new(RateLimiter::new(1));
        let server = TherminalMcpServer::new(session_mgr, trust_config, rate_limiter, None);
        let agent = agent("trusted-bot");

        // First call allowed.
        assert!(
            server
                .enforce_trust("terminal.sessions.destroy", &agent)
                .is_ok()
        );
        // Second call denied due to rate limit.
        assert!(
            server
                .enforce_trust("terminal.sessions.destroy", &agent)
                .is_err()
        );
    }

    // ── enforce_resource_trust ──────────────────────────────────────────

    /// Sandboxed agents can read pane resources (Observer tier).
    #[test]
    fn sandboxed_agent_can_read_pane_resources() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_resource_trust("terminal://pane/1/content", &agent)
                .is_ok()
        );
        assert!(
            server
                .enforce_resource_trust("terminal://pane/1/output", &agent)
                .is_ok()
        );
    }

    /// Sandboxed agents can read the Claude events resource (Observer tier).
    /// This is the trust bypass site — the resource must not silently allow
    /// lower-trust agents by failing open.
    #[test]
    fn sandboxed_agent_can_read_claude_events_resource() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_resource_trust(super::CLAUDE_EVENTS_URI, &agent)
                .is_ok(),
            "Sandboxed is Observer-tier and must be allowed for claude/events"
        );
    }

    /// An agent with no configured tier below Sandboxed would be denied — but
    /// since Sandboxed is the minimum, verify that the boundary is correctly
    /// enforced: there is no tier below Sandboxed in the current model.
    /// This test documents the current trust floor.
    #[test]
    fn sandboxed_is_minimum_tier_for_resources() {
        let server = make_server(sandboxed_config());
        let agent = agent("any-agent");
        // Sandboxed agents are always allowed for Observer resources.
        assert!(
            server
                .enforce_resource_trust("terminal://pane/99/content", &agent)
                .is_ok()
        );
        assert!(
            server
                .enforce_resource_trust("therminal://claude/events", &agent)
                .is_ok()
        );
    }

    // ── tool_definitions() surface lock ─────────────────────────────────

    /// Lock in the count: exactly 15 tools must be returned.
    #[test]
    fn tool_definitions_returns_15_tools() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 15, "expected exactly 15 tool definitions");
    }

    /// Lock in the names so a rename or accidental drop is caught immediately.
    #[test]
    fn tool_definitions_contains_all_expected_names() {
        use std::collections::HashSet;
        let tools = tool_definitions();
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        let expected = [
            "terminal.sessions.list",
            "terminal.sessions.get",
            "terminal.sessions.create",
            "terminal.sessions.destroy",
            "terminal.panes.create",
            "terminal.panes.destroy",
            "terminal.panes.list",
            "terminal.panes.write",
            "terminal.panes.get_geometry",
            "terminal.panes.get_content",
            "terminal.semantic.query_history",
            "terminal.panes.wait_for_output",
            "terminal.semantic.get_hotspots",
            "terminal.workspaces.list",
            "terminal.agents.list",
        ];
        for name in &expected {
            assert!(names.contains(name), "missing tool definition: {name}");
        }
    }

    // ── Resource surface lock ────────────────────────────────────────────

    /// `build_resource_list` must always include the Claude events URI even
    /// when there are no active panes.
    #[tokio::test]
    async fn build_resource_list_always_includes_claude_events() {
        let server = make_server(trusted_config());
        let resources = server.build_resource_list().await;
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert!(
            uris.contains(&super::CLAUDE_EVENTS_URI),
            "expected claude/events in resource list, got: {uris:?}"
        );
    }

    // ── PaneInfo serialization ──────────────────────────────────────────

    #[test]
    fn pane_info_serializes_all_optional_fields() {
        let info = super::PaneInfo {
            pane_id: 1,
            session_id: 2,
            cols: 80,
            rows: 24,
            title: String::new(),
            cwd: Some("/home/user/proj".to_string()),
            last_exit_code: Some(0),
            agent_name: Some("claude-code".to_string()),
        };
        let v = serde_json::to_value(&info).expect("serialize");
        assert_eq!(v["pane_id"], 1);
        assert_eq!(v["session_id"], 2);
        assert_eq!(v["cols"], 80);
        assert_eq!(v["rows"], 24);
        assert_eq!(v["cwd"], "/home/user/proj");
        assert_eq!(v["last_exit_code"], 0);
        assert_eq!(v["agent_name"], "claude-code");
    }

    #[test]
    fn pane_info_serializes_all_none_fields() {
        let info = super::PaneInfo {
            pane_id: 1,
            session_id: 2,
            cols: 80,
            rows: 24,
            title: String::new(),
            cwd: None,
            last_exit_code: None,
            agent_name: None,
        };
        let v = serde_json::to_value(&info).expect("serialize");
        // None fields are skipped entirely for backward compatibility.
        assert!(v.get("cwd").is_none(), "cwd should be omitted when None");
        assert!(
            v.get("last_exit_code").is_none(),
            "last_exit_code should be omitted when None"
        );
        assert!(
            v.get("agent_name").is_none(),
            "agent_name should be omitted when None"
        );
        assert_eq!(v["pane_id"], 1);
    }

    #[test]
    fn pane_info_nonzero_exit_code_round_trips() {
        let info = super::PaneInfo {
            pane_id: 7,
            session_id: 3,
            cols: 120,
            rows: 40,
            title: String::new(),
            cwd: Some("/tmp".to_string()),
            last_exit_code: Some(127),
            agent_name: None,
        };
        let v = serde_json::to_value(&info).expect("serialize");
        assert_eq!(v["last_exit_code"], 127);
        assert!(v.get("agent_name").is_none());
    }

    /// With an empty session manager, the only resource should be claude/events.
    #[tokio::test]
    async fn build_resource_list_no_panes_only_claude_events() {
        let server = make_server(trusted_config());
        let resources = server.build_resource_list().await;
        assert_eq!(
            resources.len(),
            1,
            "expected only claude/events resource with no panes"
        );
    }
}
