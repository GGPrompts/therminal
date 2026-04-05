//! PTY integration tests for the session manager.
//!
//! These tests spawn real PTYs and verify the full lifecycle:
//! create, attach, write, capture, and destroy.
//!
//! Marked `#[ignore]` because they require a real TTY / PTY and may
//! not work in all CI environments (e.g., Docker containers without
//! `/dev/ptmx`). Run explicitly with:
//!
//! ```sh
//! cargo test -p therminal-daemon --test session_pty -- --ignored
//! ```

use std::thread;
use std::time::Duration;

use therminal_daemon::session::SessionManager;
use therminal_protocol::daemon::DaemonEvent;
use tokio::sync::broadcast;

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

// ── Test: create session → verify shell is running ──────────────────────

#[test]
#[ignore] // Requires a real PTY (CI may not have /dev/ptmx)
fn create_session_shell_running() {
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr
        .create_session(Some("pty-test".into()))
        .expect("failed to create session");

    assert_eq!(mgr.session_count(), 1);
    assert!(mgr.list_sessions().contains(&session_id));

    let info = mgr.get_session_info(session_id).unwrap();
    assert_eq!(info.1.as_deref(), Some("pty-test"));
    assert!(info.2 > 0, "created_at should be non-zero");

    // Give the shell a moment to start and emit its prompt
    thread::sleep(Duration::from_millis(500));

    // The session should still exist (shell didn't crash immediately)
    assert_eq!(mgr.session_count(), 1);

    mgr.shutdown();
}

// ── Test: attach → snapshot has non-empty grid ──────────────────────────

#[test]
#[ignore] // Requires a real PTY
fn attach_snapshot_has_content() {
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr
        .create_session(Some("attach-test".into()))
        .expect("failed to create session");

    // Wait for shell to produce a prompt
    thread::sleep(Duration::from_millis(800));

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

// ── Test: write to pane → output appears in term state ──────────────────

#[test]
#[ignore] // Requires a real PTY
fn write_to_pane_echo_output() {
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");

    // Wait for shell prompt
    thread::sleep(Duration::from_millis(800));

    // Get the pane ID from the snapshot
    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;

    // Send an echo command. Use a unique marker so we can find it.
    let marker = "THERMINAL_PTY_TEST_42";
    let cmd = format!("echo {marker}\n");
    mgr.write_to_pane(session_id, pane_id, cmd.as_bytes())
        .expect("write_to_pane should succeed");

    // Wait for the echo output to be processed by the reader thread
    thread::sleep(Duration::from_millis(800));

    // Capture pane and check for the marker in the visible grid
    let snap = mgr.capture_pane(pane_id).expect("capture_pane should work");
    let text = snapshot_text(&snap.grid);

    assert!(
        text.contains(marker),
        "expected marker '{marker}' in pane output, got:\n{text}"
    );

    mgr.shutdown();
}

// ── Test: destroy session → PTY cleanup ─────────────────────────────────

#[test]
#[ignore] // Requires a real PTY
fn destroy_session_cleans_up() {
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
#[ignore] // Requires a real PTY
fn capture_pane_content() {
    let (mut mgr, _rx) = make_manager();

    let session_id = mgr.create_session(None).expect("failed to create session");

    thread::sleep(Duration::from_millis(800));

    let pane_id = mgr.attach(session_id).unwrap().panes[0].pane_id;

    // Send a command that produces known output
    let cmd = "printf 'LINE_A\\nLINE_B\\nLINE_C\\n'\n";
    mgr.write_to_pane(session_id, pane_id, cmd.as_bytes())
        .expect("write should succeed");

    thread::sleep(Duration::from_millis(800));

    let snap = mgr.capture_pane(pane_id).expect("capture should succeed");
    let text = snapshot_text(&snap.grid);

    assert!(
        text.contains("LINE_A"),
        "expected LINE_A in output, got:\n{text}"
    );
    assert!(
        text.contains("LINE_B"),
        "expected LINE_B in output, got:\n{text}"
    );
    assert!(
        text.contains("LINE_C"),
        "expected LINE_C in output, got:\n{text}"
    );

    // Verify snapshot metadata
    assert_eq!(snap.cols, 80);
    assert_eq!(snap.rows, 24);

    mgr.shutdown();
}

// ── Test: split pane and kill pane ──────────────────────────────────────

#[test]
#[ignore] // Requires a real PTY
fn split_and_kill_pane() {
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

// ── Test: multiple sessions are independent ─────────────────────────────

#[test]
#[ignore] // Requires a real PTY
fn multiple_sessions_independent() {
    let (mut mgr, _rx) = make_manager();

    let s1 = mgr.create_session(Some("s1".into())).unwrap();
    let s2 = mgr.create_session(Some("s2".into())).unwrap();

    assert_eq!(mgr.session_count(), 2);
    assert_ne!(s1, s2);

    thread::sleep(Duration::from_millis(500));

    let p1 = mgr.attach(s1).unwrap().panes[0].pane_id;
    let p2 = mgr.attach(s2).unwrap().panes[0].pane_id;

    // Write different markers to each session
    mgr.write_to_pane(s1, p1, b"echo SESSION_ONE_MARKER\n")
        .unwrap();
    mgr.write_to_pane(s2, p2, b"echo SESSION_TWO_MARKER\n")
        .unwrap();

    thread::sleep(Duration::from_millis(800));

    let text1 = snapshot_text(&mgr.capture_pane(p1).unwrap().grid);
    let text2 = snapshot_text(&mgr.capture_pane(p2).unwrap().grid);

    assert!(
        text1.contains("SESSION_ONE_MARKER"),
        "session 1 should have its marker"
    );
    assert!(
        text2.contains("SESSION_TWO_MARKER"),
        "session 2 should have its marker"
    );

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
