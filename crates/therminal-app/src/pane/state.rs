//! Per-pane terminal state: dimensions adapter, shared status, and pane state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use alacritty_terminal::grid::Dimensions;
use therminal_core::geometry::Rect;
use therminal_terminal::region_index::RegionIndex;

use super::PaneId;
use super::backend::{PaneBackend, PaneBackendKind};
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
    /// Opaque key/value tags from the daemon (tn-bbvf).
    pub tags: HashMap<String, String>,
}

// ── Per-pane state ──────────────────────────────────────────────────────

/// State for a single pane. Shared fields live here; backend-specific
/// state (PTY, Term, WebView handle, etc.) lives in `backend`.
pub struct PaneState {
    pub id: PaneId,
    /// Current viewport rect in physical pixels (set by layout computation).
    pub viewport: Rect,
    /// Shared status updated by the PTY reader thread.
    pub status: Arc<Mutex<PaneStatus>>,
    /// Semantic region index, updated by the PTY reader thread.
    pub region_index: Arc<Mutex<RegionIndex>>,
    /// The backend powering this pane (terminal, webview, etc.).
    pub backend: PaneBackendKind,
}

impl PaneState {
    /// Resize this pane's terminal and PTY to match a new viewport rect.
    ///
    /// Uses header_h = 0. Callers needing a header should use
    /// `resize_to_viewport_with_header` directly.
    #[allow(dead_code)]
    pub fn resize_to_viewport(&mut self, rect: Rect, renderer: &GridRenderer) {
        self.resize_to_viewport_with_header(rect, renderer, 0.0);
    }

    /// Resize with an explicit header height (0 for single pane).
    pub fn resize_to_viewport_with_header(
        &mut self,
        rect: Rect,
        renderer: &GridRenderer,
        header_h: f32,
    ) {
        self.viewport = rect;
        self.backend.resize_to_viewport(rect, renderer, header_h);
    }

    /// Write input data to this pane's backend.
    pub fn write_input(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.backend.write_input(data)
    }

    /// Get the backend type identifier.
    #[allow(dead_code)]
    pub fn backend_type(&self) -> &str {
        self.backend.backend_type()
    }

    /// Get visible content from the backend (for MCP queries).
    #[allow(dead_code)]
    pub fn get_content(&self) -> String {
        self.backend.get_content()
    }
}

/// Compute (cols, rows) for a viewport rect using the renderer's cell metrics.
///
/// Uses header_h = 0 (no header). Callers spawning into a multi-pane layout
/// should use `grid_size_for_rect_with_header` with `PANE_HEADER_HEIGHT`, but
/// in practice `resize_all_panes` always runs after spawn and applies the
/// correct effective header height, so using 0 here avoids a 1-row overshoot
/// on single-pane initial spawns where the header doesn't exist.
pub fn grid_size_for_rect(rect: Rect, renderer: &GridRenderer) -> (usize, usize) {
    grid_size_for_rect_with_header(rect, renderer, 0.0)
}

/// Like `grid_size_for_rect` but with an explicit header height.
pub fn grid_size_for_rect_with_header(
    rect: Rect,
    renderer: &GridRenderer,
    header_h: f32,
) -> (usize, usize) {
    grid_size_from_metrics(
        rect,
        renderer.padding_x(),
        renderer.padding_y(),
        header_h,
        renderer.cell_width,
        renderer.cell_height,
    )
}

/// Pure math helper: compute (cols, rows) from a rect plus raw cell metrics.
///
/// Extracted from `grid_size_for_rect_with_header` so unit tests can lock the
/// invariant "given rect R, the result reflects R" without needing a live
/// `GridRenderer` (which requires a GPU device). This is the low-level
/// function the deferred-spawn fix relies on: at first-Resized time, we feed
/// it the current authoritative surface rect, not a stale `inner_size()`
/// reading from window creation. See tn-ou30.
pub fn grid_size_from_metrics(
    rect: Rect,
    padding_x: f32,
    padding_y: f32,
    header_h: f32,
    cell_width: f32,
    cell_height: f32,
) -> (usize, usize) {
    let usable_w = rect.width() - padding_x * 2.0;
    let usable_h = rect.height() - padding_y * 2.0 - header_h;
    let cols = (usable_w / cell_width).floor().max(2.0) as usize;
    let rows = (usable_h / cell_height).floor().max(1.0) as usize;
    (cols, rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tn-ou30 invariant: given a rect, `grid_size_from_metrics` derives
    /// (cols, rows) from THAT rect, not some other one. If init_gpu's
    /// deferred spawn ever regresses to using stale dimensions, this test
    /// documents what "use the current rect" means.
    #[test]
    fn grid_size_tracks_input_rect_dimensions() {
        // 1920x1080 surface, 24px tab bar, 24px status bar, 8px padding,
        // 10x20 cells, no header.
        let rect = Rect::new(0.0, 24.0, 1920.0, 1080.0 - 24.0 - 24.0);
        let (cols, rows) = grid_size_from_metrics(rect, 8.0, 8.0, 0.0, 10.0, 20.0);
        // usable_w = 1920 - 16 = 1904; cols = floor(1904/10) = 190
        // usable_h = 1032 - 16 = 1016; rows = floor(1016/20) = 50
        assert_eq!(cols, 190);
        assert_eq!(rows, 50);
    }

    #[test]
    fn grid_size_changes_when_rect_changes() {
        // The same metrics applied to a smaller rect must yield smaller dims.
        // This is the crux of the tn-ou30 fix: the deferred-spawn path must
        // pick up the *new* rect after the first Resized, not the rect that
        // was implied by the (stale) winit `inner_size()` at create time.
        let small = Rect::new(0.0, 0.0, 800.0, 600.0);
        let large = Rect::new(0.0, 0.0, 1920.0, 1200.0);
        let (small_cols, small_rows) = grid_size_from_metrics(small, 0.0, 0.0, 0.0, 10.0, 20.0);
        let (large_cols, large_rows) = grid_size_from_metrics(large, 0.0, 0.0, 0.0, 10.0, 20.0);
        assert!(large_cols > small_cols);
        assert!(large_rows > small_rows);
        assert_eq!(small_cols, 80);
        assert_eq!(small_rows, 30);
        assert_eq!(large_cols, 192);
        assert_eq!(large_rows, 60);
    }

    #[test]
    fn grid_size_clamps_to_minimum_dims() {
        // Pathologically tiny rect: cols clamps to 2, rows clamps to 1.
        // No panic, no zero divides.
        let tiny = Rect::new(0.0, 0.0, 10.0, 10.0);
        let (cols, rows) = grid_size_from_metrics(tiny, 8.0, 8.0, 0.0, 10.0, 20.0);
        assert_eq!(cols, 2);
        assert_eq!(rows, 1);
    }
}
