//! First-class Claude Code integration for Therminal.
//!
//! This crate implements the Claude Code session observability pipeline that
//! was previously embedded in `therminal-daemon`. It tails Claude Code's
//! structured JSONL output under `~/.claude/projects/` and watches the hook
//! state files under `/tmp/claude-code-state/`, and re-broadcasts the resulting
//! [`TaggedAgentEvent`]s over a tokio broadcast channel for MCP resource
//! subscribers (`therminal://claude/events`).
//!
//! ## Architecture
//!
//! The pipeline is composed of four layered modules:
//!
//! ```text
//! /tmp/claude-code-state/*.json      (written by Claude Code hooks)
//!           │
//!           ▼
//! ClaudeStatePoller  (state.rs)
//!   notify-based file watcher → ClaudeSessionState updates
//!   (includes parent_session_id: Option<String> for subagent tracking)
//!           │
//!           ▼
//! ClaudeJsonlRegistry  (jsonl_tailer.rs)
//!   ├─ SessionJsonlTailer per top-level session
//!   │    byte-offset incremental reader over
//!   │    ~/.claude/projects/{hash}/{sid}.jsonl
//!   │
//!   └─ Per-subagent SessionJsonlTailer (discovered by polling
//!      ~/.claude/projects/{hash}/{parent-sid}/subagents/agent-*.jsonl
//!      on each tick, read from offset 0 to capture full lifecycle)
//!           │
//!           ▼ TaggedAgentEvent { event: AgentEvent, source: EventSource }
//!           ▼
//! ClaudePipeline  (pipeline.rs)
//!   150ms tick driving poll_all, tokio::sync::broadcast fan-out
//!           │
//!           ▼
//! therminal://claude/events  (MCP resource, delegated from the daemon)
//! ```
//!
//! ## Public surface
//!
//! The thin [`ClaudeHarness`] facade wraps the pipeline's public entry points
//! so the daemon can start the harness, observe state updates, and obtain the
//! broadcast sender it hands to its MCP resource layer.
//!
//! Modules are also re-exported directly (`pub mod ...`) so downstream code
//! that needs to reach into parsing details (e.g. the `claude-events` dev
//! binary) can still do so.

pub mod agent_events;
pub mod jsonl_tailer;
pub mod pipeline;
pub mod session_log;
pub mod state;

pub use agent_events::AgentEvent;
pub use jsonl_tailer::{ClaudeJsonlRegistry, EventSource, SessionJsonlTailer, TaggedAgentEvent};
pub use pipeline::{BROADCAST_CAPACITY, DEFAULT_POLL_INTERVAL, StateUpdateObserver};
pub use session_log::{SessionEvent, SessionEventType, parse_session_event};
pub use state::{
    ClaudeSessionState, ClaudeStatePoller, ClaudeStateUpdate, ClaudeStatus, ToolArgs, ToolDetails,
};

use std::sync::Arc;

use tokio::sync::{Notify, broadcast};

/// Thin facade around the Claude Code observability pipeline.
///
/// The daemon instantiates one [`ClaudeHarness`] at startup and holds the
/// returned broadcast sender for fan-out to MCP resource subscribers. The
/// harness is move-only: shutdown is driven by the `shutdown` `Notify` passed
/// to [`ClaudeHarness::start`], matching the pre-extraction behaviour of
/// `claude_pipeline::spawn`.
pub struct ClaudeHarness {
    events_tx: Option<broadcast::Sender<TaggedAgentEvent>>,
}

impl ClaudeHarness {
    /// Spawn the tailer + state watcher pipeline using the system default
    /// state directories. Returns a harness whose [`event_stream`] yields a
    /// broadcast sender; clone-and-`subscribe()` from any number of
    /// consumers.
    ///
    /// The `shutdown` notify is used to stop the background tick loop on
    /// daemon shutdown. `observer`, when supplied, is invoked for every
    /// drained [`ClaudeStateUpdate`] *before* it is forwarded to the JSONL
    /// registry — the daemon uses this to populate its per-pane capacity
    /// cache without coupling the pipeline to `SessionManager`.
    ///
    /// Returns a harness whose [`event_stream`] is `None` if the poller
    /// cannot be constructed (e.g. notify watcher init failure on a
    /// stripped-down container). Callers should log and continue; the
    /// `therminal://claude/events` MCP resource will simply produce zero
    /// events.
    ///
    /// [`event_stream`]: ClaudeHarness::event_stream
    pub fn start(shutdown: Arc<Notify>, observer: Option<StateUpdateObserver>) -> Self {
        let events_tx = pipeline::spawn(shutdown, observer);
        Self { events_tx }
    }

    /// Access the broadcast sender for the pipeline's event stream. Callers
    /// clone this sender and hand it to MCP resource subscribers, who each
    /// call `.subscribe()` to obtain a per-connection receiver.
    ///
    /// Returns `None` if the pipeline failed to start (see [`start`]).
    ///
    /// [`start`]: ClaudeHarness::start
    pub fn event_stream(&self) -> Option<&broadcast::Sender<TaggedAgentEvent>> {
        self.events_tx.as_ref()
    }

    /// Consume the harness and return the owned broadcast sender, if any.
    /// Used by the daemon to hand the sender to the MCP server task without
    /// keeping the harness struct alive (the background tick loop is already
    /// detached via `tokio::spawn`).
    pub fn into_event_stream(self) -> Option<broadcast::Sender<TaggedAgentEvent>> {
        self.events_tx
    }
}
