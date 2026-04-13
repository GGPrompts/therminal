//! PTY integration tests for the session manager.
//!
//! These tests spawn real PTYs and verify the full lifecycle:
//! create, attach, write, capture, and destroy.
//!
//! Each test uses polling helpers (`wait_for_prompt`, `wait_for_output`)
//! instead of fixed `thread::sleep` durations, making them reliable in
//! both fast local builds and slow CI environments.
//!
//! Tests are skipped automatically when `/dev/ptmx` is unavailable
//! (e.g. Docker containers without PTY support).

use std::thread;
use std::time::{Duration, Instant};

use therminal_daemon::session::SessionManager;
use therminal_protocol::daemon::DaemonEvent;
use tokio::sync::broadcast;

/// Maximum time to wait for shell prompt or command output before failing.
/// Set generously because all 11 tests run in parallel, each spawning a
/// real PTY, and CI runners may be slow under load.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Interval between polling attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn make_manager() -> (SessionManager, broadcast::Receiver<DaemonEvent>) {
    let (tx, rx) = broadcast::channel(256);
    (SessionManager::new(tx), rx)
}

/// Helper: extract visible text from a pane snapshot grid, trimming trailing spaces.
fn snapshot_text(grid: &[Vec<(char, bool)>]) -> String {
    grid.iter()
        .map(|row| {
            let s: String = row.iter().map(|(c, _)| c).collect();
            s.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Skip the calling test if `/dev/ptmx` is not available (CI without PTY
/// support). Expands to an early `return` in the caller so the test passes
/// as a no-op instead of failing with a confusing PTY error.
macro_rules! require_pty {
    () => {
        if !std::path::Path::new("/dev/ptmx").exists() {
            eprintln!("SKIP: /dev/ptmx not available, skipping PTY test");
            return;
        }
    };
}

/// Poll `capture_pane` until at least one non-blank line appears in the grid,
/// indicating the shell has started and emitted a prompt or initial output.
///
/// Panics if the timeout is reached without seeing any content.
fn wait_for_prompt(mgr: &SessionManager, pane_id: therminal_protocol::PaneId) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        if let Ok(snap) = mgr.capture_pane(pane_id) {
            let text = snapshot_text(&snap.grid);
            if text.lines().any(|line| !line.is_empty()) {
                return;
            }
        }
        if Instant::now() >= deadline {
            let snap = mgr.capture_pane(pane_id).ok();
            let text = snap
                .as_ref()
                .map(|s| snapshot_text(&s.grid))
                .unwrap_or_default();
            panic!(
                "timed out waiting for shell prompt ({}s). Grid contents:\n{}",
                POLL_TIMEOUT.as_secs(),
                text
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Poll `capture_pane` until the grid text contains the given `needle`.
///
/// Panics if the timeout is reached without finding the needle.
fn wait_for_output(mgr: &SessionManager, pane_id: therminal_protocol::PaneId, needle: &str) {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        if let Ok(snap) = mgr.capture_pane(pane_id) {
            let text = snapshot_text(&snap.grid);
            if text.contains(needle) {
                return;
            }
        }
        if Instant::now() >= deadline {
            let snap = mgr.capture_pane(pane_id).ok();
            let text = snap
                .as_ref()
                .map(|s| snapshot_text(&s.grid))
                .unwrap_or_default();
            panic!(
                "timed out waiting for '{needle}' in pane output ({}s). Grid contents:\n{text}",
                POLL_TIMEOUT.as_secs(),
            );
        }
        thread::sleep(POLL_INTERVAL);
    }
}

// ── Test: create session -> verify shell is running ──────────────────────

#[test]
fn create_session_shell_running() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr
        .create_session(Some("pty-test".into()))
        .expect("failed to create session");

    assert_eq!(mgr.session_count(), 1);
    assert!(mgr.list_sessions().contains(&session_id));

    let info = mgr.get_session_info(session_id).unwrap();
    assert_eq!(info.1.as_deref(), Some("pty-test"));
    assert!(info.2 > 0, "created_at should be non-zero");

    // Wait for the shell to actually start and produce output.
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;
    wait_for_prompt(&mgr, pane_id);

    // The session should still exist (shell didn't crash immediately)
    assert_eq!(mgr.session_count(), 1);

    mgr.shutdown();
}

// ── Test: attach -> snapshot has non-empty grid ──────────────────────────

#[test]
fn attach_snapshot_has_content() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr
        .create_session(Some("attach-test".into()))
        .expect("failed to create session");

    // Get pane_id and poll until the shell prompt appears.
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;
    wait_for_prompt(&mgr, pane_id);

    let snapshot = mgr
        .attach(session_id)
        .expect("attach should return a snapshot");

    assert_eq!(snapshot.session_id, session_id);
    assert!(!snapshot.panes.is_empty(), "should have at least one pane");

    let pane = &snapshot.panes[0];
    assert_eq!(pane.cols, 80);
    assert_eq!(pane.rows, 24);

    // The grid should have some non-space content (shell prompt)
    let text = snapshot_text(&pane.grid);
    let non_empty = text.lines().any(|line| !line.is_empty());
    assert!(
        non_empty,
        "grid should contain non-empty content from shell prompt, got:\n{text}"
    );

    mgr.shutdown();
}

// ── Test: write to pane -> output appears in term state ──────────────────

#[test]
fn write_to_pane_echo_output() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");

    // Wait for shell prompt before sending commands
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;
    wait_for_prompt(&mgr, pane_id);

    // Send an echo command. Use a unique marker so we can find it.
    let marker = "THERMINAL_PTY_TEST_42";
    let cmd = format!("echo {marker}\n");
    mgr.write_to_pane(session_id, pane_id, cmd.as_bytes())
        .expect("write_to_pane should succeed");

    // Poll until the marker appears in the grid
    wait_for_output(&mgr, pane_id, marker);

    mgr.shutdown();
}

// ── Test: destroy session -> PTY cleanup ─────────────────────────────────

#[test]
fn destroy_session_cleans_up() {
    require_pty!();
    let (mut mgr, mut rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");

    assert_eq!(mgr.session_count(), 1);

    // Verify we got the creation event
    let mut saw_created = false;
    while let Ok(evt) = rx.try_recv() {
        if matches!(evt, DaemonEvent::SessionCreated { .. }) {
            saw_created = true;
        }
    }
    assert!(saw_created, "should have received SessionCreated event");

    // Destroy the session
    assert!(mgr.destroy_session(session_id));
    assert_eq!(mgr.session_count(), 0);

    // Verify we got the destruction event
    let mut saw_destroyed = false;
    while let Ok(evt) = rx.try_recv() {
        if matches!(evt, DaemonEvent::SessionDestroyed { .. }) {
            saw_destroyed = true;
        }
    }
    assert!(saw_destroyed, "should have received SessionDestroyed event");

    // Session should no longer be accessible
    assert!(mgr.attach(session_id).is_none());
    assert!(mgr.get_session_info(session_id).is_none());

    // Give reader thread time to notice EOF and exit
    thread::sleep(Duration::from_millis(300));
}

// ── Test: capture pane matches expected output ──────────────────────────

#[test]
fn capture_pane_content() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");

    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;
    wait_for_prompt(&mgr, pane_id);

    // Send a command that produces known output
    let cmd = "printf 'LINE_A\\nLINE_B\\nLINE_C\\n'\n";
    mgr.write_to_pane(session_id, pane_id, cmd.as_bytes())
        .expect("write should succeed");

    // Poll until all three lines appear
    wait_for_output(&mgr, pane_id, "LINE_A");
    wait_for_output(&mgr, pane_id, "LINE_B");
    wait_for_output(&mgr, pane_id, "LINE_C");

    // Verify snapshot metadata
    let snap = mgr.capture_pane(pane_id).expect("capture should succeed");
    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);

    mgr.shutdown();
}

// ── Test: split pane and kill pane ──────────────────────────────────────

#[test]
fn split_and_kill_pane() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");

    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;

    // Split creates a sibling pane
    let new_pane_id = mgr
        .split_pane(pane_id, false)
        .expect("split_pane should succeed");

    assert_ne!(pane_id, new_pane_id);

    // Both panes should be capturable
    mgr.capture_pane(pane_id)
        .expect("original pane should exist");
    mgr.capture_pane(new_pane_id)
        .expect("new pane should exist");

    // Kill the new pane
    mgr.kill_pane(new_pane_id)
        .expect("kill_pane should succeed");

    // Original pane should still work
    mgr.capture_pane(pane_id)
        .expect("original pane should survive");

    // New pane should be gone
    assert!(mgr.capture_pane(new_pane_id).is_err());

    mgr.shutdown();
}

