//! Wire protocol types — inlined from thermal-desktop's ggl codegen.
//!
//! In thermal-desktop these were generated at build time from a `.ggl` schema
//! via `ggl-build`. For therminal we inline them directly to avoid the build
//! dependency while keeping the exact same serde representation.
//!
//! Versioned names (`AgentIdV1`, etc.) are kept as the canonical definitions;
//! unversioned type aliases live below for ergonomic use across the codebase.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ── Generated struct/enum equivalents ───────────────────────────────────────

/// Identifies a participant on the message bus.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentIdV1 {
    pub agent_type: String,
    pub key: String,
}

/// Lifecycle state for a tracked task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskStateV1 {
    Submitted,
    Working,
    Completed,
    Failed,
    InputRequired,
}

/// Status of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeStatusV1 {
    Idle,
    Processing,
    ToolUse,
    AwaitingInput,
}

/// Argument details from a tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolArgsV1 {
    pub file_path: Option<String>,
    pub command: Option<String>,
    pub pattern: Option<String>,
    pub description: Option<String>,
}

/// Tool event metadata from the agent state file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolDetailsV1 {
    pub event: Option<String>,
    pub tool: Option<String>,
    pub args: Option<ToolArgsV1>,
}

/// Unified agent session state, read from state JSON files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionStateV1 {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub status: ClaudeStatusV1,
    pub current_tool: Option<String>,
    pub subagent_count: Option<i64>,
    pub context_percent: Option<f64>,
    pub working_dir: Option<String>,
    pub last_updated: Option<String>,
    pub details: Option<ToolDetailsV1>,
    pub hook_type: Option<String>,
    pub tmux_pane: Option<String>,
    pub pid: Option<i64>,
    pub workspace: Option<i64>,
    pub source: Option<String>,
    pub last_command: Option<String>,
    pub last_exit_code: Option<i64>,
    pub last_command_started_at: Option<String>,
    pub last_command_duration_ms: Option<i64>,
    pub consecutive_failures: Option<i64>,
}

/// Pane layout strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LayoutV1 {
    Grid,
    Sidebar,
    Stack,
}

/// Operational state of a single agent pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentStateV1 {
    Idle,
    Running,
    Thinking,
    Warning,
    Error,
    Complete,
}

/// Snapshot of a single PTY pane's state and metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneInfoV1 {
    pub id: String,
    pub title: String,
    pub state: AgentStateV1,
    pub command: String,
    pub last_output_line: String,
    pub output_lines: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Top-level configuration for the conductor/daemon component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConductorConfigV1 {
    pub tmux_session: String,
    pub max_panes: i64,
    pub capture_fps: i64,
    pub layout: LayoutV1,
    pub audio_enabled: bool,
    pub dbus_enabled: bool,
}

// ── Type aliases ─────────────────────────────────────────────────────────────

/// Current version of `AgentId` used throughout the codebase.
pub type AgentId = AgentIdV1;

/// Current version of `TaskState` used throughout the codebase.
pub type TaskState = TaskStateV1;

/// Current version of `ClaudeStatus` used throughout the codebase.
pub type ClaudeStatus = ClaudeStatusV1;

/// Current version of `SessionState` used throughout the codebase.
pub type SessionState = SessionStateV1;

/// Current version of `ToolArgs` used throughout the codebase.
pub type ToolArgs = ToolArgsV1;

/// Current version of `ToolDetails` used throughout the codebase.
pub type ToolDetails = ToolDetailsV1;

/// Current version of `Layout` used throughout the codebase.
pub type Layout = LayoutV1;

/// Current version of `AgentState` used throughout the codebase.
pub type AgentState = AgentStateV1;

/// Current version of `ConductorConfig` used throughout the codebase.
pub type ConductorConfig = ConductorConfigV1;

/// Current version of `PaneInfo` used throughout the codebase.
pub type PaneInfo = PaneInfoV1;

// ── Default impls ────────────────────────────────────────────────────────────

impl Default for ClaudeStatus {
    fn default() -> Self {
        ClaudeStatusV1::Idle
    }
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            parent_session_id: None,
            agent_id: None,
            agent_type: None,
            model: None,
            status: ClaudeStatusV1::Idle,
            current_tool: None,
            subagent_count: Some(0),
            context_percent: None,
            working_dir: None,
            last_updated: None,
            details: None,
            hook_type: None,
            tmux_pane: None,
            pid: None,
            workspace: None,
            source: None,
            last_command: None,
            last_exit_code: None,
            last_command_started_at: None,
            last_command_duration_ms: None,
            consecutive_failures: None,
        }
    }
}

impl Default for ConductorConfig {
    fn default() -> Self {
        Self {
            tmux_session: "therminal-daemon".to_string(),
            max_panes: 16,
            capture_fps: 30,
            layout: LayoutV1::Grid,
            audio_enabled: true,
            dbus_enabled: true,
        }
    }
}

// ── AgentId extras ───────────────────────────────────────────────────────────

impl AgentId {
    /// Convenience constructor.
    pub fn new(agent_type: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            agent_type: agent_type.into(),
            key: key.into(),
        }
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.agent_type, self.key)
    }
}

/// Error returned when parsing an `AgentId` from a string that does not
/// contain exactly one `/` separator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseAgentIdError(pub String);

impl fmt::Display for ParseAgentIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid AgentId '{}': expected format 'type/key'",
            self.0
        )
    }
}

impl std::error::Error for ParseAgentIdError {}

// ── AgentState extras ────────────────────────────────────────────────────────

impl AgentState {
    /// Short uppercase label suitable for HUD readouts.
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Idle => "IDLE",
            AgentState::Running => "RUNNING",
            AgentState::Thinking => "THINKING",
            AgentState::Warning => "WARNING",
            AgentState::Error => "ERROR",
            AgentState::Complete => "COMPLETE",
        }
    }

    /// Single-character icon for compact status indicators.
    pub fn icon(self) -> &'static str {
        match self {
            AgentState::Idle => "○",
            AgentState::Running => "◉",
            AgentState::Thinking => "◎",
            AgentState::Warning => "▲",
            AgentState::Error => "✗",
            AgentState::Complete => "✓",
        }
    }
}

impl FromStr for AgentId {
    type Err = ParseAgentIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (agent_type, key) = s
            .split_once('/')
            .ok_or_else(|| ParseAgentIdError(s.to_string()))?;
        if agent_type.is_empty() || key.is_empty() {
            return Err(ParseAgentIdError(s.to_string()));
        }
        Ok(Self {
            agent_type: agent_type.to_string(),
            key: key.to_string(),
        })
    }
}
