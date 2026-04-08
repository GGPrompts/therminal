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
use tracing::{debug, info, warn};

use super::PaneId;
use super::PaneListener;
use super::backend::{PaneBackendKind, REMOTE_PTY_LIVE_TASKS, RemotePtyGuard};
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
) -> Result<
    (
        PaneState,
        therminal_protocol::SessionId,
        therminal_protocol::PaneId,
    ),
    anyhow::Error,
> {
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

    let state = build_remote_pane_state(
        local_id,
        remote_pane_id,
        viewport,
        cols,
        rows,
        scrollback_lines,
        interceptor_config,
        daemon_client,
        tokio_handle,
        daemon_socket,
        callbacks,
    )?;
    Ok((state, session_id, remote_pane_id))
}

/// Build a `PaneState` whose backend is a `RemotePty` subscribed to a
/// pre-existing daemon pane.
///
/// This is the reusable guts of the subscription worker + `Term` wiring,
/// independent of whether the daemon session is brand new or pre-existing.
/// Both `spawn_remote_pane` (fresh) and `attach_to_existing_session`
/// (loading) call into this helper.
///
/// Caller is responsible for having already discovered `remote_pane_id`
/// (via `CreateSession`+`GetWorkspaces`, or via `GetWorkspaces` on an
/// existing session at attach time).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_remote_pane_state(
    local_id: PaneId,
    remote_pane_id: therminal_protocol::PaneId,
    viewport: Rect,
    cols: usize,
    rows: usize,
    scrollback_lines: usize,
    interceptor_config: InterceptorConfig,
    daemon_client: Arc<DaemonClient>,
    tokio_handle: tokio::runtime::Handle,
    daemon_socket: std::path::PathBuf,
    callbacks: PaneCallbacks,
) -> Result<PaneState, anyhow::Error> {
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
    // tn-zamd: when the main thread successfully captures a structured
    // `PaneStateSnapshot` from the daemon, it sets `snapshot_applied` so
    // the forwarder drops any bytes buffered since subscribe (they're
    // stale relative to the snapshot we just painted onto the local Term)
    // and starts forwarding live bytes. If the snapshot RPC fails, the
    // flag stays false and the tn-wlu6 ESC[2J heuristic below remains
    // in charge.
    let snapshot_applied = Arc::new(AtomicBool::new(false));
    let byte_tx_for_main = byte_tx.clone();
    let snapshot_applied_for_forwarder = Arc::clone(&snapshot_applied);
    let shutdown_for_forwarder = Arc::clone(&shutdown);
    let forwarder_socket = daemon_socket.clone();
    REMOTE_PTY_LIVE_TASKS.fetch_add(1, Ordering::Release);
    let forwarder_handle = tokio_handle.spawn(async move {
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

        // tn-wlu6 mitigation: when the GUI attaches to a daemon pane that
        // already hosts a running TUI (e.g. Bubble Tea, htop), the TUI's
        // ongoing redraw loop is mid-frame. The first chunks the GUI
        // receives are partial repaints — frames the TUI emits BEFORE its
        // next clear-screen. The local Term is fresh and empty, so it
        // ingests these partial frames and pushes them into scrollback as
        // each subsequent frame "appends" to the screen, producing 2-3
        // screens of phantom scrollback (see beads tn-wlu6 for the trace).
        //
        // Heuristic: during a short post-subscribe window, buffer incoming
        // chunks instead of forwarding immediately. If we observe an
        // ESC[2J (clear screen) within the window, drop everything before
        // it and start forwarding from the clear onward — that's the
        // first frame the TUI emits cleanly. If the window expires
        // without ever seeing ESC[2J (plain shells, non-TUI panes),
        // forward the buffered chunks as-is so we don't lose any output.
        //
        // This is a narrow, GUI-side mitigation. The principled fix
        // (snapshot daemon Term state on attach including mode flags for
        // mouse capture) lives in tn-zamd and is intentionally out of
        // scope here.
        const PRE_CLEAR_BUFFER_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
        const ESC_2J: &[u8] = &[0x1b, 0x5b, 0x32, 0x4a];

        let buffer_deadline = std::time::Instant::now() + PRE_CLEAR_BUFFER_WINDOW;
        let mut pre_clear_buffer: Vec<Vec<u8>> = Vec::new();
        let mut buffering = true;

        // Helper: forward bytes into the worker, returns false if the
        // worker channel is gone (forwarder should exit).
        let send_bytes = |bytes: Vec<u8>| -> bool { byte_tx.send(bytes).is_ok() };

        loop {
            if shutdown_for_forwarder.load(Ordering::Acquire) {
                break;
            }

            // tn-zamd: if the main thread just finished applying a
            // structured snapshot, drop buffered stale bytes and go live.
            if buffering && snapshot_applied_for_forwarder.load(Ordering::Acquire) {
                debug!(
                    remote_pane_id,
                    dropped_chunks = pre_clear_buffer.len(),
                    "tn-zamd: snapshot applied, dropping pre-snapshot buffer"
                );
                pre_clear_buffer.clear();
                buffering = false;
            }

            // While buffering, race the recv against the buffer deadline so
            // we flush even if no new bytes arrive (e.g. plain shell idle).
            let event = if buffering {
                let now = std::time::Instant::now();
                if now >= buffer_deadline {
                    debug!(
                        remote_pane_id,
                        chunks = pre_clear_buffer.len(),
                        "tn-wlu6: pre-clear window expired without ESC[2J, flushing buffer"
                    );
                    for chunk in pre_clear_buffer.drain(..) {
                        if !send_bytes(chunk) {
                            return;
                        }
                    }
                    buffering = false;
                    sub_client.recv_event().await
                } else {
                    let remaining = buffer_deadline - now;
                    match tokio::time::timeout(remaining, sub_client.recv_event()).await {
                        Ok(ev) => ev,
                        Err(_) => continue, // deadline hit; loop top will flush
                    }
                }
            } else {
                sub_client.recv_event().await
            };

            match event {
                Some(DaemonEvent::PaneOutput { pane_id, data, .. })
                    if pane_id == remote_pane_id =>
                {
                    if buffering {
                        // Scan this chunk for ESC[2J. If present, drop the
                        // buffered prefix and forward from the clear onward.
                        if let Some(pos) = find_subsequence(&data, ESC_2J) {
                            debug!(
                                remote_pane_id,
                                dropped_chunks = pre_clear_buffer.len(),
                                clear_offset = pos,
                                "tn-wlu6: ESC[2J found, dropping pre-clear buffer"
                            );
                            pre_clear_buffer.clear();
                            buffering = false;
                            // Forward from the clear-screen byte onward.
                            if !send_bytes(data[pos..].to_vec()) {
                                break;
                            }
                        } else {
                            pre_clear_buffer.push(data);
                        }
                    } else if !send_bytes(data) {
                        break;
                    }
                }
                Some(DaemonEvent::PaneExited { pane_id, .. }) if pane_id == remote_pane_id => {
                    info!(remote_pane_id, "remote pane exit event");
                    break;
                }
                None => break,
                _ => {}
            }
        }
    });

    // ── 4b. tn-zamd: capture daemon-side pane state and replay it ──────
    //
    // Subscribe-then-capture ordering means any bytes that flow into the
    // daemon Term between Subscribe and CapturePaneState end up in the
    // subscription queue AND are reflected in the captured grid. That's
    // fine for mode flags and cursor (idempotent) and acceptable for
    // grid contents in V1. The forwarder drops its pre-snapshot buffer
    // once `snapshot_applied` is set, so post-capture live bytes resume
    // normally.
    //
    // Runs inline (blocking on the tokio handle) so the pane is fully
    // initialized before this function returns. Capture failure is
    // non-fatal: we log and fall through, leaving the tn-wlu6 heuristic
    // to handle the local Term startup.
    {
        let capture_client = Arc::clone(&daemon_client);
        let capture_res = tokio_handle.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                capture_client.capture_pane_state(remote_pane_id),
            )
            .await
        });
        match capture_res {
            Ok(Ok(snap)) => {
                let bytes = snap.to_replay_bytes();
                debug!(
                    remote_pane_id,
                    modes = ?snap.modes,
                    cols = snap.cols,
                    rows = snap.rows,
                    replay_bytes = bytes.len(),
                    "tn-zamd: applying captured pane state to local Term"
                );
                // Feed the synthesized bytes directly into the worker
                // thread, ahead of any live bytes the forwarder has
                // buffered (which we're about to drop).
                if byte_tx_for_main.send(bytes).is_err() {
                    warn!(
                        remote_pane_id,
                        "tn-zamd: worker channel closed before snapshot replay"
                    );
                }
                snapshot_applied.store(true, Ordering::Release);
            }
            Ok(Err(e)) => {
                warn!(remote_pane_id, error = %e, "tn-zamd: CapturePaneState failed; falling back to tn-wlu6 heuristic");
            }
            Err(_) => {
                warn!(
                    remote_pane_id,
                    "tn-zamd: CapturePaneState timed out; falling back to tn-wlu6 heuristic"
                );
            }
        }
    }

    // ── 5. Spawn the tokio writer task: drain input_tx → SendKeys ──────
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer_client = Arc::clone(&daemon_client);
    let writer_handle = tokio_handle.spawn(async move {
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

    Ok(PaneState {
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
            guard: RemotePtyGuard {
                shutdown,
                forwarder: Some(forwarder_handle),
                writer: Some(writer_handle),
            },
        },
    })
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

/// Find the first occurrence of `needle` in `haystack`. Returns the byte
/// offset of the start of the match, or None if not found. Used by the
/// tn-wlu6 pre-clear-buffer mitigation to scan for ESC[2J in incoming
/// PaneOutput chunks.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
