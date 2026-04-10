//! Hook-push path for the Claude Code harness.
//!
//! When the daemon runs on Windows native and Claude Code runs inside WSL2,
//! the daemon cannot access `~/.claude/projects/` on the WSL filesystem
//! (different mount namespace). The JSONL tailer falls back gracefully to
//! producing zero events, but that means the daemon loses all tool-state and
//! subagent-lifecycle observability for those sessions.
//!
//! The **hook-push path** solves this by inverting the data flow: Claude Code
//! hook scripts already run inside WSL *where the files live*, so they are in
//! the right place to push signals to the daemon. They do so by calling the
//! `therminal` CLI (or the `tn` wrapper) from hook scripts — the CLI runs
//! inside WSL, resolves the Windows-native daemon's named pipe via `binfmt_misc`,
//! and delivers the signal as a structured `IpcRequest::PushAgentEvent`.
//!
//! ## Signal protocol
//!
//! Each Claude Code hook event that carries session-lifecycle or tool-state
//! information maps to one [`AgentEvent`] variant:
//!
//! | Hook event      | AgentEvent variant     | Key fields pushed          |
//! |-----------------|------------------------|----------------------------|
//! | `SessionStart`  | `SessionStart`         | session_id, project_dir, pid |
//! | `Stop`          | `SessionStop`          | session_id, reason=None    |
//! | `SessionEnd`    | `SessionStop`          | session_id, reason         |
//! | `PreToolUse`    | `ToolState{tool_use}`  | session_id, tool_name, input summary |
//! | `PostToolUse`   | `ToolState{idle}`      | session_id, tool_name      |
//! | `SubagentStart` | `SubagentStart`        | parent_session_id, agent_id, agent_type |
//! | `SubagentStop`  | `SubagentStop`         | parent_session_id, agent_id |
//! | `StopFailure`   | `StopFailure`          | session_id, error_type     |
//!
//! The hook script calls the CLI like this (see `docs/integrations/claude-code.md`
//! for the full hook configuration):
//!
//! ```sh
//! # SessionStart example — pane_id comes from $THERMINAL_PANE_ID set by the PTY
//! echo "$HOOK_JSON" | therminal agent-event push \
//!     --session-id "$CLAUDE_SESSION_ID" \
//!     --event session_start \
//!     --project-dir "$PWD" \
//!     --pid "$$"
//! ```
//!
//! The CLI parses the arguments, constructs a [`HookSignal`], and sends it
//! to the daemon as `IpcRequest::PushAgentEvent { signal }`. The daemon
//! dispatches it to the harness's broadcast channel via
//! [`HookPushSink::inject`].
//!
//! ## Graceful degradation
//!
//! The hook-push path is **additive**. On Linux/WSL-hosted daemons, both the
//! JSONL tailer and the hook-push path run simultaneously. The JSONL tailer is
//! authoritative for historical data and capacity metrics; hook events provide
//! lower-latency lifecycle signals. The broadcast channel accepts events from
//! both sources interleaved; consumers use [`EventSource`] to distinguish them.
//!
//! On Windows native (where JSONL is unreachable), the JSONL path logs a
//! one-time `warn!` and emits no events — hook-push becomes the only source of
//! observability for that session.
//!
//! ## IPC plumbing
//!
//! The daemon holds a [`HookPushSink`] obtained from [`HookPushSink::new`].
//! When `IpcRequest::PushAgentEvent { signal }` arrives the server handler
//! calls `sink.inject(signal)`, which constructs a [`TaggedAgentEvent`] and
//! sends it on the harness broadcast channel. This keeps the hook-push path
//! free of any `SessionManager` or `ClaudeJsonlRegistry` coupling.
//!
//! [`AgentEvent`]: crate::agent_events::AgentEvent
//! [`EventSource`]: crate::jsonl_tailer::EventSource
//! [`TaggedAgentEvent`]: crate::jsonl_tailer::TaggedAgentEvent

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::warn;

use crate::agent_events::AgentEvent;
use crate::jsonl_tailer::{EventSource, TaggedAgentEvent};

// ── Wire type ───────────────────────────────────────────────────────────────

