//! Workspace switching: `switch_workspace`, `create_new_workspace`,
//! `send_to_workspace`. The daemon-mode and local-mode paths share the
//! same public dispatcher functions and branch on `is_daemon_mode()`
//! internally.

use std::sync::Arc;

use tracing::{info, warn};

use crate::pane::{LayoutNode, SplitDirection};
use therminal_protocol::daemon::{IpcRequest, IpcResponse};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::make_pane_callbacks;
use super::super::split_ops::DaemonSplitOnComplete;
use crate::window::App;

impl App {
    /// Switch to workspace `n` (1-9).
    pub(crate) fn switch_workspace(&mut self, n: u8) {
        // Restore layout before switching so the saved tree goes back to the
        // current workspace, not the target.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }

        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        // tn-fi1k Phase B: in daemon mode, route the fresh-pane allocation
        // through SplitPane against any existing daemon pane in the session,
        // so the new workspace's pane carries a daemon id and survives
        // restarts. Pre-spawn the pane BEFORE calling switch_to so we don't
        // need to take a `&mut self` borrow inside the WorkspaceManager
        // closure.
        if self.is_daemon_mode() {
            // Don't pre-spawn if we're already on workspace n (or n is invalid)
            // — switch_to would no-op the pane and leak it.
            let already_on_n = self
                .workspaces
                .as_ref()
                .map(|wm| wm.active_id() == n as usize)
                .unwrap_or(false);
            let target_exists = self
                .workspaces
                .as_ref()
                .map(|wm| wm.workspace_ids().contains(&(n as usize)))
                .unwrap_or(false);
            if already_on_n || !(1..=9).contains(&n) {
                return;
            }
            if target_exists {
                // Target workspace already exists with panes — no spawn needed,
                // just route through switch_to with a no-op closure.
                let switched = self
                    .workspaces
                    .as_mut()
                    .map(|wm| wm.switch_to(n as usize, || None))
                    .unwrap_or(false);
                if switched {
                    info!("Switched to workspace {n}");
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                }
                return;
            }
            // Target workspace does not exist — spawn the pane asynchronously
            // via `split_pane_remote` so we don't block the event loop. The
            // completion handler (`NewWorkspace`) creates the workspace when
            // the RPC resolves.
            let Some(anchor) = self.pane_id_map.any_daemon_id() else {
                warn!(
                    "switch_workspace: daemon mode but no daemon pane to anchor split — falling back to fresh session"
                );
                let Some(state) = self.spawn_remote_pane_fresh_session(full_rect) else {
                    return;
                };
                let new_pane_id = state.id;
                let switched = self
                    .workspaces
                    .as_mut()
                    .map(|wm| {
                        wm.switch_to(n as usize, || Some((LayoutNode::Leaf(state), new_pane_id)))
                    })
                    .unwrap_or(false);
                if switched {
                    info!("Switched to (new) workspace {n} via fresh-session spawn");
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                }
                return;
            };
            let Some(source_local) = self.pane_id_map.local_for_daemon(anchor) else {
                warn!("switch_workspace: anchor daemon pane {anchor} has no local mapping");
                return;
            };
            self.split_pane_remote(
                source_local,
                SplitDirection::Horizontal,
                DaemonSplitOnComplete::NewWorkspace { workspace_id: n },
            );
            return;
        }

        // Local-mode path: unchanged.
        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => return,
        };
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };

        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
            osc_7337: true,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));

        let switched = wm.switch_to(n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                registry.clone(),
                |pane_id| make_pane_callbacks(&proxy, pane_id),
                0.0, // new workspace: single pane, no header
            ) {
                Ok(pane) => {
                    let id = pane.id;
                    Some((LayoutNode::Leaf(pane), id))
                }
                Err(e) => {
                    warn!(error = %e, "failed to spawn pane for new workspace");
                    None
                }
            }
        });

        if switched {
            info!("Switched to workspace {n}");
            self.relayout_and_redraw();
            self.publish_workspace_state();
        }
    }

    /// Create a new workspace tab by finding the next unused slot (1-9).
    pub(crate) fn create_new_workspace(&mut self) {
        let existing = self
            .workspaces
            .as_ref()
            .map(|wm| wm.workspace_ids())
            .unwrap_or_default();
        // Find the lowest unused workspace ID in 1..=9.
        let next_id = (1..=9u8).find(|n| !existing.contains(&(*n as usize)));
        match next_id {
            Some(n) => self.switch_workspace(n),
            None => {
                info!("all workspace slots (1-9) are in use");
            }
        }
    }

    /// Send the focused pane to workspace `n` (1-9).
    pub(crate) fn send_to_workspace(&mut self, n: u8) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        // tn-fi1k Phase B: in daemon mode, the move is metadata-only on
        // the daemon side (the pane's PTY isn't touched). The replacement
        // pane in the source workspace, if needed, is allocated via
        // SplitPane so it carries a daemon id.
        if self.is_daemon_mode() {
            if !(1..=9).contains(&n) {
                return;
            }
            // Reject same-workspace moves up front.
            if let Some(wm) = self.workspaces.as_ref()
                && wm.active_id() == n as usize
            {
                return;
            }
            // Pre-spawn a replacement pane if removing the focused pane
            // would empty the source workspace AND the target already
            // exists (otherwise the moved pane will become the new
            // workspace's only pane and the source becomes empty too —
            // both cases need the replacement).
            //
            // We can't tell ahead of time whether send_pane_to will
            // actually need the replacement closure (it depends on
            // whether the focused pane was the last in its workspace).
            // The safe approach: pre-spawn lazily on demand using the
            // same anchor selection. Since the closure is `FnOnce`, we
            // pre-spawn and pass it through.
            let needs_replacement = self
                .workspaces
                .as_ref()
                .map(|wm| {
                    wm.layout().pane_count() == 1
                        && wm.layout().pane_ids().first().copied() == Some(focused)
                })
                .unwrap_or(false);

            let replacement_state: Option<crate::pane::PaneState> = if needs_replacement {
                // Pick an anchor that ISN'T the pane being moved. If the only
                // daemon pane in the GUI's map IS the moving pane (single-pane
                // workspace), fall back to a fresh CreateSession instead — we
                // must NOT use the moving pane as its own SplitPane anchor,
                // which would corrupt daemon layout state.
                let anchor = self
                    .pane_id_map
                    .any_daemon_id()
                    .filter(|&d| Some(d) != self.pane_id_map.daemon_for_local(focused));
                let state_opt = match anchor {
                    Some(a) => self.spawn_remote_pane_off_existing(a, full_rect),
                    None => self.spawn_remote_pane_fresh_session(full_rect),
                };
                let Some(state) = state_opt else {
                    return;
                };
                Some(state)
            } else {
                None
            };

            // Capture the daemon id of the moved pane BEFORE the local mutate
            // so we can MovePane on the daemon side too.
            let moved_daemon_id = self.pane_id_map.daemon_for_local(focused);

            let wm = match self.workspaces.as_mut() {
                Some(wm) => wm,
                None => return,
            };
            let mut replacement_state = replacement_state;
            let moved = wm.send_pane_to(focused, n as usize, || {
                replacement_state.take().map(|state| {
                    let id = state.id;
                    (LayoutNode::Leaf(state), id)
                })
            });

            if !moved {
                // The local move was rejected. Roll back any replacement
                // pane we pre-spawned so it doesn't become an orphan: the
                // pre-spawned pane was inserted into pane_id_map by
                // spawn_remote_pane_off_existing but never made it into a
                // local layout — if `replacement_state` is still `Some`
                // here, the take() inside the closure never ran.
                if let Some(state) = replacement_state {
                    let local_id = state.id;
                    if let Some(daemon_id) = self.pane_id_map.daemon_for_local(local_id) {
                        warn!(
                            local_id,
                            daemon_id,
                            "send_to_workspace: rolling back unused pre-spawned replacement"
                        );
                        let _ =
                            self.daemon_rpc_blocking(IpcRequest::KillPane { pane_id: daemon_id });
                        self.pane_id_map.remove_by_local(local_id);
                    }
                    drop(state);
                }
                return;
            }

            // Mirror the move on the daemon side via MovePane (metadata
            // sync; the underlying PTY is not touched). publish_workspace_state
            // below will also re-sync the full topology, but issuing an
            // explicit MovePane keeps the daemon's view tight in case the
            // batched SetWorkspaceState drops bytes.
            if let Some(daemon_id) = moved_daemon_id {
                match self.daemon_rpc_blocking(IpcRequest::MovePane {
                    pane_id: daemon_id,
                    target_workspace_id: n as therminal_protocol::WorkspaceId,
                }) {
                    Ok(IpcResponse::PaneMoved { .. }) => {}
                    Ok(IpcResponse::Error { message }) => {
                        warn!(
                            focused,
                            daemon_id, message, "send_to_workspace: daemon MovePane error"
                        );
                    }
                    Ok(other) => {
                        warn!(?other, "send_to_workspace: unexpected MovePane response");
                    }
                    Err(e) => {
                        warn!(error = %e, "send_to_workspace: MovePane RPC failed");
                    }
                }
            }

            info!("Sent pane {focused} to workspace {n} (daemon mode)");
            self.relayout_and_redraw();
            self.publish_workspace_state();
            return;
        }

        // Local-mode path: unchanged.
        let wm = match self.workspaces.as_mut() {
            Some(wm) => wm,
            None => return,
        };
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };

        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
            osc_7337: true,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));

        let moved = wm.send_pane_to(focused, n as usize, || {
            match crate::pane::spawn_pane(
                full_rect,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                registry.clone(),
                |pane_id| make_pane_callbacks(&proxy, pane_id),
                0.0, // replacement pane: single pane, no header
            ) {
                Ok(pane) => {
                    let id = pane.id;
                    Some((LayoutNode::Leaf(pane), id))
                }
                Err(e) => {
                    warn!(error = %e, "failed to spawn replacement pane");
                    None
                }
            }
        });

        if moved {
            info!("Sent pane {focused} to workspace {n}");
            self.relayout_and_redraw();
            self.publish_workspace_state();
        }
    }
}
