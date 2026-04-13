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

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── AgentId ────────────────────────────────────────────────────────

    #[test]
    fn agent_id_display() {
        let id = AgentId::new("claude", "sess-1");
        assert_eq!(id.to_string(), "claude/sess-1");
    }

    #[test]
    fn agent_id_from_str_round_trip() {
        let id: AgentId = "claude/sess-1".parse().unwrap();
        assert_eq!(id.agent_type, "claude");
        assert_eq!(id.key, "sess-1");
        assert_eq!(id.to_string(), "claude/sess-1");
    }

    #[test]
    fn agent_id_from_str_missing_slash() {
        let err = "claude".parse::<AgentId>().unwrap_err();
        assert!(err.to_string().contains("expected format 'type/key'"));
    }

    #[test]
    fn agent_id_from_str_empty_type() {
        let err = "/key".parse::<AgentId>().unwrap_err();
        assert_eq!(err.0, "/key");
    }

    #[test]
    fn agent_id_from_str_empty_key() {
        let err = "type/".parse::<AgentId>().unwrap_err();
        assert_eq!(err.0, "type/");
    }

    #[test]
    fn agent_id_from_str_multiple_slashes_takes_first() {
        let id: AgentId = "claude/a/b".parse().unwrap();
        assert_eq!(id.agent_type, "claude");
        assert_eq!(id.key, "a/b");
    }

    #[test]
    fn agent_id_serde_json_round_trip() {
        let id = AgentId::new("codex", "ws-7");
        let json = serde_json::to_string(&id).unwrap();
        let decoded: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, decoded);
    }

    #[test]
    fn agent_id_equality_and_hash() {
        use std::collections::HashSet;
        let a = AgentId::new("claude", "1");
        let b = AgentId::new("claude", "1");
        let c = AgentId::new("claude", "2");
        assert_eq!(a, b);
        assert_ne!(a, c);
        let mut set = HashSet::new();
        set.insert(a.clone());
        set.insert(b);
        assert_eq!(set.len(), 1);
        set.insert(c);
        assert_eq!(set.len(), 2);
    }

    // ── ClaudeStatus ───────────────────────────────────────────────────

    #[test]
    fn claude_status_default_is_idle() {
        assert_eq!(ClaudeStatus::default(), ClaudeStatus::Idle);
    }

    #[test]
    fn claude_status_serde_snake_case() {
        let json = serde_json::to_string(&ClaudeStatus::ToolUse).unwrap();
        assert_eq!(json, "\"tool_use\"");
        let decoded: ClaudeStatus = serde_json::from_str("\"awaiting_input\"").unwrap();
        assert_eq!(decoded, ClaudeStatus::AwaitingInput);
    }

    #[test]
    fn claude_status_all_variants_round_trip() {
        for status in [
            ClaudeStatus::Idle,
            ClaudeStatus::Processing,
            ClaudeStatus::ToolUse,
            ClaudeStatus::AwaitingInput,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let decoded: ClaudeStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, status);
        }
    }

    // ── SourceClass ────────────────────────────────────────────────────

    #[test]
    fn source_class_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&SourceClass::Harness).unwrap(),
            "\"harness\""
        );
        assert_eq!(
            serde_json::to_string(&SourceClass::Pattern).unwrap(),
            "\"pattern\""
        );
        assert_eq!(
            serde_json::to_string(&SourceClass::Core).unwrap(),
            "\"core\""
        );
    }

    #[test]
    fn source_class_round_trip() {
        for sc in [
            SourceClass::Harness,
            SourceClass::Pattern,
            SourceClass::Core,
        ] {
            let json = serde_json::to_string(&sc).unwrap();
            let decoded: SourceClass = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, sc);
        }
    }

    // ── TerminalEvent ──────────────────────────────────────────────────

    #[test]
    fn terminal_event_json_round_trip() {
        let event = TerminalEvent {
            source_class: SourceClass::Harness,
            source_id: "claude".to_string(),
            kind: "tool_call".to_string(),
            pane_id: Some(42),
            ts_ms: 1_700_000_000_000,
            cursor: 7,
            body: json!({"tool": "Read", "path": "/tmp/foo"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: TerminalEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.source_id, "claude");
        assert_eq!(decoded.pane_id, Some(42));
        assert_eq!(decoded.cursor, 7);
        assert_eq!(decoded.body["tool"], "Read");
    }

    #[test]
    fn terminal_event_none_pane_id_serializes_as_null() {
        let event = TerminalEvent {
            source_class: SourceClass::Core,
            source_id: "shell-integration".to_string(),
            kind: "agent_state".to_string(),
            pane_id: None,
            ts_ms: 0,
            cursor: 1,
            body: json!({}),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"pane_id\":null"));
    }

    #[test]
    fn terminal_event_msgpack_round_trip() {
        let event = TerminalEvent {
            source_class: SourceClass::Pattern,
            source_id: "cargo-errors".to_string(),
            kind: "compile.error".to_string(),
            pane_id: Some(1),
            ts_ms: 12345,
            cursor: 99,
            body: json!({"file": "src/main.rs", "line": 42}),
        };
        let bytes = rmp_serde::to_vec(&event).unwrap();
        let decoded: TerminalEvent = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.source_id, "cargo-errors");
        assert_eq!(decoded.kind, "compile.error");
        assert_eq!(decoded.body["line"], 42);
    }

    // ── TaskState ──────────────────────────────────────────────────────

    #[test]
    fn task_state_serde_round_trip() {
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::InputRequired,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let decoded: TaskState = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, state);
        }
    }

    // ── ParseAgentIdError ──────────────────────────────────────────────

    #[test]
    fn parse_agent_id_error_display() {
        let err = ParseAgentIdError("bad".to_string());
        assert!(err.to_string().contains("bad"));
        assert!(err.to_string().contains("expected format"));
    }

    #[test]
    fn parse_agent_id_error_is_error_trait() {
        let err: Box<dyn std::error::Error> = Box::new(ParseAgentIdError("x".into()));
        assert!(err.to_string().contains("x"));
    }
}
