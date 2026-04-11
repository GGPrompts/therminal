//! Split operations: split_focused_pane, split_pane_by_id, split_pane_remote,
//! finish_split_pane_remote, spawn helpers.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::pane::{LayoutNode, PaneId, SplitDirection};
use therminal_core::geometry::Rect;
use therminal_protocol::daemon::{IpcRequest, IpcResponse};
use therminal_terminal::interceptor::InterceptorConfig;

use super::{DAEMON_OP_TIMEOUT, cwd_from_source_pane, make_pane_callbacks, split_spawn_options};
use crate::window::{App, UserEvent};

/// Context carried from the async `SplitPane` RPC back to the main thread
/// so the layout insert can complete without blocking the event loop.
#[derive(Debug)]
pub struct DaemonSplitResult {
    /// Local pane id that was being split.
    pub source_local: PaneId,
    /// Split direction requested.
    pub direction: SplitDirection,
    /// Result of the daemon RPC — `Ok(daemon_pane_id)` or `Err(message)`.
    pub rpc_result: Result<therminal_protocol::PaneId, String>,
    /// Inherited cwd passed to the daemon (needed to wire the remote PTY).
    pub inherited_cwd: Option<String>,
    /// Optional post-split action to perform once the local pane is mounted.
    pub on_complete: DaemonSplitOnComplete,
}

/// What to do after `finish_split_pane_remote` successfully mounts the new pane.
#[derive(Debug, Default)]
pub enum DaemonSplitOnComplete {
    /// Focus the new pane, relayout, publish — the standard path.
    #[default]
    FocusAndRelayout,
    /// Auto-tile: register the new pane with the auto-tile debouncer and relayout.
    /// `parent_pane_id` is the pane whose agent spawned the split.
    AutoTile { parent_pane_id: PaneId },
    /// Swarm: send a `tail` command into the new pane and register it in `swarm_panes`.
    SwarmTail {
        agent_id: String,
        jsonl_path: std::path::PathBuf,
    },
    /// Write bytes to the new pane after it mounts (used by hotspot "Open in
    /// new pane" and WSL editor open, which need the daemon split to complete
    /// before the PTY is ready to accept input).
    WriteBytesAndFocus {
        bytes: Vec<u8>,
        toast: Option<String>,
    },
    /// Create a new workspace tab with the spawned pane as its root.
    /// The pane is NOT inserted into the current layout — it becomes the
    /// sole leaf in a brand-new workspace.
    NewWorkspace { workspace_id: u8 },
}

impl App {
    /// Phase B split path: fire the daemon `SplitPane` RPC asynchronously so
    /// the event loop never blocks. The RPC result is delivered back to the
    /// main thread via `UserEvent::DaemonSplitComplete`; `finish_split_pane_remote`
    /// handles the layout insert there.
    ///
    /// Callers should NOT mutate the layout themselves — the two-part
    /// async/finish pair owns both the RPC and the tree insert.
    pub(crate) fn split_pane_remote(
        &mut self,
        source_local: PaneId,
        direction: SplitDirection,
        on_complete: DaemonSplitOnComplete,
    ) {
        let daemon_source = match self.pane_id_map.daemon_for_local(source_local) {
            Some(d) => d,
            None => {
                warn!(
                    source_local,
                    "split_pane_remote: no daemon id mapping (local-mode pane pre-cutover); bailing"
                );
                return;
            }
        };
        let horizontal = matches!(direction, SplitDirection::Horizontal);
        let inherited_cwd = self
            .get_layout()
            .and_then(|layout| cwd_from_source_pane(layout, source_local));

        let Some(client) = self.daemon_client.as_ref() else {
            return;
        };
        let Some(handle) = self.daemon_runtime.as_ref() else {
            return;
        };
        let client = Arc::clone(client);
        let proxy = self.event_proxy.clone();
        let cwd_clone = inherited_cwd.clone();

        handle.spawn(async move {
            let rpc_result = match tokio::time::timeout(
                DAEMON_OP_TIMEOUT,
                client.send_request(IpcRequest::SplitPane {
                    pane_id: daemon_source,
                    horizontal,
                    cwd: cwd_clone,
                    startup_command: None,
                    ratio: None,
                    shell: None,
                    worktree: None,
                }),
            )
            .await
            {
                Ok(Ok(IpcResponse::PaneSplit { new_pane_id })) => Ok(new_pane_id),
                Ok(Ok(IpcResponse::Error { message })) => Err(message),
                Ok(Ok(other)) => Err(format!("unexpected response: {other:?}")),
                Ok(Err(e)) => Err(format!("rpc error: {e}")),
                Err(_) => Err(format!("rpc timed out after {DAEMON_OP_TIMEOUT:?}")),
            };
            let _ = proxy.send_event(UserEvent::DaemonSplitComplete(DaemonSplitResult {
                source_local,
                direction,
                rpc_result,
                inherited_cwd,
                on_complete,
            }));
        });
    }

