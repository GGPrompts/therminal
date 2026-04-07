//! Pane backend abstraction: Terminal vs WebView vs RemotePty.
//!
//! All backend types participate in tiling, geometry, MCP queries, and the
//! event bus through the shared `PaneBackend` trait.
//!
//! ## RemotePty (tn-5ps8)
//!
//! `RemotePty` streams PTY bytes from the daemon over IPC instead of owning
//! a local `portable_pty::Child`. The GUI still holds a local `Term` so the
//! renderer can read the grid every frame the same way it does for
//! `Terminal` — only the byte source and input sink are remote.
//!
//! Wiring:
//! - Output: a tokio task subscribed to `DaemonEvent::PaneOutput` on a
//!   per-pane `DaemonClient` connection forwards filtered byte chunks
//!   into a `std::sync::mpsc::Sender<Vec<u8>>` consumed by a dedicated
//!   worker thread that runs `processor.advance_with_interceptor()` —
//!   identical to the local PTY reader thread, just with a different
//!   byte source.
//! - Input: `write_input()` queues bytes onto a `tokio::sync::mpsc`
//!   channel drained by a writer task that calls
//!   `DaemonClient::send_request(IpcRequest::SendKeys { .. })`.
//! - Resize: `resize()` sends `IpcRequest::ResizePane { .. }` (stub on
//!   the daemon today, see tn-5rm0) and resizes the local `Term`
//!   immediately so the renderer reflects the new size.
//! - Exit: when `DaemonEvent::PaneExited` arrives for this pane (stub
//!   today), the worker thread terminates and the on_exit callback fires
//!   the same `UserEvent::PaneExited` flow used for local panes.

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
    /// A pane whose PTY lives in the daemon. Bytes flow over IPC.
    ///
    /// The local `Term` is fed by a worker thread, identical to the
    /// `Terminal` variant — only the byte source differs. See
    /// `crate::pane::remote_spawn::spawn_remote_pane`.
    RemotePty {
        /// Daemon-assigned pane id (independent of the GUI's local PaneId).
        pane_id: therminal_protocol::PaneId,
        /// Local term fed by the worker thread.
        term: Arc<FairMutex<Term<PaneListener>>>,
        /// Sink for outbound input bytes; drained by a tokio task that
        /// calls `DaemonClient::send_request(IpcRequest::SendKeys { .. })`.
        input_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        /// Persistent client used for resize requests (input goes via
        /// `input_tx` to keep `write_input` non-blocking).
        daemon_client: Arc<therminal_daemon_client::DaemonClient>,
        /// Tokio runtime handle for spawning ResizePane requests from
        /// the synchronous `resize()` path.
        tokio_handle: tokio::runtime::Handle,
        /// Shutdown signal for the output forwarding task / worker thread.
        /// Dropping the backend closes this and the workers wind down.
        #[allow(dead_code)]
        shutdown: Arc<std::sync::atomic::AtomicBool>,
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
            PaneBackendKind::RemotePty { input_tx, .. } => {
                // Fire-and-forget; the writer task drains and forwards
                // via DaemonClient::send_request(SendKeys).
                if input_tx.send(data.to_vec()).is_err() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "remote pty input channel closed",
                    ));
                }
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
            PaneBackendKind::RemotePty {
                pane_id,
                term,
                daemon_client,
                tokio_handle,
                ..
            } => {
                {
                    let mut term_guard = term.lock();
                    let size = PaneTermSize {
                        columns: cols,
                        screen_lines: rows,
                    };
                    term_guard.resize(size);
                }
                let client = Arc::clone(daemon_client);
                let pid = *pane_id;
                let cols_u16 = cols as u16;
                let rows_u16 = rows as u16;
                tokio_handle.spawn(async move {
                    use therminal_protocol::daemon::IpcRequest;
                    if let Err(e) = client
                        .send_request(IpcRequest::ResizePane {
                            pane_id: pid,
                            cols: cols_u16,
                            rows: rows_u16,
                        })
                        .await
                    {
                        // Stub today: server returns Error{unimplemented}.
                        // tn-5rm0 lands the real handler.
                        tracing::debug!(
                            error = %e,
                            "ResizePane request failed (expected until tn-5rm0)"
                        );
                    }
                });
            }
        }
    }

    fn get_content(&self) -> String {
        match self {
            PaneBackendKind::Terminal { term, .. } | PaneBackendKind::RemotePty { term, .. } => {
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
            PaneBackendKind::RemotePty { .. } => "remote_pty",
        }
    }
}

impl PaneBackendKind {
    /// Returns the terminal term if this is a Terminal backend, `None` otherwise.
    pub fn term(&self) -> Option<&Arc<FairMutex<Term<PaneListener>>>> {
        match self {
            PaneBackendKind::Terminal { term, .. } | PaneBackendKind::RemotePty { term, .. } => {
                Some(term)
            }
            _ => None,
        }
    }

    /// Returns the PTY child process group leader pid for terminal backends.
    ///
    /// Unix-only: `process_group_leader` is not implemented on Windows ConPTY.
    /// On Windows this returns `None`; the swarm watcher "current" scope
    /// filter will therefore behave as "all" there until we wire up a
    /// Windows-specific process lookup.
    pub fn root_pid(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            match self {
                PaneBackendKind::Terminal { pty_master, .. } => {
                    pty_master.process_group_leader().map(|p| p as u32)
                }
                _ => None,
            }
        }
        #[cfg(not(unix))]
        {
            None
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
