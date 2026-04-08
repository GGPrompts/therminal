//! End-to-end scenarios (tn-1kzt).
//!
//! Each test spawns a fresh `therminal-daemon` subprocess in an isolated
//! temp runtime directory and drives it via the native IPC client. These
//! are slower than unit tests but exercise the full
//! `PTY -> grid -> semantic -> IPC` path that unit tests alone can't cover.
//!
//! The tests are **not** gated behind `#[ignore]` on Unix because the
//! daemon spawns a real shell via `portable-pty` and most CI environments
//! (GitHub Actions Linux runners, macOS runners) have `/dev/ptmx` and a
//! working `$SHELL`. Environments without a PTY (e.g. `docker run` without
//! `--tty`) or a working shell will fail fast in `DaemonHarness::spawn`.
//!
//! On Windows the whole module is excluded: the daemon binary builds, but
//! the test hasn't been validated against the named-pipe path yet and
//! this crate's mission is to catch Unix-side PTY regressions first.
//! Windows-native CI can be added in a follow-up once the PTY-side
//! scenarios are stable.
//!
//! Manual run:
//! ```sh
//! cargo test -p therminal-integration-tests
//! ```

#![cfg(unix)]

use std::collections::HashMap;
use std::time::Duration;

use therminal_integration_tests::{DaemonHarness, wait_for_output};
use therminal_protocol::IpcResponse;
use therminal_protocol::daemon::IpcRequest;

/// Scenario 1 — Command capture.
///
/// Create a session, type `echo integration-marker-1kzt` into its default
/// pane, and poll the grid until that literal string appears. Exercises
/// `CreateSession` → `SendKeys` → `CapturePane` round-trip and confirms
/// that PTY output actually flows through the grid.
#[tokio::test]
async fn command_capture_echo_appears_in_grid() {
    let harness = DaemonHarness::spawn()
        .await
        .expect("daemon harness should spawn");
    let (_session_id, pane_id) = harness
        .create_session_with_pane(Some("echo-test"))
        .await
        .expect("create session");

    // Give the shell a moment to print its initial prompt — otherwise the
    // first bytes we write land in the pre-prompt buffer and the echo
    // output races with the prompt paint.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cmd = b"echo integration-marker-1kzt\n";
    let resp = harness
        .client()
        .send_request(IpcRequest::SendKeys {
            pane_id,
            keys: cmd.to_vec(),
        })
        .await
        .expect("send_keys should succeed");
    assert!(
        matches!(resp, IpcResponse::KeysSent { .. }),
        "expected KeysSent, got {resp:?}"
    );

    let text = wait_for_output(harness.client(), pane_id, Duration::from_secs(5), |grid| {
        grid.contains("integration-marker-1kzt")
    })
    .await
    .expect("echo output should appear in grid");
    assert!(
        text.contains("integration-marker-1kzt"),
        "grid text missing the marker: {text}"
    );
}

/// Scenario 2 — Pane splits.
///
/// Split the default pane and assert that `ListPanes` returns two panes
/// for the session with matching session membership. This catches
/// regressions in `SplitPane` and in the split-aware geometry tracking
/// that `ListPanes` reports.
#[tokio::test]
async fn split_pane_appears_in_list_panes() {
    let harness = DaemonHarness::spawn()
        .await
        .expect("daemon harness should spawn");
    let (session_id, pane_id) = harness
        .create_session_with_pane(Some("split-test"))
        .await
        .expect("create session");

    let resp = harness
        .client()
        .send_request(IpcRequest::SplitPane {
            pane_id,
            horizontal: true,
        })
        .await
        .expect("split_pane should succeed");
    let new_pane_id = match resp {
        IpcResponse::PaneSplit { new_pane_id } => new_pane_id,
        other => panic!("expected PaneSplit, got {other:?}"),
    };
    assert_ne!(
        new_pane_id, pane_id,
        "split should produce a distinct pane id"
    );

    let panes = harness
        .list_panes()
        .await
        .expect("list_panes should succeed");
    let in_session: Vec<_> = panes
        .iter()
        .filter(|p| p.session_id == session_id)
        .collect();
    assert_eq!(
        in_session.len(),
        2,
        "expected 2 panes after split in session {session_id}, got {in_session:#?}"
    );
    let ids: Vec<u64> = in_session.iter().map(|p| p.pane_id).collect();
    assert!(
        ids.contains(&pane_id),
        "original pane {pane_id} missing from list: {ids:?}"
    );
    assert!(
        ids.contains(&new_pane_id),
        "new pane {new_pane_id} missing from list: {ids:?}"
    );
    // Geometry sanity: both panes report non-zero dimensions.
    for p in &in_session {
        assert!(p.cols > 0, "pane {} has zero cols", p.pane_id);
        assert!(p.rows > 0, "pane {} has zero rows", p.pane_id);
    }
}