    /// Called on the main thread when the async `SplitPane` RPC resolves.
    /// Performs the local layout insert (or orphan cleanup on failure).
    pub(crate) fn finish_split_pane_remote(&mut self, result: DaemonSplitResult) {
        let DaemonSplitResult {
            source_local,
            direction,
            rpc_result,
            inherited_cwd,
            on_complete,
        } = result;

        let new_daemon_pane_id = match rpc_result {
            Ok(id) => id,
            Err(e) => {
                warn!(error = %e, "split_pane_remote: RPC failed — NOT mutating local layout");
                self.show_toast("daemon split failed");
                return;
            }
        };

        // ── NewWorkspace: build pane state independently, create workspace ──
        if let DaemonSplitOnComplete::NewWorkspace { workspace_id } = on_complete {
            self.finish_new_workspace_remote(new_daemon_pane_id, workspace_id);
            return;
        }

        // Gather immutable deps before mutable-borrowing the layout.
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_for_closure = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
        };
        let dc_for_closure = match self.daemon_client.as_ref() {
            Some(c) => Arc::clone(c),
            None => return,
        };
        let handle_for_closure = match self.daemon_runtime.as_ref() {
            Some(h) => h.clone(),
            None => return,
        };
        let socket_for_closure = dc_for_closure.socket_path().to_path_buf();
        let proxy = self.event_proxy.clone();

        let local_id = crate::pane::next_pane_id();

        let renderer_ref = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        let callbacks = make_pane_callbacks(&proxy, local_id);
        // tn-ou30: compute post-split header height so the local Term starts
        // at the correct size, avoiding a shrink→scrollback row on relayout.
        let post_split_header_h = crate::pane::effective_header_height(
            layout.pane_count() + 1,
            self.config.general.show_pane_headers,
        );
        let build_result: std::cell::RefCell<Option<anyhow::Error>> = std::cell::RefCell::new(None);
        let new_id = layout.split_pane(source_local, direction, |viewport| {
            let (cols, rows) = crate::pane::grid_size_for_rect_with_header(
                viewport,
                renderer_ref,
                post_split_header_h,
            );
            let cols = cols.max(2);
            let rows = rows.max(1);
            match crate::pane::remote_spawn::build_remote_pane_state(
                local_id,
                new_daemon_pane_id,
                viewport,
                cols,
                rows,
                scrollback,
                interceptor_for_closure,
                dc_for_closure,
                handle_for_closure,
                socket_for_closure,
                callbacks,
                inherited_cwd.clone(),
            ) {
                Ok(state) => Some(state),
                Err(e) => {
                    *build_result.borrow_mut() = Some(e);
                    None
                }
            }
        });

