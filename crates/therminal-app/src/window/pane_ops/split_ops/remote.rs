//! Daemon-mode async split path: `split_pane_remote` fires the RPC,
//! `finish_split_pane_remote` mounts the new pane on the main thread when
//! the RPC completes.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::pane::SplitDirection;
use therminal_protocol::daemon::{IpcRequest, IpcResponse};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::{DAEMON_OP_TIMEOUT, cwd_from_source_pane, make_pane_callbacks};
use super::{DaemonSplitOnComplete, DaemonSplitResult};
use crate::pane::PaneId;
use crate::window::{App, UserEvent};

/// Decide what the remote split path should do when the source pane has no
/// daemon mapping (tn-7mvn).
///
/// Factored out for unit testing — lets us verify the FocusAndRelayout
/// fallback fires for the standard header-click path (WebView or otherwise)
/// and that other completion variants still bail rather than silently
/// producing a local pane.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NoDaemonMappingDecision {
    /// Dispatch to the local split path (GUI-only fallback).
    FallbackLocal,
    /// Log and bail — completion variant can't be served by a local-only
    /// pane (e.g. NewWorkspace, WriteBytesAndFocus).
    Bail,
}

pub(crate) fn decide_no_daemon_mapping(
    on_complete: &DaemonSplitOnComplete,
) -> NoDaemonMappingDecision {
    match on_complete {
        DaemonSplitOnComplete::FocusAndRelayout => NoDaemonMappingDecision::FallbackLocal,
        DaemonSplitOnComplete::AutoTile { .. }
        | DaemonSplitOnComplete::WriteBytesAndFocus { .. }
        | DaemonSplitOnComplete::NewWorkspace { .. } => NoDaemonMappingDecision::Bail,
    }
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
                // tn-7mvn: no daemon mapping means the source pane is
                // GUI-local (e.g. a WebView pane — never round-tripped
                // through the daemon). Instead of silently dropping the
                // user's click, fall back to the local split path for
                // the standard FocusAndRelayout case. The new terminal
                // sibling stays GUI-local too (not visible to MCP), which
                // is fine — WebView panes are GUI-only by nature.
                //
                // Other completion variants (AutoTile, WriteBytesAndFocus,
                // NewWorkspace) are only issued against daemon-mapped
                // source panes in practice, so they keep the original
                // bail-and-log behavior.
                match decide_no_daemon_mapping(&on_complete) {
                    NoDaemonMappingDecision::FallbackLocal => {
                        debug!(
                            source_local,
                            "split_pane_remote: no daemon mapping (GUI-only source); falling back to local split"
                        );
                        self.split_pane_by_id_local(source_local, direction);
                    }
                    NoDaemonMappingDecision::Bail => {
                        warn!(
                            source_local,
                            ?on_complete,
                            "split_pane_remote: no daemon id mapping and non-default on_complete; bailing"
                        );
                    }
                }
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
                    profile: None,
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
            osc_7337: self.config.terminal.osc_7337,
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
        let callbacks = make_pane_callbacks(&proxy, local_id);
        // Hoist borrows of `self` before the mutable `layout` borrow to avoid
        // borrow conflicts with `layout.split_pane` which holds &mut self.
        let agent_registry_for_closure = Arc::clone(&self.agent_registry);
        let swarm_tx_for_closure = self.swarm_debouncer_tx.clone();
        let swarm_wake_for_closure = self.swarm_wake_callback();
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
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
                Some(agent_registry_for_closure),
                swarm_tx_for_closure,
                swarm_wake_for_closure,
                false, // new split pane — not pinned
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

#[cfg(test)]
mod tests {
    use super::*;

    // tn-7mvn: when a WebView (GUI-only) pane is split from its header,
    // split_pane_remote no longer has a daemon id to RPC against. The
    // `decide_no_daemon_mapping` helper encodes the "fall back to local
    // split" vs "bail with a log" decision for every completion variant.

    #[test]
    fn fallback_to_local_for_focus_and_relayout() {
        // The standard header-click path — clicking H/V on a WebView
        // header arrives here. Must fall back to local, NOT bail.
        assert_eq!(
            decide_no_daemon_mapping(&DaemonSplitOnComplete::FocusAndRelayout),
            NoDaemonMappingDecision::FallbackLocal,
        );
    }

    #[test]
    fn bail_for_auto_tile() {
        // AutoTile is only fired for daemon-mapped agent panes. A WebView
        // would never trigger it, so bail rather than silently producing
        // a local pane that skips the auto-tile debouncer registration.
        assert_eq!(
            decide_no_daemon_mapping(&DaemonSplitOnComplete::AutoTile { parent_pane_id: 7 }),
            NoDaemonMappingDecision::Bail,
        );
    }

    #[test]
    fn bail_for_write_bytes_and_focus() {
        // WriteBytesAndFocus needs the daemon pane to send keys into —
        // a local-mode pane doesn't round-trip through SendKeys RPC, so
        // the bytes would be dropped. Bail.
        assert_eq!(
            decide_no_daemon_mapping(&DaemonSplitOnComplete::WriteBytesAndFocus {
                bytes: b"ls\n".to_vec(),
                toast: None,
            }),
            NoDaemonMappingDecision::Bail,
        );
    }

    #[test]
    fn bail_for_new_workspace() {
        // NewWorkspace builds a pane state for the full viewport and
        // inserts it as the sole leaf in a new workspace tab — the local
        // split path inserts into the current workspace layout, so
        // falling back would silently violate the caller's intent. Bail.
        assert_eq!(
            decide_no_daemon_mapping(&DaemonSplitOnComplete::NewWorkspace { workspace_id: 3 }),
            NoDaemonMappingDecision::Bail,
        );
    }

    #[test]
    fn default_variant_is_focus_and_relayout_and_falls_back() {
        // The enum's #[default] is FocusAndRelayout. If someone ever flips
        // the default, this test will fail loudly — the fallback behavior
        // is load-bearing for the WebView-header-click UX fix.
        let default = DaemonSplitOnComplete::default();
        assert!(matches!(default, DaemonSplitOnComplete::FocusAndRelayout));
        assert_eq!(
            decide_no_daemon_mapping(&default),
            NoDaemonMappingDecision::FallbackLocal,
        );
    }
}
