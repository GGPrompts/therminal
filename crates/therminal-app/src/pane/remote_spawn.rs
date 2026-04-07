//! Remote pane spawning: ask the daemon for a pane and wire its byte
//! stream into a local `Term` so the GUI renderer treats it identically
//! to a local PTY-backed pane.
//!
//! This is the GUI side of tn-5ps8. The daemon owns the PTY child; the
//! GUI owns a local `Term<PaneListener>` that is fed by a worker thread
//! consuming bytes streamed over IPC via `DaemonEvent::PaneOutput`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;
use therminal_core::geometry::Rect;
use therminal_daemon_client::DaemonClient;
use therminal_protocol::daemon::{DaemonEvent, EventKind, IpcRequest, IpcResponse};
use therminal_terminal::interceptor::{InterceptorConfig, TherminalInterceptor};
use therminal_terminal::pty_runtime::TermSize;
use therminal_terminal::region_index::RegionIndex;
use tracing::{info, warn};

use super::PaneId;
use super::PaneListener;
use super::backend::PaneBackendKind;
use super::spawn::PaneCallbacks;
use super::state::{PaneState, PaneStatus, grid_size_for_rect};
use crate::grid_renderer::GridRenderer;

/// Spawn a new pane backed by the daemon.
///
/// This is the remote-mode counterpart to `spawn::spawn_pane`. It:
///
/// 1. Asks the daemon to create a session (`IpcRequest::CreateSession`).
/// 2. Looks up the session's first pane via `CapturePane`-style query
///    (currently the daemon assigns one pane per fresh session, so we
///    use the returned session id and discover the pane id via the
///    daemon's pane list — best-effort scaffolding until tn-ytw2
///    formalises the attach handshake).
/// 3. Subscribes a per-pane forwarder task to `EventKind::PaneOutput`
///    that pushes byte chunks into a worker thread running the same
///    `advance_with_interceptor()` loop the local PTY reader uses.
/// 4. Wires `write_input` -> `IpcRequest::SendKeys` via a tokio task
///    drained by an unbounded mpsc channel.
///
/// `daemon_client` is the GUI's primary client connection. We open a
/// **second** dedicated `DaemonClient` connection for this pane's event
/// subscription so other panes' subscriptions don't contend on a single
/// shared event receiver. The primary client is reused for input and
/// resize requests.
///
/// **Status**: scaffolding for tn-5ps8. The daemon's `ResizePane` and
/// `PaneExited` handlers are stubs (tn-5rm0); until those land, resize
/// updates the local Term but the remote PTY size lags, and exit relies
/// on EOF semantics that the daemon does not yet broadcast.
#[allow(clippy::too_many_arguments)]
pub fn spawn_remote_pane(
    local_id: PaneId,
    viewport: Rect,
    renderer: &GridRenderer,
    scrollback_lines: usize,
    interceptor_config: InterceptorConfig,
    daemon_client: Arc<DaemonClient>,
    tokio_handle: tokio::runtime::Handle,
    daemon_socket: std::path::PathBuf,
    callbacks: PaneCallbacks,
) -> Result<(PaneState, therminal_protocol::PaneId), anyhow::Error> {
    let (cols, rows) = grid_size_for_rect(viewport, renderer);
    let cols = cols.max(2);
    let rows = rows.max(1);

    // ── 1. Create the remote session/pane and discover its real pane id ─
    //
    // tn-pgz6: we no longer assume `pane_id == session_id`. After
    // CreateSession we issue GetWorkspaces to ask the daemon which pane id
    // it actually allocated for the new session's first pane. Both calls
    // are wrapped in a 5s timeout so a slow/hung daemon doesn't freeze
    // GUI startup.
    let rpc_timeout = std::time::Duration::from_secs(5);
    let create_resp = tokio_handle.block_on(async {
        tokio::time::timeout(
            rpc_timeout,
            daemon_client.send_request(IpcRequest::CreateSession { name: None }),
        )
        .await
    });
    let create_resp = match create_resp {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("daemon CreateSession timed out after {rpc_timeout:?}"),
    };
    let session_id = match create_resp {
        IpcResponse::SessionCreated { session_id } => session_id,
        IpcResponse::Error { message } => {
            anyhow::bail!("daemon CreateSession failed: {message}");
        }
        other => {
            anyhow::bail!("unexpected daemon response to CreateSession: {other:?}");
        }
    };
    let workspaces_resp = tokio_handle.block_on(async {
        tokio::time::timeout(
            rpc_timeout,
            daemon_client.send_request(IpcRequest::GetWorkspaces { session_id }),
        )
        .await
    });
    let workspaces_resp = match workspaces_resp {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("daemon GetWorkspaces timed out after {rpc_timeout:?}"),
    };
    let remote_pane_id = match workspaces_resp {
        IpcResponse::Workspaces { workspaces, .. } => {
            // Look at the first workspace's focused pane (or first pane id).
            // The daemon spawns one pane per fresh session so this is
            // unambiguous; if it ever returns nothing that's a daemon bug.
            let pid = workspaces
                .first()
                .and_then(|w| w.focused_pane.or_else(|| w.pane_ids.first().copied()));
            match pid {
                Some(p) => p,
                None => {
                    tracing::error!(
                        session_id,
                        "daemon returned empty workspace state for fresh session"
                    );
                    anyhow::bail!(
                        "daemon GetWorkspaces returned no panes for session {session_id}"
                    );
                }
            }
        }
        IpcResponse::Error { message } => {
            anyhow::bail!("daemon GetWorkspaces failed: {message}");
        }
        other => {
            anyhow::bail!("unexpected daemon response to GetWorkspaces: {other:?}");
        }
    };
    info!(
        local_id,
        session_id, remote_pane_id, "spawned remote pane via daemon"
    );

    // ── 2. Build the local Term that the renderer reads from ──────────
    let term_config = TermConfig {
        scrolling_history: scrollback_lines,
        ..Default::default()
    };
    let term_size = TermSize {
        columns: cols,
        screen_lines: rows,
    };
    let listener = PaneListener::new();
    let bell_flag = Arc::clone(&listener.bell_pending);
    let term = Term::new(term_config, &term_size, listener);
    let term = Arc::new(FairMutex::new(term));

    let status = Arc::new(Mutex::new(PaneStatus::default()));
    let region_index = Arc::new(Mutex::new(RegionIndex::new()));

    // ── 3. Spawn the worker thread that runs advance_with_interceptor ─
    //
    // The worker reads byte chunks from a std::sync::mpsc channel that
    // is fed by the tokio forwarder task below. This mirrors the local
    // PTY reader_loop() — only the byte source differs.
    let (byte_tx, byte_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let shutdown = Arc::new(AtomicBool::new(false));

    let term_for_worker = Arc::clone(&term);
    let status_for_worker = Arc::clone(&status);
    let region_for_worker = Arc::clone(&region_index);
    let on_exit = callbacks.on_exit;
    let on_bell = callbacks.on_bell;
    let on_notification = callbacks.on_notification;
    let wake = callbacks.wake;
    let interceptor_cfg = interceptor_config.clone();
    let shutdown_for_worker = Arc::clone(&shutdown);
    let _worker = thread::Builder::new()
        .name(format!("remote-pty-worker-{local_id}"))
        .spawn(move || {
            run_remote_worker(
                term_for_worker,
                byte_rx,
                interceptor_cfg,
                status_for_worker,
                region_for_worker,
                bell_flag,
                wake,
                on_bell,
                on_notification,
                on_exit,
                shutdown_for_worker,
            );
        })
        .map_err(|e| anyhow::anyhow!("failed to spawn remote pty worker: {e}"))?;

    // ── 4. Spawn the tokio forwarder: subscribe to PaneOutput events ──
    //
    // We open a *dedicated* DaemonClient connection for this pane so
    // multiple RemotePty panes don't fight over a single shared event
    // receiver on the primary client. Input/resize still go via the
    // shared `daemon_client` to keep the request multiplexer hot.
    let shutdown_for_forwarder = Arc::clone(&shutdown);
    let forwarder_socket = daemon_socket.clone();
    tokio_handle.spawn(async move {
        let sub_client = match DaemonClient::connect(&forwarder_socket).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "remote pane forwarder failed to open subscription connection");
                return;
            }
        };
        if let Err(e) = sub_client
            .subscribe_events(vec![EventKind::PaneOutput, EventKind::PaneExited])
            .await
        {
            warn!(error = %e, "remote pane forwarder Subscribe failed");
            return;
        }
        loop {
            if shutdown_for_forwarder.load(Ordering::Acquire) {
                break;
            }
            match sub_client.recv_event().await {
                Some(DaemonEvent::PaneOutput { pane_id, data, .. })
                    if pane_id == remote_pane_id =>
                {
                    if byte_tx.send(data).is_err() {
                        break; // worker gone
                    }
                }
                Some(DaemonEvent::PaneExited { pane_id, .. }) if pane_id == remote_pane_id => {
                    info!(remote_pane_id, "remote pane exit event");
                    break;
                }
                None => break,
                _ => {} // unrelated event for another pane
            }
        }
    });

    // ── 5. Spawn the tokio writer task: drain input_tx → SendKeys ──────
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer_client = Arc::clone(&daemon_client);
    tokio_handle.spawn(async move {
        while let Some(bytes) = input_rx.recv().await {
            if let Err(e) = writer_client
                .send_request(IpcRequest::SendKeys {
                    pane_id: remote_pane_id,
                    keys: bytes,
                })
                .await
            {
                warn!(error = %e, "remote pane SendKeys failed");
            }
        }
    });

    let state = PaneState {
        id: local_id,
        viewport,
        status,
        region_index,
        backend: PaneBackendKind::RemotePty {
            pane_id: remote_pane_id,
            term,
            input_tx,
            daemon_client,
            tokio_handle,
            shutdown,
        },
    };
    Ok((state, remote_pane_id))
}

