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

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;
use therminal_core::geometry::Rect;
use therminal_daemon_client::{DaemonClient, GUI_REQUEST_TIMEOUT};
use therminal_protocol::daemon::{DaemonEvent, EventKind, IpcRequest, IpcResponse};
use therminal_terminal::interceptor::{InterceptorConfig, TherminalInterceptor};
use therminal_terminal::pty_runtime::TermSize;
use therminal_terminal::region_index::RegionIndex;
use tracing::{debug, info, warn};

// tn-l3hk: latency instrumentation — measure IPC round-trip overhead.
//
// We record wall-clock timestamps around each blocking IPC call inside
// `spawn_remote_pane` and `build_remote_pane_state` so we can compare
// the streamed-bytes model against a hypothetical direct-PTY model.
// All timings are emitted as tracing `info!` events at the INFO level
// so they surface in normal runs without needing RUST_LOG=debug.

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
    // F11 (tn-97j6): if Some, reuse this existing daemon session instead
    // of issuing a fresh CreateSession. Used when try_attach_existing_session
    // discovered an empty-workspace daemon session that would otherwise be
    // orphaned.
    reuse_session_id: Option<therminal_protocol::SessionId>,
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
    info!(
        cols,
        rows,
        viewport_w = viewport.width(),
        viewport_h = viewport.height(),
        "spawn_remote_pane: computed grid size for CreateSession"
    );

    // ── tn-l3hk: overall spawn latency start ─────────────────────────────
    let t_spawn_start = std::time::Instant::now();

    // ── 1. Create the remote session/pane and discover its real pane id ─
    //
    // tn-pgz6: we no longer assume `pane_id == session_id`. After
    // CreateSession we issue GetWorkspaces to ask the daemon which pane id
    // it actually allocated for the new session's first pane. Both calls
    // use `GUI_REQUEST_TIMEOUT` (10s) so a slow/hung daemon doesn't freeze
    // GUI startup.
    //
    // tn-glsq: the previous 5s timeout was too tight for Windows named pipe
    // IPC where pipe connection retries (up to 1s) stack on top of the
    // daemon's session creation latency. Aligning with GUI_REQUEST_TIMEOUT
    // gives enough headroom for the named pipe retry loop + daemon processing.
    let rpc_timeout = GUI_REQUEST_TIMEOUT;
    let session_id = if let Some(sid) = reuse_session_id {
        info!(
            session_id = sid,
            "reusing existing daemon session for spawn_remote_pane (F11)"
        );
        sid
    } else {
        // tn-l3hk: time the CreateSession round-trip
        let t_create = std::time::Instant::now();
        info!(
            timeout_ms = rpc_timeout.as_millis(),
            platform = std::env::consts::OS,
            "tn-glsq: CreateSession RPC starting"
        );
        let create_resp = tokio_handle.block_on(async {
            tokio::time::timeout(
                rpc_timeout,
                daemon_client.send_request(IpcRequest::CreateSession {
                    name: None,
                    cols: Some(cols as u16),
                    rows: Some(rows as u16),
                    shell: None,
                }),
            )
            .await
        });
        let t_create_ms = t_create.elapsed().as_millis();
        let create_resp = match create_resp {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                warn!(
                    elapsed_ms = t_create_ms,
                    platform = std::env::consts::OS,
                    error = %e,
                    "tn-glsq: CreateSession RPC failed"
                );
                return Err(e);
            }
            Err(_) => {
                warn!(
                    elapsed_ms = t_create_ms,
                    timeout_ms = rpc_timeout.as_millis(),
                    platform = std::env::consts::OS,
                    "tn-glsq: CreateSession RPC timed out -- if on Windows, \
                     named pipe retries may have consumed part of the budget"
                );
                anyhow::bail!("daemon CreateSession timed out after {rpc_timeout:?}");
            }
        };
        info!(
            rtt_ms = t_create_ms,
            "tn-l3hk: CreateSession IPC round-trip"
        );
        match create_resp {
            IpcResponse::SessionCreated { session_id } => session_id,
            IpcResponse::Error { message } => {
                anyhow::bail!("daemon CreateSession failed: {message}");
            }
            other => {
                anyhow::bail!("unexpected daemon response to CreateSession: {other:?}");
            }
        }
    };
    // tn-l3hk: time the GetWorkspaces round-trip
    let t_gw = std::time::Instant::now();
    let workspaces_resp = tokio_handle.block_on(async {
        tokio::time::timeout(
            rpc_timeout,
            daemon_client.send_request(IpcRequest::GetWorkspaces { session_id }),
        )
        .await
    });
    let t_gw_ms = t_gw.elapsed().as_millis();
    let workspaces_resp = match workspaces_resp {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            warn!(
                elapsed_ms = t_gw_ms,
                platform = std::env::consts::OS,
                error = %e,
                session_id,
                "tn-glsq: GetWorkspaces RPC failed"
            );
            return Err(e);
        }
        Err(_) => {
            warn!(
                elapsed_ms = t_gw_ms,
                timeout_ms = rpc_timeout.as_millis(),
                platform = std::env::consts::OS,
                session_id,
                "tn-glsq: GetWorkspaces RPC timed out"
            );
            anyhow::bail!("daemon GetWorkspaces timed out after {rpc_timeout:?}");
        }
    };
    info!(rtt_ms = t_gw_ms, "tn-l3hk: GetWorkspaces IPC round-trip");
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
        None,
    )?;

    // tn-l3hk: total spawn wall time (includes CreateSession + GetWorkspaces +
    // CapturePaneState + worker/forwarder thread creation).
    info!(
        total_ms = t_spawn_start.elapsed().as_millis(),
        local_id, session_id, remote_pane_id, "tn-l3hk: spawn_remote_pane total wall time"
    );

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
    initial_cwd: Option<String>,
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

    let status = Arc::new(Mutex::new(PaneStatus {
        cwd: initial_cwd,
        ..Default::default()
    }));
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
    // tn-zamd review fixup: while the CapturePaneState RPC is in flight,
    // the tn-wlu6 ESC[2J fast path must NOT independently transition out
    // of buffering — otherwise it can flush live post-clear bytes to the
    // worker before the snapshot replay arrives, and the snapshot would
    // then clobber the fresh live state. `snapshot_pending` starts true
    // and is cleared by the main thread once the RPC has resolved (success
    // OR failure OR timeout), at which point the ESC[2J path is allowed
    // to act as the fallback.
    let snapshot_pending = Arc::new(AtomicBool::new(true));
    let byte_tx_for_main = byte_tx.clone();
    let snapshot_applied_for_forwarder = Arc::clone(&snapshot_applied);
    let snapshot_pending_for_forwarder = Arc::clone(&snapshot_pending);
    let shutdown_for_forwarder = Arc::clone(&shutdown);
    let term_for_forwarder = Arc::clone(&term);
    let forwarder_socket = daemon_socket.clone();
    REMOTE_PTY_LIVE_TASKS.fetch_add(1, Ordering::Release);
    let forwarder_handle = tokio_handle.spawn(async move {
        // F5 (tn-97j6): wrap connect+subscribe in a timeout so a daemon
        // that accepts the socket but never responds to Subscribe doesn't
        // leave the forwarder hung forever (pane would appear live but
        // never receive bytes).
        //
        // tn-glsq: aligned with GUI_REQUEST_TIMEOUT (10s). On Windows, this
        // second named pipe connection contends with the primary client's
        // pipe instance -- the daemon must re-arm its listener before this
        // connect succeeds, and the retry loop in ipc_transport can burn
        // up to 1s. The previous 5s left only 4s for the actual Subscribe
        // round-trip after retries.
        let connect_timeout = GUI_REQUEST_TIMEOUT;
        let t_fwd_connect = std::time::Instant::now();
        let sub_client =
            match tokio::time::timeout(connect_timeout, DaemonClient::connect(&forwarder_socket))
                .await
            {
                Ok(Ok(c)) => {
                    debug!(
                        elapsed_ms = t_fwd_connect.elapsed().as_millis(),
                        platform = std::env::consts::OS,
                        "tn-glsq: forwarder subscription connection established"
                    );
                    c
                }
                Ok(Err(e)) => {
                    warn!(
                        elapsed_ms = t_fwd_connect.elapsed().as_millis(),
                        platform = std::env::consts::OS,
                        error = %e,
                        "tn-glsq: forwarder failed to open subscription connection"
                    );
                    return;
                }
                Err(_) => {
                    warn!(
                        elapsed_ms = t_fwd_connect.elapsed().as_millis(),
                        timeout_ms = connect_timeout.as_millis(),
                        platform = std::env::consts::OS,
                        "tn-glsq: forwarder subscription connect timed out"
                    );
                    return;
                }
            };
        match tokio::time::timeout(
            connect_timeout,
            sub_client.subscribe_events(vec![
                EventKind::PaneOutput,
                EventKind::PaneExited,
                EventKind::PaneResized,
            ]),
        )
        .await
        {
            Ok(Ok(_resp)) => {}
            Ok(Err(e)) => {
                warn!(
                    elapsed_ms = t_fwd_connect.elapsed().as_millis(),
                    platform = std::env::consts::OS,
                    error = %e,
                    "tn-glsq: forwarder Subscribe RPC failed"
                );
                return;
            }
            Err(_) => {
                warn!(
                    elapsed_ms = t_fwd_connect.elapsed().as_millis(),
                    timeout_ms = connect_timeout.as_millis(),
                    platform = std::env::consts::OS,
                    "tn-glsq: forwarder Subscribe RPC timed out"
                );
                return;
            }
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
            // structured snapshot, flush the buffered post-subscribe bytes
            // through to the worker and go live.
            //
            // tn-b77d: previous code DROPPED the buffer here. That lost any
            // DEC private-mode sequences (`\e[?2004h`, `\e[?1000h`,
            // `\e[?1049h`, ...) that the daemon emitted between Subscribe
            // and the snapshot replay landing on the worker, leaving the
            // GUI's local Term with stale mode flags forever (broken paste,
            // mouse, alt-screen, app cursor in TUIs spawned around attach
            // time). The snapshot's CUP+grid replay paints the visible grid
            // unconditionally, so any duplicated visible bytes in the buffer
            // (text that was already captured into the snapshot grid) are
            // immediately overwritten by the snapshot's paint pass — but
            // mode-set sequences are idempotent and need to be preserved so
            // the local Term tracks the daemon's authoritative TermMode.
            if buffering && snapshot_applied_for_forwarder.load(Ordering::Acquire) {
                debug!(
                    remote_pane_id,
                    flushed_chunks = pre_clear_buffer.len(),
                    "tn-zamd/tn-b77d: snapshot applied, flushing pre-snapshot buffer"
                );
                for chunk in pre_clear_buffer.drain(..) {
                    if !send_bytes(chunk) {
                        return;
                    }
                }
                buffering = false;
            }

            // While buffering, race the recv against the buffer deadline so
            // we flush even if no new bytes arrive (e.g. plain shell idle).
            let event = if buffering {
                let snapshot_done = !snapshot_pending_for_forwarder.load(Ordering::Acquire);
                if !snapshot_done {
                    // tn-zamd review fixup: while the CapturePaneState RPC is
                    // still in flight, ignore the tn-wlu6 deadline entirely —
                    // the snapshot replay path will set snapshot_applied (or
                    // clear snapshot_pending on failure) and the loop top will
                    // then take over.
                    sub_client.recv_event().await
                } else {
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
                }
            } else {
                sub_client.recv_event().await
            };

            match event {
                Some(DaemonEvent::PaneOutput { pane_id, data, .. })
                    if pane_id == remote_pane_id =>
                {
                    if buffering {
                        // tn-zamd review fixup: gate the ESC[2J fast path on
                        // the snapshot RPC having resolved. Otherwise we may
                        // race the main-thread snapshot replay and end up
                        // clobbering fresh live state with stale snapshot.
                        let snapshot_done = !snapshot_pending_for_forwarder.load(Ordering::Acquire);
                        // Scan this chunk for ESC[2J. If present (and the
                        // snapshot path is no longer in play), drop the
                        // buffered prefix and forward from the clear onward.
                        if snapshot_done && let Some(pos) = find_subsequence(&data, ESC_2J) {
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
                Some(DaemonEvent::PaneResized {
                    pane_id,
                    cols,
                    rows,
                    ..
                }) if pane_id == remote_pane_id => {
                    apply_remote_resize(&term_for_forwarder, cols as usize, rows as usize);
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
        // tn-l3hk: time the CapturePaneState round-trip (includes snapshot
        // serialization on the daemon side and deserialization here).
        let t_capture = std::time::Instant::now();
        let capture_res = tokio_handle.block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                capture_client.capture_pane_state(remote_pane_id),
            )
            .await
        });
        let t_capture_ms = t_capture.elapsed().as_millis();
        match capture_res {
            Ok(Ok(snap)) => {
                let bytes = snap.to_replay_bytes();
                info!(
                    remote_pane_id,
                    rtt_ms = t_capture_ms,
                    replay_bytes = bytes.len(),
                    "tn-l3hk: CapturePaneState IPC round-trip (snapshot succeeded)"
                );
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
                // tn-zamd review fixup: only mark the snapshot applied if
                // the worker actually received the replay bytes. Otherwise
                // the forwarder would drop its buffer for nothing and the
                // pane would end up blank.
                if byte_tx_for_main.send(bytes).is_ok() {
                    snapshot_applied.store(true, Ordering::Release);
                } else {
                    warn!(
                        remote_pane_id,
                        "tn-zamd: worker channel closed before snapshot replay"
                    );
                }
                // tn-166y: copy tags from the snapshot into PaneStatus so
                // the GUI can render tag badges in pane headers immediately.
                if !snap.tags.is_empty()
                    && let Ok(mut s) = status.lock()
                {
                    s.tags = snap.tags;
                }
            }
            Ok(Err(e)) => {
                info!(
                    remote_pane_id,
                    rtt_ms = t_capture_ms,
                    "tn-l3hk: CapturePaneState IPC round-trip (failed)"
                );
                warn!(remote_pane_id, error = %e, "tn-zamd: CapturePaneState failed; falling back to tn-wlu6 heuristic");
            }
            Err(_) => {
                info!(
                    remote_pane_id,
                    rtt_ms = t_capture_ms,
                    "tn-l3hk: CapturePaneState IPC round-trip (timed out)"
                );
                warn!(
                    remote_pane_id,
                    "tn-zamd: CapturePaneState timed out; falling back to tn-wlu6 heuristic"
                );
            }
        }
        // tn-zamd review fixup: regardless of outcome, the snapshot path is
        // no longer pending. Releasing this flag lets the forwarder's
        // tn-wlu6 ESC[2J fast path / deadline flush take over as fallback.
        snapshot_pending.store(false, Ordering::Release);
    }

    // ── 5. Spawn the tokio writer task: drain input_tx → SendKeys ──────
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer_client = Arc::clone(&daemon_client);
    let writer_handle = tokio_handle.spawn(async move {
        // F1 (tn-97j6): wrap SendKeys in a 2s timeout. Without it, a stalled
        // daemon write would let keystrokes pile up in the unbounded input
        // channel and the pane would go silently dead. On timeout, warn and
        // break the loop so the pane enters an error state cleanly.
        let send_timeout = std::time::Duration::from_secs(2);
        while let Some(bytes) = input_rx.recv().await {
            match tokio::time::timeout(
                send_timeout,
                writer_client.send_request(IpcRequest::SendKeys {
                    pane_id: remote_pane_id,
                    keys: bytes,
                }),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    warn!(error = %e, "remote pane SendKeys failed");
                }
                Err(_) => {
                    warn!(
                        remote_pane_id,
                        "remote pane SendKeys timed out after {send_timeout:?}; closing writer"
                    );
                    break;
                }
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
                    if let Ok(mut s) = status.lock() {
                        s.last_exit_code = exit_code;
                    }
                }
                InterceptedEvent::DesktopNotification(text) => {
                    (on_notification)(text);
                }
                // tn-nhbv: cooperative agent self-report via OSC 7777.
                // Store tokens + model for the pane-header context gauge.
                InterceptedEvent::AgentReport { tokens, model, .. } => {
                    if (tokens.is_some() || model.is_some())
                        && let Ok(mut s) = status.lock()
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

fn apply_remote_resize(term: &Arc<FairMutex<Term<PaneListener>>>, cols: usize, rows: usize) {
    let mut guard = term.lock();
    if guard.columns() == cols && guard.screen_lines() == rows {
        return;
    }
    resize_remote_term_without_scrollback_pollution(&mut guard, cols, rows);
}

/// Resize a remote pane's local shadow `Term` without letting pre-resize
/// viewport content land in scrollback history (tn-ebdu).
///
/// ## Why this exists
///
/// A local (GUI-side) `Term` backing a `PaneBackendKind::RemotePty` is a
/// shadow of the daemon-side authoritative `Term`. It replays whatever
/// bytes the daemon streams over IPC. On a window resize the GUI has to
/// resize the shadow before the daemon broadcasts the post-SIGWINCH
/// repaint — but alacritty's primary-screen resize + TUI repaint
/// pipeline leaks duplicate content into scrollback every time it runs:
///
/// 1. `Grid::shrink_lines` (the path `Term::resize` takes on a row
///    shrink) scrolls the pre-resize viewport into history via
///    `scroll_up` so the cursor stays inside the smaller window.
/// 2. The TUI reacts to SIGWINCH by re-emitting its rendered state. On
///    the primary screen, `ESC[2J` (clear entire screen) lands as
///    `Grid::clear_viewport`, which **also** scrolls the visible
///    non-empty rows into history before blanking. Any streaming TUI
///    that clears-and-repaints on resize therefore pushes a second copy
///    of the same content into history.
/// 3. `grow_lines` on a later grow re-pulls the oldest scrollback rows
///    back into the viewport, further entangling the state.
///
/// In a standard local terminal this is xterm-spec behavior — the
/// user's `clear` on a primary-screen shell legitimately scrolls
/// command output into scrollback. But on a RemotePty pane the shadow
/// has **no independent state** to preserve: the daemon owns the
/// authoritative Term, and the shadow exists only to render what the
/// daemon just streamed. Letting `shrink_lines` + `clear_viewport`
/// push shadow-viewport rows into shadow-history produces a duplicate
/// of content the user already saw, and the bug scales linearly with
/// resize count until scrollback becomes unusable.
///
/// ## The fix
///
/// Before invoking `Term::resize`, blank the **visible** grid rows via
/// `grid_mut().reset_region(..)`. This is a pure cell reset — no
/// scroll, no history mutation, no cursor move. Then resize. Now:
///
/// - `shrink_lines`'s `scroll_up` scrolls blanks (free rows) into
///   history, not pre-resize content. The display offset still moves,
///   but the content that lands in history is inert whitespace.
/// - The TUI's subsequent `ESC[2J` walks the blank viewport, finds
///   zero non-empty cells, and `scroll_up(region, 0)` is a no-op. No
///   new history rows.
/// - The TUI's re-paint bytes (whether cursor-up + rewrite, or
///   ESC[2J + rewrite, or any other rewrite strategy) lands on a
///   clean viewport and repopulates it in the new dimensions.
/// - Legitimate pre-resize scrollback above the viewport is
///   preserved untouched — `reset_region(..)` only touches visible
///   lines 0..screen_lines.
///
/// For `ALT_SCREEN` mode this is unnecessary (alt-screen content
/// never flows to scrollback anyway) and we let the vanilla
/// `Term::resize` path run. This also avoids an unwanted flash on
/// full-screen curses apps that rely on their own post-SIGWINCH
/// `redrawwin`.
///
/// ## Cost
///
/// A brief visible blank flash on resize if the TUI does not emit a
/// repaint promptly. In practice the daemon-side PTY receives SIGWINCH
/// and emits the repaint within a few milliseconds; the user sees the
/// resize complete and the new content populate in the same frame.
///
/// For local PTY panes (`PaneBackendKind::Terminal`) this helper is
/// not used — those panes are the authoritative Term and their
/// scrollback growth on resize is spec-compliant xterm behavior.
pub(crate) fn resize_remote_term_without_scrollback_pollution(
    guard: &mut Term<PaneListener>,
    cols: usize,
    rows: usize,
) {
    use alacritty_terminal::term::TermMode;

    // Alt-screen TUIs don't touch primary-screen scrollback, so the
    // bug doesn't fire and blanking would actively destroy content
    // the alt-screen curses app expects to still be there after
    // resize.
    let is_alt = guard.mode().contains(TermMode::ALT_SCREEN);
    if !is_alt {
        // Blank every visible row without scrolling them into history.
        // `reset_region(..)` takes lines 0..screen_lines and calls
        // `Row::reset` on each, leaving scrollback storage untouched.
        guard.grid_mut().reset_region(..);
    }

    guard.resize(TermSize {
        columns: cols,
        screen_lines: rows,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::TermMode;

    /// tn-b77d regression: feeding a DEC private mode-set sequence
    /// (`\e[?2004h`) through the same VTE pipeline used by
    /// `run_remote_worker` MUST update the local `Term`'s `BRACKETED_PASTE`
    /// flag. If this stops being true, every daemon-pane-attached TUI that
    /// turns on bracketed paste / mouse / alt-screen / app-cursor after
    /// attach time will silently get stale flags on the GUI side, breaking
    /// paste, mouse routing, and overlay decisions.
    ///
    /// This locks down the Option A invariant: the GUI's local `Term` is a
    /// real VTE parser, not just a snapshot replay target.
    #[test]
    fn local_term_picks_up_post_attach_bracketed_paste_decset() {
        let term_config = TermConfig {
            scrolling_history: 100,
            ..Default::default()
        };
        let term_size = TermSize {
            columns: 80,
            screen_lines: 24,
        };
        let listener = PaneListener::new();
        let term = Term::new(term_config, &term_size, listener);
        let term = Arc::new(FairMutex::new(term));

        let (mut interceptor, _event_rx) = TherminalInterceptor::new(InterceptorConfig::default());
        let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();

        // Sanity: starts off.
        assert!(
            !term.lock().mode().contains(TermMode::BRACKETED_PASTE),
            "fresh Term should not have BRACKETED_PASTE set"
        );

        // Feed the same byte stream a TUI like Claude Code would emit
        // shortly after attach, through the same call site as the worker.
        let bytes = b"\x1b[?2004h";
        {
            let mut guard = term.lock();
            processor.advance_with_interceptor(&mut *guard, &mut interceptor, bytes);
        }

        assert!(
            term.lock().mode().contains(TermMode::BRACKETED_PASTE),
            "BRACKETED_PASTE must be set after worker processes \\e[?2004h \
             — if this fails, the GUI's local Term has stopped tracking \
             daemon-side mode-set sequences (tn-b77d regression)"
        );
    }

    /// tn-b77d regression: same invariant for mouse modes. A TUI that
    /// enters alt-screen + SGR mouse after attach must end up with those
    /// flags reflected in the local Term, otherwise mouse clicks get
    /// swallowed for selection instead of being routed to the TUI.
    #[test]
    fn local_term_picks_up_post_attach_mouse_and_alt_screen() {
        let term_config = TermConfig {
            scrolling_history: 100,
            ..Default::default()
        };
        let term_size = TermSize {
            columns: 80,
            screen_lines: 24,
        };
        let listener = PaneListener::new();
        let term = Term::new(term_config, &term_size, listener);
        let term = Arc::new(FairMutex::new(term));

        let (mut interceptor, _event_rx) = TherminalInterceptor::new(InterceptorConfig::default());
        let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();

        // ?1002 = button + drag mouse, ?1006 = SGR encoding, ?1049 = alt screen.
        let bytes = b"\x1b[?1002h\x1b[?1006h\x1b[?1049h";
        {
            let mut guard = term.lock();
            processor.advance_with_interceptor(&mut *guard, &mut interceptor, bytes);
        }

        let mode = *term.lock().mode();
        assert!(
            mode.contains(TermMode::MOUSE_DRAG),
            "MOUSE_DRAG should be set"
        );
        assert!(
            mode.contains(TermMode::SGR_MOUSE),
            "SGR_MOUSE should be set"
        );
        assert!(
            mode.contains(TermMode::ALT_SCREEN),
            "ALT_SCREEN should be set"
        );
    }

    #[test]
    fn apply_remote_resize_updates_local_term_dimensions() {
        let term_config = TermConfig {
            scrolling_history: 100,
            ..Default::default()
        };
        let term_size = TermSize {
            columns: 80,
            screen_lines: 24,
        };
        let listener = PaneListener::new();
        let term = Arc::new(FairMutex::new(Term::new(term_config, &term_size, listener)));

        apply_remote_resize(&term, 120, 40);

        let guard = term.lock();
        assert_eq!(guard.columns(), 120);
        assert_eq!(guard.screen_lines(), 40);
    }

    /// tn-ebdu regression: resize must not duplicate visible streaming-TUI
    /// content into scrollback.
    ///
    /// Reproduces the Windows-native + WSL pane flagship bug where every
    /// resize cloned the Claude Code chat into scrollback. The triggering
    /// path: GUI-side `Term::resize` runs on the main thread before the
    /// daemon-side resize + SIGWINCH repaint bytes arrive; the resize
    /// itself preserves the current viewport, but the subsequent in-place
    /// repaint (common in TUIs that print their entire state from the top
    /// on SIGWINCH) scrolls the same rows into history on top of the
    /// already-scrolled initial paint, growing scrollback linearly with
    /// resize count.
    ///
    /// Before the fix: 5 resizes grow scrollback by ~5 * chat_len rows.
    /// After the fix: scrollback stays within ~baseline + noise.
    #[test]
    fn resize_does_not_duplicate_streaming_tui_into_scrollback() {
        let term_config = TermConfig {
            scrolling_history: 10_000,
            ..Default::default()
        };
        let initial_size = TermSize {
            columns: 80,
            screen_lines: 30,
        };
        let listener = PaneListener::new();
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &initial_size,
            listener,
        )));

        let (mut interceptor, _event_rx) = TherminalInterceptor::new(InterceptorConfig::default());
        let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();

        // Step 1: paint 200 lines of "chat" — more than one viewport's
        // worth. The bottom 30 rows are visible; the other 170 scroll
        // naturally into history.
        const CHAT_LINES: usize = 200;
        let mut initial_paint = String::new();
        initial_paint.push_str("\x1b[H");
        for i in 1..=CHAT_LINES {
            if i > 1 {
                initial_paint.push_str("\r\n");
            }
            initial_paint.push_str(&format!("chat line {i:03} content"));
        }
        {
            let mut guard = term.lock();
            processor.advance_with_interceptor(
                &mut *guard,
                &mut interceptor,
                initial_paint.as_bytes(),
            );
        }

        let baseline_history = term.lock().grid().history_size();
        // Natural scrollback after 200 lines into a 30-row viewport is
        // ~170. We don't pin an exact number because alacritty may
        // round by ±1 depending on cursor position.
        assert!(
            (168..=172).contains(&baseline_history),
            "expected ~170 baseline scrollback rows, got {baseline_history}"
        );

        // Step 2: run a sequence of window resizes. Each one mirrors
        // what happens when the user drags the window: the local Term
        // is resized first (via `apply_remote_resize` or the equivalent
        // main-thread `resize_all_panes` path), then the streaming TUI
        // re-renders from the top of its buffer.
        const RESIZE_COUNT: usize = 5;
        let sizes = [(80usize, 28usize), (80, 26), (80, 24), (80, 26), (80, 28)];
        assert_eq!(sizes.len(), RESIZE_COUNT);

        for (cols, rows) in sizes.iter().copied() {
            apply_remote_resize(&term, cols, rows);

            // Some TUIs repaint on SIGWINCH by issuing ESC[2J (clear
            // entire screen) + ESC[H (home) and re-emitting from the
            // top. On alacritty this is the worst-case pattern: ESC[2J
            // scrolls the current viewport content up into history
            // *before* clearing, so every resize effectively duplicates
            // whatever was on-screen into scrollback.
            //
            // This mirrors what vim, less, ncurses apps, and some
            // React-terminal renderers do.
            let mut repaint = String::new();
            repaint.push_str("\x1b[2J\x1b[H");
            for i in (CHAT_LINES - rows + 1)..=CHAT_LINES {
                repaint.push_str(&format!("chat line {i:03} content"));
                if i < CHAT_LINES {
                    repaint.push_str("\r\n");
                }
            }
            {
                let mut guard = term.lock();
                processor.advance_with_interceptor(
                    &mut *guard,
                    &mut interceptor,
                    repaint.as_bytes(),
                );
            }
        }

        let final_history = term.lock().grid().history_size();

        // Invariant: repaints that don't produce genuinely new content
        // must not grow scrollback. With the tn-ebdu fix the viewport
        // is blanked before each `Term::resize`, so `shrink_lines`
        // scrolls empty rows into history and the TUI's subsequent
        // `ESC[2J` scrolls zero non-empty rows. Net growth should be
        // 0; we permit 1 row of slack per resize to absorb
        // cursor-at-end rounding in alacritty's reflow path, which
        // can prepend a single extra history row when the cursor
        // lands on the final column before a shrink. Any bound
        // beyond that would let a partial regression (e.g. only
        // suppressing the ESC[2J path but not `shrink_lines`) slip
        // through undetected.
        let max_allowed = baseline_history + RESIZE_COUNT;
        assert!(
            final_history <= max_allowed,
            "tn-ebdu regression: {RESIZE_COUNT} resize+repaint cycles grew \
             scrollback from {baseline_history} to {final_history} rows \
             (max_allowed={max_allowed}). Every repaint is duplicating the \
             full chat into history."
        );

        // Sanity: the visible viewport still shows the bottom of the
        // chat (the tail), not garbage.
        let guard = term.lock();
        use alacritty_terminal::index::{Column, Line};
        let last_row_idx = Line(guard.screen_lines() as i32 - 1);
        let last_row: String = (0..guard.columns())
            .map(|c| guard.grid()[last_row_idx][Column(c)].c)
            .collect();
        assert!(
            last_row.starts_with("chat line 200 content"),
            "last row should hold the last chat line, got: {last_row:?}"
        );
    }

    /// tn-ebdu follow-up: the scrollback-protection fix must NOT wipe
    /// alt-screen contents. Alt-screen curses apps (htop, vim, less,
    /// etc.) rely on the fact that resize + SIGWINCH delivers a clean
    /// new grid dimension and the app itself handles `redrawwin`. If
    /// we blanked the alt-screen viewport on every resize, the user
    /// would see a flash of empty cells until the next app tick — and
    /// worse, the alt-screen has no scrollback, so the vanilla
    /// `Term::resize` path is already safe.
    ///
    /// This test locks down the "alt-screen = keep content" branch of
    /// `resize_remote_term_without_scrollback_pollution`.
    #[test]
    fn alt_screen_resize_preserves_visible_content() {
        let term_config = TermConfig {
            scrolling_history: 1000,
            ..Default::default()
        };
        let initial_size = TermSize {
            columns: 80,
            screen_lines: 20,
        };
        let listener = PaneListener::new();
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &initial_size,
            listener,
        )));

        let (mut interceptor, _event_rx) = TherminalInterceptor::new(InterceptorConfig::default());
        let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();

        // Enter alt-screen and paint 10 rows of distinctive content.
        let mut setup = String::new();
        setup.push_str("\x1b[?1049h"); // alt-screen on
        setup.push_str("\x1b[H");
        for i in 1..=10 {
            if i > 1 {
                setup.push_str("\r\n");
            }
            setup.push_str(&format!("alt line {i:02}"));
        }
        {
            let mut guard = term.lock();
            processor.advance_with_interceptor(&mut *guard, &mut interceptor, setup.as_bytes());
        }

        // Sanity: alt-screen active, row 0 has "alt line 01".
        {
            let guard = term.lock();
            use alacritty_terminal::index::{Column, Line};
            use alacritty_terminal::term::TermMode;
            assert!(
                guard.mode().contains(TermMode::ALT_SCREEN),
                "alt-screen should be active"
            );
            let row0: String = (0..guard.columns())
                .map(|c| guard.grid()[Line(0)][Column(c)].c)
                .collect();
            assert!(
                row0.starts_with("alt line 01"),
                "alt-screen row 0 should hold 'alt line 01', got: {row0:?}"
            );
        }

        // Resize. The fix branch for alt-screen must NOT blank the
        // visible rows — curses apps depend on their grid state
        // surviving a resize until the next redrawwin tick.
        apply_remote_resize(&term, 80, 18);

        // Row 0 should STILL contain "alt line 01" after the resize.
        // (alacritty's alt-screen resize does not scroll on shrink —
        // it preserves the top of the grid when shrinking from bottom.)
        let guard = term.lock();
        use alacritty_terminal::index::{Column, Line};
        let row0: String = (0..guard.columns())
            .map(|c| guard.grid()[Line(0)][Column(c)].c)
            .collect();
        assert!(
            row0.starts_with("alt line 01"),
            "alt-screen row 0 must be preserved across resize (tn-ebdu \
             fix must not blank alt-screen viewports), got: {row0:?}"
        );
    }
}