// ── tn-ju04: split cascades resize across both children ────────────────

/// After `split_pane(source, horizontal=true)`, both the source pane and
/// the new pane should carry roughly half the source's pre-split cols
/// (minus the 1-cell separator) and the same row count.
///
/// Regression for tn-ju04 where the new pane was spawned at 80x24 and
/// the source pane kept its full width, so TUIs drew past the split.
#[test]
fn split_pane_horizontal_halves_cols_on_both_sides() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;

    // Force the source pane to a known non-default size so the halving
    // math is observable (default_cols=80 halved is still 40 so this
    // also serves as a guard against the halving being a no-op).
    mgr.resize_pane(pane_id, 120, 30)
        .expect("initial resize should succeed");

    let new_pane_id = mgr
        .split_pane(pane_id, true)
        .expect("horizontal split should succeed");

    let src_snap = mgr.capture_pane(pane_id).unwrap();
    let new_snap = mgr.capture_pane(new_pane_id).unwrap();

    // 120 cols - 1 separator = 119; halved: 59 + 60 (or 60 + 59).
    // Rows are unchanged by a horizontal split.
    assert_eq!(src_snap.rows, 30, "source rows");
    assert_eq!(new_snap.rows, 30, "new rows");
    assert!(
        src_snap.cols < 120,
        "source cols must shrink after split, got {}",
        src_snap.cols
    );
    assert!(
        new_snap.cols < 120,
        "new cols must be below full width, got {}",
        new_snap.cols
    );
    // Each half is at least 55 cells (approx 119/2 with some tolerance for rounding).
    assert!(
        src_snap.cols >= 55,
        "source cols too narrow: {}",
        src_snap.cols
    );
    assert!(
        new_snap.cols >= 55,
        "new cols too narrow: {}",
        new_snap.cols
    );

    mgr.shutdown();
}

