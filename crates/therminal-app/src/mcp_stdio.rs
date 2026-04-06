//! MCP stdio bridge — connects stdin/stdout to the daemon's MCP IPC endpoint.
//!
//! This allows MCP clients (Claude Code, etc.) to interact with therminal
//! sessions by launching `therminal mcp` as a subprocess. The bridge is a
//! simple bidirectional byte copy: stdin → daemon MCP socket, socket → stdout.
//!
//! On Unix, the daemon listens on a Unix domain socket.
//! On Windows, the daemon listens on a named pipe.
//!
//! The daemon's MCP server handles all protocol logic (JSON-RPC, tool
//! dispatch, trust enforcement). This module is just plumbing.

use anyhow::{Context, Result};
use tokio::io;
use tracing::{debug, error, info};

/// Run the MCP stdio bridge.
///
/// Connects to the daemon's MCP IPC endpoint and copies bytes bidirectionally
/// between stdin/stdout and the connection. Exits when either side closes.
pub fn run() -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    rt.block_on(run_async())
}

async fn run_async() -> Result<()> {
    let config = therminal_core::config::TherminalConfig::load();
    let socket_path = config.mcp.resolved_socket_path();

    info!(path = %socket_path.display(), "connecting to daemon MCP endpoint");

    let mut stdin = io::stdin();
    let mut stdout = io::stdout();

    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(&socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to daemon MCP socket at {}. \
                     Is the therminal daemon running?",
                    socket_path.display()
                )
            })?;

        debug!("connected to daemon MCP socket");
        let (mut sock_read, mut sock_write) = stream.into_split();

        tokio::select! {
            result = io::copy(&mut stdin, &mut sock_write) => {
                match result {
                    Ok(n) => debug!(bytes = n, "stdin → socket finished"),
                    Err(e) => error!(error = %e, "stdin → socket error"),
                }
            }
            result = io::copy(&mut sock_read, &mut stdout) => {
                match result {
                    Ok(n) => debug!(bytes = n, "socket → stdout finished"),
                    Err(e) => error!(error = %e, "socket → stdout error"),
                }
            }
        }
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;

        let pipe_path = socket_path.to_string_lossy();
        let pipe = ClientOptions::new().open(&*pipe_path).with_context(|| {
            format!(
                "failed to connect to daemon MCP named pipe at {}. \
                 Is the therminal daemon running?",
                pipe_path
            )
        })?;

        debug!("connected to daemon MCP named pipe");
        let (mut pipe_read, mut pipe_write) = io::split(pipe);

        tokio::select! {
            result = io::copy(&mut stdin, &mut pipe_write) => {
                match result {
                    Ok(n) => debug!(bytes = n, "stdin → pipe finished"),
                    Err(e) => error!(error = %e, "stdin → pipe error"),
                }
            }
            result = io::copy(&mut pipe_read, &mut stdout) => {
                match result {
                    Ok(n) => debug!(bytes = n, "pipe → stdout finished"),
                    Err(e) => error!(error = %e, "pipe → stdout error"),
                }
            }
        }
    }

    info!("MCP stdio bridge shutting down");
    Ok(())
}
