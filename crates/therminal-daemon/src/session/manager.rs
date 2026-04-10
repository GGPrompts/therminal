//! `SessionManager`: central registry of all sessions.
//!
//! Owns the session map and provides CRUD + attach/detach operations.
//! Designed to be wrapped in `Arc<tokio::sync::Mutex<SessionManager>>`
//! for sharing across IPC handler tasks.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use therminal_terminal::TaggedHarnessEvent;
use therminal_terminal::agent_registry::AgentRegistry;
use tokio::sync::broadcast;
use tracing::{debug, info};

use therminal_protocol::daemon::{DaemonEvent, LayoutSnapshot, WorkspaceInfo};
pub use therminal_protocol::{PaneId, SessionId};

use super::base::Session;
use super::layout::{
    STARTUP_COMMAND_FALLBACK, STARTUP_COMMAND_POLL_INTERVAL, layout_leaf_dims,
    normalize_startup_command, reconstruct_layout_rect, remove_layout_leaf, split_layout_leaf,
};
use super::pane::{Pane, PaneDispatchCtx};
use super::snapshots::{PaneSnapshot, SessionSnapshot};

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
}
