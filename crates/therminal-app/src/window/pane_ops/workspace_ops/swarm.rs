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
use therminal_terminal::region_index::RegionIndex;

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

    /// Open a new pane that tails a Claude subagent JSONL file.
    ///
    /// Creates a local `JsonlTail` pane (read-only, no PTY needed) that
    /// renders structured, color-coded JSONL output. Works identically in
    /// both local and daemon modes — the `JsonlTail` backend uses a
    /// `notify` file watcher, not a daemon-managed PTY (tn-xdgp).
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
        let jsonl_path_for_closure = jsonl_path.clone();
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

        let new_id = layout.split_pane(target_pane_id, direction, |viewport| {
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
                    // Reuse PtyOutput to trigger a redraw on the event loop.
                    let _ = proxy.send_event(crate::window::UserEvent::PtyOutput);
                })
            };

            match jsonl_tail::spawn_jsonl_watcher(jsonl_path_for_closure.clone(), cols, rows, wake)
            {
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
        });

        let Some(new_id) = new_id else { return };

        info!(
            agent = %agent_id,
            pane_id = new_id,
            jsonl = %jsonl_path.display(),
            "swarm: spawned JSONL tail pane for subagent"
        );

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
