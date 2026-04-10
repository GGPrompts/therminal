//! Close operations: close_focused_pane, close_pane_by_id, close_all_panes,
//! kill_pane_remote.

use tracing::{debug, info, warn};

use crate::pane::{PaneId, PaneRemoveResult};
use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use crate::window::App;

impl App {
    /// Phase B close path: ask the daemon to kill `source_local`'s
    /// daemon-side pane BEFORE we drop the local pane state. On daemon
    /// failure the local pane is left intact (user-visible) so the GUI
    /// never diverges from the daemon silently.
    ///
    /// Returns `true` if the RPC succeeded (caller may proceed to drop
    /// local state); `false` if the caller should abort the local close.
    pub(crate) fn kill_pane_remote(&mut self, source_local: PaneId) -> bool {
        let daemon_id = match self.pane_id_map.daemon_for_local(source_local) {
            Some(d) => d,
            None => {
                // Local-only pane (pre-cutover); caller should fall through
                // to pure local close.
                debug!(
                    source_local,
                    "kill_pane_remote: no daemon id mapping — proceeding with local close only"
                );
                return true;
            }
        };
        match self.daemon_rpc_blocking(IpcRequest::KillPane { pane_id: daemon_id }) {
            Ok(IpcResponse::PaneKilled { .. }) => true,
            Ok(IpcResponse::Error { message }) => {
                warn!(
                    source_local,
                    daemon_id, message, "kill_pane_remote: daemon error — keeping local pane"
                );
                self.show_toast(format!("kill failed: {message}"));
                false
            }
            Ok(other) => {
                warn!(
                    source_local,
                    daemon_id,
                    ?other,
                    "kill_pane_remote: unexpected response — keeping local pane"
                );
                false
            }
            Err(e) => {
                warn!(
                    source_local,
                    daemon_id, error = %e,
                    "kill_pane_remote: RPC failed — keeping local pane"
                );
                self.show_toast("daemon kill failed");
                false
            }
        }
    }

    /// Close the currently focused pane.
    ///
    /// Includes a 100ms cooldown to prevent double-close from keyboard repeat
    /// firing two events in the same winit event batch.
    pub(crate) fn close_focused_pane(&mut self) {
        if let Some(last) = self.last_close_action
            && last.elapsed() < std::time::Duration::from_millis(100)
        {
            debug!("close_focused_pane: debounced (< 100ms since last close)");
            return;
        }
        self.last_close_action = Some(std::time::Instant::now());

        // If zoomed, restore the full layout before closing so the tree is intact.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }

        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        // tn-beez Phase B: ask the daemon to kill the pane first. If that
        // fails, leave the local pane intact so GUI and daemon stay in sync.
        if self.is_daemon_mode() && !self.kill_pane_remote(focused) {
            return;
        }
        self.pane_id_map.remove_by_local(focused);

        // Use remove_pane_any which searches all workspaces and handles cleanup.
        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => return,
        };

