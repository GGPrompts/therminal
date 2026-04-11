//! `SessionManager`: central registry of all sessions.
//!
//! Owns the session map and provides CRUD + attach/detach operations.
//! Designed to be wrapped in `Arc<tokio::sync::Mutex<SessionManager>>`
//! for sharing across IPC handler tasks.
//!
//! Split into focused submodules:
//! - This file (`mod.rs`) — the `SessionManager` struct definition,
//!   `new`, pattern / OSC / harness / persistence wiring, and the
//!   simple setters/getters that don't touch the session map.
//! - [`crud`] — session CRUD: create / list / iter / get / attach /
//!   destroy / write_to_pane / resize_pane / capture_pane*.
//! - [`split`] — `split_pane[_with_options]`, startup-command injection,
//!   `wait_for_prompt_start`.
//! - [`kill`] — `kill_pane` and the cascade-resize logic on removal.

mod crud;
mod kill;
mod split;

use std::collections::HashMap;
use std::sync::Arc;

use therminal_terminal::TaggedHarnessEvent;
use therminal_terminal::agent_registry::AgentRegistry;
use tokio::sync::broadcast;

use therminal_protocol::daemon::DaemonEvent;
pub use therminal_protocol::{PaneId, SessionId};

use super::base::Session;
use super::pane::PaneDispatchCtx;

/// Central registry of all sessions.
///
/// Owns the session map and provides CRUD + attach/detach operations.
/// Designed to be wrapped in `Arc<tokio::sync::Mutex<SessionManager>>`
/// for sharing across IPC handler tasks.
pub struct SessionManager {
    pub(super) sessions: HashMap<SessionId, Session>,
    pub(super) event_tx: broadcast::Sender<DaemonEvent>,
    /// Default pane dimensions for new sessions. Updated by `CreateSession`
    /// when the GUI passes its viewport size, so subsequent splits inherit
    /// the same dimensions until the next resize.
    pub(crate) default_cols: u16,
    pub(crate) default_rows: u16,
    /// Optional persistence handle for debounced state saving.
    persistence: Option<crate::persistence::PersistenceHandle>,
    /// Central registry of all detected agents across panes.
    pub(super) agent_registry: AgentRegistry,
    /// Per-pane agent capacity cache fed by the Claude state poller.
    pub(super) pane_capacity: Arc<crate::pane_capacity::PaneCapacityCache>,
    /// Pattern engine + event bus + shared match counter (tn-86us).
    /// Installed by `ensure.rs` at startup; cloned into every pane's
    /// `DaemonPtyHandler` so finalized-line and prompt-boundary patterns
    /// run on the reader thread and emit on the unified bus.
    pattern_engine: Option<Arc<therminal_terminal::semantic_patterns::PatternEngine>>,
    pattern_bus: Option<Arc<crate::event_bus::EventBus>>,
    pattern_matches_total: Arc<std::sync::atomic::AtomicU64>,
    /// Shared OSC handler registry (tn-hkpz). Cloned into each new pane's
    /// `TherminalInterceptor` so harness crates can claim OSC codes at
    /// daemon startup and route sequences through typed parsers. Defaults
    /// to an empty registry so unit tests that build a bare
    /// `SessionManager` keep working; `ensure.rs` swaps in the shared
    /// daemon registry via [`SessionManager::set_osc_registry`] before
    /// any PTY is opened.
    pub(super) osc_registry: Arc<therminal_terminal::OscHandlerRegistry>,
    /// Optional sink for `TaggedHarnessEvent`s produced by harness OSC
    /// handlers in each pane's `TherminalInterceptor` (tn-gln6 #1).
    /// Cloned into every new pane at construction time; the receiver lives
    /// in `ensure.rs` and drains into a logger / future event bus.
    /// `None` for unit tests that build a bare `SessionManager`.
    pub(super) harness_event_tx: Option<std::sync::mpsc::Sender<TaggedHarnessEvent>>,
    pub(super) suppress_events: bool,
}

impl SessionManager {
    /// Create a new empty session manager.
    pub fn new(event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self {
            sessions: HashMap::new(),
            event_tx,
            default_cols: 80,
            default_rows: 24,
            persistence: None,
            agent_registry: AgentRegistry::new(),
            pane_capacity: crate::pane_capacity::PaneCapacityCache::shared(),
            osc_registry: Arc::new(therminal_terminal::OscHandlerRegistry::new()),
            harness_event_tx: None,
            pattern_engine: None,
            pattern_bus: None,
            pattern_matches_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            suppress_events: false,
        }
    }

