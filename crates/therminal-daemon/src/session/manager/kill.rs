//! `kill_pane` and the cascade-resize logic that runs after a pane is
//! removed from a workspace layout (tn-ju04).

use therminal_protocol::PaneId;
use therminal_protocol::daemon::DaemonEvent;

use super::SessionManager;
use crate::session::layout::{layout_leaf_dims, reconstruct_layout_rect, remove_layout_leaf};

impl SessionManager {
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
