#![cfg(unix)]
//! Integration tests for daemon handoff and lifecycle management.
//! Unix-only because they use raw `UnixListener` to stub a daemon.
//!
//! These tests exercise `handoff::check_daemon`, `handoff::perform_handoff`,
//! and the full server bind / ping cycle — the highest-risk code paths that
//! decide whether to reuse, replace, or start a daemon.
//!
//! Each test gets its own `tempdir` for socket files so tests never interfere
//! with each other or with a real running daemon.

use std::sync::Arc;
use std::time::Duration;

use therminal_daemon::{
    client,
    handoff::{self, DaemonCheck},
    lifecycle::{Lifecycle, LifecycleConfig},
    server::IpcServer,
};
use therminal_protocol::DaemonState;
use tokio::net::UnixListener;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Build a short-lived `LifecycleConfig` suitable for tests.
/// Disables idle exit (`keep_alive: None`) so the daemon doesn't auto-shutdown
/// during assertions, and uses very short drain/handoff timeouts.
fn test_lifecycle_config() -> LifecycleConfig {
    LifecycleConfig {
        keep_alive: None,
        drain_timeout: Duration::from_millis(200),
        handoff_timeout: Duration::from_millis(500),
    }
}

/// Spin up a real `IpcServer` bound to `socket_path`, transition lifecycle
/// to Running, and return `(lifecycle, join_handle)`.
///
/// The join handle resolves when the server's `run()` loop exits (i.e. after
/// `lifecycle.initiate_shutdown()` or a `GracefulShutdown` IPC request).
async fn start_test_server(
    socket_path: &std::path::Path,
    build_hash: &str,
) -> (Arc<Lifecycle>, tokio::task::JoinHandle<()>) {
    let lifecycle = Arc::new(Lifecycle::new(test_lifecycle_config()));
    lifecycle.transition(DaemonState::Binding).unwrap();

    let server = IpcServer::bind(
        socket_path.to_path_buf(),
        Arc::clone(&lifecycle),
        build_hash.to_string(),
        "0.0.0-test".to_string(),
    )
    .await
    .expect("IpcServer::bind should succeed");

    lifecycle.transition(DaemonState::Ready).unwrap();
    lifecycle.transition(DaemonState::Running).unwrap();

    let lc = Arc::clone(&lifecycle);
    let handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            eprintln!("test server exited with error: {e}");
        }
        if lc.state() != DaemonState::Stopped {
            let _ = lc.initiate_shutdown().await;
        }
    });

    // Give the accept loop a moment to start.
    tokio::time::sleep(Duration::from_millis(20)).await;

    (lifecycle, handle)
}

/// Wait for a socket file to disappear, up to `timeout`.
async fn wait_for_socket_removal(socket_path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !socket_path.exists() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// A running daemon with a matching protocol version should report `DaemonCheck::Reuse`.
#[tokio::test]
async fn check_daemon_reuse_when_hash_matches() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("daemon.sock");
    let build_hash = "abc123test";

    let (lifecycle, handle) = start_test_server(&socket_path, build_hash).await;

    // The server uses therminal_protocol::PROTOCOL_VERSION in its Pong,
    // so passing the same value should yield Reuse.
    let result = handoff::check_daemon(&socket_path, therminal_protocol::PROTOCOL_VERSION).await;
    assert!(
        matches!(result, DaemonCheck::Reuse),
        "expected Reuse when protocol versions match"
    );

    // Shut down cleanly.
    lifecycle.initiate_shutdown().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

/// A running daemon with a *different* protocol version should report `NeedsHandoff`.
#[tokio::test]
async fn check_daemon_needs_handoff_when_hash_differs() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("daemon.sock");
    let old_hash = "old-build-hash";

    let (lifecycle, handle) = start_test_server(&socket_path, old_hash).await;

    // Ask for a protocol version that differs from the server's PROTOCOL_VERSION.
    let different_version = therminal_protocol::PROTOCOL_VERSION + 1;
    let result = handoff::check_daemon(&socket_path, different_version).await;
    match result {
        DaemonCheck::NeedsHandoff { old_build_hash } => {
            assert_eq!(
                old_build_hash, old_hash,
                "NeedsHandoff should carry the old build hash"
            );
        }
        other => panic!("expected NeedsHandoff, got {other:?}"),
    }

    lifecycle.initiate_shutdown().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

/// When no daemon is running, `check_daemon` should return `StartFresh`.
#[tokio::test]
async fn check_daemon_start_fresh_when_no_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("no-daemon.sock");

    // Socket doesn't exist — nothing to connect to.
    let result = handoff::check_daemon(&socket_path, 999).await;
    assert!(
        matches!(result, DaemonCheck::StartFresh),
        "expected StartFresh when no daemon is running"
    );
}

