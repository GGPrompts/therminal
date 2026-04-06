//! Structured JSON output event types for AI agent sessions.
//!
//! `AgentEvent` variants are produced by the JSONL tailer
//! via `claude_session_log::parse_session_event` and consumed by overlay widgets.

/// A parsed event from an agent's structured JSON output stream.
#[derive(Debug, Clone, PartialEq)]
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
}
