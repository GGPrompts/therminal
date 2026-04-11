//! `split_pane` / `split_pane_with_options`, plus the startup-command
//! injection helpers (`maybe_send_startup_command`, `wait_for_prompt_start`).

use std::sync::Arc;
use std::time::Instant;

use tracing::debug;

use therminal_protocol::PaneId;
use therminal_protocol::daemon::{DaemonEvent, LayoutSnapshot};

use super::SessionManager;
use crate::session::layout::{
    STARTUP_COMMAND_FALLBACK, STARTUP_COMMAND_POLL_INTERVAL, normalize_startup_command,
    split_layout_leaf,
};
use crate::session::pane::Pane;

impl SessionManager {
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
}
