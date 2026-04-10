//! MCP tool parameter and result types.
//!
//! All param structs (deserialized from MCP tool call arguments) and result
//! structs (serialized back into `CallToolResult` content) live here to keep
//! `mod.rs` focused on the server struct, `ServerHandler` impl, and tests.
//!
//! Numeric fields use `deser_compat::*_flexible` deserializers so both native
//! JSON integers and stringified integers are accepted (see tn-ad0g).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::deser_compat;

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
pub(crate) struct EmptyParams {}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SessionIdParam {
    /// The numeric session ID.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) session_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct CreateSessionParam {
    /// Optional human-readable name for the session.
    pub(crate) name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct WriteToPaneParam {
    /// The numeric pane ID to write to.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// The text (or bytes as UTF-8) to send to the pane's PTY.
    pub(crate) input: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct PaneIdParam {
    /// The numeric pane ID.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
}

/// Parameters for `terminal.panes.get_content` (tn-sp3n).
///
/// All optional fields default to "no change" relative to the historical
/// shape of the call, except for `trim_trailing_whitespace`, which defaults
/// to `true` — empty rows on a sparse pane become empty strings instead of
/// 80+ space characters. This is the cache-churn fix the issue is named for.
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetContentParam {
    /// The numeric pane ID.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Trim trailing whitespace from every emitted row. Defaults to `true`.
    /// Set to `false` to get the historical fixed-width grid (each row
    /// padded out to `cols` spaces).
    #[serde(default = "default_true")]
    pub(crate) trim_trailing_whitespace: bool,
    /// Drop fully-whitespace rows from the result. Implies trimming.
    /// Defaults to `false`.
    #[serde(default)]
    pub(crate) compact: bool,
    /// If set, only return the LAST N rows of the grid (after any
    /// trim/compact filtering). Useful for "show me what just happened"
    /// against a tall pane.
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) rows: Option<usize>,
}

/// Parameters for `terminal.panes.peek` (tn-sp3n).
///
/// Returns the last N non-empty lines plus a content hash and timestamp.
/// Cheaper than `get_content` for "is anything new?" polling.
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct PeekPaneParam {
    /// The numeric pane ID.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Number of lines to return. Defaults to 10. Capped server-side at 50.
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) lines: Option<usize>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct TagPaneParam {
    /// The numeric pane ID to tag.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Opaque key/value tags to merge into the pane's tag set. Existing
    /// keys with the same name are overwritten; keys not present here are
    /// left untouched. Therminal does not interpret tag values.
    pub(crate) tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct UntagPaneParam {
    /// The numeric pane ID to untag.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Tag keys to remove. If omitted, all tags on the pane are cleared.
    pub(crate) keys: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PaneTagsResult {
    pub(crate) pane_id: u64,
    /// Full tag set currently bound to the pane after the operation.
    pub(crate) tags: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ListPanesParam {
    /// Optional session ID to filter panes by. If omitted, returns panes from all sessions.
    #[serde(default, deserialize_with = "deser_compat::u64_opt_flexible")]
    pub(crate) session_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct QuerySemanticHistoryParam {
    /// The numeric pane ID to query.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Optional region type filter. One of: Prompt, Command, Output, Error, ToolCall, Thinking, Annotation.
    pub(crate) region_type: Option<String>,
    /// Optional regex pattern to match against region content preview.
    pub(crate) pattern: Option<String>,
    /// Maximum number of regions to return (default 20).
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) limit: Option<usize>,
    /// Only return regions starting at or after this line number.
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) since_line: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct QueryCommandsParam {
    /// The numeric pane ID to query.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Only return command blocks whose `start_line` is at or after this
    /// line number. If omitted, returns all tracked commands up to `limit`.
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) since_line: Option<usize>,
    /// Maximum number of command entries to return (default 20).
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct QueryEventsParam {
    /// The numeric pane ID to query.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Only return events whose Unix-seconds timestamp is at or after this
    /// value. If omitted, returns all events in the buffer up to `limit`.
    #[serde(default, deserialize_with = "deser_compat::u64_opt_flexible")]
    pub(crate) since_timestamp_secs: Option<u64>,
    /// Maximum number of events to return (default 100). The underlying
    /// in-memory ring is capped at 5000 entries; values above that cap
    /// have no effect.
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetPaneGeometryParam {
    /// The numeric pane ID.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct WaitForOutputParam {
    /// The numeric pane ID to watch.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// String or regex pattern to match against output lines.
    pub(crate) pattern: String,
    /// Timeout in milliseconds (default 30000, max 120000).
    #[serde(
        default = "default_timeout_ms",
        deserialize_with = "deser_compat::u64_default_flexible"
    )]
    pub(crate) timeout_ms: u64,
    /// Only match output on or after this line number. If omitted, matches any new output.
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) since_line: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetHotspotsParam {
    /// The numeric pane ID to scan for hotspots.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) pane_id: u64,
    /// Optional hotspot type filter. One of: file, url, git_ref, issue.
    pub(crate) hotspot_type: Option<String>,
    /// Maximum number of hotspots to return (default 50).
    #[serde(default, deserialize_with = "deser_compat::usize_opt_flexible")]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetWorkspaceLayoutParam {
    /// The workspace slot number (1-9) to fetch the layout for.
    #[serde(deserialize_with = "deser_compat::u64_flexible")]
    pub(crate) workspace_id: u64,
    /// Optional session ID. If omitted, searches across all sessions and uses
    /// the first workspace with a matching ID.
    #[serde(default, deserialize_with = "deser_compat::u64_opt_flexible")]
    pub(crate) session_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ListWorkspacesParam {
    /// Optional session ID to filter workspaces by. If omitted, returns workspaces from all sessions.
    #[serde(default, deserialize_with = "deser_compat::u64_opt_flexible")]
    pub(crate) session_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ListAgentsParam {
    /// Optional status filter. One of: active, idle, processing, streaming, thinking, tool_use, awaiting_input.
    pub(crate) status: Option<String>,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SpawnPaneParam {
    /// Session ID to create the pane in. If omitted, uses the default (first) session.
    #[serde(default, deserialize_with = "deser_compat::u64_opt_flexible")]
    pub(crate) session_id: Option<u64>,
    /// Shell command to run. If omitted, spawns the user's default shell.
    pub(crate) command: Option<String>,
    /// Working directory for the new pane. If omitted, inherits the current directory.
    pub(crate) cwd: Option<String>,
    /// Split direction: "horizontal" or "vertical". Defaults to "vertical" when splitting.
    pub(crate) split_direction: Option<String>,
    /// Pane ID to split from. If specified, the new pane is created as a sibling of this pane.
    #[serde(default, deserialize_with = "deser_compat::u64_opt_flexible")]
    pub(crate) split_from: Option<u64>,
    /// Optional command written to the pane after the first shell prompt
    /// starts rendering. Appends a trailing newline when missing.
    pub(crate) startup_command: Option<String>,
    /// Split ratio for the source (first) child (0.1..0.9). Default 0.5.
    #[serde(default, deserialize_with = "deser_compat::f32_opt_flexible")]
    pub(crate) ratio: Option<f32>,
    /// Shell binary to spawn instead of the global default (e.g. "/bin/fish",
    /// "powershell.exe"). When omitted, the daemon uses `general.shell` from config.
    /// Distinct from `command` (which is a legacy alias for shell) and `startup_command`
    /// (which is injected after the prompt is ready).
    pub(crate) shell: Option<String>,
}

// ── Tool result types ───────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SessionListResult {
    pub(crate) session_ids: Vec<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SessionInfoResult {
    pub(crate) session_id: u64,
    pub(crate) name: Option<String>,
    pub(crate) created_at_secs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SessionCreatedResult {
    pub(crate) session_id: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SessionDestroyedResult {
    pub(crate) session_id: u64,
    pub(crate) destroyed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct WriteToPaneResult {
    pub(crate) pane_id: u64,
    pub(crate) success: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PaneContentResult {
    pub(crate) pane_id: u64,
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_col: usize,
    pub(crate) cursor_line: usize,
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    /// Stable 64-bit hash of the visible grid as the daemon saw it BEFORE
    /// any trim/compact/rows filtering. Subscribers polling repeatedly can
    /// short-circuit on an unchanged hash without paying for the lines
    /// payload. Always present (tn-sp3n).
    pub(crate) content_hash: String,
    /// Number of lines that were trimmed off the bottom of the response
    /// because the caller asked for `rows = last_N`. `0` when the entire
    /// grid was returned.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub(crate) truncated_rows: usize,
}

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

/// Light-weight pane summary returned by `terminal.panes.get_summary`
/// (tn-sp3n). Designed to fit in ~100 bytes so a conductor can poll many
/// panes without burning cache. All fields except `pane_id`, cursor pos,
/// and `content_hash` are best-effort and skipped from the wire when
/// unknown.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PaneSummaryResult {
    pub(crate) pane_id: u64,
    /// Cursor column in the visible grid (0-based).
    pub(crate) cursor_col: usize,
    /// Cursor row in the visible grid (0-based).
    pub(crate) cursor_line: usize,
    /// Stable 64-bit hash of the visible grid (same value as
    /// `PaneContentResult.content_hash`). Hex-encoded.
    pub(crate) content_hash: String,
    /// Most recent finished command's text, if shell integration is wired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_command: Option<String>,
    /// Exit code of the most recent finished command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_exit_code: Option<i32>,
    /// Number of detected hotspots in the current visible grid.
    pub(crate) hotspot_count: usize,
    /// Name of the AI agent currently running in this pane (from the
    /// `AgentRegistry`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_name: Option<String>,
    /// Lifecycle status of the agent in this pane (e.g. "active",
    /// "thinking", "tool_use"). Omitted when no agent is registered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_status: Option<String>,
    /// User-authored session title (`hookSpecificOutput.sessionTitle`).
    /// Omitted when no hook has written it. See tn-ifee.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_title: Option<String>,
    /// Wall-clock seconds when the snapshot was taken.
    pub(crate) timestamp_secs: u64,
}

/// Lightweight "what just happened?" snapshot returned by
/// `terminal.panes.peek` (tn-sp3n).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PanePeekResult {
    pub(crate) pane_id: u64,
    /// Last N lines of the visible grid, trimmed and with whitespace-only
    /// rows dropped. Oldest first within the truncated window.
    pub(crate) lines: Vec<String>,
    /// Stable 64-bit hash of the FULL visible grid (not the truncated peek
    /// window). Subscribers can short-circuit on an unchanged hash.
    pub(crate) content_hash: String,
    /// Wall-clock seconds when the snapshot was taken.
    pub(crate) timestamp_secs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PaneInfo {
    pub(crate) pane_id: u64,
    pub(crate) session_id: u64,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) title: String,
    /// Current working directory, from OSC 7 or initial spawn. `None` when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
    /// Exit code of the most recently finished command (from OSC 633 D marks
    /// via the region index). `None` when no command has finished yet or the
    /// shell integration isn't reporting exit codes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_exit_code: Option<i32>,
    /// Name of the AI agent detected in this pane (from the daemon's
    /// `AgentRegistry`). `None` when no agent is detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_name: Option<String>,
    /// Opaque key/value tags bound to this pane (tn-bbvf). Empty when
    /// no tags have been set. Set/cleared via `terminal.panes.tag` and
    /// `terminal.panes.untag`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub(crate) tags: std::collections::HashMap<String, String>,
    /// User-authored session title from the Claude Code
    /// `UserPromptSubmit` hook (`hookSpecificOutput.sessionTitle`).
    /// `None` when no hook has written it. Surfaced here so callers don't
    /// need a separate `terminal.agents.get_session_detail` call just to
    /// render a tab/header label. See tn-ifee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_title: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ListPanesResult {
    pub(crate) panes: Vec<PaneInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SpawnPaneResult {
    pub(crate) pane_id: u64,
    pub(crate) session_id: u64,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct DestroyPaneResult {
    /// Whether the pane was successfully destroyed.
    pub(crate) success: bool,
    /// Human-readable status message.
    pub(crate) message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct WaitForOutputResult {
    /// Whether the pattern was matched before timeout.
    pub(crate) matched: bool,
    /// The line number where the match was found (0 if not matched).
    pub(crate) line_number: usize,
    /// The content of the matched line (empty if not matched).
    pub(crate) line_content: String,
    /// Elapsed time in milliseconds.
    pub(crate) elapsed_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct GetPaneGeometryResult {
    pub(crate) pane_id: u64,
    /// Number of columns in the pane.
    pub(crate) cols: u16,
    /// Number of rows in the pane.
    pub(crate) rows: u16,
    /// Whether the pane can be split horizontally (top/bottom) based on minimum dimensions.
    pub(crate) can_split_h: bool,
    /// Whether the pane can be split vertically (left/right) based on minimum dimensions.
    pub(crate) can_split_v: bool,
}

/// Minimum columns required per pane (derived from MIN_PANE_WIDTH / typical cell width).
pub(crate) const MIN_PANE_COLS: u16 = 10;

/// Minimum rows required per pane (derived from MIN_PANE_HEIGHT / typical cell height).
pub(crate) const MIN_PANE_ROWS: u16 = 4;

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct HotspotInfo {
    /// The hotspot type: "file", "url", "git_ref", or "issue".
    #[serde(rename = "type")]
    pub(crate) hotspot_type: String,
    /// The matched text.
    pub(crate) text: String,
    /// Row number in the visible grid (0-based).
    pub(crate) line: usize,
    /// First column of the match (0-based, inclusive).
    pub(crate) col_start: usize,
    /// One-past-last column of the match (exclusive).
    pub(crate) col_end: usize,
    /// Type-specific metadata. For files: resolved absolute path. For URLs: the URL.
    /// For git refs: the ref text. For issues: the issue reference.
    pub(crate) metadata: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct GetHotspotsResult {
    pub(crate) pane_id: u64,
    pub(crate) hotspots: Vec<HotspotInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct WorkspaceInfoResult {
    /// Workspace slot number (1-9).
    pub(crate) workspace_id: u64,
    /// Human-readable workspace name.
    pub(crate) name: String,
    /// Number of panes in this workspace.
    pub(crate) pane_count: usize,
    /// Whether this is the currently active workspace.
    pub(crate) is_active: bool,
    /// Pane IDs assigned to this workspace.
    pub(crate) pane_ids: Vec<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ListWorkspacesResult {
    pub(crate) workspaces: Vec<WorkspaceInfoResult>,
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
pub(crate) struct GetWorkspaceLayoutResult {
    /// Workspace slot number (1-9).
    pub(crate) workspace_id: u64,
    /// Session that owns this workspace.
    pub(crate) session_id: u64,
    /// Layout tree. Currently a degraded cascade; see `LayoutNodeJson`.
    pub(crate) layout: LayoutNodeJson,
    /// Focused pane within the workspace, if any.
    pub(crate) focused_pane: Option<u64>,
    /// True when the `layout` field is the degraded shim (no real directions
    /// or ratios). Clients can ignore this and use the tree shape as-is; it
    /// exists so debugging tools can call out the limitation.
    pub(crate) degraded: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct AgentInfoResult {
    /// The pane ID where the agent is running.
    pub(crate) pane_id: u64,
    /// Human-readable agent name (e.g. "node").
    pub(crate) name: String,
    /// Agent type: claude, codex, copilot, or aider.
    pub(crate) agent_type: String,
    /// Current status: active, idle, processing, streaming, thinking, tool_use, awaiting_input.
    pub(crate) status: String,
    /// Current tool name if status is tool_use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_tool: Option<String>,
    /// Unix timestamp (seconds) when the agent was first detected.
    pub(crate) detected_at: u64,
    /// OS process ID (if known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pid: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct ListAgentsResult {
    pub(crate) agents: Vec<AgentInfoResult>,
}

/// Detailed inference data for the agent running in a specific pane.
///
/// All inference fields are optional because (a) the pane may not host an
/// agent, and (b) the `AgentStateInference` engine is not yet plumbed into
/// the daemon — see follow-up issue. The `agent_type` field is populated
/// from `AgentRegistry` when an agent is currently registered on the pane.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct AgentDetailsResult {
    /// Agent type (claude, codex, copilot, aider) if an agent is registered
    /// on this pane. `None` if no agent is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_type: Option<String>,
    /// Model string detected from the pane (e.g. "claude-sonnet-4").
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    /// Context-window usage percentage (0.0 - 100.0).
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_percent: Option<f32>,
    /// Number of consecutive failed commands. Defaults to 0 when unknown.
    pub(crate) consecutive_failures: u32,
    /// Most recent command string observed on the pane.
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_command: Option<String>,
    /// Exit code of the most recent command.
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_exit_code: Option<i32>,
    /// Duration in milliseconds of the most recent command.
    /// Currently always `None` until state inference is plumbed into the daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_command_duration_ms: Option<u64>,
}

/// One PTY chunk-arrival sample as exposed via `terminal.agents.get_cadence`.
///
/// Wall-clock times are computed from the inference engine's monotonic
/// `Instant` snapshots at the moment the snapshot is built, so they're safe
/// to serialise across IPC. `gap_ms` is `0.0` for the first sample in the
/// returned window.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CadenceSampleResult {
    /// Wall-clock arrival time, Unix epoch seconds.
    pub(crate) timestamp_secs: u64,
    /// Number of bytes in the PTY chunk.
    pub(crate) bytes: u64,
    /// Gap from the previous sample in milliseconds. `0.0` for the first
    /// sample in the window.
    pub(crate) gap_ms: f32,
}

/// Output cadence metrics for the agent running in a specific pane.
///
/// Returned by `terminal.agents.get_cadence`. Built from the per-pane
/// `AgentStateInference` engine's chunk-stats sliding window. Fields default
/// to zero / `false` / empty when the pane has no streaming activity (e.g.
/// a pane that hasn't received any PTY bytes yet, or a pane whose chunk
/// window has fewer than two entries to compute intervals from).
///
/// `recent_samples` is capped to keep the wire payload bounded —
/// the underlying chunk window is small (~20 entries today) but the cap
/// prevents future window growth from leaking unbounded data through MCP.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct AgentCadenceResult {
    /// Number of chunks currently in the analysis window.
    pub(crate) chunk_count: u64,
    /// Average inter-chunk arrival interval in milliseconds. `0.0` when
    /// fewer than two chunks have been observed.
    pub(crate) avg_arrival_ms: f32,
    /// Largest gap between consecutive chunks in milliseconds. `0.0` when
    /// fewer than two chunks have been observed.
    pub(crate) max_gap_ms: f32,
    /// `true` when recent output looks like a spinner pattern
    /// (cursor-control-heavy, low visible text per chunk).
    pub(crate) is_spinner: bool,
    /// `true` when recent output is a sustained high-throughput stream
    /// (>500 visible chars/sec for at least 2 seconds, no backspaces).
    pub(crate) is_streaming: bool,
    /// Most recent chunk samples (oldest first), capped at 50.
    pub(crate) recent_samples: Vec<CadenceSampleResult>,
}

/// Dynamic status snapshot for the agent running in a specific pane.
///
/// Strict subset of `AgentDetailsResult` focused on the live mode + capacity
/// fields used by sibling agents to coordinate. Combines the
/// `AgentRegistry` view (mode-ish: `agent_type`, `status`, `current_tool`)
/// with the `PaneCapacityCache` view (`context_percent`, `model`).
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct AgentStatusResult {
    /// The pane the agent is running in (echoed for caller convenience).
    pub(crate) pane_id: u64,
    /// Agent type (claude, codex, copilot, aider) if registered. `None`
    /// when only capacity data is known for this pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_type: Option<String>,
    /// Lifecycle status. Identical formatting to `AgentInfoResult.status`
    /// (`AgentStatus::as_str`). Falls back to "unknown" when no
    /// `AgentRegistry` entry exists for the pane.
    pub(crate) status: String,
    /// Current tool name when status is `tool_use`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_tool: Option<String>,
    /// Context-window usage percentage (0.0 - 100.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_percent: Option<f32>,
    /// Model string (e.g. "claude-opus-4-6").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
}

/// Detailed agent session info returned by
/// `terminal.agents.get_session_detail`. The five tn-ifee fields plus
/// `pane_id` for caller convenience. Every field is `Option` because
/// absence is normal — hooks may not be installed, may lag, or may not
/// have observed a tool call yet.
///
/// ## Deferred fields (intentionally NOT included — keep this list current)
///
/// The following fields were deliberately scoped out of tn-ifee to keep
/// the surface small. Add them only after a deliberate decision (file an
/// issue, justify the cost):
///
/// - `total_input_tokens` / `total_output_tokens` — token-counter UI is a
///   separate widget; expose via a dedicated tool when there's a consumer.
/// - `input_tokens_cache_read` / `input_tokens_cache_creation` — cache
///   health widget is a separate epic.
/// - `tool_args` (parsed JSON) — chrome rendering belongs to a future
///   header/status bar issue, not the wire format.
/// - `subagent_count` — exposed indirectly via the `therminal://claude/events`
///   stream's `EventSource::Subagent` lineage; a flat counter is redundant.
/// - `last_prompt_summary` — privacy-sensitive; opt-in only.
/// - Richer `status` enum — current `String` is `ClaudeStatus::as_str()`
///   and is sufficient for v1; promote to a typed enum once a second
///   consumer needs it.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct AgentSessionDetail {
    /// The pane the agent is running in (echoed for caller convenience).
    pub(crate) pane_id: u64,
    /// User-authored title from the `UserPromptSubmit` hook
    /// (`hookSpecificOutput.sessionTitle`). Inference-unrecoverable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_title: Option<String>,
    /// Tool currently in flight (e.g. "Bash", "Read"). From PreToolUse hook.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_tool: Option<String>,
    /// Claude's working directory (not the shell's). From hook `$PWD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) working_dir: Option<String>,
    /// Context-window usage percent (0.0 - 100.0). Sourced from the
    /// `PaneCapacityCache` populated by the Claude state poller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_percent: Option<f32>,
    /// Model string (e.g. "claude-opus-4-6").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
}

/// Parameters for `terminal.agents.find_with_capacity`.
///
/// `threshold_percent` is interpreted as the minimum REMAINING context-window
/// percent (0.0 - 100.0). Agents whose remaining capacity is unknown are
/// included by design (treated as "potentially has capacity").
#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct FindWithCapacityParam {
    /// Minimum remaining context-window percent (0.0 - 100.0). An agent is
    /// included if `remaining_percent >= threshold_percent`. Agents with
    /// unknown capacity are always included.
    #[serde(deserialize_with = "deser_compat::f32_flexible")]
    pub(crate) threshold_percent: f32,
}

/// One agent's capacity snapshot, as returned by
/// `terminal.agents.find_with_capacity`.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct AgentCapacityInfo {
    pub(crate) pane_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_type: Option<String>,
    pub(crate) status: String,
    /// Currently used context-window percent (0.0 - 100.0). `None` when no
    /// `PaneCapacityCache` entry exists for the pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_percent: Option<f32>,
    /// Remaining context-window percent (`100.0 - context_percent`). `None`
    /// when capacity is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) remaining_percent: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct FindWithCapacityResult {
    pub(crate) agents: Vec<AgentCapacityInfo>,
}

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SemanticRegionInfo {
    /// The region type (Prompt, Command, Output, Error, ToolCall, Thinking, Annotation).
    pub(crate) region_type: String,
    /// The terminal line where this region starts.
    pub(crate) start_line: usize,
    /// The terminal line where this region ends, or null if still open.
    pub(crate) end_line: Option<usize>,
    /// First 200 characters of the region's metadata/content preview.
    pub(crate) content_preview: String,
    /// Metadata (exit_code, cwd, command, timestamp, etc.).
    pub(crate) metadata: std::collections::HashMap<String, String>,
}

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct QuerySemanticHistoryResult {
    pub(crate) pane_id: u64,
    pub(crate) regions: Vec<SemanticRegionInfo>,
}

/// A single shell command tracked via OSC 633 `CommandTracker`.
///
/// All non-positional fields are `Option` because OSC 633 marks may be
/// partial: a command currently running has no `end_line`, `exit_code`,
/// or `duration_ms`; a shell that doesn't emit `E` marks has no
/// `command_text`.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct CommandInfo {
    /// The command text as reported by OSC 633 `E` mark, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command_text: Option<String>,
    /// Exit code from the OSC 633 `D` mark. `None` while running or not
    /// reported by the shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i32>,
    /// Wall-clock duration between `C` and `D` marks, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u64>,
    /// Grid line where the prompt started (from `A` mark).
    pub(crate) start_line: usize,
    /// Grid line where execution finished (from `D` mark). `None` while
    /// still running.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) end_line: Option<usize>,
    /// Unix timestamp (seconds) when the command was observed, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) timestamp_secs: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct QueryCommandsResult {
    pub(crate) pane_id: u64,
    /// Command blocks, oldest first (newest-last transcript order).
    pub(crate) commands: Vec<CommandInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct EventInfo {
    /// Unix timestamp (seconds) when the event was logged.
    pub(crate) timestamp_secs: u64,
    /// Event variant tag (e.g. `spawn`, `status_change`, `command_start`,
    /// `command_finish`, `resize`, `pty_eof`, `bell`). Returned as a free-form
    /// string so adding new variants does not break the wire format.
    pub(crate) event_type: String,
    /// Variant-specific payload as a JSON object. Empty object for variants
    /// like `bell` that have no payload.
    pub(crate) details: serde_json::Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct QueryEventsResult {
    pub(crate) pane_id: u64,
    /// Recent events, oldest-first within the truncated window.
    pub(crate) events: Vec<EventInfo>,
}
