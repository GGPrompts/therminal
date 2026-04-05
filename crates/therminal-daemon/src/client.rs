//! Daemon IPC client — connects to the daemon control socket for
//! request/response communication and event subscriptions.
//!
//! Supports both the legacy single-shot protocol (for backward compatibility)
//! and the full IPC envelope protocol with multiplexed request/response and
//! server-pushed events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};

use therminal_protocol::daemon::{
    decode_ipc, encode_ipc, DaemonEvent, EventKind, IpcMessage, IpcRequest, IpcResponse,
    MAX_FRAME_SIZE,
};

use crate::framing::read_frame;

/// Default timeout for daemon communication.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

// ── Single-shot IPC helpers ───────────────────────────────────────────────

/// Send a single IPC request to the daemon and return the response.
///
/// Opens a fresh connection, sends the request, reads one response frame,
/// then closes the connection. Suitable for one-off operations such as
/// health checks and shutdown requests.
///
/// Returns `Err` if the socket doesn't exist, the daemon is unreachable,
/// or the response times out (2s default).
pub async fn send_request(socket_path: &Path, request: IpcRequest) -> Result<IpcResponse> {
    send_request_with_timeout(socket_path, request, DEFAULT_TIMEOUT).await
}

/// Send a single IPC request with a custom timeout.
pub async fn send_request_with_timeout(
    socket_path: &Path,
    request: IpcRequest,
    timeout: Duration,
) -> Result<IpcResponse> {
    let result = tokio::time::timeout(timeout, async {
        let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
            format!(
                "failed to connect to daemon socket: {}",
                socket_path.display()
            )
        })?;

        // Send request wrapped in an IpcMessage envelope (request_id = 1)
        let msg = IpcMessage::Request {
            request_id: 1,
            payload: request,
        };
        let req_bytes = encode_ipc(&msg).context("failed to encode request")?;
        stream
            .write_all(&req_bytes)
            .await
            .context("failed to send request")?;
        stream.flush().await.ok();

        // Read one response frame
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .context("failed to read response length")?;
        let msg_len = u32::from_be_bytes(len_buf) as usize;

        if msg_len > MAX_FRAME_SIZE {
            anyhow::bail!("response too large: {msg_len} bytes");
        }

        let mut payload = vec![0u8; msg_len];
        stream
            .read_exact(&mut payload)
            .await
            .context("failed to read response payload")?;

        match decode_ipc(&payload).context("failed to decode response")? {
            IpcMessage::Response { payload, .. } => {
                debug!(?payload, "received daemon response");
                Ok(payload)
            }
            other => anyhow::bail!("unexpected message type from daemon: {other:?}"),
        }
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!("daemon request timed out after {}ms", timeout.as_millis()),
    }
}

/// Ping the daemon and return the Pong response, or an error if unreachable.
pub async fn ping(socket_path: &Path) -> Result<IpcResponse> {
    send_request(socket_path, IpcRequest::Ping).await
}

/// Request graceful shutdown of the daemon.
pub async fn request_shutdown(socket_path: &Path) -> Result<IpcResponse> {
    send_request(socket_path, IpcRequest::GracefulShutdown).await
}

// ── Full IPC client ───────────────────────────────────────────────────────

/// Internal message type for the writer task.
enum WriterCmd {
    Send {
        request_id: u64,
        payload: IpcRequest,
        reply_tx: oneshot::Sender<Result<IpcResponse>>,
    },
    Close,
}

/// A persistent IPC client connection to the daemon.
///
/// Supports multiplexed request/response and event subscriptions over
/// a single Unix socket connection.
///
/// # Example
///
/// ```rust,no_run
/// # async fn example() -> anyhow::Result<()> {
/// use therminal_daemon::client::DaemonClient;
/// use therminal_protocol::daemon::IpcRequest;
///
/// let client = DaemonClient::connect("/tmp/therminal/daemon.sock").await?;
/// let resp = client.send_request(IpcRequest::Ping).await?;
/// println!("{:?}", resp);
/// client.close().await;
/// # Ok(())
/// # }
/// ```
pub struct DaemonClient {
    socket_path: PathBuf,
    cmd_tx: mpsc::Sender<WriterCmd>,
    next_request_id: AtomicU64,
    /// Channel for receiving server-pushed events.
    event_rx: Mutex<mpsc::Receiver<DaemonEvent>>,
    /// Timeout for individual requests.
    timeout: Duration,
}

impl DaemonClient {
    /// Connect to the daemon socket at `socket_path`.
    pub async fn connect(socket_path: impl AsRef<Path>) -> Result<Self> {
        Self::connect_with_timeout(socket_path, DEFAULT_TIMEOUT).await
    }

    /// Connect with a custom request timeout.
    pub async fn connect_with_timeout(
        socket_path: impl AsRef<Path>,
        timeout: Duration,
    ) -> Result<Self> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let stream = UnixStream::connect(&socket_path).await.with_context(|| {
            format!(
                "failed to connect to daemon socket: {}",
                socket_path.display()
            )
        })?;

        let (cmd_tx, cmd_rx) = mpsc::channel::<WriterCmd>(64);
        let (event_tx, event_rx) = mpsc::channel::<DaemonEvent>(256);

        // Spawn the connection I/O task
        tokio::spawn(connection_task(stream, cmd_rx, event_tx));

