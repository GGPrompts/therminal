//! Per-pane terminal state: dimensions adapter, shared status, and pane state.

use std::io::Write as IoWrite;
use std::sync::{Arc, Mutex};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use portable_pty::MasterPty;
use therminal_core::geometry::Rect;
use tracing::warn;

use super::PaneId;
use super::PaneListener;
use super::geometry::PANE_HEADER_HEIGHT;
use crate::grid_renderer::GridRenderer;

// ── Dimensions adapter ──────────────────────────────────────────────────

pub(crate) struct PaneTermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl Dimensions for PaneTermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

// ── Shared pane status (updated by PTY reader, read by render loop) ────

/// Shared status data for a pane, updated by the PTY reader thread and
/// read (cheaply) by the render loop to populate the status bar.
#[derive(Debug, Default, Clone)]
pub struct PaneStatus {
    /// Current working directory (from OSC 7).
    pub cwd: Option<String>,
    /// Exit code of the last finished command (from OSC 633 D mark).
    pub last_exit_code: Option<i32>,
    /// Name of a detected AI agent (from ProcessDetector).
    pub agent_name: Option<String>,
}

// ── Per-pane state ──────────────────────────────────────────────────────

/// State for a single terminal pane.
pub struct PaneState {
    pub id: PaneId,
    pub term: Arc<FairMutex<Term<PaneListener>>>,
    pub pty_writer: Box<dyn IoWrite + Send>,
    pub pty_master: Box<dyn MasterPty + Send>,
    /// Current viewport rect in physical pixels (set by layout computation).
    pub viewport: Rect,
    /// Scrollback configuration.
    #[allow(dead_code)]
    pub scrollback_lines: usize,
    /// Shared status updated by the PTY reader thread.
    pub status: Arc<Mutex<PaneStatus>>,
}

impl PaneState {
    /// Resize this pane's terminal and PTY to match a new viewport rect.
    #[allow(dead_code)]
    pub fn resize_to_viewport(&mut self, rect: Rect, renderer: &GridRenderer) {
        self.resize_to_viewport_with_header(rect, renderer, PANE_HEADER_HEIGHT);
    }

    /// Resize with an explicit header height (0 for single pane).
    pub fn resize_to_viewport_with_header(
        &mut self,
        rect: Rect,
        renderer: &GridRenderer,
        header_h: f32,
    ) {
        self.viewport = rect;
        let (cols, rows) = grid_size_for_rect_with_header(rect, renderer, header_h);
        if cols == 0 || rows == 0 {
            return;
        }
        {
            let mut term_guard = self.term.lock();
            let size = PaneTermSize {
                columns: cols,
                screen_lines: rows,
            };
            term_guard.resize(size);
        }
        if let Err(e) =
            therminal_terminal::pty::resize(self.pty_master.as_ref(), cols as u16, rows as u16)
        {
            warn!("Failed to resize pane {} PTY: {e}", self.id);
        }
    }
}

/// Compute (cols, rows) for a viewport rect using the renderer's cell metrics.
/// `header_h` is the effective header height (0 for single pane, PANE_HEADER_HEIGHT for multi).
pub fn grid_size_for_rect(rect: Rect, renderer: &GridRenderer) -> (usize, usize) {
    grid_size_for_rect_with_header(rect, renderer, PANE_HEADER_HEIGHT)
}

/// Like `grid_size_for_rect` but with an explicit header height.
pub fn grid_size_for_rect_with_header(
    rect: Rect,
    renderer: &GridRenderer,
    header_h: f32,
) -> (usize, usize) {
    let usable_w = rect.width() - renderer.padding_x() * 2.0;
    let usable_h = rect.height() - renderer.padding_y() * 2.0 - header_h;
    let cols = (usable_w / renderer.cell_width).floor().max(2.0) as usize;
    let rows = (usable_h / renderer.cell_height).floor().max(1.0) as usize;
    (cols, rows)
}
