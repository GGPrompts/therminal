//! Accessor methods on `Pane`: snapshots, getters, write, resize.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags as CellFlags;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::{Arc, Mutex};
use therminal_terminal::event_log::StoredEvent;
use therminal_terminal::osc633::CommandBlock;
use therminal_terminal::pty_runtime::TermSize;
use therminal_terminal::region_index::RegionIndex;
use therminal_terminal::state_inference::{AgentCadenceSnapshot, AgentDetailsSnapshot};
use tracing::warn;

#[cfg(test)]
use therminal_terminal::event_log::EventLog;
#[cfg(test)]
use therminal_terminal::osc633::CommandTracker;

use super::lifecycle::Pane;
use crate::session::snapshots::{MAX_SNAPSHOT_SCROLLBACK, PaneSnapshot};

impl Pane {
    /// Snapshot the per-pane agent inference engine. Returns a plain DTO
    /// suitable for serialising into the MCP `terminal.agents.get_details`
    /// response. Holds the inference lock only for the duration of the
    /// snapshot clone.
    pub fn agent_details_snapshot(&self) -> AgentDetailsSnapshot {
        self.inference
            .lock()
            .map(|inf| inf.snapshot())
            .unwrap_or_default()
    }

    /// Snapshot the per-pane output cadence window. Returns a plain DTO
    /// suitable for serialising into the MCP `terminal.agents.get_cadence`
    /// response. Holds the inference lock only for the duration of the
    /// snapshot build (sample timestamps converted to wall-clock seconds
    /// before the lock is released).
    pub fn agent_cadence_snapshot(&self) -> AgentCadenceSnapshot {
        self.inference
            .lock()
            .map(|inf| inf.cadence_snapshot())
            .unwrap_or_default()
    }

    /// Snapshot the per-pane OSC 633 `CommandTracker`. Returns a plain
    /// `Vec<CommandBlock>` cloned under the lock so the daemon can serve
    /// `terminal.semantic.query_commands` without holding the lock for the
    /// duration of the request.
    pub fn command_tracker_snapshot(&self) -> Vec<CommandBlock> {
        self.command_tracker
            .lock()
            .map(|t| t.snapshot())
            .unwrap_or_default()
    }

    /// Test-only handle to the shared `CommandTracker` Arc. Lets unit
    /// tests inject OSC 633 marks (mirroring what the reader thread's
    /// interceptor does in production) and then read back via the public
    /// snapshot path.
    #[cfg(test)]
    pub fn command_tracker_arc(&self) -> Arc<Mutex<CommandTracker>> {
        Arc::clone(&self.command_tracker)
    }

    /// Snapshot the per-pane in-memory event log. Returns events filtered
    /// by `since_timestamp_secs` (inclusive) and capped at `limit`. Source
    /// is the rolling in-memory ring (`DEFAULT_MAX_ENTRIES` cap), no JSONL
    /// file is read. Returns oldest-first within the truncated window.
    pub fn event_log_snapshot(
        &self,
        since_timestamp_secs: Option<u64>,
        limit: usize,
    ) -> Vec<StoredEvent> {
        self.event_log
            .lock()
            .map(|log| log.snapshot(since_timestamp_secs, limit))
            .unwrap_or_default()
    }

    /// Test-only: shared event log Arc, so unit tests can directly inject
    /// `SessionEvent`s without driving them through the full pty pipeline.
    #[cfg(test)]
    pub fn event_log_arc(&self) -> Arc<Mutex<EventLog>> {
        Arc::clone(&self.event_log)
    }

    /// Access the pane's semantic region index.
    pub fn region_index(&self) -> &Arc<Mutex<RegionIndex>> {
        &self.region_index
    }

    /// Get the number of columns.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Get the number of rows.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Get the current working directory (from OSC 7 or initial spawn).
    pub fn cwd(&self) -> String {
        self.cwd.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Exit code of the most recently finished command, derived from OSC 633
    /// `D` marks captured in the region index. Returns `None` if no command
    /// has finished yet (or the shell isn't emitting OSC 633).
    ///
    /// Scans the region index backwards for the most recent `Output` or
    /// `Error` region that has an `exit_code` metadata entry.
    pub fn last_exit_code(&self) -> Option<i32> {
        use therminal_terminal::region_index::RegionKind;
        let idx = self.region_index.lock().ok()?;
        for region in idx.regions().iter().rev() {
            if matches!(region.kind, RegionKind::Output | RegionKind::Error)
                && let Some(code) = region.metadata.get("exit_code")
                && let Ok(n) = code.parse::<i32>()
            {
                return Some(n);
            }
        }
        None
    }

    /// Get the shell command used when this pane was spawned.
    pub fn shell(&self) -> &str {
        &self.shell
    }

    /// PID of the spawned shell child, if known. Used by the daemon-side
    /// `ProcessDetector` ticker (tn-pehl) to walk the process tree below
    /// the shell. Returns `None` for handoff-restored panes.
    pub fn shell_pid(&self) -> Option<u32> {
        self.shell_pid
    }

    /// Snapshot of the pane's opaque key/value tags (tn-bbvf).
    pub fn tags(&self) -> HashMap<String, String> {
        self.tags.clone()
    }

    /// Merge tags into the pane's tag set. Existing keys with the same
    /// name are overwritten; other keys are left untouched.
    pub fn merge_tags(&mut self, new_tags: HashMap<String, String>) {
        for (k, v) in new_tags {
            self.tags.insert(k, v);
        }
    }

    /// Remove specific tag keys from the pane. Keys not present are ignored.
    pub fn remove_tag_keys(&mut self, keys: &[String]) {
        for k in keys {
            self.tags.remove(k);
        }
    }

    /// Clear all tags on the pane.
    pub fn clear_tags(&mut self) {
        self.tags.clear();
    }

    /// Restore tags from persisted state.
    pub fn set_tags(&mut self, tags: HashMap<String, String>) {
        self.tags = tags;
    }

    /// Write bytes to the pane's PTY (forwarding keystrokes).
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.pty_writer.write_all(data)?;
        self.pty_writer.flush()
    }

