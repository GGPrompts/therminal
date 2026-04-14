//! Daemon-mode supporting helpers for the remote split path:
//! - `finish_new_workspace_remote` — completion path for the
//!   `NewWorkspace` `DaemonSplitOnComplete` variant
//! - `spawn_remote_pane_off_existing` — synchronous remote pane spawn used
//!   by `restore_layout` to fan out off an anchor pane
//! - `spawn_remote_pane_fresh_session` — synchronous remote pane spawn that
//!   creates a brand-new daemon session as the anchor

use std::sync::Arc;

use tracing::{info, warn};

use crate::pane::LayoutNode;
use therminal_core::geometry::Rect;
use therminal_protocol::daemon::{IpcRequest, IpcResponse};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::make_pane_callbacks;
use crate::window::App;

impl App {
    /// Handle the `NewWorkspace` completion: build a pane state for the full
    /// viewport and insert it as the sole leaf in a new workspace tab.
    pub(super) fn finish_new_workspace_remote(
        &mut self,
        new_daemon_pane_id: therminal_protocol::PaneId,
        workspace_id: u8,
    ) {
        let full_rect = match self.compute_layout_rect() {
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
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let (cols, rows) = crate::pane::grid_size_for_rect(full_rect, renderer);
        let cols = cols.max(2);
        let rows = rows.max(1);
        let dc = match self.daemon_client.as_ref() {
            Some(c) => Arc::clone(c),
            None => return,
        };
        let handle = match self.daemon_runtime.as_ref() {
            Some(h) => h.clone(),
            None => return,
        };
        let socket = dc.socket_path().to_path_buf();
        let local_id = crate::pane::next_pane_id();
        let callbacks = make_pane_callbacks(&self.event_proxy, local_id);

        let state = match crate::pane::remote_spawn::build_remote_pane_state(
            local_id,
            new_daemon_pane_id,
            full_rect,
            cols,
            rows,
            scrollback,
            interceptor_cfg,
            dc,
            handle,
            socket,
            callbacks,
            None,
            Some(Arc::clone(&self.agent_registry)),
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    error = %e,
                    new_daemon_pane_id,
                    "finish_new_workspace_remote: build_remote_pane_state failed — best-effort cleanup"
                );
                if let (Some(client), Some(handle)) =
                    (self.daemon_client.as_ref(), self.daemon_runtime.as_ref())
                {
                    let client = Arc::clone(client);
                    handle.spawn(async move {
                        let _ = client
                            .send_request(IpcRequest::KillPane {
                                pane_id: new_daemon_pane_id,
                            })
                            .await;
                    });
                }
                self.show_toast("new tab failed");
                return;
            }
        };

