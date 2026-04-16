//! Workspace reconciliation (tn-9jhx): sync GUI layout with daemon state
//! after external topology mutations (MCP tools, CLI commands).
//!
//! When the daemon broadcasts `WorkspaceChanged`, the GUI queries
//! `GetWorkspaces` to learn the authoritative layout, diffs it against
//! the local pane-id map, builds `PaneState` objects for any new daemon
//! panes, and delivers a `ReconcileResult` to the main thread via
//! `UserEvent::DaemonReconcilePanesReady`. The main thread then tears
//! down removed panes and splices new ones into the layout tree.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{info, warn};
use winit::event_loop::EventLoopProxy;

use therminal_daemon_client::DaemonClient;
use therminal_protocol::daemon::{IpcRequest, IpcResponse, WorkspaceInfo};
use therminal_terminal::interceptor::InterceptorConfig;

use super::pane_ops::DAEMON_OP_TIMEOUT;
use super::{App, NotificationSource, ReconcileResult, UserEvent};
use crate::pane::PaneId;

impl App {
    /// Handle a `DaemonWorkspaceChanged` event on the main thread.
    ///
    /// If the session matches ours and we have the required daemon client +
    /// runtime, kick off the async reconciliation pass. Otherwise ignore.
    pub(super) fn handle_daemon_workspace_changed(
        &mut self,
        session_id: therminal_protocol::SessionId,
    ) {
        // Ignore events for sessions we don't own.
        let Some(our_session) = self.daemon_session_id else {
            return;
        };
        if session_id != our_session {
            return;
        }

        let Some(client) = self.daemon_client.as_ref() else {
            return;
        };
        let Some(handle) = self.daemon_runtime.as_ref() else {
            return;
        };

        let client = Arc::clone(client);
        let handle = handle.clone();
        let proxy = self.event_proxy.clone();
        let known_daemon_ids: HashSet<therminal_protocol::PaneId> =
            self.pane_id_map.all_daemon_ids().into_iter().collect();

        let scrollback = self.config.general.scrollback_lines;
        let interceptor_config = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
            osc_7337: self.config.terminal.osc_7337,
        };
        let daemon_socket = client.socket_path().to_path_buf();

        handle.clone().spawn(async move {
            match build_reconcile_result(
                client,
                handle,
                proxy.clone(),
                session_id,
                known_daemon_ids,
                scrollback,
                interceptor_config,
                daemon_socket,
            )
            .await
            {
                Ok(result) => {
                    let _ =
                        proxy.send_event(UserEvent::DaemonReconcilePanesReady(Box::new(result)));
                }
                Err(e) => {
                    warn!(error = %e, "workspace reconciliation failed");
                }
            }
        });
    }

    /// Apply a completed reconciliation result on the main thread.
    pub(super) fn apply_reconcile_result(&mut self, result: ReconcileResult) {
        let ReconcileResult {
            workspaces,
            active_workspace,
            new_panes,
            removed_daemon_ids,
        } = result;

        // 1. Harvest all existing PaneState objects from the current layout
        //    so we can reuse them (preserving scrollback, cursor, etc.)
        //    when rebuilding from daemon state.
        let mut existing_panes: std::collections::HashMap<
            therminal_protocol::PaneId,
            crate::pane::PaneState,
        > = std::collections::HashMap::new();
        if let Some(wm) = self.workspaces.as_mut() {
            // Collect local IDs first, then extract (can't borrow wm twice).
            let local_ids: Vec<PaneId> = wm
                .iter_workspaces()
                .flat_map(|ws| ws.layout.pane_ids())
                .collect();
            for local_id in local_ids {
                if let Some(daemon_id) = self.pane_id_map.daemon_for_local(local_id) {
                    // Skip panes that the daemon removed — let them drop.
                    if removed_daemon_ids.contains(&daemon_id) {
                        info!(
                            local_id,
                            daemon_id, "reconcile: dropping pane (daemon removed it)"
                        );
                        self.pane_id_map.remove_by_local(local_id);
                        continue;
                    }
                    // Extract from the layout to reuse later.
                    for ws in wm.iter_workspaces_mut() {
                        if let Some(state) = ws.layout.extract_pane(local_id) {
                            existing_panes.insert(daemon_id, state);
                            break;
                        }
                    }
                }
            }
        }

        // 2. Pool newly-built PaneStates for daemon panes that didn't
        //    exist locally.
        let mut new_pane_map: std::collections::HashMap<
            therminal_protocol::PaneId,
            crate::pane::PaneState,
        > = new_panes
            .into_iter()
            .map(|(_, daemon_id, state)| (daemon_id, state))
            .collect();

        // 3. Rebuild the workspace manager from daemon WorkspaceInfo,
        //    reusing existing PaneStates where available and splicing in
        //    the freshly-built ones for new panes.
        let mut id_pairs: Vec<(PaneId, therminal_protocol::PaneId)> = Vec::new();
        let mut make_leaf =
            |daemon_pane_id: therminal_protocol::PaneId| -> Option<crate::pane::PaneState> {
                // Try to reuse an existing local pane first.
                if let Some(state) = existing_panes.remove(&daemon_pane_id) {
                    id_pairs.push((state.id, daemon_pane_id));
                    return Some(state);
                }
                // Try a freshly-built pane from the reconcile pass.
                if let Some(state) = new_pane_map.remove(&daemon_pane_id) {
                    id_pairs.push((state.id, daemon_pane_id));
                    return Some(state);
                }
                warn!(
                    daemon_pane_id,
                    "reconcile: no PaneState for daemon pane — skipping"
                );
                None
            };

        let new_wm = crate::pane::WorkspaceManager::from_workspace_info(
            &workspaces,
            active_workspace,
            &mut make_leaf,
        );

        match new_wm {
            Some(wm) => {
                // Rebuild the pane id map from scratch to match the new layout.
                self.pane_id_map = super::PaneIdMap::default();
                for (local, daemon) in &id_pairs {
                    self.pane_id_map.insert(*local, *daemon);
                }
                self.workspaces = Some(wm);
                info!(
                    panes = id_pairs.len(),
                    "reconcile: layout rebuilt from daemon state"
                );
            }
            None => {
                warn!("reconcile: from_workspace_info returned None — layout unchanged");
                return;
            }
        }

        // 4. Relayout and redraw.
        self.relayout_and_redraw();
    }
}