    /// Set event suppression mode (tn-j3ke batch).
    pub fn set_events_suppressed(&mut self, suppressed: bool) {
        self.suppress_events = suppressed;
    }

    /// Broadcast a daemon event, respecting suppression flag.
    pub(crate) fn broadcast_event(&self, event: DaemonEvent) {
        if !self.suppress_events {
            let _ = self.event_tx.send(event);
        }
    }

    /// Get a clone of the event sender for post-batch emission.
    pub fn event_sender(&self) -> broadcast::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    /// Install the pattern engine + event bus on this session manager.
    /// New panes created after this call will receive a cloned handle on
    /// their `DaemonPtyHandler` and dispatch finalized-line / prompt-
    /// boundary patterns onto the bus. See tn-86us.
    pub fn set_pattern_dispatch(
        &mut self,
        engine: Arc<therminal_terminal::semantic_patterns::PatternEngine>,
        bus: Arc<crate::event_bus::EventBus>,
    ) {
        self.pattern_engine = Some(engine);
        self.pattern_bus = Some(bus);
    }

    pub(super) fn pattern_ctx(&self) -> PaneDispatchCtx {
        PaneDispatchCtx {
            engine: self.pattern_engine.clone(),
            bus: self.pattern_bus.clone(),
            matches_total: Arc::clone(&self.pattern_matches_total),
        }
    }

    /// Snapshot of `(total_matches_dispatched, loaded_patterns)` for the
    /// `QueryPatternStats` IPC tool. `loaded_patterns` reads the engine's
    /// own `total_loaded` counter; `total_matches_dispatched` is the
    /// per-daemon counter bumped by every dispatched match (includes
    /// hotspot, widget, and emit_event actions).
    pub fn pattern_stats(&self) -> (u64, usize) {
        let total_matches = self
            .pattern_matches_total
            .load(std::sync::atomic::Ordering::Acquire);
        let total_loaded = self
            .pattern_engine
            .as_ref()
            .map(|e| e.stats().global.total_loaded)
            .unwrap_or(0);
        (total_matches, total_loaded)
    }

    /// Install a shared harness-event sink (tn-gln6 #1).
    ///
    /// Called once from `ensure.rs` at daemon startup. Every pane created
    /// after this call gets the sink cloned into its `TherminalInterceptor`
    /// so OSC 1341 (and any other harness OSC) events reach a daemon-side
    /// consumer instead of being silently dropped.
    pub fn set_harness_event_sink(&mut self, tx: std::sync::mpsc::Sender<TaggedHarnessEvent>) {
        self.harness_event_tx = Some(tx);
    }

    /// Install a shared OSC handler registry (tn-hkpz).
    ///
    /// Called once from `ensure.rs` after harness crates have registered
    /// their handlers but before the first session is created. Every
    /// subsequent pane created by this manager will see the same
    /// registry through its `TherminalInterceptor`.
    pub fn set_osc_registry(&mut self, registry: Arc<therminal_terminal::OscHandlerRegistry>) {
        self.osc_registry = registry;
    }

    /// Return a clone of the shared OSC handler registry handle.
    pub fn osc_registry(&self) -> Arc<therminal_terminal::OscHandlerRegistry> {
        Arc::clone(&self.osc_registry)
    }

    /// Shared handle to the per-pane capacity cache. Cloned by `ensure.rs`
    /// into the Claude state poller bridge task so it can write entries
    /// without holding the session manager mutex.
    pub fn pane_capacity_cache(&self) -> Arc<crate::pane_capacity::PaneCapacityCache> {
        Arc::clone(&self.pane_capacity)
    }

    /// Look up the most recent agent capacity entry for a pane. Returns a
    /// clone of the small DTO; the cache stays locked only briefly.
    pub fn pane_capacity(
        &self,
        pane_id: PaneId,
    ) -> Option<crate::pane_capacity::PaneCapacityEntry> {
        self.pane_capacity.get(pane_id)
    }

    /// Attach a persistence handle for debounced state saving.
    pub fn set_persistence(&mut self, handle: crate::persistence::PersistenceHandle) {
        self.persistence = Some(handle);
    }

    /// Notify the persistence layer that session state has changed.
    pub(super) fn mark_dirty(&self) {
        if let Some(ref handle) = self.persistence {
            handle.mark_dirty();
        }
    }

    /// Subscribe to daemon events via the broadcast channel.
    ///
    /// Returns a new `broadcast::Receiver` that will receive all future
    /// `DaemonEvent`s (including `PaneOutput`). Used by long-running MCP
    /// tools like `wait_for_output` that need to watch the event stream.
    pub fn subscribe_events(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }
}
