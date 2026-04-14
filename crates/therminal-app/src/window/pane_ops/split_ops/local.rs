//! Local-mode synchronous split path and the public dispatcher methods
//! shared by local and daemon modes:
//! - `split_focused_pane_auto[_with]`
//! - `split_focused_pane[_with]`
//! - `split_pane_by_id`
//! - `spawn_n_panes`
//! - `open_focused_agent_event_log_tail`

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::pane::{LayoutNode, PaneId, SplitDirection};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::{make_pane_callbacks, split_spawn_options};
use super::DaemonSplitOnComplete;
use crate::window::App;

impl App {
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
            osc_7337: true,
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
        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

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
            osc_7337: true,
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

        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

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