/// A structured signal pushed by a Claude Code hook script.
///
/// This is the wire type carried in `IpcRequest::PushAgentEvent`. It is
/// intentionally flat (all fields `Option<String>`) so that new hook events can
/// be added without a protocol version bump — unknown `event` strings are
/// forwarded as-is with the extra fields in `extra_json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct HookSignal {
    /// Claude Code session UUID (`$CLAUDE_SESSION_ID`).
    pub session_id: String,
    /// Hook event name in snake_case: `session_start`, `session_stop`,
    /// `tool_state`, `subagent_start`, `subagent_stop`, `stop_failure`.
    pub event: String,
    /// For `session_start`: absolute path to the project directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    /// For `session_start`: PID of the Claude Code process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// For `session_stop`: `session_end_reason` from `SessionEnd` hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For `tool_state`: status string (`tool_use` / `idle` / `processing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// For `tool_state`: tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// For `tool_state`: brief serialised summary of the tool's input fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_input_summary: Option<String>,
    /// For `subagent_start` / `subagent_stop`: per-subagent UUID (`agent_id`
    /// field available since Claude Code v2.1.83).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// For `subagent_start`: `agent_type` from hook stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// For `subagent_start` / `subagent_stop`: parent session UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// For `stop_failure`: error type enum string from the `StopFailure` hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
}

// ── Sink ────────────────────────────────────────────────────────────────────

/// A lightweight sink that accepts [`HookSignal`]s from the IPC server and
/// forwards them onto the harness broadcast channel as [`TaggedAgentEvent`]s.
///
/// The daemon constructs one [`HookPushSink`] from the harness event sender and
/// hands it to the server loop, which calls [`inject`] on every
/// `IpcRequest::PushAgentEvent`.
///
/// [`inject`]: HookPushSink::inject
pub struct HookPushSink {
    tx: broadcast::Sender<TaggedAgentEvent>,
}

impl HookPushSink {
    /// Construct a sink backed by the harness broadcast sender. Clone the
    /// sender from [`ClaudeHarness::event_stream`] before the harness facade
    /// is moved into the daemon's background task.
    pub fn new(tx: broadcast::Sender<TaggedAgentEvent>) -> Self {
        Self { tx }
    }

    /// Convert a [`HookSignal`] to a [`TaggedAgentEvent`] and forward it onto
    /// the broadcast channel.
    ///
    /// Unknown `signal.event` strings are silently skipped with a `warn!` log
    /// so that new hook events added in future Claude Code versions do not
    /// surface as errors to the user.
    ///
    /// Returns `true` if the event was forwarded (even if no subscribers were
    /// listening), `false` if the signal was dropped (unknown event type or
    /// missing required field).
    pub fn inject(&self, signal: HookSignal) -> bool {
        let Some((event, source)) = signal_to_event(&signal) else {
            warn!(
                event = %signal.event,
                session_id = %signal.session_id,
                "hook_push: unrecognised or malformed hook signal — skipping"
            );
            return false;
        };

        let tagged = TaggedAgentEvent { event, source };
        // Dropping the result is intentional: if there are no subscribers the
        // event is dropped, which is the same behaviour as the JSONL tailer.
        let _ = self.tx.send(tagged);
        true
    }
}

// ── Conversion ──────────────────────────────────────────────────────────────

