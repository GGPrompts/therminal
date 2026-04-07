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
use therminal_terminal::agent_registry::TaggedAgentEvent as TaggedAgentLifecycleEvent;

pub mod resources;
pub mod tools;
pub mod transport;

pub use transport::start_mcp_server;

// ── Tool parameter types ────────────────────────────────────────────────

/// Empty parameter type for tools that take no arguments.
///
/// We use this instead of `schema_for_type::<()>()` because schemars emits
/// `{"type":"null"}` for the unit type, but the MCP spec requires every
/// tool's `inputSchema` to be `{"type":"object", ...}`. Claude Code
/// strict-validates tool schemas and silently drops the entire tools/list
/// response if any tool has a non-object schema — the symptom is that
/// `claude mcp list` reports "connected" but no `mcp__therminal__*` tools
/// register in-conversation (resources, which use a separate listing path,
/// still work). See tn-q882.
#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct EmptyParams {}

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
pub(super) struct QueryCommandsParam {
    /// The numeric pane ID to query.
    pub(super) pane_id: u64,
    /// Only return command blocks whose `start_line` is at or after this
    /// line number. If omitted, returns all tracked commands up to `limit`.
    pub(super) since_line: Option<usize>,
    /// Maximum number of command entries to return (default 20).
    pub(super) limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct QueryEventsParam {
    /// The numeric pane ID to query.
    pub(super) pane_id: u64,
    /// Only return events whose Unix-seconds timestamp is at or after this
    /// value. If omitted, returns all events in the buffer up to `limit`.
    pub(super) since_timestamp_secs: Option<u64>,
    /// Maximum number of events to return (default 100). The underlying
    /// in-memory ring is capped at 5000 entries; values above that cap
    /// have no effect.
    pub(super) limit: Option<usize>,
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
pub(super) struct GetWorkspaceLayoutParam {
    /// The workspace slot number (1-9) to fetch the layout for.
    pub(super) workspace_id: u64,
    /// Optional session ID. If omitted, searches across all sessions and uses
    /// the first workspace with a matching ID.
    pub(super) session_id: Option<u64>,
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

/// Serializable mirror of `therminal-app`'s `LayoutNode`.
///
/// The real tree (with split directions + ratios) lives in `therminal-app`
/// and is not currently plumbed into the daemon. For now, `get_workspace_layout`
/// returns a DEGRADED tree built from the flat `pane_ids` stored in
/// `WorkspaceInfo`: a right-leaning cascade of horizontal splits with
/// ratio 0.5. Tracked as follow-up `tn-vs0u`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutNodeJson {
    /// A split node with a direction, ratio, and two children.
    Split {
        /// Split direction: "horizontal" or "vertical".
        direction: String,
        /// Share of the first child (0.0..1.0).
        ratio: f32,
        left: Box<LayoutNodeJson>,
        right: Box<LayoutNodeJson>,
    },
    /// A leaf pane.
    Leaf { pane_id: u64 },
    /// An empty placeholder (used when a workspace has no panes).
    Empty,
}

impl LayoutNodeJson {
    /// Build a degraded layout tree from a flat list of pane IDs.
    ///
    /// Produces a right-leaning cascade of horizontal splits with ratio 0.5.
    /// This is a temporary shim until the real `LayoutNode` tree is plumbed
    /// through to the daemon (see `tn-vs0u`).
    pub fn from_flat_pane_ids(pane_ids: &[u64]) -> Self {
        match pane_ids {
            [] => LayoutNodeJson::Empty,
            [only] => LayoutNodeJson::Leaf { pane_id: *only },
            [first, rest @ ..] => LayoutNodeJson::Split {
                direction: "horizontal".to_string(),
                ratio: 0.5,
                left: Box::new(LayoutNodeJson::Leaf { pane_id: *first }),
                right: Box::new(LayoutNodeJson::from_flat_pane_ids(rest)),
            },
        }
    }

    /// Build a faithful layout tree from a `LayoutSnapshot` provided by the
    /// app. Unlike `from_flat_pane_ids`, this preserves real split directions
    /// and ratios.
    pub fn from_snapshot(snap: &therminal_protocol::daemon::LayoutSnapshot) -> Self {
        use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
        match snap {
            LayoutSnapshot::Leaf { pane_id } => LayoutNodeJson::Leaf { pane_id: *pane_id },
            LayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => LayoutNodeJson::Split {
                direction: match direction {
                    LayoutSplitDirection::Horizontal => "horizontal".to_string(),
                    LayoutSplitDirection::Vertical => "vertical".to_string(),
                },
                ratio: *ratio,
                left: Box::new(Self::from_snapshot(first)),
                right: Box::new(Self::from_snapshot(second)),
            },
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct GetWorkspaceLayoutResult {
    /// Workspace slot number (1-9).
    pub(super) workspace_id: u64,
    /// Session that owns this workspace.
    pub(super) session_id: u64,
    /// Layout tree. Currently a degraded cascade; see `LayoutNodeJson`.
    pub(super) layout: LayoutNodeJson,
    /// Focused pane within the workspace, if any.
    pub(super) focused_pane: Option<u64>,
    /// True when the `layout` field is the degraded shim (no real directions
    /// or ratios). Clients can ignore this and use the tree shape as-is; it
    /// exists so debugging tools can call out the limitation.
    pub(super) degraded: bool,
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

/// Detailed inference data for the agent running in a specific pane.
///
/// All inference fields are optional because (a) the pane may not host an
/// agent, and (b) the `AgentStateInference` engine is not yet plumbed into
/// the daemon — see follow-up issue. The `agent_type` field is populated
/// from `AgentRegistry` when an agent is currently registered on the pane.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct AgentDetailsResult {
    /// Agent type (claude, codex, copilot, aider) if an agent is registered
    /// on this pane. `None` if no agent is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) agent_type: Option<String>,
    /// Model string detected from the pane (e.g. "claude-sonnet-4").
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
    /// Context-window usage percentage (0.0 - 100.0).
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) context_percent: Option<f32>,
    /// Number of consecutive failed commands. Defaults to 0 when unknown.
    pub(super) consecutive_failures: u32,
    /// Most recent command string observed on the pane.
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_command: Option<String>,
    /// Exit code of the most recent command.
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_exit_code: Option<i32>,
    /// Duration in milliseconds of the most recent command.
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_command_duration_ms: Option<u64>,
}

/// Dynamic status snapshot for the agent running in a specific pane.
///
/// Strict subset of `AgentDetailsResult` focused on the live mode + capacity
/// fields used by sibling agents to coordinate. Combines the
/// `AgentRegistry` view (mode-ish: `agent_type`, `status`, `current_tool`)
/// with the `PaneCapacityCache` view (`context_percent`, `model`).
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct AgentStatusResult {
    /// The pane the agent is running in (echoed for caller convenience).
    pub(super) pane_id: u64,
    /// Agent type (claude, codex, copilot, aider) if registered. `None`
    /// when only capacity data is known for this pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) agent_type: Option<String>,
    /// Lifecycle status. Identical formatting to `AgentInfoResult.status`
    /// (`AgentStatus::as_str`). Falls back to "unknown" when no
    /// `AgentRegistry` entry exists for the pane.
    pub(super) status: String,
    /// Current tool name when status is `tool_use`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) current_tool: Option<String>,
    /// Context-window usage percentage (0.0 - 100.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) context_percent: Option<f32>,
    /// Model string (e.g. "claude-opus-4-6").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
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

/// A single shell command tracked via OSC 633 `CommandTracker`.
///
/// All non-positional fields are `Option` because OSC 633 marks may be
/// partial: a command currently running has no `end_line`, `exit_code`,
/// or `duration_ms`; a shell that doesn't emit `E` marks has no
/// `command_text`.
#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CommandInfo {
    /// The command text as reported by OSC 633 `E` mark, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) command_text: Option<String>,
    /// Exit code from the OSC 633 `D` mark. `None` while running or not
    /// reported by the shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) exit_code: Option<i32>,
    /// Wall-clock duration between `C` and `D` marks, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) duration_ms: Option<u64>,
    /// Grid line where the prompt started (from `A` mark).
    pub(super) start_line: usize,
    /// Grid line where execution finished (from `D` mark). `None` while
    /// still running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) end_line: Option<usize>,
    /// Unix timestamp (seconds) when the command was observed, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) timestamp_secs: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct QueryCommandsResult {
    pub(super) pane_id: u64,
    /// Command blocks, oldest first (newest-last transcript order).
    pub(super) commands: Vec<CommandInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct EventInfo {
    /// Unix timestamp (seconds) when the event was logged.
    pub(super) timestamp_secs: u64,
    /// Event variant tag (e.g. `spawn`, `status_change`, `command_start`,
    /// `command_finish`, `resize`, `pty_eof`, `bell`). Returned as a free-form
    /// string so adding new variants does not break the wire format.
    pub(super) event_type: String,
    /// Variant-specific payload as a JSON object. Empty object for variants
    /// like `bell` that have no payload.
    pub(super) details: serde_json::Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct QueryEventsResult {
    pub(super) pane_id: u64,
    /// Recent events, oldest-first within the truncated window.
    pub(super) events: Vec<EventInfo>,
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
    /// Optional broadcast sender for agent lifecycle events from the
    /// `AgentRegistry`. Cloned per `subscribe(therminal://agents/events)` so
    /// each MCP client gets its own broadcast::Receiver.
    pub(super) agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
    /// Per-connection ring buffer of recent agent lifecycle events. Filled by
    /// the subscription forwarder; drained by `read_resource` against
    /// `therminal://agents/events`.
    pub(super) agent_event_buffer:
        Arc<std::sync::Mutex<std::collections::VecDeque<TaggedAgentLifecycleEvent>>>,
}

pub(super) const CLAUDE_EVENT_BUFFER_CAP: usize = 256;
pub(super) const AGENT_EVENT_BUFFER_CAP: usize = 256;

/// URI for the global Claude agent-event stream.
pub(super) const CLAUDE_EVENTS_URI: &str = "therminal://claude/events";
/// URI for the global agent lifecycle event stream backed by `AgentRegistry`.
pub(super) const AGENT_EVENTS_URI: &str = "therminal://agents/events";

impl TherminalMcpServer {
    /// Create a new MCP server backed by the given session manager and trust config.
    pub fn new(
        session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
        trust_config: Arc<TrustConfig>,
        rate_limiter: Arc<RateLimiter>,
        claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
        agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
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
            agent_events,
            agent_event_buffer: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(AGENT_EVENT_BUFFER_CAP),
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
    /// Accepts `terminal://pane/{id}/content`, `terminal://pane/{id}/output`,
    /// and `terminal://pane/{id}/scrollback`.
    pub(super) fn parse_pane_uri(uri: &str) -> Option<(u64, &str)> {
        let rest = uri.strip_prefix("terminal://pane/")?;
        let (id_str, kind) = rest.split_once('/')?;
        let pane_id: u64 = id_str.parse().ok()?;
        match kind {
            "content" | "output" | "scrollback" => Some((pane_id, kind)),
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
            "terminal.semantic.query_commands" => {
                let params: QueryCommandsParam = parse_args(args)?;
                self.handle_query_commands(params).await
            }
            "terminal.panes.query_events" => {
                let params: QueryEventsParam = parse_args(args)?;
                self.handle_query_events(params).await
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
            "terminal.workspaces.get_layout" => {
                let params: GetWorkspaceLayoutParam = parse_args(args)?;
                self.handle_get_workspace_layout(params).await
            }
            "terminal.agents.list" => {
                let params: ListAgentsParam = parse_args(args)?;
                self.handle_list_agents(params).await
            }
            "terminal.agents.get_details" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_get_agent_details(params).await
            }
            "terminal.agents.get_status" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_get_agent_status(params).await
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
        TherminalMcpServer::new(session_mgr, trust_config, rate_limiter, None, None)
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
    fn parse_pane_uri_scrollback() {
        let result = TherminalMcpServer::parse_pane_uri("terminal://pane/13/scrollback");
        assert_eq!(result, Some((13, "scrollback")));
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

    /// All Observer tools must be accessible to a Sandboxed agent.
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
            "terminal.semantic.query_commands",
            "terminal.panes.query_events",
            "terminal.semantic.get_hotspots",
            "terminal.workspaces.list",
            "terminal.workspaces.get_layout",
            "terminal.agents.list",
            "terminal.agents.get_details",
            "terminal.agents.get_status",
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

    // ── enforce_trust (all tools parameterized) ─────────────────────────

    /// Exhaustive: every tool has a known category — Trusted can call all.
    #[test]
    fn trusted_agent_can_call_all_tools() {
        let server = make_server(trusted_config());
        let agent = agent("trusted-bot");
        let all_tools = [
            // Observer
            "terminal.sessions.list",
            "terminal.sessions.get",
            "terminal.panes.list",
            "terminal.panes.get_geometry",
            "terminal.panes.get_content",
            "terminal.panes.wait_for_output",
            "terminal.semantic.query_history",
            "terminal.semantic.query_commands",
            "terminal.panes.query_events",
            "terminal.semantic.get_hotspots",
            "terminal.workspaces.list",
            "terminal.workspaces.get_layout",
            "terminal.agents.list",
            "terminal.agents.get_details",
            "terminal.agents.get_status",
            // Writer
            "terminal.sessions.create",
            "terminal.panes.write",
            "terminal.panes.create",
            // Admin
            "terminal.sessions.destroy",
            "terminal.panes.destroy",
        ];
        assert_eq!(all_tools.len(), 20, "expected exactly 20 tools");
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
        let server = TherminalMcpServer::new(session_mgr, trust_config, rate_limiter, None, None);
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

    /// Sandboxed agents can read pane scrollback resources (Observer tier).
    #[test]
    fn sandboxed_agent_can_read_pane_scrollback() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_resource_trust("terminal://pane/1/scrollback", &agent)
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

    /// Lock in the count: exactly 20 tools must be returned.
    #[test]
    fn tool_definitions_returns_20_tools() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 20, "expected exactly 20 tool definitions");
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
            "terminal.semantic.query_commands",
            "terminal.panes.query_events",
            "terminal.panes.wait_for_output",
            "terminal.semantic.get_hotspots",
            "terminal.workspaces.list",
            "terminal.workspaces.get_layout",
            "terminal.agents.list",
            "terminal.agents.get_details",
            "terminal.agents.get_status",
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

    /// `build_resource_list` must always include the agent lifecycle events
    /// URI even when there are no active panes.
    #[tokio::test]
    async fn build_resource_list_always_includes_agent_events() {
        let server = make_server(trusted_config());
        let resources = server.build_resource_list().await;
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert!(
            uris.contains(&super::AGENT_EVENTS_URI),
            "expected agents/events in resource list, got: {uris:?}"
        );
    }

    /// Sandboxed agents must be allowed to access the agent lifecycle event
    /// resource (Observer tier).
    #[test]
    fn sandboxed_agent_can_read_agent_events_resource() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_resource_trust(super::AGENT_EVENTS_URI, &agent)
                .is_ok(),
            "sandboxed agent should be allowed to read agents/events"
        );
    }

    /// The agent-lifecycle subscription forwarder must populate the
    /// per-connection ring buffer when events are broadcast, without
    /// deadlocking the buffer mutex.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_event_forwarder_buffers_events_without_deadlock() {
        use therminal_terminal::agent_registry::{AgentEvent, TaggedAgentEvent};
        use therminal_terminal::state_inference::AgentType;

        let server = make_server(trusted_config());
        // Replace agent_events with a fresh broadcast channel we control.
        let (tx, _) = tokio::sync::broadcast::channel::<TaggedAgentEvent>(16);
        // SAFETY: tests construct a fresh server above; we need to reach into
        // it. We rebuild via `new` instead.
        let session_mgr = server.session_mgr.clone();
        let trust_config = server.trust_config.clone();
        let rate_limiter = server.rate_limiter.clone();
        let server = TherminalMcpServer::new(
            session_mgr,
            trust_config,
            rate_limiter,
            None,
            Some(tx.clone()),
        );

        // Manually drive the same buffer-fill loop the subscribe handler uses,
        // by spawning a stripped-down forwarder that only updates the buffer.
        let buffer = std::sync::Arc::clone(&server.agent_event_buffer);
        let mut rx = tx.subscribe();
        let task = tokio::spawn(async move {
            for _ in 0..3 {
                if let Ok(evt) = rx.recv().await {
                    let mut buf = buffer.lock().unwrap();
                    if buf.len() == super::AGENT_EVENT_BUFFER_CAP {
                        buf.pop_front();
                    }
                    buf.push_back(evt);
                }
            }
        });

        // Send three events.
        for i in 0..3u64 {
            let _ = tx.send(TaggedAgentEvent {
                event: AgentEvent::Registered {
                    pane_id: i,
                    agent_type: AgentType::Claude,
                    name: format!("a{i}"),
                },
                pane_id: i,
                timestamp_secs: 0,
            });
        }

        // Wait for the forwarder to drain.
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("forwarder did not finish in time")
            .expect("forwarder task panicked");

        let buf = server.agent_event_buffer.lock().unwrap();
        assert_eq!(buf.len(), 3);
    }

    // ── LayoutNodeJson ──────────────────────────────────────────────────

    #[test]
    fn layout_node_json_from_empty_pane_ids_is_empty() {
        let tree = super::LayoutNodeJson::from_flat_pane_ids(&[]);
        assert_eq!(tree, super::LayoutNodeJson::Empty);
    }

    #[test]
    fn layout_node_json_single_pane_is_leaf() {
        let tree = super::LayoutNodeJson::from_flat_pane_ids(&[42]);
        assert_eq!(tree, super::LayoutNodeJson::Leaf { pane_id: 42 });
    }

    #[test]
    fn layout_node_json_two_panes_single_split() {
        let tree = super::LayoutNodeJson::from_flat_pane_ids(&[1, 2]);
        match tree {
            super::LayoutNodeJson::Split {
                direction,
                ratio,
                left,
                right,
            } => {
                assert_eq!(direction, "horizontal");
                assert!((ratio - 0.5).abs() < f32::EPSILON);
                assert_eq!(*left, super::LayoutNodeJson::Leaf { pane_id: 1 });
                assert_eq!(*right, super::LayoutNodeJson::Leaf { pane_id: 2 });
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn layout_node_json_many_panes_cascade() {
        let tree = super::LayoutNodeJson::from_flat_pane_ids(&[1, 2, 3, 4]);
        // Walk the right spine: leaves 1, 2, 3, 4.
        fn collect(node: &super::LayoutNodeJson, out: &mut Vec<u64>) {
            match node {
                super::LayoutNodeJson::Leaf { pane_id } => out.push(*pane_id),
                super::LayoutNodeJson::Split { left, right, .. } => {
                    collect(left, out);
                    collect(right, out);
                }
                super::LayoutNodeJson::Empty => {}
            }
        }
        let mut ids = Vec::new();
        collect(&tree, &mut ids);
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn layout_node_json_from_snapshot_preserves_directions_and_ratios() {
        use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
        // Horizontal root: leaf(1) | (vertical leaf(2) / leaf(3)).
        let snap = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.6,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.25,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
            }),
        };
        let tree = super::LayoutNodeJson::from_snapshot(&snap);
        match &tree {
            super::LayoutNodeJson::Split {
                direction,
                ratio,
                left,
                right,
            } => {
                assert_eq!(direction, "horizontal");
                assert!((ratio - 0.6).abs() < f32::EPSILON);
                assert!(matches!(
                    left.as_ref(),
                    super::LayoutNodeJson::Leaf { pane_id: 1 }
                ));
                match right.as_ref() {
                    super::LayoutNodeJson::Split {
                        direction,
                        ratio,
                        left,
                        right,
                    } => {
                        assert_eq!(direction, "vertical");
                        assert!((ratio - 0.25).abs() < f32::EPSILON);
                        assert!(matches!(
                            left.as_ref(),
                            super::LayoutNodeJson::Leaf { pane_id: 2 }
                        ));
                        assert!(matches!(
                            right.as_ref(),
                            super::LayoutNodeJson::Leaf { pane_id: 3 }
                        ));
                    }
                    _ => panic!("expected nested vertical split"),
                }
            }
            _ => panic!("expected horizontal split"),
        }
        let json = serde_json::to_string(&tree).unwrap();
        let parsed: super::LayoutNodeJson = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tree);
    }

    #[test]
    fn layout_node_json_serde_round_trip() {
        let tree = super::LayoutNodeJson::from_flat_pane_ids(&[10, 20, 30]);
        let json = serde_json::to_string(&tree).expect("serialize");
        let parsed: super::LayoutNodeJson = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, tree);
        // Spot-check the tagged union shape.
        assert!(json.contains("\"type\":\"split\""));
        assert!(json.contains("\"direction\":\"horizontal\""));
        assert!(json.contains("\"type\":\"leaf\""));
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

    // ── terminal.agents.get_details ─────────────────────────────────────

    /// Calling `handle_get_agent_details` on a nonexistent pane must return
    /// a tool error (not a transport error). This matches the convention
    /// used by `handle_get_pane_geometry` and `handle_read_pane_content`.
    #[tokio::test]
    async fn get_agent_details_nonexistent_pane_returns_tool_error() {
        let server = make_server(trusted_config());
        let params = super::PaneIdParam { pane_id: 999_999 };
        let result = server
            .handle_get_agent_details(params)
            .await
            .expect("handler should not error at transport level");
        assert_eq!(result.is_error, Some(true));
    }

    /// `AgentDetailsResult` with no agent data must serialise to a shape
    /// where only `consecutive_failures` is present (all `Option` fields
    /// are skipped when `None`).
    #[test]
    fn agent_details_all_none_serializes_minimally() {
        let details = super::AgentDetailsResult {
            agent_type: None,
            model: None,
            context_percent: None,
            consecutive_failures: 0,
            last_command: None,
            last_exit_code: None,
            last_command_duration_ms: None,
        };
        let v = serde_json::to_value(&details).expect("serialize");
        assert_eq!(v["consecutive_failures"], 0);
        assert!(v.get("agent_type").is_none());
        assert!(v.get("model").is_none());
        assert!(v.get("context_percent").is_none());
        assert!(v.get("last_command").is_none());
        assert!(v.get("last_exit_code").is_none());
        assert!(v.get("last_command_duration_ms").is_none());
    }

    /// Observer-tier enforcement for `terminal.agents.get_details`: a
    /// Sandboxed agent must be allowed, matching the rest of the
    /// `terminal.agents.*` surface.
    #[test]
    fn sandboxed_agent_can_call_get_agent_details() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_trust("terminal.agents.get_details", &agent)
                .is_ok()
        );
    }

    // ── terminal.agents.get_status ──────────────────────────────────────

    /// `terminal.agents.get_status` must be advertised in `tool_definitions()`.
    #[test]
    fn get_agent_status_tool_is_registered() {
        let defs = super::tools::tool_definitions();
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            names.contains(&"terminal.agents.get_status"),
            "tool_definitions missing terminal.agents.get_status: {names:?}"
        );
    }

    /// Observer-tier enforcement: a Sandboxed-tier caller is permitted.
    #[test]
    fn sandboxed_agent_can_call_get_agent_status() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_trust("terminal.agents.get_status", &agent)
                .is_ok()
        );
    }

    /// Regression-lock the bypass class: the trust category map must
    /// classify `get_status` as Observer (not None / unknown), so it
    /// remains gated by `enforce_trust` even if the dispatch arm is
    /// reordered.
    #[test]
    fn get_agent_status_is_observer_tier() {
        use crate::trust::{ToolCategory, tool_category};
        assert_eq!(
            tool_category("terminal.agents.get_status"),
            Some(ToolCategory::Observer)
        );
    }

    /// Calling `handle_get_agent_status` with no agent registered and no
    /// capacity entry must return a tool error rather than a transport
    /// error or an empty success.
    #[tokio::test]
    async fn get_agent_status_unknown_pane_returns_tool_error() {
        let server = make_server(trusted_config());
        let params = super::PaneIdParam { pane_id: 999_999 };
        let result = server
            .handle_get_agent_status(params)
            .await
            .expect("handler should not error at transport level");
        assert_eq!(result.is_error, Some(true));
    }

    // ── terminal.semantic.query_commands ────────────────────────────────

    #[test]
    fn sandboxed_agent_can_call_query_commands() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_trust("terminal.semantic.query_commands", &agent)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn query_commands_nonexistent_pane_returns_tool_error() {
        let server = make_server(trusted_config());
        let params = super::QueryCommandsParam {
            pane_id: 999_999,
            since_line: None,
            limit: None,
        };
        let result = server
            .handle_query_commands(params)
            .await
            .expect("handler should not error at transport level");
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn query_commands_result_empty_serializes() {
        let result = super::QueryCommandsResult {
            pane_id: 1,
            commands: Vec::new(),
        };
        let v = serde_json::to_value(&result).expect("serialize");
        assert_eq!(v["pane_id"], 1);
        assert_eq!(v["commands"].as_array().unwrap().len(), 0);
    }

    /// End-to-end check: create a real session, drive OSC 633 marks through
    /// a `TherminalInterceptor` sharing the pane's command tracker `Arc`,
    /// then assert `handle_query_commands` returns the expected blocks.
    #[tokio::test]
    async fn query_commands_returns_tracked_blocks() {
        use alacritty_terminal::vte::SequenceInterceptor;
        use therminal_terminal::interceptor::TherminalInterceptor;

        let server = make_server(trusted_config());

        // Create a real session + pane (spawns a real shell PTY).
        let pane_id = {
            let mut mgr = server.session_mgr.lock().await;
            let session_id = mgr
                .create_session(Some("test-session".to_string()))
                .expect("create session");
            // Find the pane id of the freshly created session.
            mgr.iter_sessions()
                .find(|(id, _)| **id == session_id)
                .and_then(|(_, s)| s.windows.first())
                .and_then(|w| w.panes.first())
                .map(|p| p.id)
                .expect("session has at least one pane")
        };

        // Grab the shared tracker Arc and drive OSC 633 marks through a
        // standalone interceptor that holds the same Arc — this mirrors
        // what the reader thread does in production.
        let tracker_arc = {
            let mgr = server.session_mgr.lock().await;
            mgr.pane_command_tracker_arc(pane_id)
                .expect("pane has tracker")
        };
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults_and_tracker(tracker_arc);

        // Two complete commands.
        let sequences: &[&[&[u8]]] = &[
            &[b"633", b"A"],
            &[b"633", b"B"],
            &[b"633", b"E", b"echo first"],
            &[b"633", b"C"],
            &[b"633", b"D", b"0"],
            &[b"633", b"A"],
            &[b"633", b"B"],
            &[b"633", b"E", b"false"],
            &[b"633", b"C"],
            &[b"633", b"D", b"1"],
        ];
        for params in sequences {
            interceptor.intercept_osc(params, true);
        }

        // Call the tool handler and assert the result.
        let params = super::QueryCommandsParam {
            pane_id,
            since_line: None,
            limit: None,
        };
        let result = server
            .handle_query_commands(params)
            .await
            .expect("handler ok");
        assert_ne!(result.is_error, Some(true), "expected success");

        let json = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("content");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        let cmds = parsed["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 2, "expected two tracked commands");
        assert_eq!(cmds[0]["command_text"], "echo first");
        assert_eq!(cmds[0]["exit_code"], 0);
        assert_eq!(cmds[1]["command_text"], "false");
        assert_eq!(cmds[1]["exit_code"], 1);

        // limit caps to most recent.
        let params = super::QueryCommandsParam {
            pane_id,
            since_line: None,
            limit: Some(1),
        };
        let result = server
            .handle_query_commands(params)
            .await
            .expect("handler ok");
        let json = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("content");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        let cmds = parsed["commands"].as_array().expect("commands array");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0]["command_text"], "false");
    }

    // ── terminal.panes.query_events ─────────────────────────────────────

    #[test]
    fn sandboxed_agent_can_call_query_events() {
        let server = make_server(sandboxed_config());
        let agent = agent("sandboxed-bot");
        assert!(
            server
                .enforce_trust("terminal.panes.query_events", &agent)
                .is_ok()
        );
    }

    #[test]
    fn supervised_agent_can_call_query_events() {
        let server = make_server(supervised_config());
        let agent = agent("supervised-bot");
        assert!(
            server
                .enforce_trust("terminal.panes.query_events", &agent)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn query_events_nonexistent_pane_returns_tool_error() {
        let server = make_server(trusted_config());
        let params = super::QueryEventsParam {
            pane_id: 999_999,
            since_timestamp_secs: None,
            limit: None,
        };
        let result = server
            .handle_query_events(params)
            .await
            .expect("handler should not error at transport level");
        assert_eq!(result.is_error, Some(true));
    }

    #[tokio::test]
    async fn query_events_fresh_pane_returns_empty() {
        use therminal_terminal::event_log::SessionEvent;

        let server = make_server(trusted_config());
        let pane_id = {
            let mut mgr = server.session_mgr.lock().await;
            let session_id = mgr
                .create_session(Some("evt-session".to_string()))
                .expect("create session");
            mgr.iter_sessions()
                .find(|(id, _)| **id == session_id)
                .and_then(|(_, s)| s.windows.first())
                .and_then(|w| w.panes.first())
                .map(|p| p.id)
                .expect("session has at least one pane")
        };

        let params = super::QueryEventsParam {
            pane_id,
            since_timestamp_secs: None,
            limit: None,
        };
        let result = server
            .handle_query_events(params)
            .await
            .expect("handler ok");
        assert_ne!(result.is_error, Some(true));
        let json = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("content");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(parsed["events"].as_array().unwrap().len(), 0);

        // Inject a few events directly via the shared Arc.
        let log_arc = {
            let mgr = server.session_mgr.lock().await;
            mgr.pane_event_log_arc(pane_id).expect("event log arc")
        };
        {
            let mut log = log_arc.lock().unwrap();
            log.log(&SessionEvent::Spawn {
                command: "bash".into(),
                cwd: "/home/test".into(),
            });
            log.log(&SessionEvent::Bell);
            log.log(&SessionEvent::Bell);
            log.log(&SessionEvent::Resize { cols: 80, rows: 24 });
        }

        // Default limit returns all four oldest-first.
        let params = super::QueryEventsParam {
            pane_id,
            since_timestamp_secs: None,
            limit: None,
        };
        let result = server
            .handle_query_events(params)
            .await
            .expect("handler ok");
        let json = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("content");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        let events = parsed["events"].as_array().expect("events array");
        assert_eq!(events.len(), 4);
        assert_eq!(events[0]["event_type"], "spawn");
        assert_eq!(events[0]["details"]["command"], "bash");
        assert_eq!(events[3]["event_type"], "resize");

        // Limit caps to most recent.
        let params = super::QueryEventsParam {
            pane_id,
            since_timestamp_secs: None,
            limit: Some(1),
        };
        let result = server
            .handle_query_events(params)
            .await
            .expect("handler ok");
        let json = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("content");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        let events = parsed["events"].as_array().expect("events array");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event_type"], "resize");

        // since_timestamp_secs in the far future filters everything out.
        let params = super::QueryEventsParam {
            pane_id,
            since_timestamp_secs: Some(u64::MAX),
            limit: None,
        };
        let result = server
            .handle_query_events(params)
            .await
            .expect("handler ok");
        let json = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("content");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
        assert_eq!(parsed["events"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn query_events_tool_is_registered() {
        use std::collections::HashSet;
        let tools = tool_definitions();
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(names.contains("terminal.panes.query_events"));
    }

    #[test]
    fn query_commands_tool_is_registered() {
        use std::collections::HashSet;
        let tools = tool_definitions();
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(names.contains("terminal.semantic.query_commands"));
    }

    /// With an empty session manager, the only resources should be the
    /// global event streams (claude/events and agents/events).
    #[tokio::test]
    async fn build_resource_list_no_panes_only_claude_events() {
        let server = make_server(trusted_config());
        let resources = server.build_resource_list().await;
        assert_eq!(
            resources.len(),
            2,
            "expected only the global event-stream resources with no panes"
        );
    }
}
