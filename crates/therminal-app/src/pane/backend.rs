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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use portable_pty::MasterPty;
use tracing::warn;

use super::PaneListener;
use super::jsonl_tail::{JsonlTailState, JsonlTailWatcher};
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

/// Counter tracking the number of live RemotePty task groups. Used by
/// tests to assert that dropping a pane reliably tears down its tokio
/// forwarder + writer tasks and worker thread (tn-msie).
pub static REMOTE_PTY_LIVE_TASKS: AtomicUsize = AtomicUsize::new(0);

/// RAII guard attached to the `RemotePty` variant. On drop it signals
/// shutdown to the worker thread and aborts the forwarder + writer tokio
/// tasks so the pane's IPC connection, worker thread, and tokio tasks
/// are released promptly when the pane is closed (tn-msie).
pub struct RemotePtyGuard {
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    pub forwarder: Option<tokio::task::JoinHandle<()>>,
    pub writer: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RemotePtyGuard {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(h) = self.forwarder.take() {
            h.abort();
        }
        if let Some(h) = self.writer.take() {
            h.abort();
        }
        REMOTE_PTY_LIVE_TASKS.fetch_sub(1, Ordering::Release);
    }
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
    /// A read-only pane that tails a JSONL file with structured rendering.
    ///
    /// Replaces the old `tail -F` shell pane approach for subagent
    /// observation. The `notify` file watcher detects appended lines
    /// and renders them with format-aware color coding.
    ///
    /// A shadow `Term` is maintained so the GPU render path can draw
    /// the structured JSONL content identically to a PTY-backed pane
    /// (tn-pes1). `refresh_shadow_term()` on `JsonlTailState` clears
    /// the grid and writes `formatted_content()` after every state
    /// change (new lines, scroll, expand/collapse).
    JsonlTail {
        /// Path to the JSONL file being tailed.
        path: PathBuf,
        /// Shared state with the file watcher (rows, offset, formatting).
        state: Arc<Mutex<JsonlTailState>>,
        /// Shadow term fed from `formatted_content()` for GPU rendering.
        term: Arc<FairMutex<Term<PaneListener>>>,
        /// RAII guard — dropping this stops the file watcher.
        #[allow(dead_code)]
        watcher: JsonlTailWatcher,
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
        /// RAII guard that sets the shutdown flag and aborts the
        /// forwarder + writer tokio tasks when the variant is dropped.
        /// Must be held alongside `input_tx` so that dropping the variant
        /// also closes the input channel (writer task exits on channel
        /// close as a backstop).
        #[allow(dead_code)]
        guard: RemotePtyGuard,
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
            PaneBackendKind::JsonlTail { state, term, .. } => {
                // Route keystrokes to the interactive viewer (tn-bjvl).
                if let Ok(mut s) = state.lock() {
                    let consumed = s.handle_input(data);
                    if consumed {
                        // tn-pes1: repaint the shadow term only when the
                        // viewer state actually changed (scroll, expand,
                        // follow-mode toggle, etc.).  Skipping the repaint
                        // for unrecognised keys avoids a full VTE rewrite
                        // on every keystroke.
                        s.refresh_shadow_term(term);
                    }
                }
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
        // tn-6061: defensive floor at the lowest layer. Every callsite that
        // reaches this method — `resize_to_viewport`, the daemon-driven
        // RemotePty apply, hot-reload font changes, snapshot replay — is
        // expected to pre-clamp, but enforcing `MIN_COLS` / `MIN_ROWS`
        // here means a regression upstream can't push the PTY to e.g.
        // `cols=3` and re-flow a Claude Code TUI into a mangled state.
        let cols = cols.max(super::state::MIN_COLS);
        let rows = rows.max(super::state::MIN_ROWS);
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
            PaneBackendKind::JsonlTail { state, term, .. } => {
                // Resize the shadow term first.
                {
                    let mut term_guard = term.lock();
                    let size = PaneTermSize {
                        columns: cols,
                        screen_lines: rows,
                    };
                    term_guard.resize(size);
                }
                if let Ok(mut s) = state.lock() {
                    s.cols = cols.max(20);
                    s.visible_rows = rows.max(3);
                    s.reformat_all();
                    // tn-pes1: repaint shadow term with reformatted content.
                    s.refresh_shadow_term(term);
                }
            }
            PaneBackendKind::RemotePty {
                pane_id,
                term,
                daemon_client,
                tokio_handle,
                ..
            } => {
                {
                    // tn-ebdu: the local shadow `Term` is not the
                    // authoritative state for this pane — the daemon
                    // is. Letting `Term::resize` run its normal
                    // shrink_lines + post-repaint clear_viewport path
                    // on a streaming-TUI pane duplicates the pre-resize
                    // viewport into scrollback on every resize. The
                    // shared helper in `remote_spawn.rs` blanks the
                    // visible viewport first (primary screen only) so
                    // the scroll-to-history pipeline has nothing to
                    // scroll, then invokes `Term::resize`. Both the
                    // `apply_remote_resize` path and this main-thread
                    // `resize_all_panes` path must go through the same
                    // helper so the regression test coverage in
                    // `resize_does_not_duplicate_streaming_tui_into_scrollback`
                    // protects both call sites.
                    let mut term_guard = term.lock();
                    crate::pane::remote_spawn::resize_remote_term_without_scrollback_pollution(
                        &mut term_guard,
                        cols,
                        rows,
                    );
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
            PaneBackendKind::JsonlTail { state, .. } => state
                .lock()
                .map(|s| s.formatted_content())
                .unwrap_or_default(),
        }
    }

    fn backend_type(&self) -> &str {
        match self {
            PaneBackendKind::Terminal { .. } => "terminal",
            PaneBackendKind::WebView { .. } => "webview",
            PaneBackendKind::JsonlTail { .. } => "jsonl_tail",
            PaneBackendKind::RemotePty { .. } => "remote_pty",
        }
    }
}

impl PaneBackendKind {
    /// Returns the terminal term for backends that support GPU rendering.
    /// `None` only for WebView (which has no term).
    pub fn term(&self) -> Option<&Arc<FairMutex<Term<PaneListener>>>> {
        match self {
            PaneBackendKind::Terminal { term, .. }
            | PaneBackendKind::RemotePty { term, .. }
            | PaneBackendKind::JsonlTail { term, .. } => Some(term),
            PaneBackendKind::WebView { .. } => None,
        }
    }

    /// tn-ou30: clear spurious scrollback history that accumulates during
    /// shell startup or snapshot replay. Only clears if every scrollback
    /// row is blank (spaces / NUL) — real scrollback content is preserved.
    /// No-op on WebView backends.
    pub fn compact_scrollback(&mut self) {
        let term = match self {
            PaneBackendKind::Terminal { term, .. } | PaneBackendKind::RemotePty { term, .. } => {
                term
            }
            PaneBackendKind::WebView { .. } | PaneBackendKind::JsonlTail { .. } => return,
        };
        let mut guard = term.lock();
        use alacritty_terminal::grid::Dimensions;
        let cols = guard.columns();
        let history = guard.grid().history_size();
        if history == 0 {
            tracing::info!(cols, "compact_scrollback: no scrollback history");
            return;
        }

        // Check if ALL scrollback rows are blank. If any has content,
        // this is real scrollback and we leave it alone.
        let grid = guard.grid();
        let all_blank = (0..history).all(|offset| {
            // History rows use negative Line indices: -1 is the most
            // recent scrollback row, -(history) is the oldest.
            let line = alacritty_terminal::index::Line(-1 - offset as i32);
            (0..cols).all(|c| {
                let ch = grid[line][alacritty_terminal::index::Column(c)].c;
                ch == ' ' || ch == '\0'
            })
        });

        if !all_blank {
            tracing::info!(
                cols,
                history,
                "compact_scrollback: scrollback has content, keeping"
            );
            return;
        }

        guard.grid_mut().clear_history();
        tracing::info!(
            cols,
            history,
            "compact_scrollback: cleared blank scrollback"
        );
    }

    /// Resize the backend to match new grid dimensions, with access to renderer
    /// metrics for the terminal case.
    ///
    /// tn-6061: `grid_size_for_rect_with_header` floors to `MIN_COLS` /
    /// `MIN_ROWS` so sub-minimum values cannot reach the PTY even for a
    /// 1×1 surface rect; `PaneBackendKind::resize` enforces the same floor
    /// as a second line of defence.
    pub fn resize_to_viewport(
        &mut self,
        rect: therminal_core::geometry::Rect,
        renderer: &GridRenderer,
        header_h: f32,
    ) {
        let (cols, rows) = super::state::grid_size_for_rect_with_header(rect, renderer, header_h);
        self.resize(cols, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// tn-msie: dropping 100 `RemotePtyGuard`s must return the live-task
    /// counter to its baseline and abort the spawned forwarder + writer
    /// tasks, rather than leaking them until process exit.
    #[test]
    fn remote_pty_guard_drops_release_tasks() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();

        let baseline = REMOTE_PTY_LIVE_TASKS.load(Ordering::Acquire);

        let mut guards = Vec::with_capacity(100);
        let flags: Vec<Arc<AtomicBool>> =
            (0..100).map(|_| Arc::new(AtomicBool::new(false))).collect();

        for flag in &flags {
            let shutdown = Arc::new(AtomicBool::new(false));
            let shutdown_f = Arc::clone(&shutdown);
            let flag_f = Arc::clone(flag);
            // Forwarder task: never completes unless aborted. Mimics the
            // real forwarder blocked in recv_event().await.
            let forwarder = handle.spawn(async move {
                loop {
                    if shutdown_f.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
                flag_f.store(true, Ordering::Release);
            });
            // Writer task: also never completes unless aborted.
            let writer = handle.spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            });
            REMOTE_PTY_LIVE_TASKS.fetch_add(1, Ordering::Release);
            guards.push(RemotePtyGuard {
                shutdown,
                forwarder: Some(forwarder),
                writer: Some(writer),
            });
        }

        assert_eq!(
            REMOTE_PTY_LIVE_TASKS.load(Ordering::Acquire),
            baseline + 100
        );

        drop(guards);

        assert_eq!(REMOTE_PTY_LIVE_TASKS.load(Ordering::Acquire), baseline);

        // Give the runtime a chance to observe the aborts.
        rt.block_on(async {
            tokio::task::yield_now().await;
        });
    }
}
