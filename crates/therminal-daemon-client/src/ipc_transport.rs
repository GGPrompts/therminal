//! Cross-platform IPC transport for the daemon control socket.
//!
//! Abstracts over Unix domain sockets (Linux/macOS/BSD) and Windows named
//! pipes so the rest of the daemon can speak in terms of `IpcServerStream`,
//! `IpcClientStream`, and `IpcListener` without `#[cfg]` noise.
//!
//! # Asymmetry
//!
//! `UnixStream` is symmetric — the same type is used on both sides of a
//! connection. Windows named pipes are asymmetric: the server side is a
//! `NamedPipeServer` and the client side is a `NamedPipeClient`. We mirror
//! that distinction with two type aliases (`IpcServerStream` /
//! `IpcClientStream`). On Unix both aliases resolve to `UnixStream`.
//!
//! # Socket-as-lock
//!
//! On Unix, `UnixListener::bind` fails if the path is already held → ownership.
//! On Windows, `ServerOptions::first_pipe_instance(true).create()` fails with
//! `ERROR_ACCESS_DENIED` if another process already holds the first instance
//! of that pipe name → equivalent ownership semantics.

use std::path::Path;

use anyhow::{Context, Result};

#[cfg(unix)]
pub use tokio::net::UnixStream as IpcServerStream;
#[cfg(unix)]
pub use tokio::net::UnixStream as IpcClientStream;

#[cfg(windows)]
pub use tokio::net::windows::named_pipe::NamedPipeClient as IpcClientStream;
#[cfg(windows)]
pub use tokio::net::windows::named_pipe::NamedPipeServer as IpcServerStream;

// ── Listener ──────────────────────────────────────────────────────────────

/// Cross-platform IPC listener.
///
/// On Unix this wraps a `UnixListener`. On Windows it owns the current
/// `NamedPipeServer` instance and creates the next one on each accept,
/// matching the standard one-instance-per-client pipe pattern.
pub struct IpcListener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,

    #[cfg(windows)]
    pipe_name: String,
    #[cfg(windows)]
    /// The pipe instance currently waiting for a client. Replaced after each
    /// accept with a fresh instance.
    current: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
}

impl IpcListener {
    /// Bind the listener at `socket_path`, taking the daemon ownership lock.
    ///
    /// On Unix, removes any stale socket file first then binds.
    /// On Windows, creates the first pipe instance with `first_pipe_instance(true)`
    /// — this fails if another process already holds the lock.
    pub fn bind(socket_path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            // Clean stale socket unconditionally — avoids TOCTOU race.
            match std::fs::remove_file(socket_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "failed to remove stale socket {}: {e}",
                        socket_path.display()
                    ));
                }
            }

            let inner = tokio::net::UnixListener::bind(socket_path).with_context(|| {
                format!("failed to bind daemon socket: {}", socket_path.display())
            })?;

            // Owner-only permissions.
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::Permissions::from_mode(0o700);
                let _ = std::fs::set_permissions(socket_path, perms);
            }

            Ok(Self { inner })
        }

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            let pipe_name = socket_path.to_string_lossy().into_owned();
            let first = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&pipe_name)
                .with_context(|| format!("failed to create daemon named pipe: {pipe_name}"))?;

            Ok(Self {
                pipe_name,
                current: Some(first),
            })
        }
    }

    /// Wait for the next client connection and return its stream.
    pub async fn accept(&mut self) -> Result<IpcServerStream> {
        #[cfg(unix)]
        {
            let (stream, _addr) = self
                .inner
                .accept()
                .await
                .context("failed to accept Unix socket connection")?;
            Ok(stream)
        }

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ServerOptions;

            let pipe = self
                .current
                .take()
                .expect("IpcListener::accept called without a current pipe instance");
            pipe.connect().await.context("named pipe connect failed")?;

            // Create the next pipe instance immediately so the next client
            // is not racing for an unbound name.
            let next = ServerOptions::new()
                .create(&self.pipe_name)
                .with_context(|| {
                    format!(
                        "failed to create next daemon named pipe instance: {}",
                        self.pipe_name
                    )
                })?;
            self.current = Some(next);

            Ok(pipe)
        }
    }
}

// ── Client connect ────────────────────────────────────────────────────────

/// Connect to a daemon at the given path, returning a client stream.
///
/// Cross-platform analogue of `UnixStream::connect` — opens a Unix socket
/// on Unix and a named pipe client on Windows.
pub async fn connect_client(socket_path: &Path) -> std::io::Result<IpcClientStream> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(socket_path).await
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        let name = socket_path.to_string_lossy();
        // Named pipes do not "queue" connections like Unix sockets — if all
        // server instances are busy (the daemon is already serving a client
        // and has not yet armed the next pipe instance), ClientOptions::open
        // returns ERROR_PIPE_BUSY (231). The standard pattern is to retry
        // briefly with a short sleep.
        //
        // tn-6tfn: this race fired during GUI reattach to a multi-pane
        // session. Each remote pane opens its own dedicated subscription
        // connection (remote_spawn.rs) and the second connect happened
        // before the daemon's accept loop re-armed the next pipe instance.
        // Without retry, the second pane's forwarder failed to connect, the
        // worker exited, and the pane was closed before any bytes streamed.
        const ERROR_PIPE_BUSY: i32 = 231;
        const MAX_RETRIES: u32 = 20; // ~1s total at 50ms each
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

        for _ in 0..MAX_RETRIES {
            match ClientOptions::new().open(&*name) {
                Ok(stream) => return Ok(stream),
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        // Final attempt — propagate whatever error we get on the last try.
        ClientOptions::new().open(&*name)
    }
}

// ── Cleanup ───────────────────────────────────────────────────────────────

/// Remove a stale daemon socket file. No-op on Windows where named pipes
/// have no filesystem representation.
pub fn cleanup_socket(socket_path: &Path) {
    #[cfg(unix)]
    {
        if socket_path.exists() {
            let _ = std::fs::remove_file(socket_path);
        }
    }
    #[cfg(windows)]
    {
        let _ = socket_path;
    }
}

/// Check whether a daemon socket "exists" — file presence on Unix, always
/// `false` on Windows (named pipes are not filesystem entries that we can
/// stat; presence is determined by attempting to connect).
pub fn socket_exists(socket_path: &Path) -> bool {
    #[cfg(unix)]
    {
        socket_path.exists()
    }
    #[cfg(windows)]
    {
        let _ = socket_path;
        false
    }
}
