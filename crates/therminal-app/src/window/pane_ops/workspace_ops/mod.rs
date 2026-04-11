//! Workspace operations: restore_layout, switch_workspace, send_to_workspace,
//! poll_auto_tile, poll_swarm_watcher.
//!
//! Split into focused submodules:
//! - [`restore`] — restore_layout + rebuild_from_snapshot[_remote]
//! - [`switch`] — switch_workspace, create_new_workspace, send_to_workspace
//! - [`auto_tile`] — poll_auto_tile (auto-tile debouncer drain)
//! - [`swarm`] — poll_swarm_watcher, spawn_subagent_pane, reclaim_subagent_pane

mod auto_tile;
mod restore;
mod swarm;
mod switch;
