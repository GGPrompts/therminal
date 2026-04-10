//! `SessionManager`: central registry of all sessions.
//!
//! Owns the session map and provides CRUD + attach/detach operations.
//! Designed to be wrapped in `Arc<tokio::sync::Mutex<SessionManager>>`
//! for sharing across IPC handler tasks.

use std::collections::HashMap;
#[cfg(any(unix, test))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use therminal_terminal::TaggedHarnessEvent;
use therminal_terminal::agent_registry::{AgentEntry, AgentRegistry, AgentStatus};
#[cfg(test)]
use therminal_terminal::event_log::EventLog;
use therminal_terminal::event_log::StoredEvent;
use therminal_terminal::osc633::CommandBlock;
#[cfg(test)]
use therminal_terminal::osc633::CommandTracker;
use therminal_terminal::region_index::RegionIndex;
use therminal_terminal::state_inference::{AgentCadenceSnapshot, AgentDetailsSnapshot};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use therminal_protocol::daemon::{DaemonEvent, LayoutSnapshot, WorkspaceInfo};
pub use therminal_protocol::{PaneId, SessionId, WorkspaceId};

use super::base::Session;
use super::layout::{
    STARTUP_COMMAND_FALLBACK, STARTUP_COMMAND_POLL_INTERVAL, append_layout_leaf, layout_leaf_dims,
    normalize_startup_command, reconstruct_layout_rect, remove_layout_leaf, split_layout_leaf,
    swap_layout_leaves,
};
use super::pane::{Pane, PaneDispatchCtx};
use super::snapshots::{PaneSnapshot, SessionSnapshot};
#[cfg(any(unix, test))]
use super::window::Window;
#[cfg(any(unix, test))]
use super::{NEXT_PANE_ID, NEXT_SESSION_ID};

/// Central registry of all sessions.
///
/// Owns the session map and provides CRUD + attach/detach operations.
/// Designed to be wrapped in `Arc<tokio::sync::Mutex<SessionManager>>`
/// for sharing across IPC handler tasks.
pub struct SessionManager {
    pub(super) sessions: HashMap<SessionId, Session>,
    event_tx: broadcast::Sender<DaemonEvent>,
    /// Default pane dimensions for new sessions. Updated by `CreateSession`
    /// when the GUI passes its viewport size, so subsequent splits inherit
    /// the same dimensions until the next resize.
    pub(crate) default_cols: u16,
    pub(crate) default_rows: u16,
    /// Optional persistence handle for debounced state saving.
    persistence: Option<crate::persistence::PersistenceHandle>,
    /// Central registry of all detected agents across panes.
    agent_registry: AgentRegistry,
    /// Per-pane agent capacity cache fed by the Claude state poller.
    pane_capacity: Arc<crate::pane_capacity::PaneCapacityCache>,
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
    osc_registry: Arc<therminal_terminal::OscHandlerRegistry>,
    /// Optional sink for `TaggedHarnessEvent`s produced by harness OSC
    /// handlers in each pane's `TherminalInterceptor` (tn-gln6 #1).
    /// Cloned into every new pane at construction time; the receiver lives
    /// in `ensure.rs` and drains into a logger / future event bus.
    /// `None` for unit tests that build a bare `SessionManager`.
    harness_event_tx: Option<std::sync::mpsc::Sender<TaggedHarnessEvent>>,
    suppress_events: bool,
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

