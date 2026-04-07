//! Core types for agent state inference.

use serde::Serialize;

// -- State change notifications ----------------------------------------------

/// A notification emitted when the inference engine detects a state change.
/// Used by the daemon to bridge into semantic events without polling.
#[derive(Debug, Clone)]
pub enum StateChangeNotification {
    /// Agent activity status changed.
    StatusChanged {
        old: InferredStatus,
        new: InferredStatus,
    },
    /// A tool invocation started.
    ToolStarted { tool_name: String },
    /// A tool invocation completed (inferred from status change away from ToolUse).
    ToolCompleted { tool_name: String },
    /// Agent type was detected from output.
    AgentDetected { agent_type: AgentType },
    /// Model name was detected from output.
    ModelDetected { model: String },
    /// Context percentage was updated.
    ContextUpdated { percent: f32 },
    /// OSC 633 command started executing.
    CommandStarted { command: Option<String> },
    /// OSC 633 command finished.
    CommandFinished {
        command: Option<String>,
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    /// Structured JSON output mode detected (agent launched with --output-format json).
    StructuredJsonDetected,
}

// -- Agent types -------------------------------------------------------------

/// The type of agent running in this terminal session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    Claude,
    Codex,
    Copilot,
    Aider,
}

impl AgentType {
    /// The state directory path for this agent type.
    pub fn state_dir(&self) -> &'static str {
        match self {
            AgentType::Claude => "/tmp/claude-code-state",
            AgentType::Codex => "/tmp/codex-state",
            AgentType::Copilot => "/tmp/copilot-state",
            AgentType::Aider => "/tmp/aider-state",
        }
    }

    /// Try to infer agent type from a spawn command string.
    pub fn from_command(cmd: &str) -> Option<Self> {
        let tokens: Vec<String> = cmd
            .split_whitespace()
            .map(|token| {
                token
                    .trim_matches(|c: char| {
                        !c.is_ascii_alphanumeric() && c != '-' && c != '_' && c != '/'
                    })
                    .rsplit('/')
                    .next()
                    .unwrap_or(token)
                    .to_lowercase()
            })
            .filter(|token| !token.is_empty())
            .collect();

        if tokens.iter().any(|token| token == "gh") && tokens.iter().any(|token| token == "copilot")
        {
            Some(AgentType::Copilot)
        } else if tokens.iter().any(|token| token.contains("claude")) {
            Some(AgentType::Claude)
        } else if tokens
            .iter()
            .any(|token| token == "copilot" || token.starts_with("copilot-"))
        {
            Some(AgentType::Copilot)
        } else if tokens
            .iter()
            .any(|token| token == "codex" || token.starts_with("codex-"))
        {
            Some(AgentType::Codex)
        } else if tokens.iter().any(|token| token.contains("aider")) {
            Some(AgentType::Aider)
        } else {
            None
        }
    }

    /// String representation matching the existing state file format.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentType::Claude => "claude",
            AgentType::Codex => "codex",
            AgentType::Copilot => "copilot",
            AgentType::Aider => "aider",
        }
    }
}

// -- Inferred status ---------------------------------------------------------

/// Agent status, matching the format in `ClaudeStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferredStatus {
    Idle,
    Processing,
    Streaming,
    Thinking,
    ToolUse { tool_name: String },
    AwaitingInput,
}

impl InferredStatus {
    pub(crate) fn status_str(&self) -> &str {
        match self {
            InferredStatus::Idle => "idle",
            InferredStatus::Processing => "processing",
            InferredStatus::Streaming => "streaming",
            InferredStatus::Thinking => "thinking",
            InferredStatus::ToolUse { .. } => "tool_use",
            InferredStatus::AwaitingInput => "awaiting_input",
        }
    }

    pub(crate) fn tool_name(&self) -> Option<&str> {
        match self {
            InferredStatus::ToolUse { tool_name } => Some(tool_name),
            _ => None,
        }
    }
}

// -- Configuration -----------------------------------------------------------