/// After `split_pane(source, horizontal=false)` (stacked), rows are
/// halved and cols stay unchanged.
#[test]
fn split_pane_vertical_halves_rows_on_both_sides() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;

    mgr.resize_pane(pane_id, 100, 40)
        .expect("initial resize should succeed");

    let new_pane_id = mgr
        .split_pane(pane_id, false)
        .expect("vertical split should succeed");

    let src_snap = mgr.capture_pane(pane_id).unwrap();
    let new_snap = mgr.capture_pane(new_pane_id).unwrap();

    assert_eq!(src_snap.cols, 100, "source cols");
    assert_eq!(new_snap.cols, 100, "new cols");
    assert!(src_snap.rows < 40, "source rows must shrink");
    assert!(new_snap.rows < 40, "new rows must shrink");
    assert!(
        src_snap.rows >= 15,
        "source rows too short: {}",
        src_snap.rows
    );
    assert!(new_snap.rows >= 15, "new rows too short: {}", new_snap.rows);

    mgr.shutdown();
}

/// After `kill_pane(sibling)` on a horizontal split, the surviving
/// sibling should reclaim the dead pane's cols and end up wider than it
/// was before.
#[test]
fn kill_pane_cascades_resize_to_sibling() {
    use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection, WorkspaceInfo};

    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;

    mgr.resize_pane(pane_id, 120, 30).unwrap();

    let new_pane_id = mgr.split_pane(pane_id, true).unwrap();

    // Seed workspace_state with a layout the kill cascade can walk.
    // (The daemon side of `split_pane` already does this, but we set it
    // explicitly so the test is independent of that side effect.)
    mgr.set_workspace_state(
        session_id,
        vec![WorkspaceInfo {
            id: 1,
            name: "1".into(),
            order: 0,
            pane_ids: vec![pane_id, new_pane_id],
            focused_pane: Some(pane_id),
            layout: Some(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id }),
                second: Box::new(LayoutSnapshot::Leaf {
                    pane_id: new_pane_id,
                }),
            }),
        }],
        1,
    )
    .unwrap();

    let survivor_before = mgr.capture_pane(pane_id).unwrap().cols;
    mgr.kill_pane(new_pane_id).unwrap();
    let survivor_after = mgr.capture_pane(pane_id).unwrap().cols;

    assert!(
        survivor_after > survivor_before,
        "sibling did not grow on kill: before={} after={}",
        survivor_before,
        survivor_after
    );

    mgr.shutdown();
}

// ── Test: multiple sessions are independent ─────────────────────────────

