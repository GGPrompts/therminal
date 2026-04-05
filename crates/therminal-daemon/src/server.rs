//! Daemon IPC server.
//!
//! Listens on a Unix domain socket for IPC messages (request/response multiplexing,
//! event subscriptions). Supports both the new `IpcMessage` envelope protocol and
//! the legacy `DaemonRequest`/`DaemonResponse` framing for backward compatibility.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use therminal_protocol::daemon::{
    decode_ipc, decode_request, encode_response, DaemonEvent, DaemonRequest, DaemonResponse,
    EventKind, IpcMessage, IpcRequest, IpcResponse, MAX_FRAME_SIZE,
};

use crate::lifecycle::Lifecycle;

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
        // Clean stale socket if it exists but no daemon is listening
        if socket_path.exists() {
            debug!(path = %socket_path.display(), "removing stale socket");
            std::fs::remove_file(&socket_path).with_context(|| {
                format!("failed to remove stale socket: {}", socket_path.display())
            })?;
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

        info!(path = %socket_path.display(), "daemon socket bound");
        Ok(Self {
            listener,
            socket_path,
            lifecycle,
            build_hash,
            version,
            event_tx,
            next_conn_id: AtomicU64::new(1),
        })
    }

    /// Get a sender handle for broadcasting events to subscribed clients.
    pub fn event_sender(&self) -> broadcast::Sender<DaemonEvent> {
        self.event_tx.clone()
    }

    /// Run the server accept loop until the lifecycle transitions to Stopped.
    pub async fn run(self) -> Result<()> {
        let shutdown = self.lifecycle.shutdown_notify();
        let lifecycle = Arc::clone(&self.lifecycle);
        let build_hash = self.build_hash.clone();
        let version = self.version.clone();
        let event_tx = self.event_tx.clone();

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
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, lc, bh, ver, etx, conn_id).await {
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

/// Read a single length-prefixed frame from the stream.
///
/// Returns `Ok(None)` on clean EOF, `Ok(Some(bytes))` with the payload,
/// or `Err` on protocol violations.
async fn read_frame(stream: &mut UnixStream) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let msg_len = u32::from_be_bytes(len_buf) as usize;
    if msg_len > MAX_FRAME_SIZE {
        anyhow::bail!("frame too large: {msg_len} bytes (max {MAX_FRAME_SIZE})");
    }

    let mut payload = vec![0u8; msg_len];
    stream
        .read_exact(&mut payload)
        .await
        .context("failed to read frame payload")?;

    Ok(Some(payload))
}

/// Write a length-prefixed frame to the stream.
async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> Result<()> {
    let len = payload.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Handle a single client connection.
///
/// The connection can operate in two modes:
/// 1. **Legacy mode**: A single DaemonRequest frame followed by a DaemonResponse.
/// 2. **IPC mode**: Multiple IpcMessage frames with request/response multiplexing
///    and optional event streaming.
///
/// Mode is detected by attempting to decode the first frame as `IpcMessage`.
/// If that fails, falls back to legacy `DaemonRequest` decoding.
async fn handle_connection(
    mut stream: UnixStream,
    lifecycle: Arc<Lifecycle>,
    build_hash: String,
    version: String,
    event_tx: broadcast::Sender<DaemonEvent>,
    conn_id: u64,
) -> Result<()> {
    // Read first frame
    let first_frame = match read_frame(&mut stream).await? {
        Some(f) => f,
        None => return Ok(()), // Clean disconnect
    };

    // Try IPC protocol first
    if let Ok(ipc_msg) = decode_ipc(&first_frame) {
        debug!(conn_id, "IPC protocol detected");
        return handle_ipc_connection(
            &mut stream,
            lifecycle,
            build_hash,
            version,
            event_tx,
            conn_id,
            ipc_msg,
        )
        .await;
    }

    // Fall back to legacy protocol
    debug!(conn_id, "legacy protocol detected");
    let request = decode_request(&first_frame).context("failed to decode legacy request")?;
    let response = dispatch_legacy(&request, &lifecycle, &build_hash, &version).await;
    let response_bytes = encode_response(&response).context("failed to encode response")?;
    stream.write_all(&response_bytes).await?;
    stream.flush().await.ok();
    Ok(())
}

/// Dispatch a legacy DaemonRequest and return the DaemonResponse.
async fn dispatch_legacy(
    request: &DaemonRequest,
    lifecycle: &Arc<Lifecycle>,
    build_hash: &str,
    version: &str,
) -> DaemonResponse {
    match request {
        DaemonRequest::Ping => DaemonResponse::Pong {
            build_hash: build_hash.to_string(),
            uptime_secs: lifecycle.uptime_secs(),
            sessions: lifecycle.session_count(),
            version: version.to_string(),
        },
        DaemonRequest::GracefulShutdown => {
            let lc = Arc::clone(lifecycle);
            tokio::spawn(async move {
                if let Err(e) = lc.initiate_shutdown().await {
                    error!(error = %e, "shutdown failed");
                }
            });
            DaemonResponse::ShutdownAck
        }
    }
}

/// Handle a full IPC connection (multiple frames, event streaming).
async fn handle_ipc_connection(
    stream: &mut UnixStream,
    lifecycle: Arc<Lifecycle>,
    build_hash: String,
    version: String,
    event_tx: broadcast::Sender<DaemonEvent>,
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
                                &event_tx, &mut subscribed_kinds, &mut event_rx, conn_id,
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
                subscribed_kinds,
                event_rx,
                conn_id,
            );
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
fn dispatch_ipc(
    request: &IpcRequest,
    lifecycle: &Arc<Lifecycle>,
    build_hash: &str,
    version: &str,
    event_tx: &broadcast::Sender<DaemonEvent>,
    subscribed_kinds: &mut HashSet<EventKind>,
    event_rx: &mut Option<broadcast::Receiver<DaemonEvent>>,
    conn_id: u64,
) -> IpcResponse {
    match request {
        IpcRequest::Ping => IpcResponse::Pong {
            build_hash: build_hash.to_string(),
            uptime_secs: lifecycle.uptime_secs(),
            sessions: lifecycle.session_count(),
            version: version.to_string(),
        },
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
        // Session management stubs — will be implemented when the session manager lands
        IpcRequest::ListSessions => IpcResponse::Sessions {
            session_ids: vec![],
        },
        IpcRequest::GetSession { session_id } => IpcResponse::Error {
            message: format!("session not found: {session_id}"),
        },
        IpcRequest::CreateSession { name: _ } => {
            // TODO: wire to session manager
            let session_id = format!(
                "sess-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
            );
            IpcResponse::SessionCreated { session_id }
        }
        IpcRequest::DestroySession { session_id } => IpcResponse::Error {
            message: format!("session not found: {session_id}"),
        },
    }
}

// ── Backward compatibility alias ──────────────────────────────────────────

/// Backward-compatible alias for the IPC server.
pub type DaemonServer = IpcServer;