        self.pane_id_map.insert(local_id, new_daemon_pane_id);
        let new_pane_id = state.id;
        let switched = self
            .workspaces
            .as_mut()
            .map(|wm| {
                wm.switch_to(workspace_id as usize, || {
                    Some((LayoutNode::Leaf(state), new_pane_id))
                })
            })
            .unwrap_or(false);
        if switched {
            info!(
                workspace_id,
                new_local = local_id,
                new_daemon = new_daemon_pane_id,
                "finish_new_workspace_remote: created workspace via async split"
            );
            self.relayout_and_redraw();
            self.publish_workspace_state();
        }
    }

    /// tn-fi1k: spawn a brand-new remote pane "off" an existing daemon
    /// pane (the anchor) WITHOUT inserting it into any local layout. The
    /// caller is responsible for placing the returned `PaneState` wherever
    /// it wants — typically into a fresh workspace's layout (switch_workspace
    /// / send_to_workspace replacement / restore_layout rebuild).
    ///
    /// Flow:
    /// 1. Issue `IpcRequest::SplitPane { pane_id: anchor_daemon_id, horizontal: true, cwd: None, shell: None }`
    /// 2. Allocate a fresh local id via `next_pane_id()`
    /// 3. Build the local `PaneState` via `remote_spawn::build_remote_pane_state`
    /// 4. Insert the (local, daemon) pair into `pane_id_map`
    ///
    /// On any failure, returns `None` and best-effort issues `KillPane`
    /// against the daemon-allocated pane (if step 1 succeeded but step 3
    /// failed) so we don't leak orphan daemon panes.
    ///
    /// `viewport` is the rect the caller intends to assign to the new
    /// pane in its layout — used to compute the initial grid size for
    /// the local `Term`. The pane's actual on-screen rect will be
    /// recomputed when the caller calls `relayout_and_redraw`.
    pub(crate) fn spawn_remote_pane_off_existing(
        &mut self,
        anchor_daemon_id: therminal_protocol::PaneId,
        viewport: Rect,
    ) -> Option<crate::pane::PaneState> {
        // 1. Daemon split RPC.
        let resp = match self.daemon_rpc_blocking(IpcRequest::SplitPane {
            pane_id: anchor_daemon_id,
            horizontal: true,
            cwd: None,
            startup_command: None,
            ratio: None,
            shell: None,
            worktree: None,
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, anchor_daemon_id, "spawn_remote_pane_off_existing: SplitPane RPC failed");
                self.show_toast("daemon split failed");
                return None;
            }
        };
        let new_daemon_pane_id = match resp {
            IpcResponse::PaneSplit { new_pane_id } => new_pane_id,
            IpcResponse::Error { message } => {
                warn!(
                    message,
                    anchor_daemon_id, "spawn_remote_pane_off_existing: daemon error"
                );
                self.show_toast(format!("split failed: {message}"));
                return None;
            }
            other => {
                warn!(
                    ?other,
                    "spawn_remote_pane_off_existing: unexpected response"
                );
                return None;
            }
        };

        // 2. Allocate local id and build the remote-backed PaneState.
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
        let renderer = self.grid_renderer.as_ref()?;
        let (cols, rows) = crate::pane::grid_size_for_rect(viewport, renderer);
        let cols = cols.max(2);
        let rows = rows.max(1);
        let dc = Arc::clone(self.daemon_client.as_ref()?);
        let handle = self.daemon_runtime.as_ref()?.clone();
        let socket = dc.socket_path().to_path_buf();
        let local_id = crate::pane::next_pane_id();
        let callbacks = make_pane_callbacks(&self.event_proxy, local_id);

        let state = match crate::pane::remote_spawn::build_remote_pane_state(
            local_id,
            new_daemon_pane_id,
            viewport,
            cols,
            rows,
            scrollback,
            interceptor_cfg,
            dc,
            handle,
            socket,
            callbacks,
            None,
            Some(Arc::clone(&self.agent_registry)),
        ) {
            Ok(state) => state,
            Err(e) => {
                warn!(
                    error = %e,
                    new_daemon_pane_id,
                    "spawn_remote_pane_off_existing: build_remote_pane_state failed AFTER daemon split — best-effort cleanup"
                );
                // Recover: kill the orphan daemon pane we couldn't mount.
                match self.daemon_rpc_blocking(IpcRequest::KillPane {
                    pane_id: new_daemon_pane_id,
                }) {
                    Ok(IpcResponse::PaneKilled { .. }) => {}
                    Ok(other) => warn!(
                        new_daemon_pane_id,
                        ?other,
                        "spawn_remote_pane_off_existing: orphan KillPane returned unexpected response"
                    ),
                    Err(e) => warn!(
                        new_daemon_pane_id,
                        error = %e,
                        "spawn_remote_pane_off_existing: orphan KillPane RPC failed"
                    ),
                }
                return None;
            }
        };

        self.pane_id_map.insert(local_id, new_daemon_pane_id);
        info!(
            anchor_daemon_id,
            new_local = local_id,
            new_daemon = new_daemon_pane_id,
            "spawn_remote_pane_off_existing: daemon split + local mount complete"
        );
        Some(state)
    }

    /// tn-fi1k: spawn a brand-new daemon session and materialise its
    /// initial pane locally, returning a `PaneState`. Used by
    /// `restore_layout` when no existing daemon pane is available to
    /// anchor a `SplitPane` against (e.g. after `take_layout()` cleared
    /// the workspace and no other workspace held a pane).
    ///
    /// Returns `None` and shows a toast on any failure.
    pub(crate) fn spawn_remote_pane_fresh_session(
        &mut self,
        viewport: Rect,
    ) -> Option<crate::pane::PaneState> {
        let renderer = self.grid_renderer.as_ref()?;
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
        let dc = Arc::clone(self.daemon_client.as_ref()?);
        let handle = self.daemon_runtime.as_ref()?.clone();
        let socket = dc.socket_path().to_path_buf();
        let local_id = crate::pane::next_pane_id();
        let callbacks = make_pane_callbacks(&self.event_proxy, local_id);

        match crate::pane::remote_spawn::spawn_remote_pane(
            local_id,
            viewport,
            renderer,
            scrollback,
            interceptor_cfg,
            dc,
            handle,
            socket,
            callbacks,
            None,
            Some(Arc::clone(&self.agent_registry)),
        ) {
            Ok((state, session_id, daemon_pane_id)) => {
                self.pane_id_map.insert(local_id, daemon_pane_id);
                // If we don't yet have a daemon session id, claim this one
                // so subsequent publish_workspace_state calls have somewhere
                // to send. (Should already be set in normal flows.)
                if self.daemon_session_id.is_none() {
                    self.daemon_session_id = Some(session_id);
                }
                info!(
                    local_id,
                    daemon_pane_id, session_id, "spawn_remote_pane_fresh_session: created"
                );
                Some(state)
            }
            Err(e) => {
                warn!(error = %e, "spawn_remote_pane_fresh_session: spawn_remote_pane failed");
                self.show_toast("daemon session create failed");
                None
            }
        }
    }
}
