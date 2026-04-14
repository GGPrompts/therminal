//! Swarm watcher integration: `poll_swarm_watcher` drains the
//! `SwarmDebouncer`, dispatching to `spawn_subagent_pane` /
//! `reclaim_subagent_pane`.

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::pane::{LayoutNode, SplitDirection};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::split_ops::DaemonSplitOnComplete;
use super::super::{make_pane_callbacks, split_spawn_options};
use crate::window::App;

impl App {
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
            osc_7337: true,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let base_spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
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
        // Inherit source pane's cwd (from OSC 7) or use home depending on config.
        let spawn_options = split_spawn_options(
            &base_spawn_options,
            layout,
            target_pane_id,
            self.config.general.new_pane_cwd,
        );

        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

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
                super::super::editor_clipboard::shell_quote(&jsonl_path.display().to_string()),
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
