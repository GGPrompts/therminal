//! Focus and navigation: move_focus, move_focus_spatial, swap_focused_pane,
//! swap_pane_remote, zoom_toggle_focused_pane, adjust_focused_ratio, reset_all_ratios.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::pane::{FocusDirection, LayoutNode, PaneId, SpatialDirection};
use therminal_protocol::daemon::{IpcRequest, IpcResponse};

use crate::window::App;

impl App {
    /// Best-effort `SelectPane` to keep daemon focus metadata in sync with
    /// the GUI's focused pane. Fire-and-forget: spawns the RPC on the tokio
    /// runtime so the winit event loop never blocks on it. Only runs in
    /// daemon mode.
    ///
    /// This is called from `set_focused_pane`, which fires on every
    /// click-to-focus, every split, and every close. Doing a synchronous
    /// `block_on` here would freeze the UI for up to `DAEMON_OP_TIMEOUT`
    /// (5s) per click if the daemon stalls — see code-review B2.
    pub(crate) fn select_pane_remote(&self, local_id: PaneId) {
        if !self.is_daemon_mode() {
            return;
        }
        let Some(daemon_id) = self.pane_id_map.daemon_for_local(local_id) else {
            return;
        };
        let Some(client) = self.daemon_client.as_ref() else {
            return;
        };
        let Some(handle) = self.daemon_runtime.as_ref() else {
            return;
        };
        let client = Arc::clone(client);
        // 2s is generous for an advisory metadata sync; if the daemon can't
        // ack focus in 2s, the next click will retry anyway.
        handle.spawn(async move {
            let res = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.send_request(IpcRequest::SelectPane { pane_id: daemon_id }),
            )
            .await;
            match res {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    debug!(error = %e, local_id, daemon_id, "select_pane_remote failed")
                }
                Err(_) => debug!(local_id, daemon_id, "select_pane_remote timed out"),
            }
        });
    }

    /// Toggle zoom on the focused pane.
    ///
    /// When not zoomed: saves the current layout tree (with the focused pane
    /// extracted), replaces the workspace layout with a single leaf containing
    /// only the focused pane, and stores the saved layout for later restore.
    ///
    /// When zoomed: restores the saved layout tree, re-inserting the zoomed
    /// pane back into its original position.
    pub(crate) fn zoom_toggle_focused_pane(&mut self) {
        if self.zoomed_layout.is_some() {
            // ── Unzoom: restore saved layout ────────────────────────────
            let wm = match self.workspaces.as_mut() {
                Some(wm) => wm,
                None => return,
            };

            // Take the current single-leaf layout (the zoomed pane).
            let zoomed_leaf = wm.take_layout();
            let pane = match zoomed_leaf {
                LayoutNode::Leaf(p) => p,
                _ => {
                    warn!("zoom_toggle: expected Leaf in zoomed layout");
                    // Put it back if something went wrong.
                    wm.set_layout(zoomed_leaf);
                    return;
                }
            };

            let pane_id = pane.id;

            // Put the pane back into the saved layout at the Empty slot.
            let mut saved = self.zoomed_layout.take().unwrap();
            if saved.insert_pane_at_empty(pane).is_some() {
                warn!("zoom_toggle: no Empty slot found in saved layout, pane lost");
            }

            // Restore the full layout.
            let Some(wm) = self.workspaces.as_mut() else {
                return;
            };
            wm.set_layout(saved);
            wm.set_focused_pane(Some(pane_id));
            info!("Unzoomed pane {pane_id}");
            self.relayout_and_redraw();
        } else {
            // ── Zoom: save layout, show only focused pane ───────────────
            let focused = match self.focused_pane() {
                Some(id) => id,
                None => return,
            };

            let wm = match self.workspaces.as_mut() {
                Some(wm) => wm,
                None => return,
            };

            // Only zoom if there are multiple panes.
            if wm.layout().pane_count() <= 1 {
                debug!("zoom_toggle: only one pane, nothing to zoom");
                return;
            }

            // Take the full layout, extract the focused pane leaf.
            let mut full_layout = wm.take_layout();
            let pane = match full_layout.extract_pane(focused) {
                Some(p) => p,
                None => {
                    warn!("zoom_toggle: focused pane {focused} not found in layout");
                    wm.set_layout(full_layout);
                    return;
                }
            };

            // Store the (now-holey) layout for later restore.
            self.zoomed_layout = Some(full_layout);

            // Set the workspace to just this pane.
            let Some(wm) = self.workspaces.as_mut() else {
                return;
            };
            wm.set_layout(LayoutNode::Leaf(pane));
            wm.set_focused_pane(Some(focused));
            info!("Zoomed pane {focused}");
            self.relayout_and_redraw();
        }
    }

    /// Move focus to the next or previous pane (cycling order).
    pub(crate) fn move_focus(&mut self, direction: FocusDirection) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };

        if let Some(new_id) = layout.adjacent_pane(focused, direction) {
            self.set_focused_pane(Some(new_id));
            self.request_redraw();
        }
    }

    /// Move focus to the nearest pane in a spatial direction.
    pub(crate) fn move_focus_spatial(&mut self, direction: SpatialDirection) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };

        if let Some(new_id) = layout.spatial_adjacent_pane(focused, direction) {
            self.set_focused_pane(Some(new_id));
            self.request_redraw();
        }
    }

    /// Swap the focused pane with the adjacent pane in the given direction.
    /// Focus follows the moved pane (stays on the same pane ID).
    pub(crate) fn swap_focused_pane(&mut self, direction: FocusDirection) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        // Resolve target via the layout (read-only).
        let target_id = match self.get_layout() {
            Some(l) => match l.adjacent_pane(focused, direction) {
                Some(t) => t,
                None => return,
            },
            None => return,
        };

        if self.is_daemon_mode() && !self.swap_pane_remote(focused, target_id) {
            // Daemon rejected/failed — leave the local layout untouched.
            return;
        }

        let layout = match self.get_layout_mut() {
            Some(l) => l,
            None => return,
        };
        if layout.swap_pane(focused, target_id) {
            // Focus stays on the original pane ID (it moved to the new position).
            self.request_redraw();
            self.publish_workspace_state();
        }
    }

    /// Phase B swap path: ask the daemon to swap two panes in its stored
    /// layout snapshot BEFORE the GUI mutates its local LayoutNode tree.
    /// Returns `true` on success; on failure logs loud and leaves both
    /// daemon and local state untouched (rollback semantics).
    pub(crate) fn swap_pane_remote(&mut self, a_local: PaneId, b_local: PaneId) -> bool {
        let a_daemon = match self.pane_id_map.daemon_for_local(a_local) {
            Some(d) => d,
            None => {
                debug!(
                    a_local,
                    "swap_pane_remote: no daemon id mapping for a — proceeding with local swap only"
                );
                return true;
            }
        };
        let b_daemon = match self.pane_id_map.daemon_for_local(b_local) {
            Some(d) => d,
            None => {
                debug!(
                    b_local,
                    "swap_pane_remote: no daemon id mapping for b — proceeding with local swap only"
                );
                return true;
            }
        };
        match self.daemon_rpc_blocking(IpcRequest::SwapPane {
            a: a_daemon,
            b: b_daemon,
        }) {
            Ok(IpcResponse::PaneSwapped { .. }) => true,
            Ok(IpcResponse::Error { message }) => {
                warn!(
                    a_local,
                    b_local, message, "swap_pane_remote: daemon error — NOT mutating local layout"
                );
                self.show_toast(format!("swap failed: {message}"));
                false
            }
            Ok(other) => {
                warn!(
                    a_local,
                    b_local,
                    ?other,
                    "swap_pane_remote: unexpected response — NOT mutating local layout"
                );
                false
            }
            Err(e) => {
                warn!(
                    a_local,
                    b_local, error = %e,
                    "swap_pane_remote: RPC failed — NOT mutating local layout"
                );
                self.show_toast("daemon swap failed");
                false
            }
        }
    }

    /// Adjust the split ratio around the focused pane.
    pub(crate) fn adjust_focused_ratio(&mut self, delta: f32) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        // Compute rect before the mutable borrow of workspaces via layout.
        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        let show_pane_headers = !self.focus_mode;
        // Direct field access needed: layout_mut + grid_renderer must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        if layout.adjust_ratio(focused, delta) {
            layout.layout(full_rect);
            if let Some(renderer) = self.grid_renderer.as_ref() {
                layout.resize_all_panes(renderer, show_pane_headers);
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    /// Reset all split ratios to 50/50.
    pub(crate) fn reset_all_ratios(&mut self) {
        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        let show_pane_headers = !self.focus_mode;
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        layout.reset_all_ratios();
        layout.layout(full_rect);
        if let Some(renderer) = self.grid_renderer.as_ref() {
            layout.resize_all_panes(renderer, show_pane_headers);
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Toggle pinned state on the focused pane (tn-n5jk).
    ///
    /// Pinned panes stay visible during workspace switches. The pinned
    /// flag is also mirrored as a `pinned=true` tag on the daemon side
    /// so it persists across restarts.
    pub(crate) fn toggle_pin_focused_pane(&mut self) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        let pane = match layout.find_pane_mut(focused) {
            Some(p) => p,
            None => return,
        };

        pane.pinned = !pane.pinned;
        let now_pinned = pane.pinned;
        info!(pane_id = focused, pinned = now_pinned, "toggled pane pin");

        // Mirror pin state to daemon tags for persistence.
        if self.is_daemon_mode()
            && let Some(daemon_id) = self.pane_id_map.daemon_for_local(focused)
        {
            if now_pinned {
                let mut tags = std::collections::HashMap::new();
                tags.insert("pinned".to_string(), "true".to_string());
                let _ = self.daemon_rpc_blocking(IpcRequest::TagPane {
                    pane_id: daemon_id,
                    tags,
                });
            } else {
                let _ = self.daemon_rpc_blocking(IpcRequest::UntagPane {
                    pane_id: daemon_id,
                    keys: Some(vec!["pinned".to_string()]),
                });
            }
        }

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}