#[test]
fn multiple_sessions_independent() {
    require_pty!();
    let (mut mgr, _rx) = make_manager();

    let s1 = mgr.create_session(Some("s1".into())).unwrap();
    let s2 = mgr.create_session(Some("s2".into())).unwrap();

    assert_eq!(mgr.session_count(), 2);
    assert_ne!(s1, s2);

    let p1 = mgr.attach(s1).unwrap().panes[0].pane_id;
    let p2 = mgr.attach(s2).unwrap().panes[0].pane_id;

    // Wait for both shells to be ready
    wait_for_prompt(&mgr, p1);
    wait_for_prompt(&mgr, p2);

    // Write different markers to each session
    mgr.write_to_pane(s1, p1, b"echo SESSION_ONE_MARKER\n")
        .unwrap();
    mgr.write_to_pane(s2, p2, b"echo SESSION_TWO_MARKER\n")
        .unwrap();

    // Poll until markers appear
    wait_for_output(&mgr, p1, "SESSION_ONE_MARKER");
    wait_for_output(&mgr, p2, "SESSION_TWO_MARKER");

    let text1 = snapshot_text(&mgr.capture_pane(p1).unwrap().grid);
    let text2 = snapshot_text(&mgr.capture_pane(p2).unwrap().grid);

    // Markers should not cross sessions
    assert!(
        !text1.contains("SESSION_TWO_MARKER"),
        "session 1 should not contain session 2's marker"
    );
    assert!(
        !text2.contains("SESSION_ONE_MARKER"),
        "session 2 should not contain session 1's marker"
    );

    // Destroy one, the other should be unaffected
    mgr.destroy_session(s1);
    assert_eq!(mgr.session_count(), 1);
    assert!(mgr.capture_pane(p2).is_ok());

    mgr.shutdown();
}

// ── tn-gln6 #1: harness OSC events reach the daemon-side sink ──────────
//
// Regression test for the "dropped in production" bug: the OSC handler
// registry was shared into pane interceptors, but the harness-event sink
// was not. Handlers parsed sequences and threw the events away. This
// test proves a full production path: real PTY -> shell echoes OSC 1341
// -> reader thread -> TherminalInterceptor -> OscHandlerRegistry::dispatch
// -> SessionManager-installed harness_event_tx -> test receiver.

#[test]
fn harness_osc_event_reaches_session_manager_sink() {
    use std::sync::Arc;
    use std::sync::mpsc;
    use therminal_terminal::{HarnessEvent, OscHandlerRegistry};

    require_pty!();
    let (mut mgr, _rx) = make_manager();

    // 1. Build a fresh registry, register a handler for OSC 1341 that
    //    emits a HarnessEvent carrying the payload chunk, and install
    //    the registry on the session manager.
    let registry = Arc::new(OscHandlerRegistry::new());
    registry
        .register(
            1341,
            "claude",
            Box::new(|params: &[&[u8]]| {
                let payload = params
                    .get(1)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("")
                    .to_string();
                Some(HarnessEvent {
                    kind: "claude.state".to_string(),
                    body: serde_json::json!({ "payload": payload }),
                })
            }),
        )
        .expect("register OSC 1341");
    mgr.set_osc_registry(Arc::clone(&registry));

    // 2. Install the harness-event sink BEFORE the first session spawns
    //    so the pane's interceptor picks it up at construction time.
    let (harness_tx, harness_rx) = mpsc::channel();
    mgr.set_harness_event_sink(harness_tx);

    // 3. Spawn a real PTY session and wait for the shell prompt.
    let session_id = mgr
        .create_session(Some("harness-osc-test".into()))
        .expect("create_session");
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;
    wait_for_prompt(&mgr, pane_id);

    // 4. Drive a synthetic OSC 1341 sequence through the shell. `printf`
    //    is universal across bash/zsh/sh. The sequence is:
    //      ESC ] 1341 ; state=tool_use BEL
    //    \033 = ESC, \007 = BEL.
    let cmd = "printf '\\033]1341;state=tool_use\\007'\n";
    mgr.write_to_pane(session_id, pane_id, cmd.as_bytes())
        .expect("write_to_pane");

    // 5. Poll the harness receiver for up to the standard timeout. The
    //    reader thread needs a moment to drain the PTY output and dispatch.
    let deadline = Instant::now() + POLL_TIMEOUT;
    let mut received: Option<therminal_terminal::TaggedHarnessEvent> = None;
    while Instant::now() < deadline {
        match harness_rx.recv_timeout(POLL_INTERVAL) {
            Ok(tagged) => {
                received = Some(tagged);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let tagged = received.expect(
        "expected TaggedHarnessEvent from OSC 1341 dispatch \u{2014} \
         the production-path sink is dropping events (tn-gln6 #1)",
    );
    assert_eq!(tagged.source_id, "claude");
    assert_eq!(tagged.event.kind, "claude.state");
    assert_eq!(
        tagged.event.body,
        serde_json::json!({ "payload": "state=tool_use" })
    );

    mgr.shutdown();
}
