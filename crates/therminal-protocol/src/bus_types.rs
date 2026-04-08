//! Core wire types for the therminal message bus.
//!
//! These types originated in thermal-desktop's ggl codegen but are now
//! maintained directly. Only the types actually used by the bus protocol
//! are kept here.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

// ── AgentId ──────────────────────────────────────────────────────────────────

/// Identifies a participant on the message bus.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub agent_type: String,
    pub key: String,
}

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

// ── TaskState ────────────────────────────────────────────────────────────────

/// Lifecycle state for a tracked task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    Submitted,
    Working,
    Completed,
    Failed,
    InputRequired,
}

// ── ClaudeStatus ─────────────────────────────────────────────────────────────

/// Status of an agent session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeStatus {
    #[default]
    Idle,
    Processing,
    ToolUse,
    AwaitingInput,
}

// ── Unified event bus types ───────────────────────────────────────────────────
//
// These types define the wire shape for the unified event bus described in
// `docs/event-bus-spec.md`. They are protocol types only — no ring buffer,
// MCP handler, or publisher logic lives here. Implementation is tracked in
// tn-xula.

/// Which integration surface emitted a [`TerminalEvent`].
///
/// Matches the three-surface taxonomy in the root `CLAUDE.md`:
/// core capabilities, harness crates, and pattern packs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceClass {
    /// A harness crate (`crates/therminal-harness-*/`): Claude Code, Codex, etc.
    Harness,
    /// A pattern pack (`plugins/`): TOML regex-match patterns.
    Pattern,
    /// A core capability: shell integration, agent detection, cadence analysis, etc.
    Core,
}

/// A single event on the unified therminal event bus.
///
/// All three integration surfaces (harness crates, pattern packs, core
/// capabilities) publish events with this shape. Subscribers receive events
/// via the `therminal://events` MCP resource; see `docs/event-bus-spec.md`
/// for the full filter grammar and subscription semantics.
///
/// ## Body size caps
///
/// The `body` field should stay under 4 KB (recommended). The bus enforces a
/// hard cap of 64 KB; events that exceed it are replaced by a
/// `bus.body_too_large` error event emitted by the `core` source class.
///
/// ## Trust and redaction
///
/// Events are low-trust by default. If `body` contains `"secret": true`, the
/// bus replaces `body` with `{"redacted": true}` for low-trust subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalEvent {
    /// Which integration surface produced this event.
    pub source_class: SourceClass,

    /// Identifier of the specific publisher within `source_class`.
    ///
    /// Convention: lowercase `[a-z0-9_-]+`. Examples: `claude`, `codex`,
    /// `cargo-errors`, `shell-integration`.
    pub source_id: String,

    /// Event kind string; see `docs/event-bus-kinds.md` for the cross-source
    /// vocabulary. Source-specific kinds use dot-namespacing: `claude.thinking_started`.
    pub kind: String,

    /// Pane the event is scoped to, if applicable. `None` for session-scoped events.
    pub pane_id: Option<u64>,

    /// Wall-clock timestamp in milliseconds since Unix epoch, set by the publisher.
    pub ts_ms: u64,

    /// Monotonic bus position assigned by the ring buffer (not by the publisher).
    ///
    /// Starts at 1 and increments per accepted event. Resets to 1 on daemon
    /// restart. Use as an opaque token for `since=<cursor>` resumption.
    pub cursor: u64,

    /// Source-defined payload. Must be a JSON object (`{}`), not a bare value.
    /// May include `"secret": true` to trigger body redaction for low-trust subscribers.
    pub body: serde_json::Value,
}
