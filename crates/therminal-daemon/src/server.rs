//! Daemon control socket server.
//!
//! Listens on a Unix domain socket for control messages (Ping, GracefulShutdown)
//! and dispatches them to the lifecycle state machine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, error, info, warn};

use therminal_protocol::daemon::{decode_request, encode_response, DaemonRequest, DaemonResponse};

use crate::lifecycle::Lifecycle;

/// The daemon control socket server.
pub struct DaemonServer {
    listener: UnixListener,
    socket_path: PathBuf,
    lifecycle: Arc<Lifecycle>,
    build_hash: String,
    version: String,
}

impl DaemonServer {
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

        info!(path = %socket_path.display(), "daemon socket bound");
        Ok(Self {
            listener,
            socket_path,
            lifecycle,
            build_hash,
            version,
        })
    }

    /// Run the server accept loop until the lifecycle transitions to Stopped.
    pub async fn run(self) -> Result<()> {
        let shutdown = self.lifecycle.shutdown_notify();
        let lifecycle = Arc::clone(&self.lifecycle);
        let build_hash = self.build_hash.clone();
        let version = self.version.clone();

        loop {
            tokio::select! {
                accept_result = self.listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            let lc = Arc::clone(&lifecycle);
                            let bh = build_hash.clone();
                            let ver = version.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(stream, lc, bh, ver).await {
                                    debug!(error = %e, "connection handler error");
                                }
                            });
                        }
                        Err(e) => {
                            error!(error = %e, "accept failed");
                            // Brief pause to avoid spinning on persistent errors
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

impl Drop for DaemonServer {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Handle a single client connection on the control socket.
async fn handle_connection(
    mut stream: UnixStream,
    lifecycle: Arc<Lifecycle>,
    build_hash: String,
    version: String,
) -> Result<()> {
    // Read length-prefixed message
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .context("failed to read message length")?;
    let msg_len = u32::from_be_bytes(len_buf) as usize;

    if msg_len > 1024 * 64 {
        anyhow::bail!("message too large: {msg_len} bytes");
    }

    let mut payload = vec![0u8; msg_len];
    stream
        .read_exact(&mut payload)
        .await
        .context("failed to read message payload")?;

    let request = decode_request(&payload).context("failed to decode request")?;
    debug!(?request, "received daemon request");

    let response = match request {
        DaemonRequest::Ping => DaemonResponse::Pong {
            build_hash,
            uptime_secs: lifecycle.uptime_secs(),
            sessions: lifecycle.session_count(),
            version,
        },
        DaemonRequest::GracefulShutdown => {
            // Spawn shutdown in background so we can reply first
            let lc = Arc::clone(&lifecycle);
            tokio::spawn(async move {
                if let Err(e) = lc.initiate_shutdown().await {
                    error!(error = %e, "shutdown failed");
                }
            });
            DaemonResponse::ShutdownAck
        }
    };

    let response_bytes = encode_response(&response).context("failed to encode response")?;
    stream
        .write_all(&response_bytes)
        .await
        .context("failed to write response")?;
    stream.flush().await.ok();

    Ok(())
}
