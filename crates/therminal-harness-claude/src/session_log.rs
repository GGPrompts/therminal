//! JSONL session log reader for Claude Code sessions.
//!
//! Loads and parses a CC session JSONL file into typed [`SessionEvent`]s.
//! Each line is a JSON object with a `type` field and optional `timestamp`.
//! Tool call lifecycles are paired by `tool_use_id` to compute durations.
//!
//! # CC JSONL Format
//!
//! ```json
//! {"type":"user","content":"Hello","timestamp":"2026-04-02T00:13:52.232Z","sessionId":"abc"}
//! {"type":"assistant","content":[{"type":"text","text":"I'll help"}],"timestamp":"..."}
//! {"type":"tool_use","tool":"Bash","tool_use_id":"tu_01","input":{...},"timestamp":"..."}
//! {"type":"tool_result","tool":"Bash","tool_use_id":"tu_01","output":"ok","timestamp":"..."}
//! {"type":"thinking","content":"Let me analyze...","timestamp":"..."}
//! {"type":"progress","tool":"Bash","status":"running","message":"...","timestamp":"..."}
//! {"type":"system","content":"...","timestamp":"..."}
//! ```

use serde::Deserialize;

// ── Types ────────────────────────────────────────────────────────────────────

/// The kind of event in a session log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionEventType {
    UserMessage,
    AssistantText,
    ToolUse,
    ToolResult,
    Thinking,
    Progress,
    SystemMessage,
}

impl std::fmt::Display for SessionEventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserMessage => write!(f, "user"),
            Self::AssistantText => write!(f, "assistant"),
            Self::ToolUse => write!(f, "tool_use"),
            Self::ToolResult => write!(f, "tool_result"),
            Self::Thinking => write!(f, "thinking"),
            Self::Progress => write!(f, "progress"),
            Self::SystemMessage => write!(f, "system"),
        }
    }
}

/// A single parsed event from a CC session JSONL file.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    /// ISO 8601 timestamp string from the JSONL line.
    pub timestamp: String,
    /// Event type classification.
    pub event_type: SessionEventType,
    /// Primary content (message text, tool output, thinking text, etc.).
    pub content: String,
    /// Tool name, if this is a tool-related event.
    pub tool_name: Option<String>,
    /// Tool use ID for pairing ToolUse <-> ToolResult.
    pub tool_use_id: Option<String>,
    /// Whether a ToolResult was an error.
    pub is_error: bool,
}

// ── Raw JSONL envelope ───────────────────────────────────────────────────────

/// Flexible deserialization envelope for CC JSONL lines.
///
/// CC JSONL uses a nested format:
/// - `{"type":"user",      "message":{"role":"user","content":"..." or [...]}, "timestamp":"..."}`
/// - `{"type":"assistant",  "message":{"role":"assistant","content":[...]}, "timestamp":"..."}`
/// - `{"type":"system",     "content":"...", "timestamp":"..."}`
/// - `{"type":"permission-mode", ...}`
/// - `{"type":"attachment", ...}`
///
/// User/assistant content is nested under `message.content`. Assistant content is
/// an array of items: `{"type":"text","text":"..."}`, `{"type":"tool_use","name":"...","id":"...","input":{...}}`,
/// or `{"type":"thinking","thinking":"..."}`. Tool results appear as user messages
/// with content array items: `{"type":"tool_result","tool_use_id":"...","content":"..."}`.
#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    msg_type: Option<String>,

    #[serde(default)]
    timestamp: Option<String>,
    /// Top-level content (used by `system` type and legacy flat format).
    #[serde(default)]
    content: Option<serde_json::Value>,
    /// Nested message envelope (used by `user` and `assistant` types).
    #[serde(default)]
    message: Option<serde_json::Value>,
    // Legacy flat fields (kept for backward compat with old/hypothetical format).
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    status: Option<String>,
}

// ── Parsing helpers ──────────────────────────────────────────────────────────

