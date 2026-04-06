//! Pane spawning: PTY creation, reader thread, and lifecycle callbacks.

use std::io::Read as IoRead;
use std::sync::{Arc, Mutex};
use std::thread;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;
use therminal_core::geometry::Rect;
use tracing::{info, warn};

use super::state::{grid_size_for_rect, PaneState, PaneStatus, PaneTermSize};
use super::PaneId;
use super::PaneListener;
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

    let term_config = TermConfig {
        scrolling_history: scrollback_lines,
        ..Default::default()
    };
    let term_size = PaneTermSize {
        columns: cols,
        screen_lines: rows,
    };
    let term = Term::new(term_config, &term_size, PaneListener);
    let term = Arc::new(FairMutex::new(term));

    let (pty_master, _child) =
        therminal_terminal::pty::spawn_shell_with_options(cols as u16, rows as u16, spawn_options)
            .map_err(|e| anyhow::anyhow!("failed to spawn shell for pane: {e}"))?;

    let pty_reader = pty_master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("failed to clone PTY reader for pane: {e}"))?;
    let pty_writer = pty_master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("failed to get PTY writer for pane: {e}"))?;

    // Shared status for status bar rendering.
    let status = Arc::new(Mutex::new(PaneStatus::default()));
    let status_for_reader = Arc::clone(&status);

    // Spawn PTY reader thread for this pane.
    let term_for_reader = Arc::clone(&term);
    let callbacks = callback_fn(id);
    let wake = callbacks.wake;
    let on_exit = callbacks.on_exit;
    thread::Builder::new()
        .name(format!("pty-reader-{id}"))
        .spawn(move || {
            pane_pty_reader_loop(
                pty_reader,
                term_for_reader,
                wake,
                interceptor_config,
                scan_interval_secs,
                status_for_reader,
            );
            on_exit();
        })
        .map_err(|e| anyhow::anyhow!("failed to spawn pane PTY reader thread: {e}"))?;

    info!(pane_id = id, cols, rows, "Pane spawned");

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

/// PTY reader loop for a single pane.
fn pane_pty_reader_loop(
    mut reader: Box<dyn IoRead + Send>,
    term: Arc<FairMutex<Term<PaneListener>>>,
    wake: Box<dyn Fn() + Send + 'static>,
    interceptor_config: therminal_terminal::interceptor::InterceptorConfig,
    scan_interval_secs: u64,
    status: Arc<Mutex<PaneStatus>>,
) {
    use std::time::Duration;

    use therminal_terminal::interceptor::{InterceptedEvent, TherminalInterceptor};
    use therminal_terminal::process_detector::ProcessDetector;

    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();
    let (mut interceptor, event_rx) = TherminalInterceptor::new(interceptor_config);

    // Build process detector; 0 = disabled (interval set to 0 yields instant rescans,
    // so we gate on the configured value before constructing).
    let scan_interval = if scan_interval_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(scan_interval_secs))
    };
    let mut process_detector =
        scan_interval.map(|interval| ProcessDetector::new(None).with_interval(interval));

    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                info!("Pane PTY closed (EOF)");
                break;
            }
            Ok(n) => {
                {
                    let mut term_guard = term.lock();
                    processor.advance_with_interceptor(
                        &mut *term_guard,
                        &mut interceptor,
                        &buf[..n],
                    );
                }

                // Drain intercepted events and update shared status.
                while let Ok(event) = event_rx.try_recv() {
                    match event {
                        InterceptedEvent::CurrentDirectory(path) => {
                            if let Ok(mut s) = status.lock() {
                                s.cwd = Some(path);
                            }
                        }
                        InterceptedEvent::Osc633(
                            therminal_terminal::osc633::Osc633Mark::CommandFinished { exit_code },
                        )
                        | InterceptedEvent::Osc133(
                            therminal_terminal::osc633::Osc633Mark::CommandFinished { exit_code },
                        ) => {
                            if let Ok(mut s) = status.lock() {
                                s.last_exit_code = exit_code;
                            }
                        }
                        _ => {}
                    }
                }

                // Run process-tree scan if enabled and interval has elapsed.
                if let Some(ref mut detector) = process_detector {
                    if let Some(agents) = detector.scan_if_due() {
                        if let Ok(mut s) = status.lock() {
                            s.agent_name = agents.first().map(|a| a.name.clone());
                        }
                        if !agents.is_empty() {
                            tracing::debug!("detected agents: {:?}", agents);
                        }
                    }
                }
                wake();
            }
            Err(e) => {
                warn!("Pane PTY read error: {e}");
                break;
            }
        }
    }
}