/// A stale socket file (exists on disk but nobody is listening) should yield
/// `StartFresh` — the client will fail to connect and the caller is expected
/// to clean up the file before binding.
#[tokio::test]
async fn check_daemon_start_fresh_with_stale_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("stale.sock");

    // Create a stale socket file: bind and immediately drop the listener so
    // the file remains but nothing is listening.
    {
        let _listener = UnixListener::bind(&socket_path).expect("should bind stale socket");
        // _listener is dropped here, closing the listening end.
    }

    assert!(socket_path.exists(), "stale socket file should still exist");

    // check_daemon should fail to connect and return StartFresh.
    let result = handoff::check_daemon(&socket_path, 999).await;
    assert!(
        matches!(result, DaemonCheck::StartFresh),
        "expected StartFresh for a stale (nobody listening) socket"
    );
}

/// `perform_handoff` sends `GracefulShutdown`, waits for the socket to
/// disappear, and returns `Ok(())`.
#[tokio::test]
async fn perform_handoff_succeeds_when_daemon_shuts_down() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("handoff.sock");
    let build_hash = "handoff-test-hash";

    let (_lifecycle, handle) = start_test_server(&socket_path, build_hash).await;

    // perform_handoff should send GracefulShutdown and wait for socket removal.
    let result = handoff::perform_handoff(&socket_path).await;
    assert!(result.is_ok(), "perform_handoff should succeed: {result:?}");

    // Socket should be gone.
    assert!(
        !socket_path.exists(),
        "socket should be removed after handoff"
    );

    // Server task should exit cleanly.
    let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
}

/// When the old daemon acknowledges shutdown but takes longer than normal to
/// remove the socket, `perform_handoff` force-removes it after the timeout.
///
/// We simulate a "slow" daemon by binding a raw `UnixListener` that reads the
/// shutdown request but never closes the socket file on its own.
#[tokio::test]
async fn perform_handoff_force_removes_socket_on_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("slow-daemon.sock");
    let sp = socket_path.clone();

    // Spawn a minimal server that:
    // 1. Accepts one connection.
    // 2. Reads the request frame (so the client doesn't get a broken pipe).
    // 3. Sends a ShutdownAck.
    // 4. Then HOLDS the socket open indefinitely (simulates a stuck daemon).
    let listener = UnixListener::bind(&socket_path).expect("bind slow-daemon socket");
    tokio::spawn(async move {
        use therminal_protocol::daemon::{IpcMessage, IpcResponse, encode_ipc};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        if let Ok((mut stream, _)) = listener.accept().await {
            // Read the length-prefixed request frame.
            let mut len_buf = [0u8; 4];
            let _ = stream.read_exact(&mut len_buf).await;
            let msg_len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; msg_len];
            let _ = stream.read_exact(&mut buf).await;

            // Send ShutdownAck.
            let ack = IpcMessage::Response {
                request_id: 1,
                payload: IpcResponse::ShutdownAck,
            };
            if let Ok(frame) = encode_ipc(&ack) {
                let _ = stream.write_all(&frame).await;
                let _ = stream.flush().await;
            }

            // Hold the socket open — simulate a daemon that doesn't shut down.
            // The test has a shorter HANDOFF_TIMEOUT in perform_handoff (5 s),
            // but we keep the file alive until the test drops `_sp_guard`.
            //
            // NOTE: the `listener` is held alive in this scope, so `sp` (the
            // socket file) remains on disk.  perform_handoff will poll until
            // its timeout and then force-remove the file.
            let _keep_alive = sp; // prevents early drop
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });

    // Give the "slow daemon" time to start listening.
    tokio::time::sleep(Duration::from_millis(30)).await;

    // perform_handoff has a 5-second timeout; we override with a fast-path
    // by testing at the handoff module level.  Since HANDOFF_TIMEOUT is a
    // compile-time constant (5 s), we verify the force path by confirming:
    //  a) The call eventually returns Ok (force-remove succeeds).
    //  b) The socket file is gone.
    //
    // We wrap in a generous outer timeout so the test doesn't hang forever.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        handoff::perform_handoff(&socket_path),
    )
    .await;

    assert!(result.is_ok(), "perform_handoff should not itself time out");
    let inner = result.unwrap();
    assert!(
        inner.is_ok(),
        "perform_handoff should succeed via force-remove: {inner:?}"
    );
    assert!(
        !socket_path.exists(),
        "socket file should be gone after forced removal"
    );
}

