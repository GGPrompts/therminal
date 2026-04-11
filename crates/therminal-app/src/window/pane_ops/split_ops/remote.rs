//! Daemon-mode async split path: `split_pane_remote` fires the RPC,
//! `finish_split_pane_remote` mounts the new pane on the main thread when
//! the RPC completes.

use std::sync::Arc;

use tracing::{info, warn};

use crate::pane::SplitDirection;
use therminal_protocol::daemon::{IpcRequest, IpcResponse};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::{DAEMON_OP_TIMEOUT, cwd_from_source_pane, make_pane_callbacks};
use super::{DaemonSplitOnComplete, DaemonSplitResult};
use crate::pane::PaneId;
use crate::window::{App, UserEvent};

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
        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);
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
                        super::super::editor_clipboard::shell_quote(
                            &jsonl_path.display().to_string()
                        ),
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
}
