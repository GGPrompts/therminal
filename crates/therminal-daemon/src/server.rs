//! Daemon IPC server.
//!
//! Listens on a Unix domain socket for IPC messages (request/response multiplexing,
//! event subscriptions) using the `IpcMessage` envelope protocol.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;

use crate::framing::{read_frame, write_frame};
use tracing::{debug, error, info, warn};

use therminal_protocol::daemon::{
    DaemonEvent, EventKind, IpcMessage, IpcRequest, IpcResponse, MAX_FRAME_SIZE, decode_ipc,
};

use crate::control;
use crate::lifecycle::Lifecycle;
use crate::session::SessionManager;

/// Capacity of the event broadcast channel.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// The daemon IPC server.
///
/// Accepts connections on the control socket and dispatches IPC messages.
/// Each connection can send requests and optionally subscribe to events.
pub struct IpcServer {
    listener: UnixListener,
    socket_path: PathBuf,
    lifecycle: Arc<Lifecycle>,
    build_hash: String,
    version: String,
    /// Broadcast channel for pushing events to subscribed clients.
    event_tx: broadcast::Sender<DaemonEvent>,
    /// Monotonically increasing connection ID for logging.
    next_conn_id: AtomicU64,
    /// Session manager shared across all connection handlers.
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
}

impl IpcServer {
    /// Bind the control socket at the given path.
    ///
    /// This is the "socket-as-lock" pattern: successful bind = we own the daemon role.
    /// Cleans up stale sockets before binding.
    pub async fn bind(
        socket_path: PathBuf,
        lifecycle: Arc<Lifecycle>,
        build_hash: String,
        version: String,
    ) -> Result<Self> {
        // Clean stale socket unconditionally — avoids TOCTOU race between exists() and remove().
        match std::fs::remove_file(&socket_path) {
            Ok(()) => debug!(path = %socket_path.display(), "removed stale socket"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to remove stale socket {}: {e}",
                    socket_path.display()
                ));
            }
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("failed to bind daemon socket: {}", socket_path.display()))?;

        // Set socket permissions on Unix (owner-only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&socket_path, perms).ok();
        }

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);

        let session_mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new(
            event_tx.clone(),
        )));

        info!(path = %socket_path.display(), "daemon socket bound");
        Ok(Self {
            listener,
            socket_path,
            lifecycle,
            build_hash,
            version,
            event_tx,
            next_conn_id: AtomicU64::new(1),
            session_mgr,
        })
    }

    /// Get a sender handle for broadcasting events to subscribed clients.
    pub fn event_sender(&self) -> broadcast::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    /// Get a handle to the session manager.
    pub fn session_manager(&self) -> Arc<tokio::sync::Mutex<SessionManager>> {
        Arc::clone(&self.session_mgr)
    }

    /// Run the server accept loop until the lifecycle transitions to Stopped.
    pub async fn run(self) -> Result<()> {
        let shutdown = self.lifecycle.shutdown_notify();
        let lifecycle = Arc::clone(&self.lifecycle);
        let build_hash = self.build_hash.clone();
        let version = self.version.clone();
        let event_tx = self.event_tx.clone();
        let session_mgr = Arc::clone(&self.session_mgr);

        loop {
            tokio::select! {
                accept_result = self.listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
                            let lc = Arc::clone(&lifecycle);
                            let bh = build_hash.clone();
                            let ver = version.clone();
                            let etx = event_tx.clone();
                            let sm = Arc::clone(&session_mgr);
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, lc, bh, ver, etx, sm, conn_id).await {
                                    debug!(conn_id, error = %e, "connection handler error");
                                }
                            });
                        }
                        Err(e) => {
                            error!(error = %e, "accept failed");
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                }
                _ = shutdown.notified() => {
                    info!("shutdown signal received, stopping accept loop");
                    break;
                }
            }
        }

        // Graceful shutdown: destroy all sessions
        {
            let mut mgr = self.session_mgr.lock().await;
            mgr.shutdown();
        }

        // Clean up socket
        self.cleanup();
        Ok(())
    }

    /// Get the socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Clean up the socket file.
    fn cleanup(&self) {
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                warn!(error = %e, path = %self.socket_path.display(), "failed to remove socket on cleanup");
            } else {
                debug!(path = %self.socket_path.display(), "socket cleaned up");
            }
        }
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        self.cleanup();
    }
}

// ── Connection handler ────────────────────────────────────────────────────

