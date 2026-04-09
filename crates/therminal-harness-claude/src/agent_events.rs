//! Structured JSON output event types for AI agent sessions.
//!
//! `AgentEvent` variants are produced by the JSONL tailer
//! via `session_log::parse_session_event` and consumed by overlay widgets.

use serde::Serialize;

/// A parsed event from an agent's structured JSON output stream.
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
    /// [`crate::pipeline`] when a [`AgentEvent::ToolUse`] carries a
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
}