/// Scenario 3 — Pane tagging round-trip.
///
/// Tag the default pane via `TagPane`, list panes, and assert the tag
/// reappears in the pane's `tags` map. Exercises the opaque per-pane
/// metadata bag (tn-bbvf) end-to-end through the IPC layer.
#[tokio::test]
async fn tag_pane_persists_in_list_panes() {
    let harness = DaemonHarness::spawn()
        .await
        .expect("daemon harness should spawn");
    let (session_id, pane_id) = harness
        .create_session_with_pane(Some("tag-test"))
        .await
        .expect("create session");

    let mut tags = HashMap::new();
    tags.insert("issue".to_string(), "tn-1kzt".to_string());
    tags.insert("worker".to_string(), "claude-code-1".to_string());

    let resp = harness
        .client()
        .send_request(IpcRequest::TagPane {
            pane_id,
            tags: tags.clone(),
        })
        .await
        .expect("tag_pane should succeed");
    match resp {
        IpcResponse::PaneTagged { tags: merged, .. } => {
            assert_eq!(merged.get("issue").map(String::as_str), Some("tn-1kzt"));
            assert_eq!(
                merged.get("worker").map(String::as_str),
                Some("claude-code-1")
            );
        }
        other => panic!("expected PaneTagged, got {other:?}"),
    }

    // Round-trip via ListPanes — this is the surface the CLI and GUI
    // actually read, so a tag that only shows up in `PaneTagged` but not
    // in `ListPanes` would be invisible to real callers.
    let panes = harness
        .list_panes()
        .await
        .expect("list_panes should succeed");
    let pane = panes
        .iter()
        .find(|p| p.pane_id == pane_id && p.session_id == session_id)
        .expect("tagged pane should be in list");
    assert_eq!(
        pane.tags.get("issue").map(String::as_str),
        Some("tn-1kzt"),
        "tag 'issue' did not round-trip through ListPanes: {:?}",
        pane.tags
    );
    assert_eq!(
        pane.tags.get("worker").map(String::as_str),
        Some("claude-code-1"),
        "tag 'worker' did not round-trip through ListPanes: {:?}",
        pane.tags
    );
}

/// Scenario 4 — Agent detection has no false positives on benign processes.
///
/// Spawn `sleep 10` inside a pane and assert that `ListAgents` returns
/// zero agents immediately after. The process detector runs on a 3s
/// ticker so we only need to verify the steady state — any non-zero
/// result here would mean the classifier is over-eager.
///
/// This is a regression guard for the per-pane agent classification in
/// `process_detector_task`: a false positive on `sleep` would cascade
/// into the GUI's agent status bar, the capacity cache, and the
/// `therminal://agents/events` MCP resource.
#[tokio::test]
async fn sleep_does_not_register_as_agent() {
    let harness = DaemonHarness::spawn()
        .await
        .expect("daemon harness should spawn");
    let (_session_id, pane_id) = harness
        .create_session_with_pane(Some("agent-fp-test"))
        .await
        .expect("create session");

    // Let the shell print its initial prompt first.
    tokio::time::sleep(Duration::from_millis(300)).await;

    harness
        .client()
        .send_request(IpcRequest::SendKeys {
            pane_id,
            keys: b"sleep 10\n".to_vec(),
        })
        .await
        .expect("send_keys should succeed");

    // Briefly wait so that the shell has spawned the sleep child and the
    // process-detector ticker has had a chance to observe it. The ticker
    // runs at 3s intervals (see process_detector_task.rs) — we check
    // after a single tick plus a small buffer to avoid racing the very
    // first tick.
    tokio::time::sleep(Duration::from_millis(3500)).await;

    let resp = harness
        .client()
        .send_request(IpcRequest::ListAgents)
        .await
        .expect("list_agents should succeed");
    match resp {
        IpcResponse::Agents { agents } => {
            assert!(
                agents.is_empty(),
                "sleep should not register as an agent, got {agents:#?}"
            );
        }
        other => panic!("expected Agents, got {other:?}"),
    }
}