        Ok(Self {
            socket_path,
            cmd_tx,
            next_request_id: AtomicU64::new(1),
            event_rx: Mutex::new(event_rx),
            timeout,
        })
    }

    /// Send an IPC request and wait for the response.
    pub async fn send_request(&self, request: IpcRequest) -> Result<IpcResponse> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();

        self.cmd_tx
            .send(WriterCmd::Send {
                request_id,
                payload: request,
                reply_tx,
            })
            .await
            .map_err(|_| anyhow::anyhow!("connection closed"))?;

        match tokio::time::timeout(self.timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => anyhow::bail!("connection closed while waiting for response"),
            Err(_) => anyhow::bail!("request timed out after {}ms", self.timeout.as_millis()),
        }
    }

    /// Convenience: ping the daemon.
    pub async fn ping(&self) -> Result<IpcResponse> {
        self.send_request(IpcRequest::Ping).await
    }

    /// Convenience: request graceful shutdown.
    pub async fn shutdown(&self) -> Result<IpcResponse> {
        self.send_request(IpcRequest::GracefulShutdown).await
    }

    /// Convenience: get daemon state.
    pub async fn get_state(&self) -> Result<IpcResponse> {
        self.send_request(IpcRequest::GetState).await
    }

    /// Subscribe to daemon events with an optional filter.
    ///
    /// After calling this, events matching the filter will be available
    /// via `recv_event()`.
    pub async fn subscribe_events(&self, filter: Vec<EventKind>) -> Result<IpcResponse> {
        self.send_request(IpcRequest::Subscribe { filter }).await
    }

    /// Unsubscribe from all events.
    pub async fn unsubscribe_events(&self) -> Result<IpcResponse> {
        self.send_request(IpcRequest::Unsubscribe).await
    }

    /// Receive the next server-pushed event.
    ///
    /// Returns `None` if the connection is closed.
    pub async fn recv_event(&self) -> Option<DaemonEvent> {
        self.event_rx.lock().await.recv().await
    }

    /// Try to receive an event without blocking.
    pub fn try_recv_event(&self) -> Option<DaemonEvent> {
        // Use try_lock to avoid blocking; if locked, no event available
        match self.event_rx.try_lock() {
            Ok(mut rx) => rx.try_recv().ok(),
            Err(_) => None,
        }
    }

    /// Get the socket path this client is connected to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Close the connection gracefully.
    pub async fn close(&self) {
        let _ = self.cmd_tx.send(WriterCmd::Close).await;
    }
}

/// The I/O task that owns the socket and multiplexes reads/writes.
async fn connection_task(
    stream: UnixStream,
    mut cmd_rx: mpsc::Receiver<WriterCmd>,
    event_tx: mpsc::Sender<DaemonEvent>,
) {
    let (read_half, write_half) = stream.into_split();
    let read_half = Arc::new(Mutex::new(read_half));
    let write_half = Arc::new(Mutex::new(write_half));

    // Pending response waiters keyed by request_id
    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<IpcResponse>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Spawn reader task
    let reader_pending = Arc::clone(&pending);
    let reader_event_tx = event_tx;
    let reader_read = Arc::clone(&read_half);
    let reader_handle = tokio::spawn(async move {
        loop {
            let frame = {
                let mut r = reader_read.lock().await;
                read_frame(&mut *r).await
            };
            match frame {
                Ok(Some(data)) => match decode_ipc(&data) {
                    Ok(IpcMessage::Response {
                        request_id,
                        payload,
                    }) => {
                        let mut p = reader_pending.lock().await;
                        if let Some(tx) = p.remove(&request_id) {
                            let _ = tx.send(Ok(payload));
                        } else {
                            warn!(request_id, "received response for unknown request");
                        }
                    }
                    Ok(IpcMessage::Event { payload }) => {
                        if reader_event_tx.send(payload).await.is_err() {
                            break; // Event receiver dropped
                        }
                    }
                    Ok(IpcMessage::Request { .. }) => {
                        warn!("received Request from server, ignoring");
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to decode IPC message from server");
                    }
                },
                Ok(None) => break, // Clean EOF
                Err(e) => {
                    debug!(error = %e, "reader error");
                    break;
                }
            }
        }

        // Connection closed — fail all pending requests
        let mut p = reader_pending.lock().await;
        for (_, tx) in p.drain() {
            let _ = tx.send(Err(anyhow::anyhow!("connection closed")));
        }
    });

    // Process commands from the client API
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            WriterCmd::Send {
                request_id,
                payload,
                reply_tx,
            } => {
                let msg = IpcMessage::Request {
                    request_id,
                    payload,
                };
                match encode_ipc(&msg) {
                    Ok(frame_bytes) => {
                        pending.lock().await.insert(request_id, reply_tx);
                        let mut w = write_half.lock().await;
                        if let Err(e) = w.write_all(&frame_bytes).await {
                            let _ = pending
                                .lock()
                                .await
                                .remove(&request_id)
                                .map(|tx| tx.send(Err(e.into())));
                        } else {
                            let _ = w.flush().await;
                        }
                    }
                    Err(e) => {
                        let _ = reply_tx.send(Err(e.into()));
                    }
                }
            }
            WriterCmd::Close => break,
        }
    }

    // Shut down reader
    reader_handle.abort();
}
