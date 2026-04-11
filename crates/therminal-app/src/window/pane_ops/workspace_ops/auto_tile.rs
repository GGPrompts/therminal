//! Auto-tile debouncer drain: `poll_auto_tile`. Subscribed to
//! `AgentRegistry` events upstream and yields `AutoTileAction`s here for
//! application against the active layout.

use std::sync::Arc;

use tracing::info;

use crate::pane::{LayoutNode, SplitDirection};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::split_ops::DaemonSplitOnComplete;
use super::super::{make_pane_callbacks, split_spawn_options};
use crate::window::App;

impl App {
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
                    // Inherit source pane's cwd (from OSC 7) or use home depending on config.
                    let spawn_options = split_spawn_options(
                        &base_spawn_options,
                        layout,
                        target_pane_id,
                        self.config.general.new_pane_cwd,
                    );

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
}