/// Async half: query daemon state and build PaneStates for new panes.
#[allow(clippy::too_many_arguments)]
async fn build_reconcile_result(
    client: Arc<DaemonClient>,
    handle: tokio::runtime::Handle,
    proxy: EventLoopProxy<UserEvent>,
    session_id: therminal_protocol::SessionId,
    known_daemon_ids: HashSet<therminal_protocol::PaneId>,
    scrollback: usize,
    interceptor_config: InterceptorConfig,
    daemon_socket: std::path::PathBuf,
) -> anyhow::Result<ReconcileResult> {
    // 1. Query authoritative workspace state.
    let resp = tokio::time::timeout(
        DAEMON_OP_TIMEOUT,
        client.send_request(IpcRequest::GetWorkspaces { session_id }),
    )
    .await
    .map_err(|_| anyhow::anyhow!("GetWorkspaces timed out"))??;

    let (workspaces, active_workspace) = match resp {
        IpcResponse::Workspaces {
            workspaces,
            active_workspace,
            ..
        } => (workspaces, active_workspace),
        IpcResponse::Error { message } => {
            anyhow::bail!("GetWorkspaces failed: {message}");
        }
        other => {
            anyhow::bail!("unexpected GetWorkspaces response: {other:?}");
        }
    };

    // 2. Collect all daemon pane IDs from the workspace list.
    let daemon_pane_ids: HashSet<therminal_protocol::PaneId> = workspaces
        .iter()
        .flat_map(collect_pane_ids_from_workspace)
        .collect();

    // 3. Diff: find new and removed panes.
    let new_daemon_ids: Vec<therminal_protocol::PaneId> = daemon_pane_ids
        .difference(&known_daemon_ids)
        .copied()
        .collect();
    let removed_daemon_ids: Vec<therminal_protocol::PaneId> = known_daemon_ids
        .difference(&daemon_pane_ids)
        .copied()
        .collect();

    info!(
        new = new_daemon_ids.len(),
        removed = removed_daemon_ids.len(),
        total_daemon = daemon_pane_ids.len(),
        total_local = known_daemon_ids.len(),
        "reconcile: pane diff computed"
    );

    // 3b. Fetch pane summaries so we can restore per-pane state (e.g.
    //     pinned flag, tn-tl6u) when building new PaneStates.
    let pinned_map: HashMap<therminal_protocol::PaneId, bool> = {
        match tokio::time::timeout(
            DAEMON_OP_TIMEOUT,
            client.send_request(IpcRequest::ListPanes {
                session_id: Some(session_id),
            }),
        )
        .await
        {
            Ok(Ok(IpcResponse::Panes { panes })) => {
                panes.into_iter().map(|s| (s.pane_id, s.pinned)).collect()
            }
            Ok(Ok(other)) => {
                warn!(
                    ?other,
                    "reconcile: unexpected response to ListPanes — pinned state will default to false"
                );
                HashMap::new()
            }
            Ok(Err(e)) => {
                warn!(error = %e, "reconcile: ListPanes failed — pinned state will default to false");
                HashMap::new()
            }
            Err(_) => {
                warn!("reconcile: ListPanes timed out — pinned state will default to false");
                HashMap::new()
            }
        }
    };

    // 4. Build PaneState for each new daemon pane. These need a full
    //    RemotePty setup (dedicated connection, worker thread, etc.).
    //    Use a placeholder viewport — the main thread will relayout.
    //
    //    tn-x2yh: `build_remote_pane_state` uses `Handle::block_on`
    //    internally, which panics inside a tokio runtime context. Since
    //    this function runs as a spawned async task, we must move each
    //    build call onto the blocking thread pool via `spawn_blocking`.
    //    The blocking pool is separate from the (single) async worker
    //    thread, so `block_on` from the blocking thread can schedule
    //    futures back onto the worker without deadlocking.
    let placeholder_viewport = therminal_core::geometry::Rect::new(0.0, 0.0, 800.0, 600.0);
    let (cols, rows) = (80usize, 24usize);

    let mut new_panes = Vec::with_capacity(new_daemon_ids.len());
    for daemon_pane_id in new_daemon_ids {
        let local_id = crate::pane::next_pane_id();
        let p1 = proxy.clone();
        let p2 = proxy.clone();
        let p3 = proxy.clone();
        let p4 = proxy.clone();
        let on_exit_local_id = local_id;
        let on_bell_local_id = local_id;
        let callbacks = crate::pane::PaneCallbacks {
            wake: Box::new(move || {
                let _ = p1.send_event(UserEvent::PtyOutput);
            }),
            on_exit: Box::new(move || {
                let _ = p2.send_event(UserEvent::PaneExited(on_exit_local_id));
            }),
            on_bell: Box::new(move || {
                let _ = p3.send_event(UserEvent::Bell(on_bell_local_id));
            }),
            on_notification: Box::new(move |text| {
                let _ = p4.send_event(UserEvent::DesktopNotification {
                    title: "Therminal".to_string(),
                    body: text,
                    source: NotificationSource::Osc9,
                });
            }),
        };
        let client_for_build = Arc::clone(&client);
        let handle_for_build = handle.clone();
        let socket_for_build = daemon_socket.clone();
        let icfg = interceptor_config.clone();
        // tn-tl6u: look up pinned state before moving into the blocking closure.
        let is_pinned = pinned_map.get(&daemon_pane_id).copied().unwrap_or(false);
        let result = tokio::task::spawn_blocking(move || {
            crate::pane::remote_spawn::build_remote_pane_state(
                local_id,
                daemon_pane_id,
                placeholder_viewport,
                cols,
                rows,
                scrollback,
                icfg,
                client_for_build,
                handle_for_build,
                socket_for_build,
                callbacks,
                None,
                // tn-alpb: reconcile runs in an async task without access
                // to the App's agent_registry. PaneStatus.agent_name is
                // still updated by the forwarder's AgentChanged handler;
                // the AgentRegistry update is skipped here — the pane
                // header will show agent info once the next daemon-side
                // scan fires an AgentChanged event and the init/split
                // paths (which DO pass the registry) handle it.
                None,
                None,      // tn-s8w3: swarm_tx not available in reconcile context
                None,      // swarm_wake
                is_pinned, // tn-tl6u: restore pinned state from daemon
            )
        })
        .await;
        match result {
            Ok(Ok(state)) => {
                new_panes.push((local_id, daemon_pane_id, state));
            }
            Ok(Err(e)) => {
                warn!(
                    daemon_pane_id,
                    error = %e,
                    "reconcile: build_remote_pane_state failed — skipping pane"
                );
            }
            Err(e) => {
                warn!(
                    daemon_pane_id,
                    error = %e,
                    "reconcile: build_remote_pane_state panicked — skipping pane"
                );
            }
        }
    }

    Ok(ReconcileResult {
        workspaces,
        active_workspace,
        new_panes,
        removed_daemon_ids,
    })
}

/// Extract all pane IDs from a workspace (walking the layout snapshot tree).
fn collect_pane_ids_from_workspace(ws: &WorkspaceInfo) -> Vec<therminal_protocol::PaneId> {
    match &ws.layout {
        Some(snap) => collect_pane_ids_from_snapshot(snap),
        None => ws.pane_ids.clone(),
    }
}

fn collect_pane_ids_from_snapshot(
    snap: &therminal_protocol::daemon::LayoutSnapshot,
) -> Vec<therminal_protocol::PaneId> {
    use therminal_protocol::daemon::LayoutSnapshot;
    match snap {
        LayoutSnapshot::Leaf { pane_id } => vec![*pane_id],
        LayoutSnapshot::Split { first, second, .. } => {
            let mut ids = collect_pane_ids_from_snapshot(first);
            ids.extend(collect_pane_ids_from_snapshot(second));
            ids
        }
    }
}