/// Handle a single client connection.
///
/// The connection can operate in two modes:
/// 1. **Control mode**: Text-based protocol for programmatic control (tmux -CC style).
///    Detected when the first bytes are `mode: control\n`.
/// 2. **IPC mode**: Multiple `IpcMessage` frames with request/response multiplexing
///    and optional event streaming.
///
/// Mode is detected by peeking at the first bytes on the connection.
async fn handle_connection(
    mut stream: UnixStream,
    lifecycle: Arc<Lifecycle>,
    build_hash: String,
    version: String,
    event_tx: broadcast::Sender<DaemonEvent>,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    conn_id: u64,
) -> Result<()> {
    // Read the first 4 bytes. For binary protocols this is the length prefix.
    // For control mode, the handshake starts with "mode" (ASCII).
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(e) => return Err(e.into()),
    }

    // Detect control mode: "mode" in ASCII = [0x6d, 0x6f, 0x64, 0x65]
    if &header == b"mode" {
        // Read the rest of the handshake line (": control\n")
        let mut byte = [0u8; 1];
        while stream.read_exact(&mut byte).await.is_ok() {
            if byte[0] == b'\n' {
                break;
            }
        }
        debug!(conn_id, "control mode detected");
        control::handle_control_connection(
            stream,
            lifecycle,
            event_tx,
            session_mgr,
            build_hash,
            version,
        )
        .await;
        return Ok(());
    }

    // Binary protocol: interpret header as 4-byte BE length prefix
    let msg_len = u32::from_be_bytes(header) as usize;
    if msg_len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {msg_len} bytes (max {MAX_FRAME_SIZE})");
    }

    let mut payload = vec![0u8; msg_len];
    stream
        .read_exact(&mut payload)
        .await
        .context("failed to read frame payload")?;

    let first_frame = payload;

    // Decode as IPC message
    match decode_ipc(&first_frame) {
        Ok(ipc_msg) => {
            debug!(conn_id, "IPC protocol detected");
            handle_ipc_connection(
                &mut stream,
                lifecycle,
                build_hash,
                version,
                event_tx,
                session_mgr,
                conn_id,
                ipc_msg,
            )
            .await
        }
        Err(e) => {
            warn!(conn_id, error = %e, "unrecognized frame, closing connection");
            Ok(())
        }
    }
}