/// Parse a single JSONL line into one or more SessionEvents.
///
/// A single line may produce multiple events because CC nests multiple content
/// items (text, tool_use, thinking) inside a single assistant message.
pub fn parse_session_event(line: &str) -> Vec<SessionEvent> {
    let raw: RawLine = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let msg_type = match raw.msg_type.as_deref() {
        Some(t) => t,
        None => return Vec::new(),
    };

    let timestamp = raw.timestamp.clone().unwrap_or_default();

    // Helper to build an event with common fields.
    let make_event = |event_type,
                      content: String,
                      tool_name: Option<String>,
                      tool_use_id: Option<String>,
                      is_error: bool| {
        SessionEvent {
            timestamp: timestamp.clone(),
            event_type,
            content,
            tool_name,
            tool_use_id,
            is_error,
        }
    };

    // Extract the message.content field (nested format used by user/assistant).
    let msg_content = raw.message.as_ref().and_then(|m| m.get("content"));

    match msg_type {
        "user" => {
            // User messages can be:
            // 1. Plain text: message.content is a string
            // 2. Tool results: message.content is an array with tool_result items
            // 3. System reminders: message.content is an array with text items
            match msg_content {
                Some(serde_json::Value::String(s)) => {
                    vec![make_event(
                        SessionEventType::UserMessage,
                        s.clone(),
                        None,
                        None,
                        false,
                    )]
                }
                Some(serde_json::Value::Array(arr)) => {
                    let mut events = Vec::new();
                    for item in arr {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("tool_result") => {
                                let content = item
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let tool_use_id = item
                                    .get("tool_use_id")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string());
                                let is_error = item
                                    .get("is_error")
                                    .and_then(|e| e.as_bool())
                                    .unwrap_or(false);
                                events.push(make_event(
                                    SessionEventType::ToolResult,
                                    content,
                                    None, // tool name filled later via tool_use_id pairing
                                    tool_use_id,
                                    is_error,
                                ));
                            }
                            Some("text") => {
                                let text = item
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.is_empty() {
                                    events.push(make_event(
                                        SessionEventType::UserMessage,
                                        text,
                                        None,
                                        None,
                                        false,
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                    events
                }
                // Fallback: try top-level content (old system messages).
                None => {
                    let content = extract_content_string(&raw.content);
                    if content.is_empty() {
                        Vec::new()
                    } else {
                        vec![make_event(
                            SessionEventType::UserMessage,
                            content,
                            None,
                            None,
                            false,
                        )]
                    }
                }
                _ => Vec::new(),
            }
        }

        "assistant" => {
            // Assistant content is always an array of items:
            //   {"type":"text","text":"..."}
            //   {"type":"tool_use","name":"...","id":"...","input":{...}}
            //   {"type":"thinking","thinking":"..."}
            match msg_content {
                Some(serde_json::Value::Array(arr)) => {
                    let mut events = Vec::new();
                    for item in arr {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                let text = item
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.is_empty() {
                                    events.push(make_event(
                                        SessionEventType::AssistantText,
                                        text,
                                        None,
                                        None,
                                        false,
                                    ));
                                }
                            }
                            Some("tool_use") => {
                                let name = item
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("?")
                                    .to_string();
                                let id = item
                                    .get("id")
                                    .and_then(|i| i.as_str())
                                    .map(|s| s.to_string());
                                let input_json = item
                                    .get("input")
                                    .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                    .unwrap_or_default();
                                events.push(make_event(
                                    SessionEventType::ToolUse,
                                    input_json,
                                    Some(name),
                                    id,
                                    false,
                                ));
                            }
                            Some("thinking") => {
                                let thinking = item
                                    .get("thinking")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !thinking.is_empty() {
                                    events.push(make_event(
                                        SessionEventType::Thinking,
                                        thinking,
                                        None,
                                        None,
                                        false,
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                    events
                }
                // Fallback: try top-level content.
                _ => {
                    let content = extract_content_string(&raw.content);
                    if content.is_empty() {
                        Vec::new()
                    } else {
                        vec![make_event(
                            SessionEventType::AssistantText,
                            content,
                            None,
                            None,
                            false,
                        )]
                    }
                }
            }
        }

        // System messages use flat top-level content.
        "system" | "permission-mode" | "last-prompt" | "attachment" => {
            let content = extract_content_string(&raw.content);
            if content.is_empty() {
                Vec::new()
            } else {
                vec![make_event(
                    SessionEventType::SystemMessage,
                    content,
                    None,
                    None,
                    false,
                )]
            }
        }

        // Legacy flat format fields (kept for backward compat).
        "tool_use" => {
            let content = raw
                .input
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            vec![make_event(
                SessionEventType::ToolUse,
                content,
                raw.tool.clone(),
                raw.tool_use_id.clone(),
                false,
            )]
        }
        "tool_result" => {
            let content = raw.output.clone().unwrap_or_default();
            vec![make_event(
                SessionEventType::ToolResult,
                content,
                raw.tool.clone(),
                raw.tool_use_id.clone(),
                raw.is_error.unwrap_or(false),
            )]
        }
        "thinking" => {
            let content = extract_content_string(&raw.content);
            vec![make_event(
                SessionEventType::Thinking,
                content,
                None,
                None,
                false,
            )]
        }
        "progress" => {
            // Legacy progress: prefer `message` string, fall back to `status`.
            let content = raw
                .message
                .as_ref()
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or(raw.status.clone())
                .unwrap_or_default();
            vec![make_event(
                SessionEventType::Progress,
                content,
                raw.tool.clone(),
                None,
                false,
            )]
        }

        _ => Vec::new(),
    }
}

/// Extract text content from CC's polymorphic `content` field.
///
/// CC uses either a plain string or an array of `{"type":"text","text":"..."}` objects.
fn extract_content_string(value: &Option<serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    parts.push(text);
                }
            }
            parts.join("")
        }
        _ => String::new(),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_lines(lines: &[&str]) -> Vec<SessionEvent> {
        lines
            .iter()
            .flat_map(|line| parse_session_event(line))
            .collect()
    }

    #[test]
    fn parse_empty_input() {
        let events = parse_lines(&[]);
        assert!(events.is_empty());
    }

    #[test]
    fn parse_user_message() {
        let events = parse_lines(&[
            r#"{"type":"user","content":"Hello world","timestamp":"2026-04-02T00:13:52.232Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::UserMessage);
        assert_eq!(e.content, "Hello world");
    }

    #[test]
    fn parse_assistant_with_content_array() {
        let events = parse_lines(&[
            r#"{"type":"assistant","content":[{"type":"text","text":"I'll help you"}],"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::AssistantText);
        assert_eq!(e.content, "I'll help you");
    }

    #[test]
    fn parse_tool_use_and_result() {
        let events = parse_lines(&[
            r#"{"type":"tool_use","tool":"Bash","tool_use_id":"tu_01","input":{"command":"ls"},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_01","output":"file.rs","timestamp":"2026-04-02T00:14:02.500Z"}"#,
        ]);
        assert_eq!(events.len(), 2);

        let use_event = &events[0];
        assert_eq!(use_event.event_type, SessionEventType::ToolUse);
        assert_eq!(use_event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(use_event.tool_use_id.as_deref(), Some("tu_01"));

        let result_event = &events[1];
        assert_eq!(result_event.event_type, SessionEventType::ToolResult);
        assert_eq!(result_event.tool_name.as_deref(), Some("Bash"));
        assert_eq!(result_event.content, "file.rs");
    }

    #[test]
    fn parse_thinking_event() {
        let events = parse_lines(&[
            r#"{"type":"thinking","content":"Let me analyze the code...","timestamp":"2026-04-02T00:15:00.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::Thinking);
        assert_eq!(e.content, "Let me analyze the code...");
    }

    #[test]
    fn parse_progress_event() {
        let events = parse_lines(&[
            r#"{"type":"progress","tool":"Bash","status":"running","message":"Compiling...","timestamp":"2026-04-02T00:15:01.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::Progress);
        assert_eq!(e.content, "Compiling...");
        assert_eq!(e.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn parse_system_message() {
        let events = parse_lines(&[
            r#"{"type":"system","content":"Session started","timestamp":"2026-04-02T00:10:00.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::SystemMessage);
    }

    #[test]
    fn unknown_type_skipped() {
        let events = parse_lines(&[r#"{"type":"unknown_future_type","data":"something"}"#]);
        assert!(events.is_empty());
    }

    #[test]
    fn invalid_json_skipped() {
        let events = parse_lines(&[
            "not json at all",
            r#"{"type":"user","content":"valid","timestamp":"2026-04-02T00:10:00.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn tool_result_error_flag() {
        let events = parse_lines(&[
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_02","output":"command not found","is_error":true,"timestamp":"2026-04-02T00:14:05.000Z"}"#,
        ]);
        assert!(events[0].is_error);
    }

    // ── Tests for actual CC nested JSONL format ─────────────────────────

    #[test]
    fn nested_user_message_plain_string() {
        let events = parse_lines(&[
            r#"{"type":"user","message":{"role":"user","content":"Hello world"},"timestamp":"2026-04-02T00:13:52.232Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::UserMessage);
        assert_eq!(e.content, "Hello world");
    }

    #[test]
    fn nested_assistant_text() {
        let events = parse_lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll help you with that."}]},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::AssistantText);
        assert_eq!(e.content, "I'll help you with that.");
    }

    #[test]
    fn nested_tool_use_in_assistant() {
        let events = parse_lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","id":"toolu_01ABC","input":{"command":"ls -la"}}]},"timestamp":"2026-04-02T00:14:01.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::ToolUse);
        assert_eq!(e.tool_name.as_deref(), Some("Bash"));
        assert_eq!(e.tool_use_id.as_deref(), Some("toolu_01ABC"));
        assert!(e.content.contains("ls -la"));
    }

    #[test]
    fn nested_tool_result_in_user() {
        let events = parse_lines(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01ABC","content":"file.rs\nCargo.toml"}]},"timestamp":"2026-04-02T00:14:02.500Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::ToolResult);
        assert_eq!(e.tool_use_id.as_deref(), Some("toolu_01ABC"));
        assert_eq!(e.content, "file.rs\nCargo.toml");
    }

    #[test]
    fn nested_tool_result_tool_use_id_preserved() {
        let events = parse_lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","id":"toolu_01XYZ","input":{"file_path":"/tmp/test"}}]},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01XYZ","content":"file contents here"}]},"timestamp":"2026-04-02T00:14:03.000Z"}"#,
        ]);
        assert_eq!(events.len(), 2);

        let result = &events[1];
        assert_eq!(result.event_type, SessionEventType::ToolResult);
        assert_eq!(result.tool_use_id.as_deref(), Some("toolu_01XYZ"));
    }

    #[test]
    fn nested_mixed_assistant_content() {
        // Assistant message with both text and tool_use in the same content array.
        let events = parse_lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Let me check that."},{"type":"tool_use","name":"Grep","id":"toolu_02DEF","input":{"pattern":"TODO"}}]},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        ]);
        assert_eq!(events.len(), 2);

        let text_event = &events[0];
        assert_eq!(text_event.event_type, SessionEventType::AssistantText);
        assert_eq!(text_event.content, "Let me check that.");

        let tool_event = &events[1];
        assert_eq!(tool_event.event_type, SessionEventType::ToolUse);
        assert_eq!(tool_event.tool_name.as_deref(), Some("Grep"));
    }

    #[test]
    fn nested_tool_result_error() {
        let events = parse_lines(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ERR","content":"command not found","is_error":true}]},"timestamp":"2026-04-02T00:14:05.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        assert!(events[0].is_error);
    }

    #[test]
    fn nested_thinking_in_assistant() {
        let events = parse_lines(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me analyze the structure..."}]},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        ]);
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.event_type, SessionEventType::Thinking);
        assert_eq!(e.content, "Let me analyze the structure...");
    }

    #[test]
    fn permission_mode_skipped_gracefully() {
        let events = parse_lines(&[
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"abc"}"#,
        ]);
        // permission-mode has no content field, so it produces empty content and is skipped.
        assert!(events.is_empty());
    }
}