/// Configuration for the inference engine.
pub struct InferenceConfig {
    /// Session ID used in state files.
    pub session_id: String,
    /// PID of the PTY child process.
    pub child_pid: u32,
    /// Agent type (if known from spawn command). Inferred from output if None.
    pub agent_type: Option<AgentType>,
    /// Working directory of the session.
    pub working_dir: Option<String>,
}

// -- State file format -------------------------------------------------------

/// JSON structure written to state files for agent session state.
///
/// Field names and types are aligned with the `ClaudeStatus` enum in
/// `therminal-protocol` so that the JSON written here deserializes correctly
/// via `ClaudeStatePoller` (therminal-core). We keep a local struct rather
/// than importing `therminal-core` to avoid pulling GPU/Wayland dependencies
/// into this lightweight, Android-compatible crate.
#[derive(Debug, Serialize)]
pub(crate) struct StateFile {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_command_started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_command_duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Agent type detection ------------------------------------------------

    #[test]
    fn agent_type_from_command() {
        assert_eq!(
            AgentType::from_command("claude --model opus"),
            Some(AgentType::Claude)
        );
        assert_eq!(
            AgentType::from_command("codex --model o4-mini"),
            Some(AgentType::Codex)
        );
        assert_eq!(
            AgentType::from_command("gh copilot suggest"),
            Some(AgentType::Copilot)
        );
        assert_eq!(
            AgentType::from_command("gh copilot suggest --prompt 'compare this to codex'"),
            Some(AgentType::Copilot)
        );
        assert_eq!(
            AgentType::from_command("/usr/bin/gh copilot suggest"),
            Some(AgentType::Copilot)
        );
        assert_eq!(
            AgentType::from_command("/usr/local/bin/codex-wrapper"),
            Some(AgentType::Codex)
        );
        assert_eq!(AgentType::from_command("vim file.rs"), None);
    }

    #[test]
    fn agent_type_state_dir() {
        assert_eq!(AgentType::Claude.state_dir(), "/tmp/claude-code-state");
        assert_eq!(AgentType::Codex.state_dir(), "/tmp/codex-state");
        assert_eq!(AgentType::Copilot.state_dir(), "/tmp/copilot-state");
    }

    // -- State file format ---------------------------------------------------

    #[test]
    fn state_file_serialization() {
        let state = StateFile {
            session_id: "abc-123".to_string(),
            agent_type: Some("claude".to_string()),
            status: "tool_use".to_string(),
            current_tool: Some("Read".to_string()),
            working_dir: Some("/home/user/project".to_string()),
            last_updated: Some("2026-03-30T12:00:00Z".to_string()),
            pid: Some(12345),
            model: Some("claude-sonnet-4-20250514".to_string()),
            context_percent: Some(42.0),
            source: Some("terminal_inference".to_string()),
            last_command: None,
            last_exit_code: None,
            last_command_started_at: None,
            last_command_duration_ms: None,
            consecutive_failures: None,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(json.contains("\"session_id\": \"abc-123\""));
        assert!(json.contains("\"status\": \"tool_use\""));
        assert!(json.contains("\"current_tool\": \"Read\""));
        assert!(json.contains("\"source\": \"terminal_inference\""));
        assert!(json.contains("\"pid\": 12345"));
    }

    #[test]
    fn state_file_omits_none_fields() {
        let state = StateFile {
            session_id: "abc-123".to_string(),
            agent_type: Some("claude".to_string()),
            status: "idle".to_string(),
            current_tool: None,
            working_dir: None,
            last_updated: Some("2026-03-30T12:00:00Z".to_string()),
            pid: Some(12345),
            model: None,
            context_percent: None,
            source: Some("terminal_inference".to_string()),
            last_command: None,
            last_exit_code: None,
            last_command_started_at: None,
            last_command_duration_ms: None,
            consecutive_failures: None,
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(!json.contains("current_tool"));
        assert!(!json.contains("working_dir"));
        assert!(!json.contains("model"));
        assert!(!json.contains("context_percent"));
    }

    // -- Status string coverage ------------------------------------------------

    #[test]
    fn status_str_streaming() {
        assert_eq!(InferredStatus::Streaming.status_str(), "streaming");
    }

    #[test]
    fn status_str_thinking() {
        assert_eq!(InferredStatus::Thinking.status_str(), "thinking");
    }
}
