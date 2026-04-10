//! Workspace operations: restore_layout, rebuild_from_snapshot,
//! rebuild_from_snapshot_remote, switch_workspace, create_new_workspace,
//! send_to_workspace, poll_auto_tile, poll_swarm_watcher.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::pane::{LayoutNode, LayoutSnapshot, SplitDirection};
use therminal_core::geometry::Rect;
use therminal_protocol::daemon::{IpcRequest, IpcResponse};
use therminal_terminal::interceptor::InterceptorConfig;

use super::split_ops::DaemonSplitOnComplete;
use super::{make_pane_callbacks, split_spawn_options};
use crate::window::App;

impl App {
    /// Restore a previously saved layout by respawning panes to match the snapshot.
    pub(crate) fn restore_layout(&mut self) {
        let snapshot = match self
            .workspaces
            .as_mut()
            .and_then(|wm| wm.take_saved_layout())
        {
            Some(s) => s,
            None => {
                info!("No saved layout to restore");
                return;
            }
        };

        let leaf_count = LayoutNode::snapshot_leaf_count(&snapshot);
        if leaf_count == 0 {
            return;
        }

        // If there's already a layout, close it first (no re-snapshot).
        if self.get_layout().is_some() {
            let layout = self.workspaces.as_mut().unwrap().take_layout();
            drop(layout);
            self.set_focused_pane(None);
        }

        let full_rect = match self.compute_layout_rect() {
            Some(r) => r,
            None => return,
        };

        // tn-fi1k Phase B: in daemon mode, route the rebuild through
        // SplitPane / CreateSession so each leaf carries a daemon id.
        if self.is_daemon_mode() {
            match self.rebuild_from_snapshot_remote(&snapshot, full_rect) {
                Some(node) => {
                    if let Some(wm) = self.workspaces.as_mut() {
                        wm.set_layout(node);
                    }
                    let first_id = self
                        .get_layout()
                        .map(|l| l.pane_ids())
                        .and_then(|ids| ids.first().copied());
                    let pane_count = self.get_layout().map(|l| l.pane_ids().len()).unwrap_or(0);
                    self.set_focused_pane(first_id);
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                    info!(
                        panes = pane_count,
                        "Restored layout from snapshot (daemon mode)"
                    );
                }
                None => {
                    warn!("Failed to restore layout from snapshot (daemon mode)");
                }
            }
            return;
        }

        // Local-mode rebuild path: unchanged.
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
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            env: self.config.general.env.clone(),
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();

        match self.rebuild_from_snapshot(
            &snapshot,
            full_rect,
            renderer,
            scrollback,
            &interceptor_cfg,
            scan_interval_secs,
            &spawn_options,
            &proxy,
        ) {
            Some(node) => {
                if let Some(wm) = self.workspaces.as_mut() {
                    wm.set_layout(node);
                }
                // Focus the first pane (must read IDs before relayout borrows layout).
                let first_id = self
                    .get_layout()
                    .map(|l| l.pane_ids())
                    .and_then(|ids| ids.first().copied());
                let pane_count = self.get_layout().map(|l| l.pane_ids().len()).unwrap_or(0);
                self.set_focused_pane(first_id);
                self.relayout_and_redraw();

                info!(panes = pane_count, "Restored layout from snapshot");
            }
            None => {
                warn!("Failed to restore layout from snapshot");
            }
        }
    }

