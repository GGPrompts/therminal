//! Pane backend abstraction: Terminal vs WebView.
//!
//! Both backend types participate in tiling, geometry, MCP queries, and the
//! event bus through the shared `PaneBackend` trait.

use std::io::Write as IoWrite;
use std::sync::Arc;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use portable_pty::MasterPty;
use tracing::warn;

use super::PaneListener;
use super::state::PaneTermSize;
use crate::grid_renderer::GridRenderer;

/// Trait that all pane backends must implement.
///
/// Provides a uniform interface for input delivery, resize handling,
/// content extraction, and type identification.
pub trait PaneBackend {
    /// Write user input to the backend (keystrokes, paste data).
    fn write_input(&mut self, data: &[u8]) -> std::io::Result<()>;

    /// Resize the backend to match new grid dimensions.
    fn resize(&mut self, cols: usize, rows: usize);

    /// Extract visible text content (for MCP queries, search, etc.).
    fn get_content(&self) -> String;

    /// Human-readable backend type identifier.
    fn backend_type(&self) -> &str;
}

/// The concrete backend kind stored in each `PaneState`.
#[allow(dead_code)]
pub enum PaneBackendKind {
    /// A terminal pane backed by a PTY and alacritty_terminal.
    Terminal {
        term: Arc<FairMutex<Term<PaneListener>>>,
        pty_writer: Box<dyn IoWrite + Send>,
        pty_master: Box<dyn MasterPty + Send>,
        /// Scrollback configuration.
        #[allow(dead_code)]
        scrollback_lines: usize,
    },
    /// A web view pane (stub — placeholder for future wry integration).
    WebView {
        /// URL currently loaded in the web view.
        #[allow(dead_code)]
        url: String,
        /// Placeholder content buffer for MCP queries.
        #[allow(dead_code)]
        content: String,
    },
}

impl PaneBackend for PaneBackendKind {
    fn write_input(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            PaneBackendKind::Terminal { pty_writer, .. } => pty_writer.write_all(data),
            PaneBackendKind::WebView { .. } => {
                // WebView input handling is a stub for now.
                Ok(())
            }
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        match self {
            PaneBackendKind::Terminal {
                term, pty_master, ..
            } => {
                {
                    let mut term_guard = term.lock();
                    let size = PaneTermSize {
                        columns: cols,
                        screen_lines: rows,
                    };
                    term_guard.resize(size);
                }
                if let Err(e) =
                    therminal_terminal::pty::resize(pty_master.as_ref(), cols as u16, rows as u16)
                {
                    warn!("Failed to resize PTY: {e}");
                }
            }
            PaneBackendKind::WebView { .. } => {
                // WebView resize is a stub for now.
            }
        }
    }

    fn get_content(&self) -> String {
        match self {
            PaneBackendKind::Terminal { term, .. } => {
                use alacritty_terminal::grid::Dimensions;
                let term_guard = term.lock();
                let rows = term_guard.screen_lines();
                let cols = term_guard.columns();
                let grid = term_guard.grid();
                let mut content = String::new();
                for row_idx in 0..rows {
                    use alacritty_terminal::index::{Column, Line};
                    for col_idx in 0..cols {
                        let cell = &grid[Line(row_idx as i32)][Column(col_idx)];
                        content.push(cell.c);
                    }
                    // Trim trailing spaces and add newline.
                    let trimmed = content.trim_end_matches(' ');
                    let trimmed_len = trimmed.len();
                    content.truncate(trimmed_len);
                    content.push('\n');
                }
                content
            }
            PaneBackendKind::WebView { content, .. } => content.clone(),
        }
    }

    fn backend_type(&self) -> &str {
        match self {
            PaneBackendKind::Terminal { .. } => "terminal",
            PaneBackendKind::WebView { .. } => "webview",
        }
    }
}

impl PaneBackendKind {
    /// Returns the terminal term if this is a Terminal backend, `None` otherwise.
    pub fn term(&self) -> Option<&Arc<FairMutex<Term<PaneListener>>>> {
        match self {
            PaneBackendKind::Terminal { term, .. } => Some(term),
            _ => None,
        }
    }

    /// Returns a mutable reference to the PTY writer if this is a Terminal backend.
    // TODO(code-review): unused — verify zero refs workspace-wide and remove
    #[allow(dead_code)]
    pub fn pty_writer_mut(&mut self) -> Option<&mut Box<dyn IoWrite + Send>> {
        match self {
            PaneBackendKind::Terminal { pty_writer, .. } => Some(pty_writer),
            _ => None,
        }
    }

    /// Resize the backend to match new grid dimensions, with access to renderer
    /// metrics for the terminal case.
    pub fn resize_to_viewport(
        &mut self,
        rect: therminal_core::geometry::Rect,
        renderer: &GridRenderer,
        header_h: f32,
    ) {
        let (cols, rows) = super::state::grid_size_for_rect_with_header(rect, renderer, header_h);
        if cols == 0 || rows == 0 {
            return;
        }
        self.resize(cols, rows);
    }
}