/// End-to-end: bind a server, ping it, confirm it's healthy.
#[tokio::test]
async fn server_bind_and_ping() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("ping-test.sock");

    let (lifecycle, handle) = start_test_server(&socket_path, "ping-build-hash").await;

    let resp = client::ping(&socket_path)
        .await
        .expect("ping should succeed");

    match resp {
        therminal_protocol::IpcResponse::Pong {
            build_hash,
            sessions,
            ..
        } => {
            assert_eq!(build_hash, "ping-build-hash");
            assert_eq!(sessions, 0);
        }
        other => panic!("expected Pong, got {other:?}"),
    }

    lifecycle.initiate_shutdown().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

/// End-to-end: request shutdown via IPC, confirm the socket is removed.
#[tokio::test]
async fn server_graceful_shutdown_via_ipc() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("shutdown-test.sock");

    let (_lifecycle, handle) = start_test_server(&socket_path, "shutdown-hash").await;

    let resp = client::request_shutdown(&socket_path)
        .await
        .expect("shutdown request should succeed");

    assert!(
        matches!(resp, therminal_protocol::IpcResponse::ShutdownAck),
        "expected ShutdownAck"
    );

    // Socket should disappear once the server exits.
    let removed = wait_for_socket_removal(&socket_path, Duration::from_secs(3)).await;
    assert!(removed, "socket should be removed after graceful shutdown");

    let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
}

/// A server that accepts connections but sends garbage data should be detected
/// as an incompatible daemon — `check_daemon` returns `IncompatibleDaemon`,
/// not `StartFresh`.
#[tokio::test]
async fn check_daemon_incompatible_when_garbage_response() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("garbage.sock");

    // Spawn a mock server that accepts connections and sends garbage bytes.
    let listener = UnixListener::bind(&socket_path).expect("bind garbage socket");
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;

        // Accept connections in a loop so both the probe connect and the
        // ping connect succeed.
        while let Ok((mut stream, _)) = listener.accept().await {
            // Send garbage: a valid length prefix followed by random bytes.
            let garbage = b"\x00\x00\x00\x04JUNK";
            let _ = stream.write_all(garbage).await;
            let _ = stream.flush().await;
            // Keep the stream alive briefly so the client can read.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    // Give the mock server time to start.
    tokio::time::sleep(Duration::from_millis(20)).await;

    let result = handoff::check_daemon(&socket_path, 999).await;
    assert!(
        matches!(result, DaemonCheck::IncompatibleDaemon),
        "expected IncompatibleDaemon for a server sending garbage, got {result:?}"
    );
}

/// A server that accepts connections but immediately closes them should be
/// detected as incompatible (not StartFresh), since something is listening.
#[tokio::test]
async fn check_daemon_incompatible_when_immediate_close() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("close-immediately.sock");

    let listener = UnixListener::bind(&socket_path).expect("bind socket");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            // Accept and immediately drop — simulates a server that
            // doesn't speak our protocol at all.
            drop(stream);
        }
    });

    tokio::time::sleep(Duration::from_millis(20)).await;

    let result = handoff::check_daemon(&socket_path, 999).await;
    assert!(
        matches!(result, DaemonCheck::IncompatibleDaemon),
        "expected IncompatibleDaemon for a server that immediately closes, got {result:?}"
    );
}

/// Verify that `IpcServer::bind` cleans up a stale socket file automatically
/// (the socket-as-lock pattern: second bind wins without manual cleanup).
#[tokio::test]
async fn server_bind_removes_stale_socket() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("stale-bind.sock");

    // Create a stale socket by dropping the listener immediately.
    {
        let _l = UnixListener::bind(&socket_path).unwrap();
    }
    assert!(
        socket_path.exists(),
        "stale socket file should exist before bind"
    );

    // IpcServer::bind should clean up the stale file and succeed.
    let lifecycle = Arc::new(Lifecycle::new(test_lifecycle_config()));
    lifecycle.transition(DaemonState::Binding).unwrap();
    let server = IpcServer::bind(
        socket_path.clone(),
        Arc::clone(&lifecycle),
        "stale-bind-hash".to_string(),
        "0.0.0-test".to_string(),
    )
    .await;

    if let Err(e) = &server {
        panic!("IpcServer::bind should succeed on a stale socket: {e}");
    }

    // Trigger lifecycle shutdown to properly clean up.
    lifecycle.transition(DaemonState::Ready).unwrap();
    lifecycle.transition(DaemonState::Running).unwrap();
    lifecycle.initiate_shutdown().await.unwrap();
    drop(server);
}
