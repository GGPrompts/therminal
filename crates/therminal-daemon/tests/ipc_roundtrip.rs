//! Integration tests for IPC round-trips between `IpcServer` and `DaemonClient`.
//!
//! Each test spins up a real `IpcServer` bound to a unique temp socket path,
//! exercises the protocol, then tears down cleanly. Tests are independent and
//! use different socket paths to avoid interference.

use std::sync::Arc;
use std::time::Duration;

use therminal_daemon::client::DaemonClient;
use therminal_daemon::lifecycle::{Lifecycle, LifecycleConfig};
use therminal_daemon::server::IpcServer;
use therminal_protocol::daemon::{
    DaemonEvent, DaemonState, EventKind, IpcResponse, MAX_FRAME_SIZE,
};

// ── Helpers ───────────────────────────────────────────────────────────────

/// Generate a unique temp socket path for each test using the thread id.
fn temp_socket(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "therminal_ipc_test_{label}_{}.sock",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ))
}

/// Build a `Lifecycle` already in the `Running` state so the server can serve.
fn running_lifecycle() -> Arc<Lifecycle> {
    let lc = Arc::new(Lifecycle::new(LifecycleConfig {
        // No idle exit during tests.
        keep_alive: None,
        drain_timeout: Duration::from_millis(200),
        handoff_timeout: Duration::from_millis(200),
    }));
    // Starting -> Binding -> Ready -> Running
    lc.transition(DaemonState::Binding).unwrap();
    lc.transition(DaemonState::Ready).unwrap();
    lc.transition(DaemonState::Running).unwrap();
    lc
}

/// Bind the server on `socket_path` and spawn its accept loop in the
/// background. Returns the server handle so we can trigger shutdown later.
async fn spawn_server(socket_path: std::path::PathBuf, lifecycle: Arc<Lifecycle>) -> IpcServer {
    IpcServer::bind(
        socket_path,
        lifecycle,
        "test-build-hash".to_string(),
        "0.0.0-test".to_string(),
    )
    .await
    .expect("server bind failed")
}

// ── Tests ─────────────────────────────────────────────────────────────────

