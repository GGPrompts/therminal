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
use std::time::{Duration, Instant};

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
            cwd: None,
            startup_command: None,
            ratio: None,
            shell: None,
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

/// Scenario 5 — Pattern engine finalized-line dispatch.
///
/// Drop a minimal pattern pack into `$XDG_CONFIG_HOME/therminal/patterns/`
/// before the daemon starts, spawn a session, run a command whose output
/// matches the pattern, and assert the daemon's `QueryPatternStats` IPC
/// reports a non-zero dispatched-match count. Exercises the tn-86us
/// dispatch plumbing end-to-end: PTY reader → ANSI stripper → line
/// accumulator → `PatternEngine::process_finalized_line` → `EventBus`
/// publish → counter bump.
#[tokio::test]
async fn pattern_engine_dispatches_finalized_line_match() {
    let pack_toml = r#"
pack_name = "integration-86us"
pack_description = "tn-86us integration test pack"

[[pattern]]
name = "tn_marker"
scope = "finalized_line"
action = "emit_event"
match = "INTEGRATION_MARKER_86US_[0-9]+"
"#;

    let harness = DaemonHarness::spawn_with_setup(|config_dir| {
        let pack_dir = config_dir.join("therminal").join("patterns");
        std::fs::create_dir_all(&pack_dir)?;
        std::fs::write(pack_dir.join("integration-86us.toml"), pack_toml)?;
        Ok(())
    })
    .await
    .expect("daemon harness should spawn");

    // Patterns load at startup; confirm our pack actually made it in.
    let stats_resp = harness
        .client()
        .send_request(IpcRequest::QueryPatternStats)
        .await
        .expect("query_pattern_stats should succeed");
    match stats_resp {
        IpcResponse::PatternStats {
            total_matches_dispatched,
            total_loaded,
        } => {
            assert_eq!(total_matches_dispatched, 0, "baseline should be zero");
            assert!(
                total_loaded >= 1,
                "expected our pack to load at least 1 pattern, got total_loaded={total_loaded}"
            );
        }
        other => panic!("expected PatternStats, got {other:?}"),
    }

    let (_session_id, pane_id) = harness
        .create_session_with_pane(Some("pattern-test"))
        .await
        .expect("create session");

    // Let the shell print its initial prompt before we write.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Running `echo <marker>` produces one finalized line containing the
    // marker. The pattern is finalized_line-scoped and should match
    // exactly once per unique marker value we emit.
    let cmd = b"echo INTEGRATION_MARKER_86US_42\n";
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

    // Wait for the marker to surface in the grid so we know the shell
    // actually ran our command.
    let _ = wait_for_output(harness.client(), pane_id, Duration::from_secs(5), |grid| {
        grid.contains("INTEGRATION_MARKER_86US_42")
    })
    .await
    .expect("marker should appear in grid");

    // Poll the pattern stats until at least one dispatched match is
    // observed. The dispatch is synchronous on the PTY reader thread, so
    // this should become true within a few ticks of the line committing.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_total: u64 = 0;
    loop {
        let resp = harness
            .client()
            .send_request(IpcRequest::QueryPatternStats)
            .await
            .expect("query_pattern_stats should succeed");
        if let IpcResponse::PatternStats {
            total_matches_dispatched,
            ..
        } = resp
        {
            last_total = total_matches_dispatched;
            if total_matches_dispatched >= 1 {
                break;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "pattern match was never dispatched: last total_matches_dispatched={last_total}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        last_total >= 1,
        "expected at least one pattern match, got {last_total}"
    );
}

/// Scenario 6 -- Pattern engine hotspot-action dispatch (tn-f9cl).
#[tokio::test]
async fn pattern_engine_dispatches_hotspot_action_match() {
    let pack_toml = r#"
pack_name = "integration-f9cl"
pack_description = "tn-f9cl integration test pack (hotspot action)"

[[pattern]]
name = "f9cl_marker"
scope = "finalized_line"
action = "hotspot"
match = "HOTSPOT_MARKER_F9CL_(?P<num>[0-9]+)"

[pattern.hotspot]
on_click = "open_editor"
target = "/tmp/test-{num}.rs"
kind = "file"
"#;

    let harness = DaemonHarness::spawn_with_setup(|config_dir| {
        let pack_dir = config_dir.join("therminal").join("patterns");
        std::fs::create_dir_all(&pack_dir)?;
        std::fs::write(pack_dir.join("integration-f9cl.toml"), pack_toml)?;
        Ok(())
    })
    .await
    .expect("daemon harness should spawn");
    let stats_resp = harness
        .client()
        .send_request(IpcRequest::QueryPatternStats)
        .await
        .unwrap();
    match stats_resp {
        IpcResponse::PatternStats {
            total_matches_dispatched,
            total_loaded,
        } => {
            assert_eq!(total_matches_dispatched, 0);
            assert!(total_loaded >= 1);
        }
        other => panic!("expected PatternStats, got {other:?}"),
    }
    let (_session_id, pane_id) = harness
        .create_session_with_pane(Some("hotspot-test"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    harness
        .client()
        .send_request(IpcRequest::SendKeys {
            pane_id,
            keys: b"echo HOTSPOT_MARKER_F9CL_99\n".to_vec(),
        })
        .await
        .unwrap();
    let _ = wait_for_output(harness.client(), pane_id, Duration::from_secs(5), |g| {
        g.contains("HOTSPOT_MARKER_F9CL_99")
    })
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let IpcResponse::PatternStats {
            total_matches_dispatched,
            ..
        } = harness
            .client()
            .send_request(IpcRequest::QueryPatternStats)
            .await
            .unwrap()
        {
            if total_matches_dispatched >= 1 {
                break;
            }
        }
        if Instant::now() >= deadline {
            panic!("hotspot pattern match not dispatched");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
