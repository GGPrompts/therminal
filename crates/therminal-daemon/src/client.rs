//! Daemon client — used by `ensure_daemon()` and CLI tools to communicate
//! with a running daemon over the control socket.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::debug;

use therminal_protocol::daemon::{decode_response, encode_request, DaemonRequest, DaemonResponse};

/// Default timeout for daemon communication.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Send a request to the daemon and read the response.
///
/// Returns `Err` if the socket doesn't exist, the daemon is unreachable,
/// or the response times out (2s default).
pub async fn send_request(socket_path: &Path, request: &DaemonRequest) -> Result<DaemonResponse> {
    send_request_with_timeout(socket_path, request, DEFAULT_TIMEOUT).await
}

/// Send a request with a custom timeout.
pub async fn send_request_with_timeout(
    socket_path: &Path,
    request: &DaemonRequest,
    timeout: Duration,
) -> Result<DaemonResponse> {
    let result = tokio::time::timeout(timeout, async {
        let mut stream = UnixStream::connect(socket_path).await.with_context(|| {
            format!(
                "failed to connect to daemon socket: {}",
                socket_path.display()
            )
        })?;

        // Send request
        let req_bytes = encode_request(request).context("failed to encode request")?;
        stream
            .write_all(&req_bytes)
            .await
            .context("failed to send request")?;
        stream.flush().await.ok();

        // Read response
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .context("failed to read response length")?;
        let msg_len = u32::from_be_bytes(len_buf) as usize;

        if msg_len > 1024 * 64 {
            anyhow::bail!("response too large: {msg_len} bytes");
        }

        let mut payload = vec![0u8; msg_len];
        stream
            .read_exact(&mut payload)
            .await
            .context("failed to read response payload")?;

        let response = decode_response(&payload).context("failed to decode response")?;
        debug!(?response, "received daemon response");
        Ok(response)
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!("daemon request timed out after {}ms", timeout.as_millis()),
    }
}

/// Ping the daemon and return the Pong response, or an error if unreachable.
pub async fn ping(socket_path: &Path) -> Result<DaemonResponse> {
    send_request(socket_path, &DaemonRequest::Ping).await
}

/// Request graceful shutdown of the daemon.
pub async fn request_shutdown(socket_path: &Path) -> Result<DaemonResponse> {
    send_request(socket_path, &DaemonRequest::GracefulShutdown).await
}
