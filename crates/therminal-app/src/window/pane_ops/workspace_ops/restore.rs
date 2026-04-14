//! Layout restore: `restore_layout` plus the local and daemon-mode
//! recursive rebuild helpers.

use std::sync::Arc;

use tracing::{info, warn};

use crate::pane::{LayoutNode, LayoutSnapshot, SplitDirection};
use therminal_core::geometry::Rect;
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::make_pane_callbacks;
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
        proxy: &crate::window::EventLoopProxy<crate::window::UserEvent>,
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
}
