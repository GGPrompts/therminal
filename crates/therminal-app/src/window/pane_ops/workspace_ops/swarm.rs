//! Swarm watcher integration: `poll_swarm_watcher` drains the
//! `SwarmDebouncer`, dispatching to `spawn_subagent_pane` /
//! `reclaim_subagent_pane`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tracing::{debug, info, warn};

use crate::pane::backend::PaneBackendKind;
use crate::pane::jsonl_tail;
use crate::pane::state::{PaneState, PaneStatus};
use crate::pane::{LayoutNode, SplitDirection};
use therminal_terminal::interceptor::InterceptorConfig;
use therminal_terminal::region_index::RegionIndex;

use super::super::make_pane_callbacks;
use crate::window::App;

impl App {
    /// Drain the `SwarmDebouncer` and dispatch any expired spawn/reclaim
    /// events. Called from `handle_redraw_requested` (parallel to
    /// `poll_auto_tile`) and from the `SwarmWatcherTick` user-event handler.
    pub(crate) fn poll_swarm_watcher(&mut self) {
        // Refresh the shared session-ID set for the swarm watcher's
        // owned-session computation (tn-twfg). Cheap (visit each leaf once)
        // and only populated when the user opted into
        // `swarm_watch_scope = "current"`.
        if let Some(provider) = self.swarm_pane_session_ids.clone() {
            let session_ids = self.collect_all_claude_session_ids();
            if let Ok(mut g) = provider.lock() {
                *g = session_ids;
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

    /// Open a new pane for a Claude subagent.
    ///
    /// When `jsonl_path` is `Some`, creates a local `JsonlTail` pane
    /// (read-only, no PTY needed) that renders structured, color-coded
    /// JSONL output. When `jsonl_path` is `None` (hook-driven subagents
    /// without a known JSONL file), creates a regular terminal pane
    /// instead — the pane can still receive marker events via the daemon's
    /// event stream.
    pub(crate) fn spawn_subagent_pane(
        &mut self,
        agent_id: String,
        jsonl_path: Option<std::path::PathBuf>,
    ) {
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

        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };

        let direction = self
            .get_layout()
            .and_then(|l| l.find_pane(target_pane_id))
            .map(|p| LayoutNode::auto_split_direction(p.viewport, SplitDirection::Horizontal))
            .unwrap_or(SplitDirection::Horizontal);

        let proxy = self.event_proxy.clone();

        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

        let new_id = if let Some(ref jsonl_path) = jsonl_path {
            // ── JSONL tail pane ──────────────────────────────────────────
            let jsonl_path_for_closure = jsonl_path.clone();
            layout.split_pane(target_pane_id, direction, |viewport| {
                let (cols, rows) = crate::pane::state::grid_size_for_rect_with_header(
                    viewport,
                    renderer,
                    post_split_header_h,
                );
                let cols = cols.max(20);
                let rows = rows.max(3);

                let wake = {
                    let proxy = proxy.clone();
                    Box::new(move || {
                        let _ = proxy.send_event(crate::window::UserEvent::PtyOutput);
                    })
                };

                match jsonl_tail::spawn_jsonl_watcher(
                    jsonl_path_for_closure.clone(),
                    cols,
                    rows,
                    wake,
                ) {
                    Ok((state, term, watcher)) => {
                        let id = crate::pane::spawn::next_pane_id();
                        Some(PaneState {
                            id,
                            viewport,
                            status: Arc::new(Mutex::new(PaneStatus::default())),
                            region_index: Arc::new(Mutex::new(RegionIndex::new())),
                            backend: PaneBackendKind::JsonlTail {
                                path: jsonl_path_for_closure,
                                state,
                                term,
                                watcher,
                            },
                        })
                    }
                    Err(e) => {
                        warn!(error = %e, "swarm: failed to create JSONL tail watcher");
                        None
                    }
                }
            })
        } else {
            // ── Terminal pane (no JSONL available) ────────────────────────
            // Hook-driven subagents may not have a JSONL file yet. Spawn a
            // regular terminal pane so the subagent has a visible presence
            // in the layout. It can receive marker events via the daemon's
            // event stream.
            info!(
                agent = %agent_id,
                "swarm: no JSONL path for subagent, spawning terminal pane \
                 instead of JsonlTail"
            );
            let scrollback = self.config.general.scrollback_lines;
            let interceptor_cfg = InterceptorConfig {
                osc_633: self.config.terminal.osc_633,
                osc_133: self.config.terminal.osc_133,
                osc_7: self.config.terminal.osc_7,
                osc_9: self.config.terminal.osc_9,
                osc_1337: self.config.terminal.osc_1337,
                osc_7777: self.config.terminal.osc_7777,
                osc_7337: self.config.terminal.osc_7337,
            };
            let scan_interval_secs = self.config.trust.agent_scan_interval;
            let spawn_options = therminal_terminal::pty::SpawnOptions {
                shell: self.config.general.shell.clone(),
                shell_args: self.config.general.shell_args.clone(),
                env: self.config.general.env.clone(),
                ..Default::default()
            };
            let registry = Some(Arc::clone(&self.agent_registry));

            layout.split_pane(target_pane_id, direction, |viewport| {
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
                        warn!(
                            error = %e,
                            "swarm: failed to spawn terminal pane for subagent"
                        );
                        None
                    }
                }
            })
        };

        let Some(new_id) = new_id else { return };

        if let Some(ref path) = jsonl_path {
            info!(
                agent = %agent_id,
                pane_id = new_id,
                jsonl = %path.display(),
                "swarm: spawned JSONL tail pane for subagent"
            );
        } else {
            info!(
                agent = %agent_id,
                pane_id = new_id,
                "swarm: spawned terminal pane for subagent (no JSONL path)"
            );
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

    /// Collect the Claude session IDs from all panes across all workspaces.
    ///
    /// Delegates to `WorkspaceManager::collect_all_claude_session_ids()`.
    /// Used by the swarm watcher's "current" scope filter (tn-twfg).
    fn collect_all_claude_session_ids(&self) -> HashSet<String> {
        self.workspaces
            .as_ref()
            .map(|wm| wm.collect_all_claude_session_ids())
            .unwrap_or_default()
    }
}