#[allow(clippy::too_many_arguments)]
fn run_remote_worker(
    term: Arc<FairMutex<Term<PaneListener>>>,
    byte_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    interceptor_config: InterceptorConfig,
    status: Arc<Mutex<PaneStatus>>,
    region_index: Arc<Mutex<RegionIndex>>,
    bell_flag: Arc<AtomicBool>,
    wake: Box<dyn Fn() + Send + 'static>,
    on_bell: Box<dyn Fn() + Send + 'static>,
    on_notification: Box<dyn Fn(String) + Send + 'static>,
    on_exit: Box<dyn FnOnce() + Send + 'static>,
    shutdown: Arc<AtomicBool>,
) {
    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();
    let (mut interceptor, event_rx) = TherminalInterceptor::new(interceptor_config);

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let chunk = match byte_rx.recv() {
            Ok(c) => c,
            Err(_) => break, // forwarder gone
        };

        let current_line = {
            let mut term_guard = term.lock();
            processor.advance_with_interceptor(&mut *term_guard, &mut interceptor, &chunk);
            use alacritty_terminal::grid::Dimensions;
            let grid = term_guard.grid();
            grid.history_size() + grid.cursor.point.line.0.max(0) as usize
        };
        if let Ok(mut idx) = region_index.lock() {
            idx.set_current_line(current_line);
        }

        if bell_flag.swap(false, Ordering::AcqRel) {
            (on_bell)();
        }

        while let Ok(event) = event_rx.try_recv() {
            use therminal_terminal::interceptor::InterceptedEvent;
            if let Ok(mut idx) = region_index.lock() {
                idx.push_event(&event);
            }
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
                InterceptedEvent::DesktopNotification(text) => {
                    (on_notification)(text);
                }
                _ => {}
            }
        }

        (wake)();
    }

    info!("remote pty worker exiting");
    on_exit();
    let _ = PaneId::default; // silence unused import in some cfgs
}
