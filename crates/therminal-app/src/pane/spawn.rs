//! Pane spawning: PTY creation via shared PtyPaneCore, reader thread, and lifecycle callbacks.

use std::sync::{Arc, Mutex};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi;
use therminal_core::geometry::Rect;
use therminal_terminal::pty_runtime::{PtyPaneCore, PtyReaderHandler};
use tracing::info;

use super::PaneId;
use super::PaneListener;
use super::state::{PaneState, PaneStatus, grid_size_for_rect};
use crate::grid_renderer::GridRenderer;

/// Counter for generating unique pane IDs.
static NEXT_PANE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Callbacks for pane lifecycle events.
pub struct PaneCallbacks {
    /// Called repeatedly when new PTY data arrives (wake the event loop).
    pub wake: Box<dyn Fn() + Send + 'static>,
    /// Called once when the PTY closes (shell exited).
    pub on_exit: Box<dyn FnOnce() + Send + 'static>,
}

// ── Reader-thread state (lazily initialised) ───────────────────────────

/// State created on the reader thread on first `process_bytes` call.
struct ReaderState {
    interceptor: therminal_terminal::interceptor::TherminalInterceptor,
    event_rx: std::sync::mpsc::Receiver<therminal_terminal::interceptor::InterceptedEvent>,
    process_detector: Option<therminal_terminal::process_detector::ProcessDetector>,
}

// ── App-side PtyReaderHandler ──────────────────────────────────────────

/// Handler that runs the interceptor + process detector and wakes the event loop.
struct AppPtyHandler {
    wake: Box<dyn Fn() + Send + 'static>,
    on_exit: Option<Box<dyn FnOnce() + Send + 'static>>,
    interceptor_config: therminal_terminal::interceptor::InterceptorConfig,
    scan_interval_secs: u64,
    status: Arc<Mutex<PaneStatus>>,
    /// Lazily initialised on the reader thread.
    reader_state: Option<ReaderState>,
}

impl AppPtyHandler {
    fn ensure_init(&mut self) {
        if self.reader_state.is_some() {
            return;
        }
        use std::time::Duration;
        use therminal_terminal::interceptor::TherminalInterceptor;
        use therminal_terminal::process_detector::ProcessDetector;

        let (interceptor, event_rx) = TherminalInterceptor::new(self.interceptor_config.clone());

        let scan_interval = if self.scan_interval_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(self.scan_interval_secs))
        };
        let process_detector =
            scan_interval.map(|interval| ProcessDetector::new(None).with_interval(interval));

        self.reader_state = Some(ReaderState {
            interceptor,
            event_rx,
            process_detector,
        });
    }
}

impl PtyReaderHandler for AppPtyHandler {
    type Listener = PaneListener;

    fn process_bytes(
        &mut self,
        processor: &mut ansi::Processor<ansi::StdSyncHandler>,
        term: &Arc<FairMutex<Term<PaneListener>>>,
        data: &[u8],
    ) {
        self.ensure_init();
        let state = self.reader_state.as_mut().unwrap();

        {
            let mut term_guard = term.lock();
            processor.advance_with_interceptor(&mut *term_guard, &mut state.interceptor, data);
        }

        // Drain intercepted events and update shared status.
        while let Ok(event) = state.event_rx.try_recv() {
            use therminal_terminal::interceptor::InterceptedEvent;
            match event {
                InterceptedEvent::CurrentDirectory(path) => {
                    if let Ok(mut s) = self.status.lock() {
                        s.cwd = Some(path);
                    }
                }
                InterceptedEvent::Osc633(
                    therminal_terminal::osc633::Osc633Mark::CommandFinished { exit_code },
                )
                | InterceptedEvent::Osc133(
                    therminal_terminal::osc633::Osc633Mark::CommandFinished { exit_code },
                ) => {
                    if let Ok(mut s) = self.status.lock() {
                        s.last_exit_code = exit_code;
                    }
                }
                _ => {}
            }
        }

        // Run process-tree scan if enabled and interval has elapsed.
        if let Some(ref mut detector) = state.process_detector
            && let Some(agents) = detector.scan_if_due()
        {
            if let Ok(mut s) = self.status.lock() {
                s.agent_name = agents.first().map(|a| a.name.clone());
            }
            if !agents.is_empty() {
                tracing::debug!("detected agents: {:?}", agents);
            }
        }

        (self.wake)();
    }

    fn on_eof(&mut self) {
        info!("Pane PTY closed (EOF)");
        if let Some(on_exit) = self.on_exit.take() {
            on_exit();
        }
    }

    fn on_error(&mut self, _error: &std::io::Error) {
        if let Some(on_exit) = self.on_exit.take() {
            on_exit();
        }
    }
}

// ── Public spawn function ──────────────────────────────────────────────

/// Spawn a new pane with its own PTY, Term, and reader thread.
///
/// `callback_fn` is called with the pane_id to create wake and exit callbacks.
///
/// `interceptor_config` controls which OSC sequence families are intercepted.
/// `scan_interval_secs` sets the process-detector scan interval (0 = disabled).
#[allow(clippy::too_many_arguments)]
pub fn spawn_pane<F>(
    viewport: Rect,
    renderer: &GridRenderer,
    scrollback_lines: usize,
    interceptor_config: therminal_terminal::interceptor::InterceptorConfig,
    scan_interval_secs: u64,
    spawn_options: &therminal_terminal::pty::SpawnOptions,
    callback_fn: F,
) -> Result<PaneState, anyhow::Error>
where
    F: FnOnce(PaneId) -> PaneCallbacks,
{
    let id = next_pane_id();
    let (cols, rows) = grid_size_for_rect(viewport, renderer);
    let cols = cols.max(2);
    let rows = rows.max(1);

    // Shared status for status bar rendering.
    let status = Arc::new(Mutex::new(PaneStatus::default()));

    let callbacks = callback_fn(id);

    let handler = AppPtyHandler {
        wake: callbacks.wake,
        on_exit: Some(callbacks.on_exit),
        interceptor_config,
        scan_interval_secs,
        status: Arc::clone(&status),
        reader_state: None,
    };

    let mut core = PtyPaneCore::spawn(
        cols,
        rows,
        scrollback_lines,
        PaneListener,
        spawn_options,
        handler,
    )
    .map_err(|e| anyhow::anyhow!("failed to spawn shell for pane: {e}"))?;

    info!(pane_id = id, cols, rows, "Pane spawned");

    let term = Arc::clone(core.term());
    let pty_writer = core.take_writer();
    let pty_master = core.take_pty_master();

    Ok(PaneState {
        id,
        term,
        pty_writer,
        pty_master,
        viewport,
        scrollback_lines,
        status,
    })
}