/// Convert a [`HookSignal`] to an `(AgentEvent, EventSource)` pair.
///
/// Returns `None` for unknown event kinds or signals missing required fields.
/// The caller logs and drops in that case.
fn signal_to_event(sig: &HookSignal) -> Option<(AgentEvent, EventSource)> {
    let source = if let Some(parent) = &sig.parent_session_id {
        // Subagent-context events carry parent session id.
        let agent_id = sig.agent_id.clone().unwrap_or_default();
        EventSource::Subagent {
            parent_session_id: parent.clone(),
            agent_id,
        }
    } else {
        EventSource::Hook {
            session_id: sig.session_id.clone(),
        }
    };

    let event = match sig.event.as_str() {
        "session_start" => AgentEvent::SessionStart {
            session_id: sig.session_id.clone(),
            project_dir: sig.project_dir.clone().unwrap_or_default(),
            pid: sig.pid.unwrap_or(0),
        },

        "session_stop" => AgentEvent::SessionStop {
            session_id: sig.session_id.clone(),
            reason: sig.reason.clone(),
        },

        "tool_state" => AgentEvent::ToolState {
            session_id: sig.session_id.clone(),
            status: sig.status.clone().unwrap_or_else(|| "idle".to_string()),
            tool_name: sig.tool_name.clone(),
            tool_input_summary: sig.tool_input_summary.clone(),
        },

        "subagent_start" => {
            let parent_session_id = sig.parent_session_id.clone()?;
            let agent_id = sig.agent_id.clone().unwrap_or_default();
            AgentEvent::SubagentStart {
                parent_session_id,
                agent_id,
                agent_type: sig.agent_type.clone(),
            }
        }

        "subagent_stop" => {
            let parent_session_id = sig.parent_session_id.clone()?;
            let agent_id = sig.agent_id.clone().unwrap_or_default();
            AgentEvent::SubagentStop {
                parent_session_id,
                agent_id,
            }
        }

        "stop_failure" => AgentEvent::StopFailure {
            session_id: sig.session_id.clone(),
            error_type: sig
                .error_type
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        },

        _ => return None,
    };

    Some((event, source))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_events::AgentEvent;

    fn make_sink() -> (HookPushSink, broadcast::Receiver<TaggedAgentEvent>) {
        let (tx, rx) = broadcast::channel(16);
        (HookPushSink::new(tx), rx)
    }

    #[test]
    fn session_start_round_trips() {
        let (sink, mut rx) = make_sink();
        let sig = HookSignal {
            session_id: "sess-abc".to_string(),
            event: "session_start".to_string(),
            project_dir: Some("/home/user/myproject".to_string()),
            pid: Some(12345),
            ..Default::default()
        };
        assert!(sink.inject(sig));
        let got = rx.try_recv().expect("event");
        assert!(
            matches!(&got.event, AgentEvent::SessionStart { session_id, .. } if session_id == "sess-abc")
        );
        assert!(
            matches!(&got.source, EventSource::Hook { session_id } if session_id == "sess-abc")
        );
    }

    #[test]
    fn session_stop_with_reason() {
        let (sink, mut rx) = make_sink();
        let sig = HookSignal {
            session_id: "sess-abc".to_string(),
            event: "session_stop".to_string(),
            reason: Some("clear".to_string()),
            ..Default::default()
        };
        assert!(sink.inject(sig));
        let got = rx.try_recv().expect("event");
        assert!(
            matches!(&got.event, AgentEvent::SessionStop { reason: Some(r), .. } if r == "clear")
        );
    }

    #[test]
    fn tool_state_round_trips() {
        let (sink, mut rx) = make_sink();
        let sig = HookSignal {
            session_id: "sess-abc".to_string(),
            event: "tool_state".to_string(),
            status: Some("tool_use".to_string()),
            tool_name: Some("Edit".to_string()),
            tool_input_summary: Some("src/main.rs".to_string()),
            ..Default::default()
        };
        assert!(sink.inject(sig));
        let got = rx.try_recv().expect("event");
        assert!(
            matches!(&got.event, AgentEvent::ToolState { tool_name: Some(t), .. } if t == "Edit")
        );
    }

    #[test]
    fn subagent_start_uses_subagent_source() {
        let (sink, mut rx) = make_sink();
        let sig = HookSignal {
            session_id: "agent-xyz".to_string(),
            event: "subagent_start".to_string(),
            parent_session_id: Some("sess-abc".to_string()),
            agent_id: Some("agent-xyz".to_string()),
            agent_type: Some("subagent".to_string()),
            ..Default::default()
        };
        assert!(sink.inject(sig));
        let got = rx.try_recv().expect("event");
        assert!(matches!(
            &got.source,
            EventSource::Subagent { parent_session_id, agent_id }
                if parent_session_id == "sess-abc" && agent_id == "agent-xyz"
        ));
    }

    #[test]
    fn stop_failure_round_trips() {
        let (sink, mut rx) = make_sink();
        let sig = HookSignal {
            session_id: "sess-abc".to_string(),
            event: "stop_failure".to_string(),
            error_type: Some("rate_limit".to_string()),
            ..Default::default()
        };
        assert!(sink.inject(sig));
        let got = rx.try_recv().expect("event");
        assert!(
            matches!(&got.event, AgentEvent::StopFailure { error_type, .. } if error_type == "rate_limit")
        );
    }

    #[test]
    fn unknown_event_returns_false() {
        let (sink, _rx) = make_sink();
        let sig = HookSignal {
            session_id: "sess-abc".to_string(),
            event: "future_hook_v99".to_string(),
            ..Default::default()
        };
        assert!(!sink.inject(sig));
    }

    #[test]
    fn subagent_start_missing_parent_returns_false() {
        let (sink, _rx) = make_sink();
        let sig = HookSignal {
            session_id: "sess-abc".to_string(),
            event: "subagent_start".to_string(),
            agent_id: Some("agent-xyz".to_string()),
            // parent_session_id intentionally absent
            ..Default::default()
        };
        assert!(!sink.inject(sig));
    }
}