/// Handle a full IPC connection (multiple frames, event streaming).
#[allow(clippy::too_many_arguments)]
async fn handle_ipc_connection(
    stream: &mut UnixStream,
    lifecycle: Arc<Lifecycle>,
    build_hash: String,
    version: String,
    event_tx: broadcast::Sender<DaemonEvent>,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    conn_id: u64,
    first_msg: IpcMessage,
) -> Result<()> {
    // Process the first message
    let mut subscribed_kinds: HashSet<EventKind> = HashSet::new();
    let mut event_rx: Option<broadcast::Receiver<DaemonEvent>> = None;

    process_ipc_message(
        stream,
        &first_msg,
        &lifecycle,
        &build_hash,
        &version,
        &event_tx,
        &session_mgr,
        &mut subscribed_kinds,
        &mut event_rx,
        conn_id,
    )
    .await?;

    // Continue reading frames until disconnect
    loop {
        if let Some(ref mut rx) = event_rx {
            // When subscribed, we need to multiplex between incoming requests
            // and outgoing events.
            tokio::select! {
                frame = read_frame(stream) => {
                    match frame? {
                        Some(data) => {
                            let msg = decode_ipc(&data).context("failed to decode IPC message")?;
                            process_ipc_message(
                                stream, &msg, &lifecycle, &build_hash, &version,
                                &event_tx, &session_mgr, &mut subscribed_kinds,
                                &mut event_rx, conn_id,
                            ).await?;
                        }
                        None => break, // Clean disconnect
                    }
                }
                event = rx.recv() => {
                    match event {
                        Ok(evt) => {
                            if subscribed_kinds.is_empty() || subscribed_kinds.contains(&evt.kind()) {
                                let msg = IpcMessage::Event { payload: evt };
                                let payload = rmp_serde::to_vec(&msg)?;
                                write_frame(stream, &payload).await?;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(conn_id, lagged = n, "event subscriber lagged, some events dropped");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        } else {
            // Not subscribed — just read requests
            match read_frame(stream).await? {
                Some(data) => {
                    let msg = decode_ipc(&data).context("failed to decode IPC message")?;
                    process_ipc_message(
                        stream,
                        &msg,
                        &lifecycle,
                        &build_hash,
                        &version,
                        &event_tx,
                        &session_mgr,
                        &mut subscribed_kinds,
                        &mut event_rx,
                        conn_id,
                    )
                    .await?;
                }
                None => break,
            }
        }
    }

    debug!(conn_id, "connection closed");
    Ok(())
}

/// Process a single IPC message and send the response (if any).
#[allow(clippy::too_many_arguments)]
async fn process_ipc_message(
    stream: &mut UnixStream,
    msg: &IpcMessage,
    lifecycle: &Arc<Lifecycle>,
    build_hash: &str,
    version: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
    session_mgr: &Arc<tokio::sync::Mutex<SessionManager>>,
    subscribed_kinds: &mut HashSet<EventKind>,
    event_rx: &mut Option<broadcast::Receiver<DaemonEvent>>,
    conn_id: u64,
) -> Result<()> {
    match msg {
        IpcMessage::Request {
            request_id,
            payload,
        } => {
            let response = dispatch_ipc(
                payload,
                lifecycle,
                build_hash,
                version,
                event_tx,
                session_mgr,
                subscribed_kinds,
                event_rx,
                conn_id,
            )
            .await;
            let resp_msg = IpcMessage::Response {
                request_id: *request_id,
                payload: response,
            };
            let payload_bytes = rmp_serde::to_vec(&resp_msg)?;
            write_frame(stream, &payload_bytes).await?;
        }
        IpcMessage::Response { .. } => {
            warn!(conn_id, "received Response from client, ignoring");
        }
        IpcMessage::Event { .. } => {
            warn!(conn_id, "received Event from client, ignoring");
        }
    }
    Ok(())
}

/// Dispatch an IPC request and return the response.
#[allow(clippy::too_many_arguments)]
async fn dispatch_ipc(
    request: &IpcRequest,
    lifecycle: &Arc<Lifecycle>,
    build_hash: &str,
    version: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
    session_mgr: &Arc<tokio::sync::Mutex<SessionManager>>,
    subscribed_kinds: &mut HashSet<EventKind>,
    event_rx: &mut Option<broadcast::Receiver<DaemonEvent>>,
    conn_id: u64,
) -> IpcResponse {
    match request {
        IpcRequest::Ping => {
            let mgr = session_mgr.lock().await;
            IpcResponse::Pong {
                protocol_version: therminal_protocol::PROTOCOL_VERSION,
                build_hash: build_hash.to_string(),
                uptime_secs: lifecycle.uptime_secs(),
                sessions: mgr.session_count(),
                version: version.to_string(),
            }
        }
        IpcRequest::GracefulShutdown => {
            let lc = Arc::clone(lifecycle);
            tokio::spawn(async move {
                if let Err(e) = lc.initiate_shutdown().await {
                    error!(error = %e, "shutdown failed");
                }
            });
            IpcResponse::ShutdownAck
        }
        IpcRequest::Subscribe { filter } => {
            *subscribed_kinds = filter.iter().copied().collect();
            *event_rx = Some(event_tx.subscribe());
            debug!(conn_id, kinds = ?subscribed_kinds, "client subscribed to events");
            IpcResponse::Subscribed {
                filter: filter.clone(),
            }
        }
        IpcRequest::Unsubscribe => {
            subscribed_kinds.clear();
            *event_rx = None;
            debug!(conn_id, "client unsubscribed from events");
            IpcResponse::Unsubscribed
        }
        IpcRequest::GetState => IpcResponse::State {
            state: lifecycle.state(),
        },
        IpcRequest::ListSessions => {
            let mgr = session_mgr.lock().await;
            IpcResponse::Sessions {
                session_ids: mgr.list_sessions(),
            }
        }
        IpcRequest::GetSession { session_id } => {
            let mgr = session_mgr.lock().await;
            match mgr.get_session_info(*session_id) {
                Some((id, name, created_at_secs)) => IpcResponse::SessionInfo {
                    session_id: id,
                    name,
                    created_at_secs,
                },
                None => IpcResponse::Error {
                    message: format!("session not found: {session_id}"),
                },
            }
        }
        IpcRequest::CreateSession { name } => {
            let mut mgr = session_mgr.lock().await;
            match mgr.create_session(name.clone()) {
                Ok(session_id) => {
                    // Update lifecycle session count
                    lifecycle.set_session_count(mgr.session_count());
                    IpcResponse::SessionCreated { session_id }
                }
                Err(e) => IpcResponse::Error {
                    message: format!("failed to create session: {e}"),
                },
            }
        }
        IpcRequest::DestroySession { session_id } => {
            let mut mgr = session_mgr.lock().await;
            if mgr.destroy_session(*session_id) {
                lifecycle.set_session_count(mgr.session_count());
                IpcResponse::SessionDestroyed {
                    session_id: *session_id,
                }
            } else {
                IpcResponse::Error {
                    message: format!("session not found: {session_id}"),
                }
            }
        }
        IpcRequest::SendKeys { pane_id, keys } => {
            let mut mgr = session_mgr.lock().await;
            match mgr.send_keys_to_pane(*pane_id, keys) {
                Ok(()) => IpcResponse::KeysSent { pane_id: *pane_id },
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::CapturePane { pane_id } => {
            let mgr = session_mgr.lock().await;
            match mgr.capture_pane(*pane_id) {
                Ok(snap) => {
                    let lines: Vec<String> = snap
                        .grid
                        .iter()
                        .map(|row| row.iter().map(|(ch, _)| ch).collect())
                        .collect();
                    IpcResponse::PaneCaptured {
                        pane_id: snap.pane_id,
                        lines,
                        cursor_col: snap.cursor_col,
                        cursor_line: snap.cursor_line,
                        cols: snap.cols,
                        rows: snap.rows,
                    }
                }
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::SplitPane {
            pane_id,
            horizontal,
        } => {
            let mut mgr = session_mgr.lock().await;
            match mgr.split_pane(*pane_id, *horizontal) {
                Ok(new_pane_id) => IpcResponse::PaneSplit { new_pane_id },
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::KillPane { pane_id } => {
            let mut mgr = session_mgr.lock().await;
            match mgr.kill_pane(*pane_id) {
                Ok(()) => {
                    lifecycle.set_session_count(mgr.session_count());
                    IpcResponse::PaneKilled { pane_id: *pane_id }
                }
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::SelectPane { pane_id } => {
            let mgr = session_mgr.lock().await;
            match mgr.select_pane(*pane_id) {
                Ok(()) => IpcResponse::PaneSelected { pane_id: *pane_id },
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::SetWorkspaceState {
            session_id,
            workspaces,
            active_workspace,
        } => {
            let mut mgr = session_mgr.lock().await;
            match mgr.set_workspace_state(*session_id, workspaces.clone(), *active_workspace) {
                Ok(()) => IpcResponse::WorkspaceStateSet {
                    session_id: *session_id,
                },
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::GetWorkspaces { session_id } => {
            let mgr = session_mgr.lock().await;
            match mgr.get_workspace_state(*session_id) {
                Ok((workspaces, active_workspace)) => IpcResponse::Workspaces {
                    session_id: *session_id,
                    workspaces,
                    active_workspace,
                },
                Err(e) => IpcResponse::Error { message: e },
            }
        }
        IpcRequest::RequestHandoffFds => {
            #[cfg(unix)]
            {
                use crate::fd_passing;

                let mgr = session_mgr.lock().await;
                let (payload, fds) = mgr.collect_handoff_fds();

                if fds.is_empty() {
                    // No sessions to hand off -- just shut down.
                    let lc = Arc::clone(lifecycle);
                    tokio::spawn(async move {
                        if let Err(e) = lc.initiate_shutdown().await {
                            error!(error = %e, "shutdown failed");
                        }
                    });
                    return IpcResponse::HandoffReady {
                        handoff_socket: String::new(),
                        pane_count: 0,
                    };
                }

                // Create a temporary socket for FD transfer.
                let runtime_dir = therminal_runtime::paths::runtime_dir();
                let handoff_path = runtime_dir.join("handoff.sock");

                // Remove stale socket.
                let _ = std::fs::remove_file(&handoff_path);

                let handoff_path_str = handoff_path.display().to_string();
                let pane_count = fds.len();

                // Spawn a task that listens on the handoff socket, sends FDs,
                // then triggers shutdown.
                let lc = Arc::clone(lifecycle);
                tokio::spawn(async move {
                    match fd_passing::serve_handoff_fds(&handoff_path, &payload, &fds).await {
                        Ok(()) => {
                            info!("handoff FDs sent successfully");
                        }
                        Err(e) => {
                            warn!(error = %e, "failed to send handoff FDs");
                        }
                    }
                    // Clean up handoff socket.
                    let _ = std::fs::remove_file(&handoff_path);
                    // Initiate shutdown after FD transfer.
                    if let Err(e) = lc.initiate_shutdown().await {
                        error!(error = %e, "shutdown after handoff failed");
                    }
                });

                IpcResponse::HandoffReady {
                    handoff_socket: handoff_path_str,
                    pane_count,
                }
            }
            #[cfg(not(unix))]
            {
                IpcResponse::Error {
                    message: "FD-passing handoff is only supported on Unix".to_string(),
                }
            }
        }
    }
}

// ── Backward compatibility alias ──────────────────────────────────────────

/// Backward-compatible alias for the IPC server.
pub type DaemonServer = IpcServer;
