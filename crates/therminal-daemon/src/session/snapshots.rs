//! Snapshot types sent to clients on attach or pane capture.

use therminal_protocol::{PaneId, SessionId};

/// Maximum number of scrollback lines included in a snapshot to avoid huge payloads.
pub(super) const MAX_SNAPSHOT_SCROLLBACK: usize = 10_000;

/// A snapshot of a pane's terminal state, sent to the client on attach.
#[derive(Debug, Clone)]
pub struct PaneSnapshot {
    pub pane_id: PaneId,
    pub title: String,
    /// Scrollback history above the visible screen (oldest first).
    /// Each row is a Vec of (character, bold flag). Capped at [`MAX_SNAPSHOT_SCROLLBACK`] lines.
    pub scrollback: Vec<Vec<(char, bool)>>,
    /// Visible grid contents: Vec of rows, each row a Vec of (character, bold flag).
    pub grid: Vec<Vec<(char, bool)>>,
    /// Cursor position (column, line) in the visible grid.
    pub cursor_col: usize,
    pub cursor_line: usize,
    /// Grid dimensions.
    pub cols: usize,
    pub rows: usize,
}

/// Snapshot of all panes in a session (sent on attach).
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub name: Option<String>,
    pub panes: Vec<PaneSnapshot>,
}
