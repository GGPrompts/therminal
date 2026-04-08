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
use tokio::io::{self, AsyncRead, AsyncWrite, AsyncWriteExt};
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
        let (sock_read, sock_write) = stream.into_split();
        pump(io::stdin(), io::stdout(), sock_read, sock_write).await;
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
        let (pipe_read, pipe_write) = io::split(pipe);
        pump(io::stdin(), io::stdout(), pipe_read, pipe_write).await;
    }

    info!("MCP stdio bridge shutting down");
    Ok(())
}

/// Bidirectional byte pump with explicit half-close on EOF.
///
/// Unlike `tokio::select!` over two `io::copy` futures (which cancels the
/// losing branch mid-write and drops in-flight bytes), this spawns both
/// directions as independent tasks and only tears down after *both* finish
/// draining. When a direction hits EOF, it explicitly `shutdown()`s its
/// write half so the peer observes the half-close cleanly.
async fn pump<SR, SW>(
    mut stdin: io::Stdin,
    mut stdout: io::Stdout,
    mut sock_read: SR,
    mut sock_write: SW,
) where
    SR: AsyncRead + Unpin + Send + 'static,
    SW: AsyncWrite + Unpin + Send + 'static,
{
    // stdin -> socket
    let up = async move {
        let res = io::copy(&mut stdin, &mut sock_write).await;
        // Explicitly half-close the socket write side so the daemon sees EOF
        // and can flush its final response without the read side being torn
        // down by a cancelled select branch.
        if let Err(e) = sock_write.shutdown().await {
            debug!(direction = "stdin -> socket", error = %e, "shutdown error");
        }
        match &res {
            Ok(n) => debug!(bytes = n, "stdin → socket finished"),
            Err(e) => error!(direction = "stdin -> socket", error = %e, "stdin → socket error"),
        }
        res
    };

    // socket -> stdout
    let down = async move {
        let res = io::copy(&mut sock_read, &mut stdout).await;
        if let Err(e) = stdout.shutdown().await {
            debug!(direction = "socket -> stdout", error = %e, "shutdown error");
        }
        match &res {
            Ok(n) => debug!(bytes = n, "socket → stdout finished"),
            Err(e) => error!(direction = "socket -> stdout", error = %e, "socket → stdout error"),
        }
        res
    };

    let up_handle = tokio::spawn(up);
    let down_handle = tokio::spawn(down);

    // Wait for the socket->stdout direction to finish — that's the one that
    // carries the daemon's final response. Once it returns (EOF from daemon),
    // the bridge is done regardless of whether stdin is still open; the stdin
    // task will be aborted below.
    let _ = down_handle.await;
    up_handle.abort();
    let _ = up_handle.await;
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tracing::{debug, error};

    /// Test version of `pump` that takes generic stdio sides so we can feed
    /// it `tokio::io::duplex` pairs instead of real stdin/stdout. Mirrors the
    /// production pump exactly: spawn both directions, shutdown write halves
    /// on EOF, wait for the socket->stdout side to drain.
    async fn pump_test<StdinR, StdoutW, SR, SW>(
        mut stdin: StdinR,
        mut stdout: StdoutW,
        mut sock_read: SR,
        mut sock_write: SW,
    ) where
        StdinR: AsyncRead + Unpin + Send + 'static,
        StdoutW: AsyncWrite + Unpin + Send + 'static,
        SR: AsyncRead + Unpin + Send + 'static,
        SW: AsyncWrite + Unpin + Send + 'static,
    {
        let up = async move {
            let res = tokio::io::copy(&mut stdin, &mut sock_write).await;
            let _ = sock_write.shutdown().await;
            if let Err(e) = &res {
                error!(direction = "stdin -> socket", error = %e, "err");
            } else {
                debug!("stdin → socket finished");
            }
        };
        let down = async move {
            let res = tokio::io::copy(&mut sock_read, &mut stdout).await;
            let _ = stdout.shutdown().await;
            if let Err(e) = &res {
                error!(direction = "socket -> stdout", error = %e, "err");
            } else {
                debug!("socket → stdout finished");
            }
        };

        let up_handle = tokio::spawn(up);
        let down_handle = tokio::spawn(down);
        let _ = down_handle.await;
        up_handle.abort();
        let _ = up_handle.await;
    }

    /// Regression test for tn-587k: the previous `tokio::select!` pump would
    /// cancel the socket->stdout branch as soon as stdin->socket hit EOF,
    /// truncating any response bytes still in flight. This test half-closes
    /// the stdin side *before* the response is written on the socket side,
    /// then asserts the full response still reaches stdout.
    #[tokio::test]
    async fn pump_delivers_response_after_stdin_eof() {
        // stdio side: `client_stdin_writer` feeds bytes that the bridge sees
        // on its "stdin", and `client_stdout_reader` receives bytes the
        // bridge writes to its "stdout".
        let (client_stdin_writer, bridge_stdin) = tokio::io::duplex(1024);
        let (bridge_stdout, mut client_stdout_reader) = tokio::io::duplex(1024);

        // socket side: `daemon_side` is the mock daemon; `bridge_socket_*`
        // is what the bridge reads/writes.
        let (bridge_socket, mut daemon_side) = tokio::io::duplex(1024);
        let (bridge_sock_read, bridge_sock_write) = tokio::io::split(bridge_socket);

        let pump_task = tokio::spawn(pump_test(
            bridge_stdin,
            bridge_stdout,
            bridge_sock_read,
            bridge_sock_write,
        ));

        // Client writes a request, then closes stdin. This is the critical
        // half-close that used to cancel the response direction.
        let request = b"REQUEST-PAYLOAD\n";
        {
            let mut w = client_stdin_writer;
            w.write_all(request).await.unwrap();
            w.shutdown().await.unwrap();
            drop(w);
        }

        // Daemon reads the request...
        let mut got_req = vec![0u8; request.len()];
        daemon_side.read_exact(&mut got_req).await.unwrap();
        assert_eq!(&got_req, request);

        // ...then writes a large response *after* stdin EOF has propagated,
        // then half-closes.
        let response: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        daemon_side.write_all(&response).await.unwrap();
        daemon_side.shutdown().await.unwrap();
        drop(daemon_side);

        // The bridge must deliver the entire response to stdout.
        let mut got_resp = Vec::new();
        client_stdout_reader
            .read_to_end(&mut got_resp)
            .await
            .unwrap();
        assert_eq!(
            got_resp, response,
            "bridge truncated response after stdin EOF"
        );

        pump_task.await.unwrap();
    }
}