    pub(crate) fn has_seen_prompt_start(&self) -> bool {
        use therminal_terminal::osc633::CommandState;

        self.command_tracker
            .lock()
            .ok()
            .and_then(|tracker| tracker.current_block().map(|block| block.state.clone()))
            .is_some_and(|state| {
                matches!(
                    state,
                    CommandState::PromptStart
                        | CommandState::Input
                        | CommandState::Executing
                        | CommandState::Finished
                )
            })
    }

    /// Take a snapshot of the current terminal state, including scrollback history.
    pub fn snapshot(&self) -> PaneSnapshot {
        let term = self.term.lock();
        let grid = term.grid();
        let cols = term.columns();
        let rows = term.screen_lines();
        let cursor_point = grid.cursor.point;
        let history_size = grid.history_size();

        // Collect scrollback rows (oldest first), capped at MAX_SNAPSHOT_SCROLLBACK.
        let scrollback_lines = history_size.min(MAX_SNAPSHOT_SCROLLBACK);
        let mut scrollback = Vec::with_capacity(scrollback_lines);
        let start = -(scrollback_lines as i32);
        for line_idx in start..0 {
            let line = alacritty_terminal::index::Line(line_idx);
            let mut row = Vec::with_capacity(cols);
            for col_idx in 0..cols {
                let col = alacritty_terminal::index::Column(col_idx);
                let cell = &grid[line][col];
                row.push((cell.c, cell.flags.contains(CellFlags::BOLD)));
            }
            scrollback.push(row);
        }

        // Collect visible grid rows.
        let mut visible = Vec::with_capacity(rows);
        for line_idx in 0..rows {
            let line = alacritty_terminal::index::Line(line_idx as i32);
            let mut row = Vec::with_capacity(cols);
            for col_idx in 0..cols {
                let col = alacritty_terminal::index::Column(col_idx);
                let cell = &grid[line][col];
                row.push((cell.c, cell.flags.contains(CellFlags::BOLD)));
            }
            visible.push(row);
        }

        PaneSnapshot {
            pane_id: self.id,
            title: String::new(),
            scrollback,
            grid: visible,
            cursor_col: cursor_point.column.0,
            cursor_line: (cursor_point.line.0.max(0) as usize).min(rows.saturating_sub(1)),
            cols,
            rows,
        }
    }

    /// Capture a structured state snapshot for tn-zamd replay.
    ///
    /// Reads TermMode bits, cursor position, dimensions, and the visible
    /// grid. The lock is held only for the copy into owned types and
    /// released before returning.
    pub fn snapshot_state(&self) -> therminal_protocol::daemon::PaneStateSnapshot {
        use alacritty_terminal::term::TermMode;
        use therminal_protocol::daemon::{PaneModeFlags, PaneStateSnapshot};

        let term = self.term.lock();
        let mode = *term.mode();
        let cols = term.columns();
        let rows = term.screen_lines();
        let grid = term.grid();
        let cursor_point = grid.cursor.point;

        let mut grid_chars = Vec::with_capacity(rows);
        for line_idx in 0..rows {
            let line = alacritty_terminal::index::Line(line_idx as i32);
            let mut row = String::with_capacity(cols);
            for col_idx in 0..cols {
                let col = alacritty_terminal::index::Column(col_idx);
                row.push(grid[line][col].c);
            }
            grid_chars.push(row);
        }

        let modes = PaneModeFlags {
            show_cursor: mode.contains(TermMode::SHOW_CURSOR),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            mouse_report_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
            mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            focus_in_out: mode.contains(TermMode::FOCUS_IN_OUT),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            line_wrap: mode.contains(TermMode::LINE_WRAP),
        };

        PaneStateSnapshot {
            version: PaneStateSnapshot::CURRENT_VERSION,
            cols: cols as u16,
            rows: rows as u16,
            modes,
            cursor_col: cursor_point.column.0 as u16,
            cursor_line: (cursor_point.line.0.max(0) as usize).min(rows.saturating_sub(1)) as u16,
            grid_chars,
            tags: self.tags.clone(),
        }
    }

    /// Resize the pane's PTY and terminal.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if let Err(e) = therminal_terminal::pty::resize(self._pty_master.as_ref(), cols, rows) {
            warn!(pane_id = %self.id, error = %e, "failed to resize PTY");
            return;
        }
        let mut term = self.term.lock();
        let size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        term.resize(size);
        self.cols = cols;
        self.rows = rows;
    }
}