    /// Recursively rebuild a LayoutNode tree from a snapshot.
    #[allow(clippy::too_many_arguments)]
    fn rebuild_from_snapshot(
        &self,
        snapshot: &LayoutSnapshot,
        rect: Rect,
        renderer: &crate::grid_renderer::GridRenderer,
        scrollback: usize,
        interceptor_cfg: &InterceptorConfig,
        scan_interval_secs: u64,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
        proxy: &super::super::EventLoopProxy<super::super::UserEvent>,
    ) -> Option<LayoutNode> {
        use crate::pane::SEPARATOR_GAP;

        match snapshot {
            LayoutSnapshot::Leaf => {
                let cfg = interceptor_cfg.clone();
                let registry = Some(Arc::clone(&self.agent_registry));
                match crate::pane::spawn_pane(
                    rect,
                    renderer,
                    scrollback,
                    cfg,
                    scan_interval_secs,
                    spawn_options,
                    registry,
                    |pane_id| make_pane_callbacks(proxy, pane_id),
                    0.0, // restore: relayout_and_redraw will correct
                ) {
                    Ok(pane) => Some(LayoutNode::Leaf(pane)),
                    Err(e) => {
                        warn!(error = %e, "failed to spawn pane during layout restore");
                        None
                    }
                }
            }
            LayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (r1, r2) = match direction {
                    SplitDirection::Horizontal => {
                        rect.split_horizontal_ratio(*ratio, SEPARATOR_GAP)
                    }
                    SplitDirection::Vertical => rect.split_vertical_ratio(*ratio, SEPARATOR_GAP),
                };

                let first_node = self.rebuild_from_snapshot(
                    first,
                    r1,
                    renderer,
                    scrollback,
                    interceptor_cfg,
                    scan_interval_secs,
                    spawn_options,
                    proxy,
                )?;
                let second_node = self.rebuild_from_snapshot(
                    second,
                    r2,
                    renderer,
                    scrollback,
                    interceptor_cfg,
                    scan_interval_secs,
                    spawn_options,
                    proxy,
                )?;

                Some(LayoutNode::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first_node),
                    second: Box::new(second_node),
                })
            }
        }
    }

    /// tn-fi1k: daemon-mode counterpart to `rebuild_from_snapshot`.
    ///
    /// Walks the snapshot tree and allocates each leaf via the daemon
    /// (`SplitPane` against an existing daemon pane, falling back to
    /// `CreateSession` for the very first leaf when the GUI has no
    /// existing daemon panes left to anchor against). The first
    /// successfully spawned leaf is then used as the anchor for all
    /// subsequent splits.
    ///
    /// Returns `None` if any leaf fails to spawn — partial trees are
    /// dropped to avoid leaking daemon panes (the helpers
    /// `spawn_remote_pane_off_existing` / `spawn_remote_pane_fresh_session`
    /// already best-effort cleanup their own failures).
    fn rebuild_from_snapshot_remote(
        &mut self,
        snapshot: &LayoutSnapshot,
        rect: Rect,
    ) -> Option<LayoutNode> {
        use crate::pane::SEPARATOR_GAP;

        match snapshot {
            LayoutSnapshot::Leaf => {
                // Pick an anchor: any existing daemon pane in the map.
                // If none exist (we just took the layout), fall back to
                // CreateSession.
                let state = if let Some(anchor) = self.pane_id_map.any_daemon_id() {
                    self.spawn_remote_pane_off_existing(anchor, rect)?
                } else {
                    self.spawn_remote_pane_fresh_session(rect)?
                };
                Some(LayoutNode::Leaf(state))
            }
            LayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (r1, r2) = match direction {
                    SplitDirection::Horizontal => {
                        rect.split_horizontal_ratio(*ratio, SEPARATOR_GAP)
                    }
                    SplitDirection::Vertical => rect.split_vertical_ratio(*ratio, SEPARATOR_GAP),
                };
                let first_node = self.rebuild_from_snapshot_remote(first, r1)?;
                let second_node = self.rebuild_from_snapshot_remote(second, r2)?;
                Some(LayoutNode::Split {
                    direction: *direction,
                    ratio: *ratio,
                    first: Box::new(first_node),
                    second: Box::new(second_node),
                })
            }
        }
    }

    // ── Workspace operations ──────────────────────────────────────────

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
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
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
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
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

    // ── Auto-tile ───────────────────────────────────────────────────────

    /// Poll the auto-tile debouncer and apply any ready actions.
    pub(crate) fn poll_auto_tile(&mut self) {
        let actions = match self.auto_tile_debouncer.as_mut() {
            Some(debouncer) => debouncer.poll(),
            None => return,
        };

        for action in actions {
            match action {
                crate::pane::AutoTileAction::Split {
                    parent_pane_id,
                    agent_name,
                    ..
                } => {
                    // WM-style: split the largest pane instead of always
                    // splitting the parent -- avoids tiny unusable panes
                    // from nested binary splits.
                    let target_pane_id = self
                        .get_layout()
                        .and_then(|l| l.find_largest_pane())
                        .unwrap_or(parent_pane_id);

                    info!(
                        parent_pane_id,
                        target_pane_id, agent_name, "Auto-tiling: splitting largest pane for agent"
                    );
                    // Determine split direction from target pane's viewport.
                    let direction = self
                        .get_layout()
                        .and_then(|l| l.find_pane(target_pane_id))
                        .map(|p| {
                            LayoutNode::auto_split_direction(p.viewport, SplitDirection::Horizontal)
                        })
                        .unwrap_or(SplitDirection::Horizontal);

                    // tn-ll6l: in daemon mode, route through split_pane_remote
                    // so the new pane carries a daemon id (visible to MCP /
                    // persisted across daemon restart) and the
                    // pane_id_map stays consistent for publish_workspace_state.
                    if self.is_daemon_mode() {
                        self.split_pane_remote(
                            target_pane_id,
                            direction,
                            DaemonSplitOnComplete::AutoTile { parent_pane_id },
                        );
                        continue;
                    }

                    // Perform the split (reuses existing split_pane_by_id logic).
                    let renderer = match self.grid_renderer.as_ref() {
                        Some(r) => r,
                        None => continue,
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
                        env: self.config.general.env.clone(),
                        ..Default::default()
                    };
                    let proxy = self.event_proxy.clone();
                    let registry = Some(Arc::clone(&self.agent_registry));
                    let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
                        Some(l) => l,
                        None => continue,
                    };
                    // Inherit source pane's cwd (from OSC 7).
                    let spawn_options =
                        split_spawn_options(&base_spawn_options, layout, target_pane_id);

                    let post_split_header_h = crate::pane::effective_header_height(
                        layout.pane_count() + 1,
                        self.config.general.show_pane_headers,
                    );

                    let new_id = layout.split_pane(target_pane_id, direction, |viewport| {
                        match crate::pane::spawn_pane(
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
                                tracing::warn!(
                                    error = %e,
                                    "failed to spawn pane for auto-tile split"
                                );
                                None
                            }
                        }
                    });

                    if let Some(new_id) = new_id {
                        info!(parent_pane_id, new_id, "Auto-tile split complete");
                        // Register the auto-tiled pane so we can reclaim it later.
                        if let Some(ref mut debouncer) = self.auto_tile_debouncer {
                            debouncer.register_auto_tiled(parent_pane_id, new_id);
                        }
                        // Don't change focus for auto-tiled panes.
                        self.relayout_and_redraw();
                    }
                }
                crate::pane::AutoTileAction::Reclaim { pane_id } => {
                    info!(pane_id, "Auto-tiling: reclaiming pane after agent exit");
                    self.close_pane_by_id(pane_id);
                    // Clean up any Empty leaves and rebalance after reclaim.
                    if let Some(layout) = self.get_layout_mut() {
                        layout.compact_layout();
                    }
                }
            }
        }
    }

    // ── Swarm watcher integration ──────────────────────────────────────

    /// Drain the `SwarmDebouncer` and dispatch any expired spawn/reclaim
    /// events. Called from `handle_redraw_requested` (parallel to
    /// `poll_auto_tile`) and from the `SwarmWatcherTick` user-event handler.
    pub(crate) fn poll_swarm_watcher(&mut self) {
        // Refresh the shared pane-pid list for the swarm watcher's
        // owned-session computation. Cheap (visit each leaf once) and only
        // populated when the user opted into `swarm_watch_scope = "current"`.
        if let Some(provider) = self.swarm_pane_pids.clone() {
            let pids = self
                .workspaces
                .as_ref()
                .map(|w| w.collect_all_root_pids())
                .unwrap_or_default();
            if let Ok(mut g) = provider.lock() {
                *g = pids;
            }
        }

        let events = match self.swarm_debouncer.as_mut() {
            Some(d) => d.poll(),
            None => return,
        };
        for event in events {
            match event {
                crate::pane::swarm_watcher::SwarmWatcherEvent::SpawnSubagent {
                    agent_id,
                    jsonl_path,
                } => {
                    self.spawn_subagent_pane(agent_id, jsonl_path);
                }
                crate::pane::swarm_watcher::SwarmWatcherEvent::ReclaimSubagent { agent_id } => {
                    self.reclaim_subagent_pane(&agent_id);
                }
            }
        }
    }

    /// Open a new pane that tails a Claude subagent JSONL file.
    ///
    /// Splits the largest existing pane (mirroring `poll_auto_tile`) and
    /// writes a `tail -F <path>` command into the new PTY so the user sees the
    /// subagent's events as they're written.
    pub(crate) fn spawn_subagent_pane(&mut self, agent_id: String, jsonl_path: std::path::PathBuf) {
        if self.swarm_panes.contains_key(&agent_id) {
            debug!(agent = %agent_id, "swarm: pane already exists, ignoring duplicate spawn");
            return;
        }

        // Restore zoom before splitting so the new pane joins the full tree.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }

        let target_pane_id = match self.get_layout().and_then(|l| l.find_largest_pane()) {
            Some(id) => id,
            None => {
                warn!("swarm: no panes available to split for subagent");
                return;
            }
        };

        // tn-ll6l: in daemon mode, route through split_pane_remote so the
        // new pane is daemon-managed. SplitPane has no command-override, so
        // we follow up with a SendKeys RPC carrying the `tail` command.
        // Both the split and the SendKeys are deferred off the event loop;
        // the SwarmTail on_complete handler fires them in sequence once the
        // pane is mounted.
        if self.is_daemon_mode() {
            let direction = self
                .get_layout()
                .and_then(|l| l.find_pane(target_pane_id))
                .map(|p| LayoutNode::auto_split_direction(p.viewport, SplitDirection::Horizontal))
                .unwrap_or(SplitDirection::Horizontal);

            self.split_pane_remote(
                target_pane_id,
                direction,
                DaemonSplitOnComplete::SwarmTail {
                    agent_id,
                    jsonl_path,
                },
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
            env: self.config.general.env.clone(),
            ..Default::default()
        };

        let direction = self
            .get_layout()
            .and_then(|l| l.find_pane(target_pane_id))
            .map(|p| LayoutNode::auto_split_direction(p.viewport, SplitDirection::Horizontal))
            .unwrap_or(SplitDirection::Horizontal);

        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        // Inherit source pane's cwd (from OSC 7).
        let spawn_options = split_spawn_options(&base_spawn_options, layout, target_pane_id);

        let post_split_header_h = crate::pane::effective_header_height(
            layout.pane_count() + 1,
            self.config.general.show_pane_headers,
        );

        let new_id =
            layout.split_pane(
                target_pane_id,
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
                        warn!(error = %e, "swarm: failed to spawn pane");
                        None
                    }
                },
            );

        let Some(new_id) = new_id else { return };

        info!(
            agent = %agent_id,
            pane_id = new_id,
            jsonl = %jsonl_path.display(),
            "swarm: spawned pane tailing subagent JSONL"
        );

        // Write the tail command into the new pane's PTY. We do this after
        // a short delay isn't required — the PTY reader is already running.
        // Use `--lines=+1` to start from the top of the file so the full
        // subagent transcript is captured.
        if let Some(pane) = layout.find_pane_mut(new_id) {
            let cmd = format!(
                "clear && tail --lines=+1 -F {}\n",
                super::editor_clipboard::shell_quote(&jsonl_path.display().to_string()),
            );
            if let Err(e) = pane.write_input(cmd.as_bytes()) {
                warn!(error = %e, "swarm: failed to write tail command to new pane");
            }
        }

        self.swarm_panes.insert(agent_id, new_id);
        self.relayout_and_redraw();
    }

    /// Close the pane that was tailing a subagent's JSONL.
    pub(crate) fn reclaim_subagent_pane(&mut self, agent_id: &str) {
        let Some(pane_id) = self.swarm_panes.remove(agent_id) else {
            debug!(agent = %agent_id, "swarm: reclaim for unknown agent, ignoring");
            return;
        };
        info!(agent = %agent_id, pane_id, "swarm: reclaiming pane after subagent stale");
        self.close_pane_by_id(pane_id);
        if let Some(layout) = self.get_layout_mut() {
            layout.compact_layout();
        }
    }
}
