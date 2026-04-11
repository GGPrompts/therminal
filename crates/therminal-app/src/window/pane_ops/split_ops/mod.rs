//! Split operations: split_focused_pane, split_pane_by_id, split_pane_remote,
//! finish_split_pane_remote, spawn helpers.
//!
//! Split into focused submodules:
//! - [`local`] — local-mode synchronous split path + dispatcher methods
//! - [`remote`] — daemon-mode async split path (RPC + finish)
//! - [`remote_helpers`] — daemon-mode supporting helpers (new workspace,
//!   spawn off existing, fresh session)

mod local;
mod remote;
mod remote_helpers;

use crate::pane::{PaneId, SplitDirection};

/// Context carried from the async `SplitPane` RPC back to the main thread
/// so the layout insert can complete without blocking the event loop.
#[derive(Debug)]
pub struct DaemonSplitResult {
    /// Local pane id that was being split.
    pub source_local: PaneId,
    /// Split direction requested.
    pub direction: SplitDirection,
    /// Result of the daemon RPC — `Ok(daemon_pane_id)` or `Err(message)`.
    pub rpc_result: Result<therminal_protocol::PaneId, String>,
    /// Inherited cwd passed to the daemon (needed to wire the remote PTY).
    pub inherited_cwd: Option<String>,
    /// Optional post-split action to perform once the local pane is mounted.
    pub on_complete: DaemonSplitOnComplete,
}

/// What to do after `finish_split_pane_remote` successfully mounts the new pane.
#[derive(Debug, Default)]
pub enum DaemonSplitOnComplete {
    /// Focus the new pane, relayout, publish — the standard path.
    #[default]
    FocusAndRelayout,
    /// Auto-tile: register the new pane with the auto-tile debouncer and relayout.
    /// `parent_pane_id` is the pane whose agent spawned the split.
    AutoTile { parent_pane_id: PaneId },
    /// Swarm: send a `tail` command into the new pane and register it in `swarm_panes`.
    SwarmTail {
        agent_id: String,
        jsonl_path: std::path::PathBuf,
    },
    /// Write bytes to the new pane after it mounts (used by hotspot "Open in
    /// new pane" and WSL editor open, which need the daemon split to complete
    /// before the PTY is ready to accept input).
    WriteBytesAndFocus {
        bytes: Vec<u8>,
        toast: Option<String>,
    },
    /// Create a new workspace tab with the spawned pane as its root.
    /// The pane is NOT inserted into the current layout — it becomes the
    /// sole leaf in a brand-new workspace.
    NewWorkspace { workspace_id: u8 },
}
