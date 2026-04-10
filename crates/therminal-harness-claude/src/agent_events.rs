//! Structured JSON output event types for AI agent sessions.
//!
//! `AgentEvent` variants are produced by the JSONL tailer
//! via `session_log::parse_session_event` and consumed by overlay widgets.
//!
//! Hook-driven variants (`SessionStart`, `SessionStop`, `ToolState`,
//! `SubagentStart`, `SubagentStop`, `StopFailure`) are produced by the
//! hook-push path (`hook_push` module) when the JSONL tailer is unavailable
//! (e.g. Windows-native daemon cannot reach the WSL filesystem).

use serde::Serialize;

/// A parsed event from an agent's structured JSON output stream.
///
/// Two production paths feed into this enum:
///
/// 1. **JSONL tailer** -- parses structured lines from
///    `~/.claude/projects/{hash}/{session-id}.jsonl`.
/// 2. **Hook push** -- accepts signals pushed directly via the
///    `therminal` CLI from Claude Code hook scripts running in an
///    environment where the JSONL files are reachable (e.g. WSL when the
///    daemon runs on Windows native). Hook-sourced events carry
///    `EventSource::Hook` and use the variants below.
///
/// Both paths emit `TaggedAgentEvent` on the same broadcast channel so MCP
/// subscribers observe a unified stream regardless of which path is active.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// User message sent to the agent.
    UserMessage { content: String },
    /// Assistant text response.
    AssistantMessage { content: String },
    /// Tool invocation request from the agent.
    ToolUse {
        tool: String,
        input: serde_json::Value,
        tool_use_id: Option<String>,
    },
    /// Result of a tool execution.
    ToolResult {
        tool: String,
        output: String,
        is_error: bool,
        tool_use_id: Option<String>,
    },
    /// Progress update for a running tool.
    Progress {
        tool: String,
        status: String,
        message: Option<String>,
        tool_use_id: Option<String>,
    },
    /// Model thinking/reasoning content.
    Thinking { content: String },
    /// A tool call whose path argument has been resolved against the
    /// agent's current working directory (tn-gidy). Emitted by
    /// `crate::pipeline` when a `AgentEvent::ToolUse` carries a
    /// recognised path-shaped input and the cwd index knows the agent's
    /// working directory for the source session.
    ///
    /// Consumers: MCP subscribers observing `source_class=harness` +
    /// `kind=tool_call`; the renderer bridge that turns these into
    /// clickable hotspots.
    ToolCallResolved {
        /// Tool name (e.g. `"Update"`, `"Read"`, `"Edit"`).
        tool: String,
        /// The path as Claude printed it (typically relative to the agent cwd).
        display_path: String,
        /// The absolute path after joining against the agent cwd.
        resolved_path: String,
        /// Claude Code session id that owns this tool call. Clients use
        /// this to correlate with `EventSource` and the cwd index.
        session_id: String,
    },

    // -- Hook-push variants ------------------------------------------------
    // Produced by the hook-push path when the JSONL tailer is unavailable.
    // Hook scripts running in WSL push these signals to the Windows-native
    // daemon via `therminal agent-event push`.
    /// Session lifecycle start (maps to Claude Code `SessionStart` hook).
    SessionStart {
        /// Claude Code session UUID (from `$CLAUDE_SESSION_ID`).
        session_id: String,
        /// Absolute path to the project directory.
        project_dir: String,
        /// Claude Code process PID.
        pid: u32,
    },
    /// Session lifecycle stop (maps to `Stop` / `SessionEnd` hooks).
    SessionStop {
        session_id: String,
        /// Reason string from `SessionEnd.session_end_reason` if available.
        reason: Option<String>,
    },
    /// Tool-use state update (maps to `PreToolUse` / `PostToolUse` hooks).
    ToolState {
        session_id: String,
        /// `tool_use` / `idle` / `processing` / `awaiting_input`.
        status: String,
        /// Tool name when `status == "tool_use"`.
        tool_name: Option<String>,
        /// Brief serialised tool input summary.
        tool_input_summary: Option<String>,
    },
    /// Subagent spawned (maps to `SubagentStart` hook).
    SubagentStart {
        parent_session_id: String,
        /// Per-subagent UUID from `agent_id` field (v2.1.83+).
        agent_id: String,
        /// `agent_type` from hook stdin (e.g. `"subagent"`).
        agent_type: Option<String>,
    },
    /// Subagent finished (maps to `SubagentStop` hook).
    SubagentStop {
        parent_session_id: String,
        agent_id: String,
    },
    /// Session stopped due to an API error (maps to `StopFailure` hook).
    StopFailure {
        session_id: String,
        /// `error_type` enum: `rate_limit`, `authentication_failed`,
        /// `billing_error`, `invalid_request`, `server_error`,
        /// `max_output_tokens`, `unknown`.
        error_type: String,
    },
}