    fn pattern_ctx(&self) -> PaneDispatchCtx {
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
    fn mark_dirty(&self) {
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

    /// Create a new session with a default window/pane.
    pub fn create_session(
        &mut self,
        name: Option<String>,
    ) -> Result<SessionId, therminal_terminal::pty::PtyError> {
        self.create_session_with_options(name, &therminal_terminal::pty::SpawnOptions::default())
    }

    /// Create a new session with a default window/pane and custom spawn options.
    pub fn create_session_with_options(
        &mut self,
        name: Option<String>,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
    ) -> Result<SessionId, therminal_terminal::pty::PtyError> {
        let mut session = Session::new(name, self.event_tx.clone());
        let default_pane_id = session
            .create_default_pane(
                self.default_cols,
                self.default_rows,
                spawn_options,
                Arc::clone(&self.osc_registry),
                self.harness_event_tx.clone(),
                self.pattern_ctx(),
            )?
            .id;

        // Seed workspace_state with a single default workspace containing the
        // newly-spawned pane. Without this, GetWorkspaces on a fresh session
        // returns an empty vec, which broke the GUI attach flow in tn-ytw2
        // (remote_spawn.rs couldn't discover the initial pane id).
        session.workspace_state = vec![WorkspaceInfo {
            id: 1,
            name: "1".to_string(),
            order: 0,
            pane_ids: vec![default_pane_id],
            focused_pane: Some(default_pane_id),
            layout: Some(LayoutSnapshot::Leaf {
                pane_id: default_pane_id,
            }),
        }];
        session.active_workspace = 1;

        let session_id = session.id;
        info!(session_id = session_id, "session created");

        // Broadcast creation event
        let _ = self
            .event_tx
            .send(DaemonEvent::SessionCreated { session_id });

        self.sessions.insert(session_id, session);
        self.mark_dirty();
        Ok(session_id)
    }

    /// Iterate over all sessions.
    pub fn iter_sessions(&self) -> impl Iterator<Item = (&SessionId, &Session)> {
        self.sessions.iter()
    }

    /// List all session IDs.
    pub fn list_sessions(&self) -> Vec<SessionId> {
        self.sessions.keys().copied().collect()
    }

    /// Get session info (id, name, created_at).
    pub fn get_session_info(
        &self,
        session_id: SessionId,
    ) -> Option<(SessionId, Option<String>, u64)> {
        self.sessions
            .get(&session_id)
            .map(|s| (s.id, s.name.clone(), s.created_at_secs))
    }

    /// Attach to a session: returns a snapshot of the current terminal state.
    pub fn attach(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        self.sessions.get(&session_id).map(|s| s.snapshot())
    }

    /// Write input data to a specific pane in a session.
    pub fn write_to_pane(
        &mut self,
        session_id: SessionId,
        pane_id: PaneId,
        data: &[u8],
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let pane = session
            .find_pane_mut(pane_id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;
        pane.write(data).map_err(|e| format!("write error: {e}"))
    }

    /// Destroy a session and all its panes.
    pub fn destroy_session(&mut self, session_id: SessionId) -> bool {
        if let Some(session) = self.sessions.remove(&session_id) {
            // Unregister all agents from panes in this session.
            for window in &session.windows {
                for pane in &window.panes {
                    self.agent_registry.unregister(pane.id);
                }
            }
            info!(session_id = session_id, "session destroyed");
            let _ = self
                .event_tx
                .send(DaemonEvent::SessionDestroyed { session_id });
            self.mark_dirty();
            true
        } else {
            false
        }
    }

    /// Number of active sessions.
    pub fn session_count(&self) -> u32 {
        self.sessions.len() as u32
    }

    /// Send keys to a pane by pane ID (searches all sessions).
    pub fn send_keys_to_pane(&mut self, pane_id: PaneId, keys: &[u8]) -> Result<(), String> {
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                return pane.write(keys).map_err(|e| format!("write error: {e}"));
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Resize a pane's PTY by pane ID (searches all sessions).
    ///
    /// tn-ju04: also broadcasts `DaemonEvent::PaneResized` so CLI /
    /// subscription watchers re-read geometry whenever the GUI (or MCP)
    /// drives a resize.
    pub fn resize_pane(&mut self, pane_id: PaneId, cols: u16, rows: u16) -> Result<(), String> {
        let mut found_session: Option<SessionId> = None;
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                pane.resize(cols, rows);
                found_session = Some(session.id);
                break;
            }
        }
        match found_session {
            Some(session_id) => {
                self.broadcast_event(DaemonEvent::PaneResized {
                    session_id,
                    pane_id,
                    cols,
                    rows,
                });
                Ok(())
            }
            None => Err(format!("pane not found: {pane_id}")),
        }
    }

    /// Capture structured pane state (mode flags, cursor, visible grid)
    /// for tn-zamd replay on attach. See `Pane::snapshot_state`.
    pub fn capture_pane_state(
        &self,
        pane_id: PaneId,
    ) -> Result<therminal_protocol::daemon::PaneStateSnapshot, String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Ok(pane.snapshot_state());
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Capture pane content by pane ID (searches all sessions).
    pub fn capture_pane(&self, pane_id: PaneId) -> Result<PaneSnapshot, String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Ok(pane.snapshot());
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Test-only: get the shared command tracker `Arc` for a pane so
    /// tests can inject OSC 633 marks bypassing the PTY reader thread.
    #[cfg(test)]
    pub fn pane_command_tracker_arc(&self, pane_id: PaneId) -> Option<Arc<Mutex<CommandTracker>>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.command_tracker_arc());
                }
            }
        }
        None
    }

    /// Snapshot a pane's OSC 633 command tracker by pane ID. Returns
    /// `None` if the pane does not exist.
    pub fn pane_command_blocks(&self, pane_id: PaneId) -> Option<Vec<CommandBlock>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.command_tracker_snapshot());
                }
            }
        }
        None
    }

    /// Snapshot a pane's in-memory event log by pane ID. Returns `None`
    /// if the pane does not exist; otherwise the (possibly empty) list of
    /// recent events filtered by the optional `since_timestamp_secs` and
    /// capped at `limit`.
    pub fn pane_event_log_snapshot(
        &self,
        pane_id: PaneId,
        since_timestamp_secs: Option<u64>,
        limit: usize,
    ) -> Option<Vec<StoredEvent>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.event_log_snapshot(since_timestamp_secs, limit));
                }
            }
        }
        None
    }

    /// Test-only: shared event log Arc for a pane.
    #[cfg(test)]
    pub fn pane_event_log_arc(&self, pane_id: PaneId) -> Option<Arc<Mutex<EventLog>>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.event_log_arc());
                }
            }
        }
        None
    }

    /// Snapshot a pane's agent inference state by pane ID. Returns `None`
    /// if the pane does not exist.
    pub fn pane_agent_details(&self, pane_id: PaneId) -> Option<AgentDetailsSnapshot> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.agent_details_snapshot());
                }
            }
        }
        None
    }

    /// Snapshot a pane's output cadence window by pane ID. Returns `None`
    /// if the pane does not exist. The DTO is plain owned data with sample
    /// timestamps already converted to wall-clock Unix seconds, so the
    /// caller can serialise it after the session-manager lock is released.
    pub fn pane_agent_cadence(&self, pane_id: PaneId) -> Option<AgentCadenceSnapshot> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.agent_cadence_snapshot());
                }
            }
        }
        None
    }

    /// Access a pane's region index by pane ID (searches all sessions).
    pub fn pane_region_index(&self, pane_id: PaneId) -> Result<Arc<Mutex<RegionIndex>>, String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Ok(Arc::clone(pane.region_index()));
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Split a pane: creates a new sibling pane in the same window.
    /// Returns the new pane's ID. `horizontal=true` splits cols
    /// (side-by-side), `horizontal=false` splits rows (stacked).
    pub fn split_pane(&mut self, pane_id: PaneId, horizontal: bool) -> Result<PaneId, String> {
        self.split_pane_with_options(pane_id, horizontal, &Default::default(), None, None)
    }

    /// Split a pane with custom spawn options for the new pane's PTY.
    ///
    /// tn-ju04: after creating the new pane, this method also
    ///
    /// 1. Halves the source pane's current dimensions along the split
    ///    axis and spawns the new pane at that size (instead of the
    ///    stale `default_cols`/`default_rows` constants).
    /// 2. Resizes the source pane's PTY + `Term` to the halved size.
    /// 3. Updates the stored `workspace_state.layout` so MCP consumers
    ///    (and the GUI on next attach) see the new leaf.
    /// 4. Broadcasts `DaemonEvent::PaneResized` for both affected panes
    ///    so subscribed clients re-read geometry.
    ///
    /// The GUI still publishes a fresh `SetWorkspaceState` after every
    /// split it drives, which overwrites the layout tree we compute
    /// here — that is fine. This path is what keeps CLI / MCP driven
    /// splits sane (and what prevents TUIs from drawing past their
    /// render area immediately after a GUI split, before the GUI's
    /// follow-up `ResizePane` lands).
    pub fn split_pane_with_options(
        &mut self,
        pane_id: PaneId,
        horizontal: bool,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
        startup_command: Option<&str>,
        ratio: Option<f32>,
    ) -> Result<PaneId, String> {
        use therminal_protocol::daemon::LayoutSplitDirection;

        let pattern_ctx = self.pattern_ctx();

        // Find which session and window this pane belongs to.
        let session_id = self
            .sessions
            .values()
            .find(|s| {
                s.windows
                    .iter()
                    .any(|w| w.panes.iter().any(|p| p.id == pane_id))
            })
            .map(|s| s.id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;

        // F9 (tn-97j6): a concurrent DestroySession + SplitPane race could
        // remove the session/window between the find above and these
        // lookups. Return a soft error instead of panicking the daemon task.
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "session/window disappeared under concurrent request".to_string())?;
        let window = session
            .windows
            .iter_mut()
            .find(|w| w.panes.iter().any(|p| p.id == pane_id))
            .ok_or_else(|| "session/window disappeared under concurrent request".to_string())?;

        // tn-ju04: halve the source pane's current dimensions along the
        // split axis so both children inherit roughly half the parent's
        // cells. One cell is reserved for the visual separator gap the
        // GUI draws between siblings, keeping the daemon's arithmetic in
        // step with `layout_leaf_dims`.
        let (src_cols, src_rows) = {
            let src = window
                .panes
                .iter()
                .find(|p| p.id == pane_id)
                .ok_or_else(|| format!("pane not found: {pane_id}"))?;
            (src.cols(), src.rows())
        };
        // Clamp ratio to [0.1, 0.9] to prevent degenerate layouts.
        // Guard against NaN/Inf before clamping — non-finite values would
        // propagate through the arithmetic and corrupt column/row counts.
        let r = ratio.unwrap_or(0.5);
        let r = if r.is_finite() { r } else { 0.5 };
        let r = r.clamp(0.1, 0.9);
        let (first_cols, first_rows, second_cols, second_rows) = if horizontal {
            let usable = src_cols.saturating_sub(1);
            let first = ((usable as f32 * r).round() as u16).max(1);
            let second = usable.saturating_sub(first).max(1);
            (first, src_rows, second, src_rows)
        } else {
            let usable = src_rows.saturating_sub(1);
            let first = ((usable as f32 * r).round() as u16).max(1);
            let second = usable.saturating_sub(first).max(1);
            (src_cols, first, src_cols, second)
        };

        let new_pane = Pane::spawn(
            second_cols,
            second_rows,
            self.event_tx.clone(),
            session_id,
            spawn_options,
            Arc::clone(&self.osc_registry),
            self.harness_event_tx.clone(),
            pattern_ctx,
        )
        .map_err(|e| format!("failed to spawn pane: {e}"))?;

        let new_id = new_pane.id;
        window.add_pane(new_pane);

        // Resize the source pane to its post-split halved geometry. The
        // new pane is already sized via `Pane::spawn` above. Broadcast
        // PaneResized for both so watchers re-read.
        if let Some(src) = window.panes.iter_mut().find(|p| p.id == pane_id)
            && (src.cols() != first_cols || src.rows() != first_rows)
        {
            src.resize(first_cols, first_rows);
        }
        self.broadcast_event(DaemonEvent::PaneResized {
            session_id,
            pane_id,
            cols: first_cols,
            rows: first_rows,
        });
        self.broadcast_event(DaemonEvent::PaneResized {
            session_id,
            pane_id: new_id,
            cols: second_cols,
            rows: second_rows,
        });

        // tn-ju04: reflect the new leaf in the stored workspace layout
        // so MCP `terminal.workspaces.get_layout` and CLI split paths
        // agree with `terminal.panes.list`. The GUI's next
        // `SetWorkspaceState` publish overwrites this, so we only need a
        // best-effort patch that keeps things consistent in the meantime.
        let direction = if horizontal {
            LayoutSplitDirection::Horizontal
        } else {
            LayoutSplitDirection::Vertical
        };
        // Re-borrow session as immutable-through-mutable; `window` went
        // out of scope above along with the mutable borrow of `session`.
        if let Some(session) = self.sessions.get_mut(&session_id) {
            // Find the workspace currently containing `pane_id`, or fall
            // back to the active workspace if the layout tree is
            // missing / stale. We prefer the workspace whose layout
            // actually references the source so concurrent workspaces
            // don't have unrelated layouts clobbered.
            let target_idx = session
                .workspace_state
                .iter()
                .position(|ws| ws.pane_ids.contains(&pane_id));
            if let Some(idx) = target_idx {
                let ws = &mut session.workspace_state[idx];
                if !ws.pane_ids.contains(&new_id) {
                    ws.pane_ids.push(new_id);
                }
                ws.layout = Some(match ws.layout.take() {
                    Some(existing) => split_layout_leaf(existing, pane_id, new_id, direction, r),
                    None => LayoutSnapshot::Split {
                        direction,
                        ratio: r,
                        first: Box::new(LayoutSnapshot::Leaf { pane_id }),
                        second: Box::new(LayoutSnapshot::Leaf { pane_id: new_id }),
                    },
                });
                ws.focused_pane = Some(new_id);
                let active_workspace = session.active_workspace;
                self.broadcast_event(DaemonEvent::WorkspaceChanged {
                    session_id,
                    active_workspace,
                });
            }
        }

        self.maybe_send_startup_command(new_id, startup_command)?;

        self.mark_dirty();
        Ok(new_id)
    }

    pub fn maybe_send_startup_command(
        &mut self,
        pane_id: PaneId,
        startup_command: Option<&str>,
    ) -> Result<(), String> {
        let Some(startup_bytes) = normalize_startup_command(startup_command) else {
            return Ok(());
        };

        let saw_prompt = self.wait_for_prompt_start(pane_id, STARTUP_COMMAND_FALLBACK);
        if !saw_prompt {
            debug!(
                pane_id,
                fallback_ms = STARTUP_COMMAND_FALLBACK.as_millis(),
                "startup_command prompt wait timed out; using fallback"
            );
        }

        self.send_keys_to_pane(pane_id, &startup_bytes)
    }

    fn wait_for_prompt_start(&self, pane_id: PaneId, timeout: std::time::Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .sessions
                .values()
                .flat_map(|session| session.windows.iter())
                .find_map(|window| window.pane(pane_id))
                .is_some_and(Pane::has_seen_prompt_start)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(STARTUP_COMMAND_POLL_INTERVAL);
        }
    }

    /// Kill (destroy) a single pane by ID. Removes it from its window.
    /// If the window becomes empty, removes the window. If the session
    /// becomes empty, destroys the session.
    ///
    /// tn-ju04: after removal, any siblings left behind in the stored
    /// `workspace_state.layout` are resized up to reclaim the dead
    /// pane's cells. For each surviving pane whose dimensions changed,
    /// the PTY + `Term` are resized and a `PaneResized` event is
    /// broadcast. Without this cascade, killing a pane via MCP / CLI
    /// leaves TUIs in sibling panes still believing they have the
    /// pre-kill cell count.
    pub fn kill_pane(&mut self, pane_id: PaneId) -> Result<(), String> {
        // Unregister any agent tracked for this pane.
        self.agent_registry.unregister(pane_id);

        let session_id = self
            .sessions
            .values()
            .find(|s| {
                s.windows
                    .iter()
                    .any(|w| w.panes.iter().any(|p| p.id == pane_id))
            })
            .map(|s| s.id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;

        // tn-ju04: before mutating state, capture the parent rect of the
        // layout subtree that owns `pane_id` so siblings can be resized
        // after removal. We sum the current cell dimensions of every
        // leaf the layout references as a best-effort reconstruction of
        // the total window size — the daemon has no direct notion of
        // window pixels, but the existing per-pane (cols, rows) plus
        // the layout ratios are enough to cascade up.
        let (cascade_dims, affected_workspace) = {
            let session = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| format!("session vanished: {session_id}"))?;
            let ws_with_layout = session
                .workspace_state
                .iter()
                .enumerate()
                .find(|(_, ws)| ws.pane_ids.contains(&pane_id));
            match ws_with_layout {
                Some((idx, ws)) => {
                    // Reconstruct the parent rect from live pane sizes.
                    // For a single-root leaf, the parent rect is that
                    // leaf's own size. For a split, we sum along the
                    // split axis and take the max along the orthogonal
                    // axis. This is invariant to ratio drift.
                    let parent = ws.layout.as_ref().map(|layout| {
                        reconstruct_layout_rect(layout, |id| {
                            for w in &session.windows {
                                if let Some(p) = w.pane(id) {
                                    return Some((p.cols(), p.rows()));
                                }
                            }
                            None
                        })
                    });
                    (parent.flatten().map(|rect| (idx, rect)), Some(idx))
                }
                None => (None, None),
            }
        };

        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session vanished: {session_id}"))?;
        for window in &mut session.windows {
            if let Some(pos) = window.panes.iter().position(|p| p.id == pane_id) {
                window.panes.remove(pos);
                break;
            }
        }
        // Remove empty windows
        session.windows.retain(|w| !w.panes.is_empty());

        // tn-ju04: patch the stored layout so the dead leaf is gone
        // before we cascade sizes. If `workspace_state` has no layout
        // for this pane the patch is a no-op; the GUI will resync on
        // its next `SetWorkspaceState`.
        if let Some(idx) = affected_workspace {
            let ws = &mut session.workspace_state[idx];
            ws.pane_ids.retain(|id| *id != pane_id);
            if let Some(layout) = ws.layout.take() {
                ws.layout = remove_layout_leaf(layout, pane_id);
            }
            if ws.focused_pane == Some(pane_id) {
                ws.focused_pane = ws.pane_ids.first().copied();
            }
        }

        // tn-ju04: cascade resizes across the surviving leaves of the
        // affected workspace's layout. `cascade_dims` holds the pre-kill
        // parent rect we computed above; now we re-walk the patched
        // layout to produce the post-kill dims.
        let mut resize_events: Vec<(PaneId, u16, u16)> = Vec::new();
        if let Some((idx, (parent_cols, parent_rows))) = cascade_dims
            && let Some(ws) = session.workspace_state.get(idx).cloned()
            && let Some(layout) = ws.layout.as_ref()
        {
            let leaves = layout_leaf_dims(layout, parent_cols, parent_rows);
            for leaf in leaves {
                let Some(pane) = session
                    .windows
                    .iter_mut()
                    .flat_map(|w| w.panes.iter_mut())
                    .find(|p| p.id == leaf.pane_id)
                else {
                    continue;
                };
                if pane.cols() != leaf.cols || pane.rows() != leaf.rows {
                    pane.resize(leaf.cols, leaf.rows);
                    resize_events.push((leaf.pane_id, leaf.cols, leaf.rows));
                }
            }
        }

        // If no windows left, destroy session (which also marks dirty)
        if session.windows.is_empty() {
            self.destroy_session(session_id);
        } else {
            // Broadcast WorkspaceChanged so MCP layout queries re-read.
            let active_workspace = session.active_workspace;
            self.broadcast_event(DaemonEvent::WorkspaceChanged {
                session_id,
                active_workspace,
            });
            self.mark_dirty();
        }

        for (pid, cols, rows) in resize_events {
            self.broadcast_event(DaemonEvent::PaneResized {
                session_id,
                pane_id: pid,
                cols,
                rows,
            });
        }

        Ok(())
    }

    /// Select (focus) a pane. Currently a no-op since the daemon is headless,
    /// but validates the pane exists and can be extended with focus tracking.
    pub fn select_pane(&self, pane_id: PaneId) -> Result<(), String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if window.pane(pane_id).is_some() {
                    return Ok(());
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Swap two panes' positions in the layout tree of their session.
    ///
    /// Both panes must currently belong to the same session — cross-session
    /// swaps are not expressible in the wire protocol and are rejected here.
    /// Updates `WorkspaceInfo::pane_ids` ordering and rewrites any
    /// `LayoutSnapshot::Leaf` nodes referencing either pane within all of
    /// the session's workspaces, so a follow-up `set_workspace_state` from
    /// the GUI will be a no-op.
    pub fn swap_panes(&mut self, a: PaneId, b: PaneId) -> Result<(), String> {
        if a == b {
            return Ok(());
        }
        let session_a = self
            .session_for_pane(a)
            .ok_or_else(|| format!("pane not found: {a}"))?;
        let session_b = self
            .session_for_pane(b)
            .ok_or_else(|| format!("pane not found: {b}"))?;
        if session_a != session_b {
            return Err(format!(
                "cross-session swap not supported: pane {a} in session {session_a}, pane {b} in session {session_b}"
            ));
        }

        let session = self
            .sessions
            .get_mut(&session_a)
            .ok_or_else(|| "session disappeared under concurrent request".to_string())?;

        for ws in session.workspace_state.iter_mut() {
            for pid in ws.pane_ids.iter_mut() {
                if *pid == a {
                    *pid = b;
                } else if *pid == b {
                    *pid = a;
                }
            }
            if let Some(layout) = ws.layout.as_mut() {
                swap_layout_leaves(layout, a, b);
            }
        }

        self.mark_dirty();
        Ok(())
    }

    /// Move a pane between workspaces inside its containing session
    /// (tn-fi1k). Metadata-only: the underlying PTY is not touched.
    ///
    /// Returns `(source_workspace_id, target_workspace_id)` on success.
    /// Errors if the pane does not exist anywhere in any session, or if
    /// it is somehow not present in any workspace's `pane_ids` (a corrupt
    /// state that should be loud).
    ///
    /// If the target workspace doesn't exist yet, it is created as a
    /// fresh single-pane workspace whose layout is just `Leaf { pane_id }`.
    ///
    /// If the move is a no-op (target == source), it succeeds with the
    /// source as both source and target.
    pub fn move_pane(
        &mut self,
        pane_id: PaneId,
        target_workspace_id: WorkspaceId,
    ) -> Result<(WorkspaceId, WorkspaceId), String> {
        let session_id = self
            .session_for_pane(pane_id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;

        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "session disappeared under concurrent request".to_string())?;

        // 1. Find the workspace currently owning the pane.
        let source_idx = session
            .workspace_state
            .iter()
            .position(|ws| ws.pane_ids.contains(&pane_id))
            .ok_or_else(|| {
                format!(
                    "pane {pane_id} exists in session {session_id} but is not bound to any workspace"
                )
            })?;
        let source_workspace_id = session.workspace_state[source_idx].id;

        if source_workspace_id == target_workspace_id {
            // No-op move: nothing to do, but report it as success so callers
            // can keep the local <-> daemon state mirror in sync without
            // special-casing.
            return Ok((source_workspace_id, target_workspace_id));
        }

        // 2. Remove the pane from the source workspace's pane_ids and layout.
        {
            let src = &mut session.workspace_state[source_idx];
            src.pane_ids.retain(|p| *p != pane_id);
            if src.focused_pane == Some(pane_id) {
                src.focused_pane = src.pane_ids.first().copied();
            }
            if let Some(layout) = src.layout.as_mut() {
                let new_layout = remove_layout_leaf(layout.clone(), pane_id);
                src.layout = new_layout;
            }
        }

        // 3. Add the pane to the target workspace, creating it if missing.
        let target_idx_opt = session
            .workspace_state
            .iter()
            .position(|ws| ws.id == target_workspace_id);
        match target_idx_opt {
            Some(idx) => {
                let target = &mut session.workspace_state[idx];
                if !target.pane_ids.contains(&pane_id) {
                    target.pane_ids.push(pane_id);
                }
                target.layout = Some(append_layout_leaf(target.layout.take(), pane_id));
            }
            None => {
                // Create a fresh workspace tab for the target id with the
                // moved pane as its only leaf.
                let next_order = session
                    .workspace_state
                    .iter()
                    .map(|w| w.order)
                    .max()
                    .map(|m| m + 1)
                    .unwrap_or(0);
                session.workspace_state.push(WorkspaceInfo {
                    id: target_workspace_id,
                    name: target_workspace_id.to_string(),
                    order: next_order,
                    pane_ids: vec![pane_id],
                    focused_pane: Some(pane_id),
                    layout: Some(LayoutSnapshot::Leaf { pane_id }),
                });
            }
        }

        self.mark_dirty();
        Ok((source_workspace_id, target_workspace_id))
    }

    /// Merge opaque key/value tags into a pane (tn-bbvf). Returns the
    /// resulting full tag set on success.
    pub fn tag_pane(
        &mut self,
        pane_id: PaneId,
        tags: HashMap<String, String>,
    ) -> Result<HashMap<String, String>, String> {
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                pane.merge_tags(tags);
                let snap = pane.tags();
                self.mark_dirty();
                return Ok(snap);
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Remove tags from a pane. `keys = None` clears all tags. Returns
    /// the remaining tag set.
    pub fn untag_pane(
        &mut self,
        pane_id: PaneId,
        keys: Option<Vec<String>>,
    ) -> Result<HashMap<String, String>, String> {
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                match keys {
                    Some(ref ks) => pane.remove_tag_keys(ks),
                    None => pane.clear_tags(),
                }
                let snap = pane.tags();
                self.mark_dirty();
                return Ok(snap);
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Snapshot a pane's tags by ID. `None` if the pane does not exist.
    pub fn pane_tags(&self, pane_id: PaneId) -> Option<HashMap<String, String>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.tags());
                }
            }
        }
        None
    }

    /// Find the session ID that contains a given pane.
    pub fn session_for_pane(&self, pane_id: PaneId) -> Option<SessionId> {
        self.sessions
            .values()
            .find(|s| {
                s.windows
                    .iter()
                    .any(|w| w.panes.iter().any(|p| p.id == pane_id))
            })
            .map(|s| s.id)
    }

    /// Set the workspace topology for a session.
    ///
    /// The app calls this whenever workspace state changes (switch, create,
    /// rename, pane move). The daemon stores it as the source of truth so
    /// MCP tools and reattaching clients can query it.
    pub fn set_workspace_state(
        &mut self,
        session_id: SessionId,
        workspaces: Vec<WorkspaceInfo>,
        active_workspace: WorkspaceId,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        session.workspace_state = workspaces;
        session.active_workspace = active_workspace;
        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace,
        });
        self.mark_dirty();
        Ok(())
    }

    /// Switch the active workspace for a session without touching the
    /// stored topology (tn-8ysl). Validates that the requested workspace
    /// exists in the session's `workspace_state` (or is `1` for legacy
    /// sessions that haven't populated their workspace state yet), then
    /// updates `active_workspace` and broadcasts `WorkspaceChanged`.
    pub fn set_active_workspace(
        &mut self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let exists = session.workspace_state.iter().any(|w| w.id == workspace_id)
            || (session.workspace_state.is_empty() && workspace_id == 1);
        if !exists {
            return Err(format!(
                "workspace {workspace_id} not found in session {session_id}"
            ));
        }
        session.active_workspace = workspace_id;
        // no subscribers is normal — events broadcast to whatever clients are attached
        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace: workspace_id,
        });
        self.mark_dirty();
        Ok(())
    }

    /// Get the workspace topology for a session.
    pub fn get_workspace_state(
        &self,
        session_id: SessionId,
    ) -> Result<(Vec<WorkspaceInfo>, WorkspaceId), String> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        Ok((session.workspace_state.clone(), session.active_workspace))
    }

    /// Create a new empty workspace in a session (tn-ceqw).
    ///
    /// Picks the lowest unused workspace ID in 1..=9, appends a
    /// `WorkspaceInfo` entry, sets the new workspace as active, and
    /// broadcasts `WorkspaceChanged`.
    pub fn create_workspace(
        &mut self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<WorkspaceId, String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;

        // Pick the lowest unused workspace slot in 1..=9.
        let used: std::collections::HashSet<WorkspaceId> =
            session.workspace_state.iter().map(|w| w.id).collect();
        let new_id = (1..=9u64)
            .find(|id| !used.contains(id))
            .ok_or_else(|| "all workspace slots 1-9 are occupied".to_string())?;

        let ws_name = name.unwrap_or_else(|| format!("Workspace {new_id}"));
        session.workspace_state.push(WorkspaceInfo {
            id: new_id,
            name: ws_name,
            order: new_id as u32,
            pane_ids: vec![],
            focused_pane: None,
            layout: None,
        });
        session.active_workspace = new_id;

        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace: new_id,
        });
        self.mark_dirty();
        Ok(new_id)
    }

    /// Rename an existing workspace (tn-ceqw).
    pub fn rename_workspace(
        &mut self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        name: String,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let ws = session
            .workspace_state
            .iter_mut()
            .find(|w| w.id == workspace_id)
            .ok_or_else(|| format!("workspace {workspace_id} not found in session {session_id}"))?;
        ws.name = name;
        let active_workspace = session.active_workspace;
        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace,
        });
        self.mark_dirty();
        Ok(())
    }

    /// Return the ID of the first (default) session, if any.
    pub fn default_session_id(&self) -> Option<SessionId> {
        self.sessions.keys().next().copied()
    }

    /// Snapshot of `(pane_id, shell_pid)` pairs for every live pane across
    /// all sessions. Returned as plain owned values so callers (notably
    /// the daemon-side process-detector ticker — tn-pehl) can drop the
    /// `SessionManager` lock before performing the scan.
    pub fn pane_shell_pids(&self) -> Vec<(PaneId, Option<u32>)> {
        let mut out = Vec::new();
        for session in self.sessions.values() {
            for window in &session.windows {
                for pane in &window.panes {
                    out.push((pane.id, pane.shell_pid()));
                }
            }
        }
        out
    }

    // ── Agent registry ─────────────────────────────────────────────────

    /// Access the agent registry (read-only).
    pub fn agent_registry(&self) -> &AgentRegistry {
        &self.agent_registry
    }

    /// Install a broadcaster on the agent registry. Used by `ensure.rs` to
    /// forward lifecycle events into the MCP `therminal://agents/events`
    /// resource pipeline.
    pub fn set_agent_event_broadcaster(
        &mut self,
        broadcaster: therminal_terminal::agent_registry::AgentEventBroadcaster,
    ) {
        self.agent_registry.set_broadcaster(broadcaster);
    }

    /// Register an agent for a pane in the central registry.
    pub fn register_agent(
        &mut self,
        pane_id: PaneId,
        name: String,
        agent_type: therminal_terminal::state_inference::AgentType,
        pid: Option<u32>,
    ) {
        self.agent_registry.register(pane_id, name, agent_type, pid);
    }

    /// Unregister the agent for a pane.
    pub fn unregister_agent(&mut self, pane_id: PaneId) {
        self.agent_registry.unregister(pane_id);
    }

    /// Update the status of a tracked agent.
    pub fn update_agent_status(&mut self, pane_id: PaneId, status: AgentStatus) {
        self.agent_registry.update_status(pane_id, status);
    }

    /// Return a snapshot of all tracked agents.
    pub fn list_agents(&self) -> Vec<AgentEntry> {
        self.agent_registry.agents()
    }

    /// Return agents filtered by status string.
    pub fn list_agents_by_status(&self, status: &str) -> Vec<AgentEntry> {
        self.agent_registry.agents_by_status(status)
    }

    /// Collect handoff metadata and raw FDs for all panes (Unix only).
    ///
    /// Returns a `HandoffPayload` and a Vec of `RawFd` in matching order.
    /// The FDs are borrowed from the panes' PTY masters -- the caller must
    /// send them via SCM_RIGHTS before the panes are dropped.
    #[cfg(unix)]
    pub fn collect_handoff_fds(
        &self,
    ) -> (
        therminal_protocol::daemon::HandoffPayload,
        Vec<std::os::unix::io::RawFd>,
    ) {
        use therminal_protocol::daemon::{HandoffPaneMeta, HandoffPayload};

        let mut panes_meta = Vec::new();
        let mut fds = Vec::new();

        for session in self.sessions.values() {
            for window in &session.windows {
                for pane in &window.panes {
                    if let Some(raw_fd) = pane._pty_master.as_raw_fd() {
                        panes_meta.push(HandoffPaneMeta {
                            session_id: session.id,
                            session_name: session.name.clone(),
                            pane_id: pane.id,
                            cols: pane.cols(),
                            rows: pane.rows(),
                        });
                        fds.push(raw_fd);
                    } else {
                        warn!(
                            pane_id = %pane.id,
                            "pane has no raw FD, skipping in handoff"
                        );
                    }
                }
            }
        }

        (HandoffPayload { panes: panes_meta }, fds)
    }

    /// Reconstruct sessions from handoff metadata and received PTY master FDs (Unix only).
    ///
    /// Each received FD is wrapped in a `FdPtyMaster` that implements `MasterPty`,
    /// and a new reader thread is spawned to feed the headless `Term`. This is the
    /// counterpart to `collect_handoff_fds()`.
    #[cfg(unix)]
    pub fn restore_from_handoff(
        &mut self,
        payload: &therminal_protocol::daemon::HandoffPayload,
        fds: Vec<std::os::unix::io::RawFd>,
    ) -> usize {
        use std::collections::HashMap as StdHashMap;

        type PaneEntry = (
            therminal_protocol::daemon::HandoffPaneMeta,
            std::os::unix::io::RawFd,
        );
        type SessionGroup = (Option<String>, Vec<PaneEntry>);

        let mut restored = 0usize;

        // Group panes by session_id so we can reconstruct session -> window -> pane.
        let mut session_groups: StdHashMap<SessionId, SessionGroup> = StdHashMap::new();

        for (meta, fd) in payload.panes.iter().zip(fds.into_iter()) {
            let entry = session_groups
                .entry(meta.session_id)
                .or_insert_with(|| (meta.session_name.clone(), Vec::new()));
            entry.1.push((meta.clone(), fd));
        }

        for (session_id, (session_name, pane_entries)) in session_groups {
            let mut session = Session::new(session_name, self.event_tx.clone());
            // Override the auto-generated ID with the original.
            session.id = session_id;

            let mut window = Window::new();

            for (meta, raw_fd) in pane_entries {
                match Pane::from_raw_fd(
                    meta.pane_id,
                    meta.cols,
                    meta.rows,
                    raw_fd,
                    self.event_tx.clone(),
                    session_id,
                    Arc::clone(&self.osc_registry),
                    self.harness_event_tx.clone(),
                    self.pattern_ctx(),
                ) {
                    Ok(pane) => {
                        window.add_pane(pane);
                        restored += 1;
                    }
                    Err(e) => {
                        warn!(
                            pane_id = meta.pane_id,
                            error = %e,
                            "failed to restore pane from FD, closing FD"
                        );
                        unsafe {
                            libc::close(raw_fd);
                        }
                    }
                }
            }

            if !window.panes.is_empty() {
                session.windows.push(window);
                info!(
                    session_id = session_id,
                    pane_count = session.pane_count(),
                    "restored session from handoff"
                );
                self.sessions.insert(session_id, session);
            }
        }

        // Update the ID counters so new sessions/panes don't collide.
        if let Some(max_session) = self.sessions.keys().max() {
            let current = NEXT_SESSION_ID.load(Ordering::Relaxed);
            if *max_session >= current {
                NEXT_SESSION_ID.store(max_session + 1, Ordering::Relaxed);
            }
        }
        let max_pane = self
            .sessions
            .values()
            .flat_map(|s| s.windows.iter())
            .flat_map(|w| w.panes.iter())
            .map(|p| p.id)
            .max()
            .unwrap_or(0);
        let current_pane = NEXT_PANE_ID.load(Ordering::Relaxed);
        if max_pane >= current_pane {
            NEXT_PANE_ID.store(max_pane + 1, Ordering::Relaxed);
        }

        restored
    }

    /// Restore sessions from persisted state.
    ///
    /// For each persisted session, spawns a new session with fresh PTYs using
    /// the saved cwd. Does not restore terminal grid content -- only layout
    /// and metadata.
    pub fn restore_from_persisted(
        &mut self,
        state: &therminal_protocol::daemon::PersistedState,
    ) -> usize {
        let mut restored = 0usize;
        for persisted_session in &state.sessions {
            if persisted_session.panes.is_empty() {
                continue;
            }

            // Use the first pane to create the session (which creates a default pane).
            let first_pane = &persisted_session.panes[0];
            let spawn_opts = therminal_terminal::pty::SpawnOptions {
                cwd: first_pane.cwd.clone(),
                shell: first_pane.shell.clone(),
                ..Default::default()
            };

            let mut session = Session::new(persisted_session.name.clone(), self.event_tx.clone());
            match session.create_default_pane(
                first_pane.cols,
                first_pane.rows,
                &spawn_opts,
                Arc::clone(&self.osc_registry),
                self.harness_event_tx.clone(),
                self.pattern_ctx(),
            ) {
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        name = ?persisted_session.name,
                        error = %e,
                        "failed to restore session from persisted state"
                    );
                    continue;
                }
            }

            // Restore tags onto the freshly-spawned default pane.
            if !first_pane.tags.is_empty()
                && let Some(window) = session.windows.first_mut()
                && let Some(pane) = window.panes.first_mut()
            {
                pane.set_tags(first_pane.tags.clone());
            }

            let session_id = session.id;

            // Spawn additional panes for multi-pane sessions.
            for pane_meta in &persisted_session.panes[1..] {
                let opts = therminal_terminal::pty::SpawnOptions {
                    cwd: pane_meta.cwd.clone(),
                    shell: pane_meta.shell.clone(),
                    ..Default::default()
                };
                match Pane::spawn(
                    pane_meta.cols,
                    pane_meta.rows,
                    self.event_tx.clone(),
                    session_id,
                    &opts,
                    Arc::clone(&self.osc_registry),
                    self.harness_event_tx.clone(),
                    self.pattern_ctx(),
                ) {
                    Ok(mut pane) => {
                        if !pane_meta.tags.is_empty() {
                            pane.set_tags(pane_meta.tags.clone());
                        }
                        // Add to the first (default) window.
                        if let Some(window) = session.windows.first_mut() {
                            window.add_pane(pane);
                        }
                    }
                    Err(e) => {
                        warn!(
                            session_id = session_id,
                            error = %e,
                            "failed to restore pane in persisted session"
                        );
                    }
                }
            }

            // Restore workspace topology if saved. If the persisted data
            // predates workspace_state (old format), seed a default workspace
            // from whatever panes were restored so GetWorkspaces returns
            // something usable to the GUI attach flow.
            if !persisted_session.workspaces.is_empty() {
                session.workspace_state = persisted_session.workspaces.clone();
                session.active_workspace = persisted_session.active_workspace;
            } else {
                let pane_ids: Vec<PaneId> = session
                    .windows
                    .iter()
                    .flat_map(|w| w.panes.iter().map(|p| p.id))
                    .collect();
                if let Some(&first_pane) = pane_ids.first() {
                    let layout = if pane_ids.len() == 1 {
                        Some(LayoutSnapshot::Leaf {
                            pane_id: first_pane,
                        })
                    } else {
                        // Multi-pane session with no stored layout — leave
                        // layout as None so the client falls back to a flat
                        // cascade rather than guessing at split ratios.
                        None
                    };
                    session.workspace_state = vec![WorkspaceInfo {
                        id: 1,
                        name: "1".to_string(),
                        order: 0,
                        pane_ids,
                        focused_pane: Some(first_pane),
                        layout,
                    }];
                    session.active_workspace = 1;
                }
            }

            let pane_count = session.pane_count();
            info!(
                session_id = session_id,
                name = ?persisted_session.name,
                pane_count,
                workspaces = persisted_session.workspaces.len(),
                "restored session from persisted state"
            );

            let _ = self
                .event_tx
                .send(DaemonEvent::SessionCreated { session_id });
            self.sessions.insert(session_id, session);
            restored += pane_count;
        }

        restored
    }

    /// Graceful shutdown: destroy all sessions.
    pub fn shutdown(&mut self) {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in ids {
            self.destroy_session(id);
        }
    }
}