        if let Some(new_id) = new_id {
            self.pane_id_map.insert(new_id, new_daemon_pane_id);
            // tn-ou30: schedule scrollback compaction for the new pane.
            self.scrollback_compact_countdown = 30;
            info!(
                source_local,
                new_local = new_id,
                new_daemon = new_daemon_pane_id,
                "split_pane_remote: daemon split + local mount complete"
            );
            match on_complete {
                DaemonSplitOnComplete::FocusAndRelayout => {
                    self.last_split_direction = direction;
                    self.set_focused_pane(Some(new_id));
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                }
                DaemonSplitOnComplete::AutoTile { parent_pane_id } => {
                    if let Some(ref mut debouncer) = self.auto_tile_debouncer {
                        debouncer.register_auto_tiled(parent_pane_id, new_id);
                    }
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                }
                DaemonSplitOnComplete::SwarmTail {
                    agent_id,
                    jsonl_path,
                } => {
                    let cmd = format!(
                        "clear && tail --lines=+1 -F {}\n",
                        super::editor_clipboard::shell_quote(&jsonl_path.display().to_string()),
                    );
                    if let Some(daemon_id) = self.pane_id_map.daemon_for_local(new_id) {
                        match self.daemon_rpc_blocking(IpcRequest::SendKeys {
                            pane_id: daemon_id,
                            keys: cmd.into_bytes(),
                        }) {
                            Ok(_) => {}
                            Err(e) => warn!(error = %e, "swarm: SendKeys for tail command failed"),
                        }
                    }
                    self.swarm_panes.insert(agent_id, new_id);
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                }
                DaemonSplitOnComplete::WriteBytesAndFocus { bytes, toast } => {
                    self.last_split_direction = direction;
                    self.set_focused_pane(Some(new_id));
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                    if let Some(msg) = toast {
                        self.show_toast(msg);
                    }
                    if let Some(daemon_id) = self.pane_id_map.daemon_for_local(new_id) {
                        match self.daemon_rpc_blocking(IpcRequest::SendKeys {
                            pane_id: daemon_id,
                            keys: bytes,
                        }) {
                            Ok(_) => {}
                            Err(e) => warn!(error = %e, "WriteBytesAndFocus: SendKeys failed"),
                        }
                    }
                }
                DaemonSplitOnComplete::NewWorkspace { .. } => unreachable!(),
            }
        } else if let Some(e) = build_result.into_inner() {
            warn!(error = %e, "split_pane_remote: build_remote_pane_state failed AFTER daemon split — daemon now has orphan pane");
            // F2 (tn-97j6): best-effort kill the orphan pane we couldn't
            // mount. Fire-and-forget since we're already on the happy path failure branch.
            if let (Some(client), Some(handle)) =
                (self.daemon_client.as_ref(), self.daemon_runtime.as_ref())
            {
                let client = Arc::clone(client);
                handle.spawn(async move {
                    match client
                        .send_request(IpcRequest::KillPane {
                            pane_id: new_daemon_pane_id,
                        })
                        .await
                    {
                        Ok(IpcResponse::PaneKilled { .. }) => {}
                        Ok(other) => warn!(
                            new_daemon_pane_id,
                            ?other,
                            "split_pane_remote: orphan KillPane recovery returned unexpected response"
                        ),
                        Err(e) => warn!(
                            new_daemon_pane_id,
                            error = %e,
                            "split_pane_remote: orphan KillPane recovery RPC failed — daemon orphan persists"
                        ),
                    }
                });
            }
        }
    }

    /// Handle the `NewWorkspace` completion: build a pane state for the full
    /// viewport and insert it as the sole leaf in a new workspace tab.
    fn finish_new_workspace_remote(
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

    /// Split the currently focused pane with auto-detected direction.
    pub(crate) fn split_focused_pane_auto(&mut self) {
        self.split_focused_pane_auto_with(DaemonSplitOnComplete::FocusAndRelayout);
    }

    /// Split the currently focused pane with auto-detected direction and a
    /// custom daemon-mode completion action.
    pub(crate) fn split_focused_pane_auto_with(&mut self, on_complete: DaemonSplitOnComplete) {
        // Restore layout before splitting so the new pane joins the full tree.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(focused) {
            Some(p) => p,
            None => return,
        };
        let fallback = match self.last_split_direction {
            SplitDirection::Horizontal => SplitDirection::Vertical,
            SplitDirection::Vertical => SplitDirection::Horizontal,
        };
        let direction = LayoutNode::auto_split_direction(pane.viewport, fallback);
        self.split_focused_pane_with(direction, on_complete);
    }

    /// Split the currently focused pane.
    pub(crate) fn split_focused_pane(&mut self, direction: SplitDirection) {
        self.split_focused_pane_with(direction, DaemonSplitOnComplete::FocusAndRelayout);
    }

    /// Split the currently focused pane with a custom daemon-mode completion action.
    pub(crate) fn split_focused_pane_with(
        &mut self,
        direction: SplitDirection,
        on_complete: DaemonSplitOnComplete,
    ) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        // tn-beez Phase B: in daemon mode, route splits through the daemon
        // so the resulting pane id is the canonical daemon id and shows up
        // in MCP `terminal.panes.list` + persists across daemon restart.
        // The RPC is fired asynchronously; completion arrives via
        // `UserEvent::DaemonSplitComplete` → `finish_split_pane_remote`.
        if self.is_daemon_mode() {
            self.split_pane_remote(focused, direction, on_complete);
            return;
        }
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
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let base_spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        // Direct field access needed here: layout_mut + renderer + config must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        // Inherit source pane's cwd (from OSC 7) so the new shell starts in
        // the same directory the user was working in.
        let spawn_options = split_spawn_options(
            &base_spawn_options,
            layout,
            focused,
            self.config.general.new_pane_cwd,
        );

        // tn-ou30: compute the header height that resize_all_panes will apply
        // AFTER the split so the PTY starts at the correct size.
        let post_split_header_h = crate::pane::effective_header_height(
            layout.pane_count() + 1,
            self.config.general.show_pane_headers,
        );

        let new_id = layout.split_pane(
            focused,
            direction,
            |viewport| match crate::pane::spawn_pane(
                viewport,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                registry.clone(),
                |pane_id| make_pane_callbacks(&proxy, pane_id),
                post_split_header_h,
            ) {
                Ok(pane) => Some(pane),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to spawn pane for split");
                    None
                }
            },
        );

        if let Some(new_id) = new_id {
            info!("Split pane {focused} {:?} -> new pane {new_id}", direction);
            self.last_split_direction = direction;
            self.set_focused_pane(Some(new_id));
            self.relayout_and_redraw();
            self.publish_workspace_state();
        } else {
            self.request_redraw();
        }
    }

    /// Open a horizontal split running `tail -F` on the focused pane's
    /// agent event log JSONL file.
    ///
    /// Triggered by clicking the `[agent: <name>]` indicator in the status
    /// bar. The new pane is small and narrow (horizontal split) so it acts
    /// as a side panel without dominating the layout.
    pub(crate) fn open_focused_agent_event_log_tail(&mut self) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => {
                debug!("open_focused_agent_event_log_tail: no focused pane");
                return;
            }
        };

        // The session_id used for event logs corresponds 1:1 with the pane
        // id in this single-process app. The daemon uses the same naming
        // scheme, so this matches if/when the daemon is also writing logs.
        let session_id = format!("pane-{focused}");
        let log_path = therminal_runtime::paths::runtime_dir()
            .join("sessions")
            .join(format!("{session_id}.events.jsonl"));
        let log_path_str = log_path.to_string_lossy().into_owned();

        info!(
            "Opening agent event log tail pane for pane {} at {}",
            focused, log_path_str
        );

        // `tail -F` follows file rotation/recreation and tolerates a
        // non-existent file (it will retry until the file appears).
        let cmd = format!("tail -F {log_path_str}\n");

        // In daemon mode the split is async — carry the command bytes in the
        // completion callback so they're written after the PTY is live.
        if self.is_daemon_mode() {
            self.split_focused_pane_with(
                SplitDirection::Horizontal,
                DaemonSplitOnComplete::WriteBytesAndFocus {
                    bytes: cmd.into_bytes(),
                    toast: None,
                },
            );
            return;
        }

        // Local mode: split is synchronous — write immediately.
        // Horizontal split keeps the tail pane narrow (top/bottom layout).
        self.split_focused_pane(SplitDirection::Horizontal);

        let new_pane = match self.focused_pane() {
            Some(id) if id != focused => id,
            _ => {
                warn!("open_focused_agent_event_log_tail: split did not produce a new pane");
                return;
            }
        };

        self.pty_write_to_pane(cmd.as_bytes(), new_pane);
    }

    /// Split a specific pane by ID.
    pub(crate) fn split_pane_by_id(&mut self, target_id: PaneId, direction: SplitDirection) {
        // tn-beez Phase B: daemon mode routes through the daemon so the
        // new pane carries a daemon id (visible to MCP / persisted).
        if self.is_daemon_mode() {
            self.split_pane_remote(
                target_id,
                direction,
                DaemonSplitOnComplete::FocusAndRelayout,
            );
            return;
        }
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
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let base_spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        // Direct field access needed here: layout_mut + renderer + config must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        // Inherit source pane's cwd (from OSC 7) or use home depending on config.
        let spawn_options = split_spawn_options(
            &base_spawn_options,
            layout,
            target_id,
            self.config.general.new_pane_cwd,
        );

        let post_split_header_h = crate::pane::effective_header_height(
            layout.pane_count() + 1,
            self.config.general.show_pane_headers,
        );

        let new_id =
            layout.split_pane(
                target_id,
                direction,
                |viewport| match crate::pane::spawn_pane(
                    viewport,
                    renderer,
                    scrollback,
                    interceptor_cfg.clone(),
                    scan_interval_secs,
                    &spawn_options,
                    registry.clone(),
                    |pane_id| make_pane_callbacks(&proxy, pane_id),
                    post_split_header_h,
                ) {
                    Ok(pane) => Some(pane),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to spawn pane for split");
                        None
                    }
                },
            );

        if let Some(new_id) = new_id {
            info!(
                "Split pane {target_id} {:?} -> new pane {new_id}",
                direction
            );
            self.last_split_direction = direction;
            self.set_focused_pane(Some(new_id));
            self.relayout_and_redraw();
            self.publish_workspace_state();
        } else {
            self.request_redraw();
        }
    }

    /// Spawn N panes with auto-tiling layout.
    /// Creates panes one at a time using the existing split infrastructure,
    /// with a single relayout at the end.
    #[allow(dead_code)]
    pub(crate) fn spawn_n_panes(&mut self, n: usize) {
        if n == 0 {
            return;
        }

        if self.workspaces.is_none() {
            info!("No layout exists, cannot spawn panes without initial setup");
            return;
        }

        for _ in 0..n {
            self.split_focused_pane_auto();
        }

        info!("Spawned {n} additional panes via auto-split");
    }
}
