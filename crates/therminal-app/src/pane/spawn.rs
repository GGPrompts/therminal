//! Pane spawning: PTY creation via shared PtyPaneCore, reader thread, and lifecycle callbacks.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi;
use therminal_core::geometry::Rect;
use therminal_terminal::pty_runtime::{PtyPaneCore, PtyReaderHandler};
use therminal_terminal::region_index::RegionIndex;
use tracing::info;

use super::PaneId;
use super::PaneListener;
use super::backend::PaneBackendKind;
use super::state::{PaneState, PaneStatus, grid_size_for_rect_with_header};
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
    /// Called when a BEL character is received.
    pub on_bell: Box<dyn Fn() + Send + 'static>,
    /// Called when a desktop notification is requested (OSC 9).
    pub on_notification: Box<dyn Fn(String) + Send + 'static>,
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
    pane_id: PaneId,
    wake: Box<dyn Fn() + Send + 'static>,
    on_exit: Option<Box<dyn FnOnce() + Send + 'static>>,
    interceptor_config: therminal_terminal::interceptor::InterceptorConfig,
    scan_interval_secs: u64,
    status: Arc<Mutex<PaneStatus>>,
    /// Shared semantic region index, updated from intercepted events.
    region_index: Arc<Mutex<RegionIndex>>,
    /// Shared agent registry for auto-tiling.
    agent_registry: Option<Arc<Mutex<therminal_terminal::agent_registry::AgentRegistry>>>,
    /// Whether we currently have an agent registered for this pane.
    has_registered_agent: bool,
    /// Shared bell flag from PaneListener (set when BEL fires).
    bell_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Callback to fire when a bell is detected.
    on_bell: Box<dyn Fn() + Send + 'static>,
    /// Callback to fire for desktop notifications (OSC 9).
    on_notification: Box<dyn Fn(String) + Send + 'static>,
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
        let Some(state) = self.reader_state.as_mut() else {
            return;
        };

        let current_line = {
            let mut term_guard = term.lock();
            processor.advance_with_interceptor(&mut *term_guard, &mut state.interceptor, data);
            // Compute absolute line for region indexing: scrollback history +
            // cursor row within the visible viewport.
            use alacritty_terminal::grid::Dimensions;
            let grid = term_guard.grid();
            grid.history_size() + grid.cursor.point.line.0.max(0) as usize
        };
        if let Ok(mut idx) = self.region_index.lock() {
            idx.set_current_line(current_line);
        }

        // Check if a BEL fired during processing.
        if self.bell_flag.swap(false, Ordering::AcqRel) {
            (self.on_bell)();
        }

        // Drain intercepted events and update shared status + region index.
        while let Ok(event) = state.event_rx.try_recv() {
            use therminal_terminal::interceptor::InterceptedEvent;
            if let Ok(mut idx) = self.region_index.lock() {
                idx.push_event(&event);
            }
            match event {
                // OSC 7 only — OSC 9;9 (WslCwd) carries the same location
                // as a Windows-native path, and overwriting `s.cwd` with
                // it breaks WSL pane detection (tn-5o34): `is_wsl_pane_path`
                // requires POSIX-absolute cwd to route hotspot clicks
                // through the WSL branch. `linux_to_unc` recomputes the
                // Windows form on demand wherever it's actually needed.
                InterceptedEvent::CurrentDirectory(path) => {
                    if let Ok(mut s) = self.status.lock() {
                        s.git_state = crate::git_state::detect(std::path::Path::new(&path));
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
                InterceptedEvent::DesktopNotification(text) => {
                    (self.on_notification)(text);
                }
                // tn-nhbv: cooperative agent self-report via OSC 7777.
                // Store tokens + model for the pane-header context gauge.
                // We deliberately leave `agent_name` to the process-tree
                // detector path above so tn-5fgz's chrome surface doesn't
                // race with OSC-reported identity.
                InterceptedEvent::AgentReport { tokens, model, .. } => {
                    if (tokens.is_some() || model.is_some())
                        && let Ok(mut s) = self.status.lock()
                    {
                        if let Some(t) = tokens {
                            s.agent_tokens = Some(t);
                        }
                        if let Some(m) = model {
                            s.agent_model = Some(m);
                        }
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

            // Update agent registry for auto-tiling.
            if let Some(ref registry) = self.agent_registry
                && let Ok(mut reg) = registry.lock()
            {
                if let Some(agent) = agents.first() {
                    reg.register(
                        self.pane_id,
                        agent.name.clone(),
                        agent.agent_type,
                        Some(agent.pid),
                    );
                    self.has_registered_agent = true;
                } else if self.has_registered_agent {
                    reg.unregister(self.pane_id);
                    self.has_registered_agent = false;
                }
            }
        }

        (self.wake)();
    }

    fn on_eof(&mut self) {
        info!("Pane PTY closed (EOF)");
        // Unregister from agent registry on exit.
        if self.has_registered_agent
            && let Some(ref registry) = self.agent_registry
            && let Ok(mut reg) = registry.lock()
        {
            reg.unregister(self.pane_id);
            self.has_registered_agent = false;
        }
        if let Some(on_exit) = self.on_exit.take() {
            on_exit();
        }
    }

    fn on_error(&mut self, _error: &std::io::Error) {
        // Unregister from agent registry on error.
        if self.has_registered_agent
            && let Some(ref registry) = self.agent_registry
            && let Ok(mut reg) = registry.lock()
        {
            reg.unregister(self.pane_id);
            self.has_registered_agent = false;
        }
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
/// `agent_registry` is an optional shared registry for auto-tiling integration.
#[allow(clippy::too_many_arguments)]
pub fn spawn_pane<F>(
    viewport: Rect,
    renderer: &GridRenderer,
    scrollback_lines: usize,
    interceptor_config: therminal_terminal::interceptor::InterceptorConfig,
    scan_interval_secs: u64,
    spawn_options: &therminal_terminal::pty::SpawnOptions,
    agent_registry: Option<Arc<Mutex<therminal_terminal::agent_registry::AgentRegistry>>>,
    callback_fn: F,
    header_h: f32,
) -> Result<PaneState, anyhow::Error>
where
    F: FnOnce(PaneId) -> PaneCallbacks,
{
    let id = next_pane_id();
    let (cols, rows) = grid_size_for_rect_with_header(viewport, renderer, header_h);
    let cols = cols.max(2);
    let rows = rows.max(1);

    // Shared status for status bar rendering.
    let status = Arc::new(Mutex::new(PaneStatus::default()));
    // Shared semantic region index, populated from intercepted events.
    let region_index = Arc::new(Mutex::new(RegionIndex::new()));

    let callbacks = callback_fn(id);

    let listener = PaneListener::new();
    let bell_flag = Arc::clone(&listener.bell_pending);

    let handler = AppPtyHandler {
        pane_id: id,
        wake: callbacks.wake,
        on_exit: Some(callbacks.on_exit),
        interceptor_config,
        scan_interval_secs,
        status: Arc::clone(&status),
        region_index: Arc::clone(&region_index),
        agent_registry,
        has_registered_agent: false,
        bell_flag,
        on_bell: callbacks.on_bell,
        on_notification: callbacks.on_notification,
        reader_state: None,
    };

    let mut core = PtyPaneCore::spawn(
        cols,
        rows,
        scrollback_lines,
        listener,
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
        viewport,
        status,
        region_index,
        backend: PaneBackendKind::Terminal {
            term,
            pty_writer,
            pty_master,
            scrollback_lines,
        },
        pinned: false,
    })
}

/// Create a WebView pane (tn-s5vj). No PTY, no Term — the content is
/// rendered by a platform-native webview managed by `WebViewManager`.
/// The pane participates in layout like any other pane.
#[allow(dead_code)]
pub fn spawn_webview_pane(viewport: Rect, url: &str) -> PaneState {
    let id = next_pane_id();
    let status = Arc::new(Mutex::new(PaneStatus::default()));
    let region_index = Arc::new(Mutex::new(RegionIndex::new()));

    info!(pane_id = id, url, "WebView pane created");

    PaneState {
        id,
        viewport,
        status,
        region_index,
        backend: PaneBackendKind::WebView {
            url: url.to_string(),
            content: String::new(),
        },
        pinned: false,
    }
}