        match wm.remove_pane_any(focused) {
            PaneRemoveResult::LastInWorkspace => {
                if wm.gc_empty_workspaces() {
                    // Switched to another workspace that still has panes.
                    info!(
                        "Last pane in workspace closed, switched to workspace {}",
                        wm.active_id()
                    );
                    let focus = wm.focused_pane();
                    self.set_focused_pane(focus);
                    self.relayout_and_redraw();
                } else {
                    // Truly the last pane across all workspaces.
                    info!("Last pane closed, exiting");
                    self.set_focused_pane(None);
                    self.workspaces = None;
                    self.request_redraw();
                }
            }
            PaneRemoveResult::Removed => {
                info!("Closed pane {focused}");
                // Move focus to first available pane.
                let new_focus = self
                    .get_layout()
                    .map(|l| l.pane_ids())
                    .and_then(|ids| ids.first().copied());
                self.set_focused_pane(new_focus);
                self.relayout_and_redraw();
            }
            PaneRemoveResult::NotFound => {
                // Pane not found (shouldn't happen for focused pane).
                warn!("Focused pane {focused} not found in layout");
            }
        }
        self.publish_workspace_state();
    }

    /// Close a specific pane by ID.
    ///
    /// Includes a 100ms cooldown to prevent double-close from keyboard repeat.
    pub(crate) fn close_pane_by_id(&mut self, target_id: PaneId) {
        // Code-review B4: capture the daemon id and drop the local↔daemon
        // PaneId mapping BEFORE the debounce guard. Two daemon panes
        // exiting <100ms apart used to leave the second as a zombie in the
        // map; the next publish_workspace_state would then publish a stale
        // daemon id and the next attach would hang trying to
        // build_remote_pane_state for it. The map cleanup is idempotent
        // and cheap — there is no reason to gate it on the debounce.
        let daemon_id_for_kill = if self.is_daemon_mode() {
            self.pane_id_map.daemon_for_local(target_id)
        } else {
            None
        };
        self.pane_id_map.remove_by_local(target_id);

        if let Some(last) = self.last_close_action
            && last.elapsed() < std::time::Duration::from_millis(100)
        {
            debug!("close_pane_by_id: debounced (< 100ms since last close)");
            return;
        }
        self.last_close_action = Some(std::time::Instant::now());

        // tn-beez Phase B: issue a best-effort KillPane to the daemon so
        // user-initiated closes tear down the remote child. Errors are
        // tolerated because `close_pane_by_id` is also called from the
        // `PaneExited` event path where the remote child is already gone.
        if let Some(daemon_id) = daemon_id_for_kill {
            let _ = self.daemon_rpc_blocking(IpcRequest::KillPane { pane_id: daemon_id });
        }

        // If zoomed, restore the full layout so tree removal works correctly.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }

        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => {
                warn!(
                    target_id,
                    "close_pane_by_id: no workspaces (already torn down?)"
                );
                return;
            }
        };

        let pane_count_before = wm.total_pane_count();
        info!(
            target_id,
            pane_count_before,
            focused = ?wm.focused_pane(),
            "close_pane_by_id called"
        );

        // Search all workspaces for the pane, not just the active one.
        match wm.remove_pane_any(target_id) {
            PaneRemoveResult::LastInWorkspace => {
                // Last pane in some workspace — check if others remain.
                if wm.total_pane_count() == 0 && !wm.gc_empty_workspaces() {
                    // Truly the last pane across all workspaces.
                    info!("Last pane closed, exiting");
                    self.set_focused_pane(None);
                    self.workspaces = None;
                    self.request_redraw();
                } else {
                    // Other workspaces have panes; clean up the empty one.
                    wm.gc_empty_workspaces();
                    info!(
                        "Pane {target_id} was last in its workspace, switched to workspace {}",
                        wm.active_id()
                    );
                    // Update focused pane from the now-active workspace.
                    let focus = wm.focused_pane();
                    self.set_focused_pane(focus);
                    self.relayout_and_redraw();
                }
            }
            PaneRemoveResult::Removed => {
                let pane_count_after = wm.total_pane_count();
                info!(
                    target_id,
                    pane_count_before, pane_count_after, "Closed pane"
                );
                // If we closed the focused pane of the active workspace, move focus.
                if self.focused_pane() == Some(target_id) {
                    let new_focus = self
                        .get_layout()
                        .map(|l| l.pane_ids())
                        .and_then(|ids| ids.first().copied());
                    self.set_focused_pane(new_focus);
                }
                self.relayout_and_redraw();
            }
            PaneRemoveResult::NotFound => {
                warn!(
                    target_id,
                    pane_count_before,
                    "Pane not found in any workspace (double-close or stale event?)"
                );
            }
        }
        self.publish_workspace_state();
    }

    /// Close all panes in the current workspace.
    ///
    /// If other workspaces still have panes, removes the now-empty workspace
    /// and switches to the nearest one. Only exits the app when no panes
    /// remain across all workspaces.
    pub(crate) fn close_all_panes(&mut self) {
        if let Some(last) = self.last_close_action
            && last.elapsed() < std::time::Duration::from_millis(100)
        {
            debug!("close_all_panes: debounced (< 100ms since last close)");
            return;
        }
        self.last_close_action = Some(std::time::Instant::now());

        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => return,
        };

        // Drop the active workspace's layout (kills all PTYs in this tab).
        let layout = wm.take_layout();
        drop(layout);

        if wm.gc_empty_workspaces() {
            // Other workspaces have panes — switch to one.
            info!(
                "Closed all panes in workspace, switched to workspace {}",
                wm.active_id()
            );
            let focus = wm.focused_pane();
            self.set_focused_pane(focus);
            self.relayout_and_redraw();
        } else {
            // No panes anywhere — exit.
            info!("Closed all panes, exiting");
            self.set_focused_pane(None);
            self.workspaces = None;
            self.request_redraw();
        }

        self.selection_pane = None;
        self.selection_in_progress = false;
        self.publish_workspace_state();
    }
}