/// Basic round-trip: Ping → Pong.
#[tokio::test]
async fn test_ping_pong_round_trip() {
    let path = temp_socket("ping");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;

    // Run the server accept loop in the background.
    tokio::spawn(async move { server.run().await });

    // Give the server a moment to enter its accept loop.
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
        .await
        .expect("client connect failed");

    let resp = client.ping().await.expect("ping failed");

    match resp {
        IpcResponse::Pong {
            build_hash,
            version,
            sessions,
            ..
        } => {
            assert_eq!(build_hash, "test-build-hash");
            assert_eq!(version, "0.0.0-test");
            assert_eq!(sessions, 0);
        }
        other => panic!("expected Pong, got {other:?}"),
    }

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// Multiple concurrent requests correlate by request_id.
#[tokio::test]
async fn test_concurrent_requests_correlate() {
    let path = temp_socket("concurrent");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = Arc::new(
        DaemonClient::connect_with_timeout(&path, Duration::from_secs(5))
            .await
            .expect("client connect failed"),
    );

    // Fire 20 concurrent Ping requests and verify every response is a Pong.
    const N: usize = 20;
    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let c = Arc::clone(&client);
        handles.push(tokio::spawn(async move { c.ping().await }));
    }

    for handle in handles {
        let resp = handle.await.expect("task panicked").expect("ping failed");
        assert!(
            matches!(resp, IpcResponse::Pong { .. }),
            "expected Pong, got {resp:?}"
        );
    }

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// Subscribe → trigger event → client receives it.
#[tokio::test]
async fn test_event_subscription_and_delivery() {
    let path = temp_socket("events");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    let event_tx = server.event_sender();
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
        .await
        .expect("client connect failed");

    // Subscribe to all events.
    let sub_resp = client
        .subscribe_events(vec![])
        .await
        .expect("subscribe failed");
    assert!(
        matches!(sub_resp, IpcResponse::Subscribed { .. }),
        "expected Subscribed, got {sub_resp:?}"
    );

    // Broadcast a StateChanged event from the server side.
    let _ = event_tx.send(DaemonEvent::StateChanged {
        old: DaemonState::Running,
        new: DaemonState::Draining,
    });

    // Receive the event on the client with a short timeout.
    let received = tokio::time::timeout(Duration::from_secs(2), client.recv_event())
        .await
        .expect("timed out waiting for event")
        .expect("connection closed before event");

    assert!(
        matches!(
            received,
            DaemonEvent::StateChanged {
                old: DaemonState::Running,
                new: DaemonState::Draining,
            }
        ),
        "unexpected event: {received:?}"
    );

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// Event subscription with a kind filter — only matching events arrive.
#[tokio::test]
async fn test_event_subscription_filtered() {
    let path = temp_socket("filtered");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    let event_tx = server.event_sender();
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
        .await
        .expect("client connect failed");

    // Subscribe only to SessionCreated events.
    client
        .subscribe_events(vec![EventKind::SessionCreated])
        .await
        .expect("subscribe failed");

    // Send a StateChanged event (should be filtered out).
    let _ = event_tx.send(DaemonEvent::StateChanged {
        old: DaemonState::Running,
        new: DaemonState::Draining,
    });

    // Send a SessionCreated event (should pass through).
    let _ = event_tx.send(DaemonEvent::SessionCreated { session_id: 99 });

    // We should receive the SessionCreated, not the StateChanged.
    let received = tokio::time::timeout(Duration::from_secs(2), client.recv_event())
        .await
        .expect("timed out waiting for event")
        .expect("connection closed before event");

    assert!(
        matches!(received, DaemonEvent::SessionCreated { session_id: 99 }),
        "unexpected event: {received:?}"
    );

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// Large frame near MAX_FRAME_SIZE is accepted without error.
///
/// We send a `PaneOutput` event that packs the payload close to the limit.
/// This exercises the framing layer's size check on both encode and decode paths.
#[tokio::test]
async fn test_large_frame_near_max_size() {
    let path = temp_socket("large");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    let event_tx = server.event_sender();
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(5))
        .await
        .expect("client connect failed");

    client
        .subscribe_events(vec![EventKind::PaneOutput])
        .await
        .expect("subscribe failed");

    // Build a large data payload. The IpcMessage envelope adds overhead
    // (MessagePack tags, field names), so we stay well under MAX_FRAME_SIZE
    // while still being a large frame. ~900 KiB leaves enough headroom.
    let large_data = vec![0u8; 900 * 1024];
    let _ = event_tx.send(DaemonEvent::PaneOutput {
        session_id: 1,
        pane_id: 1,
        data: large_data.clone(),
    });

    let received = tokio::time::timeout(Duration::from_secs(5), client.recv_event())
        .await
        .expect("timed out waiting for large frame event")
        .expect("connection closed before event");

    match received {
        DaemonEvent::PaneOutput {
            session_id,
            pane_id,
            data,
        } => {
            assert_eq!(session_id, 1);
            assert_eq!(pane_id, 1);
            assert_eq!(data.len(), large_data.len());
        }
        other => panic!("expected PaneOutput, got {other:?}"),
    }

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// Client disconnect is handled gracefully — the server must not panic.
///
/// We open a connection, send one request, then drop the client immediately
/// (simulating an abrupt disconnect). The server's connection handler should
/// observe EOF and return without panicking.
#[tokio::test]
async fn test_client_disconnect_graceful() {
    let path = temp_socket("disconnect");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    {
        // Connect and immediately drop without closing gracefully.
        let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
            .await
            .expect("client connect failed");

        // Send one request to ensure the server has an active handler.
        let _ = client.ping().await;

        // `client` is dropped here — socket closed abruptly.
    }

    // Give the server's connection handler time to notice the disconnect.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The server should still be up and able to serve new connections.
    let client2 = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
        .await
        .expect("second client connect failed after disconnect");
    let resp = client2.ping().await.expect("ping on second client failed");
    assert!(
        matches!(resp, IpcResponse::Pong { .. }),
        "expected Pong from server after previous client disconnect, got {resp:?}"
    );

    client2.close().await;
    shutdown_notify.notify_waiters();
}

/// GetState round-trip returns the current daemon state.
#[tokio::test]
async fn test_get_state_round_trip() {
    let path = temp_socket("getstate");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
        .await
        .expect("client connect failed");

    let resp = client.get_state().await.expect("get_state failed");
    assert!(
        matches!(
            resp,
            IpcResponse::State {
                state: DaemonState::Running
            }
        ),
        "expected State {{ Running }}, got {resp:?}"
    );

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// ListSessions round-trip returns an empty list on a fresh server.
#[tokio::test]
async fn test_list_sessions_empty() {
    let path = temp_socket("listsessions");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
        .await
        .expect("client connect failed");

    let resp = client
        .send_request(therminal_protocol::daemon::IpcRequest::ListSessions)
        .await
        .expect("list sessions failed");

    assert!(
        matches!(resp, IpcResponse::Sessions { ref session_ids } if session_ids.is_empty()),
        "expected empty Sessions list, got {resp:?}"
    );

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// Unsubscribe after subscribe stops event delivery.
#[tokio::test]
async fn test_unsubscribe_stops_events() {
    let path = temp_socket("unsub");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    let event_tx = server.event_sender();
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(2))
        .await
        .expect("client connect failed");

    // Subscribe, then immediately unsubscribe.
    client
        .subscribe_events(vec![])
        .await
        .expect("subscribe failed");

    let unsub = client
        .unsubscribe_events()
        .await
        .expect("unsubscribe failed");
    assert!(
        matches!(unsub, IpcResponse::Unsubscribed),
        "expected Unsubscribed, got {unsub:?}"
    );

    // Broadcast an event — client should NOT receive it.
    let _ = event_tx.send(DaemonEvent::SessionCreated { session_id: 42 });

    // Non-blocking check: no event should be available.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let maybe = client.try_recv_event();
    assert!(
        maybe.is_none(),
        "expected no event after unsubscribe, got {maybe:?}"
    );

    client.close().await;
    shutdown_notify.notify_waiters();
}

/// Verify that `MAX_FRAME_SIZE` is exactly 1 MiB as documented.
#[test]
fn test_max_frame_size_is_one_mib() {
    assert_eq!(MAX_FRAME_SIZE, 1024 * 1024);
}

/// tn-l3hk: IPC round-trip latency baseline.
///
/// Measures the overhead of a single request/response cycle over a local
/// Unix socket (or named pipe on Windows). This characterises the per-IPC-call
/// overhead that `spawn_remote_pane` pays three times at startup:
/// CreateSession + GetWorkspaces + CapturePaneState.
///
/// **Decision criteria (from tn-l3hk acceptance):**
/// - If median RTT < 2 ms and p99 < 10 ms → streamed-bytes model is fine,
///   FD passing deferred.
/// - If overhead is >10% of perceived keystroke latency or p99 > 10 ms →
///   file a follow-up to use fd_passing.rs machinery.
///
/// Prints results to stdout; the test always passes (it is an observation,
/// not a correctness gate). Run with `-- --nocapture` to see the numbers.
#[tokio::test]
async fn ipc_rtt_latency_baseline() {
    let path = temp_socket("latency");
    let lifecycle = running_lifecycle();
    let shutdown_notify = lifecycle.shutdown_notify();

    let server = spawn_server(path.clone(), Arc::clone(&lifecycle)).await;
    tokio::spawn(async move { server.run().await });
    tokio::time::sleep(Duration::from_millis(10)).await;

    let client = DaemonClient::connect_with_timeout(&path, Duration::from_secs(5))
        .await
        .expect("client connect failed");

    // Warm-up: 5 pings to prime the socket and tokio task scheduling.
    for _ in 0..5 {
        client.ping().await.expect("warm-up ping failed");
    }

    // Measurement: 100 sequential Ping/Pong round-trips.
    const SAMPLES: usize = 100;
    let mut samples_us: Vec<u64> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = std::time::Instant::now();
        client.ping().await.expect("latency ping failed");
        samples_us.push(t.elapsed().as_micros() as u64);
    }

    // Compute statistics.
    samples_us.sort_unstable();
    let min_us = samples_us[0];
    let max_us = samples_us[SAMPLES - 1];
    let median_us = samples_us[SAMPLES / 2];
    let p95_us = samples_us[(SAMPLES as f64 * 0.95) as usize];
    let p99_us = samples_us[(SAMPLES as f64 * 0.99) as usize];
    let mean_us: u64 = samples_us.iter().sum::<u64>() / SAMPLES as u64;

    println!(
        "\ntn-l3hk IPC round-trip latency (Ping/Pong, {} samples, local Unix socket):",
        SAMPLES
    );
    println!(
        "  min:    {:>6} µs  ({:.3} ms)",
        min_us,
        min_us as f64 / 1000.0
    );
    println!(
        "  mean:   {:>6} µs  ({:.3} ms)",
        mean_us,
        mean_us as f64 / 1000.0
    );
    println!(
        "  median: {:>6} µs  ({:.3} ms)",
        median_us,
        median_us as f64 / 1000.0
    );
    println!(
        "  p95:    {:>6} µs  ({:.3} ms)",
        p95_us,
        p95_us as f64 / 1000.0
    );
    println!(
        "  p99:    {:>6} µs  ({:.3} ms)",
        p99_us,
        p99_us as f64 / 1000.0
    );
    println!(
        "  max:    {:>6} µs  ({:.3} ms)",
        max_us,
        max_us as f64 / 1000.0
    );
    println!();

    // spawn_remote_pane pays 3 round-trips: CreateSession + GetWorkspaces +
    // CapturePaneState. Estimate total IPC overhead at startup.
    let estimated_startup_ipc_us = mean_us * 3;
    println!(
        "  Estimated IPC overhead at spawn_remote_pane startup (3 RPCs): {} µs  ({:.1} ms)",
        estimated_startup_ipc_us,
        estimated_startup_ipc_us as f64 / 1000.0
    );

    // tn-l3hk decision gate: median < 2ms and p99 < 10ms → acceptable.
    let median_ok = median_us < 2_000;
    let p99_ok = p99_us < 10_000;
    println!(
        "  Decision: median < 2ms = {}, p99 < 10ms = {}",
        if median_ok { "PASS" } else { "FAIL" },
        if p99_ok { "PASS" } else { "FAIL" }
    );
    if median_ok && p99_ok {
        println!("  → streamed-bytes model acceptable; FD passing deferred.");
    } else {
        println!(
            "  → IPC overhead exceeds threshold; file FD-passing follow-up (see crates/therminal-daemon/src/fd_passing.rs)."
        );
    }

    client.close().await;
    shutdown_notify.notify_waiters();
}
