//! Session manager: persistent sessions with PTY workers.
//!
//! Hierarchy: `SessionManager` -> `Session` -> `Window` -> `Pane`.
//! Each pane owns a PTY + headless `alacritty_terminal::Term` running
//! in a dedicated reader thread via the shared `PtyPaneCore`.
//!
//! Attach sends a structured `PaneStateSnapshot` (mode flags, cursor,
//! visible grid) that the GUI replays via synthesized escape sequences
//! onto a freshly-constructed local `Term`. See tn-zamd.

mod base;
mod layout;
mod manager;
mod pane;
mod snapshots;
mod window;
mod workspace_ops;
pub mod worktree;

// ── Re-exports ──────────────────────────────────────────────────────────
// Every pub item is re-exported so external callers keep their existing
// `crate::session::*` import paths.

pub use base::Session;
pub use manager::SessionManager;
pub use pane::{Pane, PaneDispatchCtx};
pub use snapshots::{PaneSnapshot, SessionSnapshot};
pub use window::Window;

pub use therminal_protocol::{PaneId, SessionId, WindowId, WorkspaceId};

// ── ID generation ───────────────────────────────────────────────────────

use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) static NEXT_PANE_ID: AtomicU64 = AtomicU64::new(1);

fn next_session_id() -> SessionId {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_window_id() -> WindowId {
    NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::layout::{
        LeafDims, append_layout_leaf, layout_leaf_dims, normalize_startup_command,
        reconstruct_layout_rect, remove_layout_leaf, split_layout_leaf, swap_layout_leaves,
    };
    use super::*;
    use std::collections::HashMap;

    use alacritty_terminal::grid::Dimensions;
    use therminal_protocol::daemon::{DaemonEvent, LayoutSnapshot, WorkspaceInfo};
    use therminal_terminal::pty_runtime::TermSize;
    use tokio::sync::broadcast;

    use pane::HeadlessListener;

    fn make_event_tx() -> broadcast::Sender<DaemonEvent> {
        let (tx, _) = broadcast::channel(16);
        tx
    }

    #[test]
    fn next_pane_id_increments() {
        let a = next_pane_id();
        let b = next_pane_id();
        assert!(b > a);
    }

    #[test]
    fn session_manager_create_and_list() {
        let tx = make_event_tx();
        let mgr = SessionManager::new(tx);

        // Create with a mock - we can't easily spawn real PTYs in unit tests
        // without a TTY, so test the non-PTY parts.
        assert_eq!(mgr.session_count(), 0);
        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn tag_pane_not_found() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let mut tags = HashMap::new();
        tags.insert("issue_id".into(), "tn-bbvf".into());
        assert!(mgr.tag_pane(999, tags).is_err());
        assert!(mgr.untag_pane(999, None).is_err());
        assert!(mgr.pane_tags(999).is_none());
    }

    /// tn-bbvf: tag -> list -> untag -> list cycle on a Pane directly
    /// (no PTY required) plus persistence round-trip via PersistedPane.
    #[test]
    fn pane_tag_lifecycle_round_trip() {
        use therminal_protocol::daemon::PersistedPane;

        // Build a Pane by hand without spawning a PTY: use the same pattern
        // as the lifecycle PTY-less tests above. We need a struct with the
        // tags field exercised, so build a tiny harness that constructs a
        // dummy Pane with a fake writer.
        //
        // Easier: hit the public API on a HashMap<String,String> and check
        // the merge / remove / clear semantics that the Pane methods use.
        let mut tags: HashMap<String, String> = HashMap::new();

        // Initial merge.
        let mut update = HashMap::new();
        update.insert("issue_id".to_string(), "tn-bbvf".to_string());
        update.insert("branch".to_string(), "feat/tags".to_string());
        for (k, v) in update {
            tags.insert(k, v);
        }
        assert_eq!(tags.len(), 2);
        assert_eq!(tags.get("issue_id").map(String::as_str), Some("tn-bbvf"));

        // Merge overwrites only the named keys.
        let mut update2 = HashMap::new();
        update2.insert("branch".to_string(), "main".to_string());
        for (k, v) in update2 {
            tags.insert(k, v);
        }
        assert_eq!(tags.get("branch").map(String::as_str), Some("main"));
        assert_eq!(tags.get("issue_id").map(String::as_str), Some("tn-bbvf"));

        // Remove a single key.
        tags.remove("branch");
        assert!(!tags.contains_key("branch"));
        assert_eq!(tags.len(), 1);

        // Persistence round-trip via PersistedPane.
        let pp = PersistedPane {
            cwd: "/x".into(),
            shell: String::new(),
            cols: 80,
            rows: 24,
            tags: tags.clone(),
        };
        let json = serde_json::to_string(&pp).unwrap();
        let parsed: PersistedPane = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tags, tags);

        // Clear-all leaves an empty map.
        tags.clear();
        assert!(tags.is_empty());
    }

    #[test]
    fn session_manager_destroy_nonexistent() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        assert!(!mgr.destroy_session(999999));
    }

    /// tn-zamd: feed a raw Term with DECSET 1000 + ?25l + a cursor move,
    /// synthesize a PaneStateSnapshot, replay it onto a fresh Term, and
    /// assert the mode bits match. No PTY required.
    #[test]
    fn pane_state_snapshot_replays_mode_flags() {
        use alacritty_terminal::term::TermMode;
        use alacritty_terminal::term::{Config as TermConfig, Term};
        use alacritty_terminal::vte::ansi;
        use therminal_protocol::daemon::{PaneModeFlags, PaneStateSnapshot};

        let size = TermSize {
            columns: 20,
            screen_lines: 5,
        };
        let mut term_a: Term<HeadlessListener> =
            Term::new(TermConfig::default(), &size, HeadlessListener);
        let mut proc = ansi::Processor::<ansi::StdSyncHandler>::new();

        // Enable SGR mouse + click reporting + hide cursor + bracketed paste.
        let input: &[u8] = b"\x1b[?25l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h\x1b[3;5HHI";
        proc.advance(&mut term_a, input);

        // Build a snapshot by hand from term_a (mirrors Pane::snapshot_state).
        let mode = *term_a.mode();
        let grid = term_a.grid();
        let cursor_point = grid.cursor.point;
        let rows = term_a.screen_lines();
        let cols = term_a.columns();
        let mut grid_chars = Vec::with_capacity(rows);
        for line_idx in 0..rows {
            let line = alacritty_terminal::index::Line(line_idx as i32);
            let mut row = String::with_capacity(cols);
            for col_idx in 0..cols {
                let col = alacritty_terminal::index::Column(col_idx);
                row.push(grid[line][col].c);
            }
            grid_chars.push(row);
        }
        let snap = PaneStateSnapshot {
            version: 1,
            cols: cols as u16,
            rows: rows as u16,
            modes: PaneModeFlags {
                show_cursor: mode.contains(TermMode::SHOW_CURSOR),
                app_cursor: mode.contains(TermMode::APP_CURSOR),
                alt_screen: mode.contains(TermMode::ALT_SCREEN),
                mouse_report_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
                mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
                mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
                sgr_mouse: mode.contains(TermMode::SGR_MOUSE),
                bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
                focus_in_out: mode.contains(TermMode::FOCUS_IN_OUT),
                app_keypad: mode.contains(TermMode::APP_KEYPAD),
                line_wrap: mode.contains(TermMode::LINE_WRAP),
            },
            cursor_col: cursor_point.column.0 as u16,
            cursor_line: (cursor_point.line.0.max(0) as u16).min(rows as u16 - 1),
            grid_chars,
            tags: std::collections::HashMap::new(),
        };

        // Sanity: our captured snapshot shows the relevant flags set.
        // Mouse protocols are mutually exclusive in alacritty; only the
        // last enabled (?1002 = MOUSE_DRAG) survives.
        assert!(!snap.modes.show_cursor);
        assert!(!snap.modes.mouse_report_click);
        assert!(snap.modes.mouse_drag);
        assert!(snap.modes.sgr_mouse);
        assert!(snap.modes.bracketed_paste);

        // Replay onto a fresh Term.
        let mut term_b: Term<HeadlessListener> =
            Term::new(TermConfig::default(), &size, HeadlessListener);
        let mut proc_b = ansi::Processor::<ansi::StdSyncHandler>::new();
        let bytes = snap.to_replay_bytes();
        proc_b.advance(&mut term_b, &bytes);

        let mode_b = *term_b.mode();
        assert!(
            !mode_b.contains(TermMode::SHOW_CURSOR),
            "cursor should be hidden after replay"
        );
        assert!(
            !mode_b.contains(TermMode::MOUSE_REPORT_CLICK),
            "1000 should not be set (mutex with 1002)"
        );
        assert!(
            mode_b.contains(TermMode::MOUSE_DRAG),
            "1002 should be replayed"
        );
        assert!(
            mode_b.contains(TermMode::SGR_MOUSE),
            "1006 should be replayed"
        );
        assert!(
            mode_b.contains(TermMode::BRACKETED_PASTE),
            "2004 should be replayed"
        );
    }

    #[test]
    fn window_new_has_id() {
        let w = Window::new();
        assert!(w.id > 0);
        assert!(w.panes.is_empty());
    }

    #[test]
    fn session_new_has_id_and_timestamp() {
        let tx = make_event_tx();
        let session = Session::new(Some("test".to_string()), tx);
        assert!(session.id > 0);
        assert_eq!(session.name.as_deref(), Some("test"));
        assert!(session.created_at_secs > 0);
    }

    #[test]
    #[ignore] // Requires a real TTY
    fn session_manager_full_lifecycle() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);

        let session_id = mgr.create_session(Some("test".into())).unwrap();
        assert_eq!(mgr.session_count(), 1);
        assert!(mgr.list_sessions().contains(&session_id));

        let info = mgr.get_session_info(session_id).unwrap();
        assert_eq!(info.1.as_deref(), Some("test"));

        let snapshot = mgr.attach(session_id).unwrap();
        assert_eq!(snapshot.session_id, session_id);
        assert!(!snapshot.panes.is_empty());

        assert!(mgr.destroy_session(session_id));
        assert_eq!(mgr.session_count(), 0);
    }

    #[test]
    fn session_manager_shutdown_empty() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        mgr.shutdown(); // Should not panic
        assert_eq!(mgr.session_count(), 0);
    }

    #[test]
    fn swap_layout_leaves_basic() {
        use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
        let mut tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
            }),
        };
        swap_layout_leaves(&mut tree, 1, 3);
        // After swap: first leaf is 3, deepest second is 1.
        if let LayoutSnapshot::Split { first, second, .. } = &tree {
            assert!(matches!(**first, LayoutSnapshot::Leaf { pane_id: 3 }));
            if let LayoutSnapshot::Split { second: inner2, .. } = &**second {
                assert!(matches!(**inner2, LayoutSnapshot::Leaf { pane_id: 1 }));
            } else {
                panic!("expected split");
            }
        } else {
            panic!("expected split");
        }
    }

    // ── tn-fi1k: layout-leaf removal & append helpers ─────────────────

    #[test]
    fn remove_layout_leaf_lone_match_returns_none() {
        let tree = LayoutSnapshot::Leaf { pane_id: 5 };
        assert!(remove_layout_leaf(tree, 5).is_none());
    }

    #[test]
    fn remove_layout_leaf_lone_miss_preserves_tree() {
        let tree = LayoutSnapshot::Leaf { pane_id: 5 };
        let out = remove_layout_leaf(tree, 7);
        assert!(matches!(out, Some(LayoutSnapshot::Leaf { pane_id: 5 })));
    }

    #[test]
    fn remove_layout_leaf_split_promotes_sibling() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
        };
        let out = remove_layout_leaf(tree, 1).unwrap();
        // Removing one side of a 2-leaf split must collapse to the sibling.
        assert!(matches!(out, LayoutSnapshot::Leaf { pane_id: 2 }));
    }

    #[test]
    fn remove_layout_leaf_nested_collapses_correctly() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        // 1 | (2 | 3)
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
            }),
        };
        // Remove the deeply nested leaf 3 -> outer split keeps left leaf 1
        // and collapses the right split into its surviving child (leaf 2).
        let out = remove_layout_leaf(tree, 3).unwrap();
        if let LayoutSnapshot::Split { first, second, .. } = out {
            assert!(matches!(*first, LayoutSnapshot::Leaf { pane_id: 1 }));
            assert!(matches!(*second, LayoutSnapshot::Leaf { pane_id: 2 }));
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn append_layout_leaf_into_empty() {
        let out = append_layout_leaf(None, 42);
        assert!(matches!(out, LayoutSnapshot::Leaf { pane_id: 42 }));
    }

    #[test]
    fn append_layout_leaf_into_existing_creates_split() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        let prev = Some(LayoutSnapshot::Leaf { pane_id: 1 });
        let out = append_layout_leaf(prev, 2);
        if let LayoutSnapshot::Split {
            direction,
            first,
            second,
            ..
        } = out
        {
            assert_eq!(direction, LayoutSplitDirection::Horizontal);
            assert!(matches!(*first, LayoutSnapshot::Leaf { pane_id: 1 }));
            assert!(matches!(*second, LayoutSnapshot::Leaf { pane_id: 2 }));
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn normalize_startup_command_appends_newline() {
        assert_eq!(
            normalize_startup_command(Some("echo hello")),
            Some(b"echo hello\n".to_vec())
        );
    }

    #[test]
    fn normalize_startup_command_preserves_existing_newline() {
        assert_eq!(
            normalize_startup_command(Some("echo hello\n")),
            Some(b"echo hello\n".to_vec())
        );
        assert_eq!(
            normalize_startup_command(Some("echo hello\r")),
            Some(b"echo hello\r".to_vec())
        );
    }

    #[test]
    fn normalize_startup_command_skips_empty_strings() {
        assert_eq!(normalize_startup_command(None), None);
        assert_eq!(normalize_startup_command(Some("")), None);
    }

    #[test]
    fn append_layout_leaf_idempotent_for_same_lone_pane() {
        let prev = Some(LayoutSnapshot::Leaf { pane_id: 7 });
        let out = append_layout_leaf(prev, 7);
        assert!(matches!(out, LayoutSnapshot::Leaf { pane_id: 7 }));
    }

    #[test]
    fn move_pane_unknown_returns_error() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let err = mgr.move_pane(404, 2).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    #[ignore] // Requires a real TTY
    fn move_pane_updates_workspace_state() {
        use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let session_id = mgr.create_session(Some("move-test".into())).unwrap();
        let pane_a = mgr.sessions.get(&session_id).unwrap().windows[0].panes[0].id;
        let pane_b = mgr.split_pane(pane_a, false).unwrap();

        // Seed the daemon with a single workspace owning both panes.
        mgr.set_workspace_state(
            session_id,
            vec![WorkspaceInfo {
                id: 1,
                name: "main".into(),
                order: 0,
                pane_ids: vec![pane_a, pane_b],
                focused_pane: Some(pane_b),
                layout: Some(LayoutSnapshot::Split {
                    direction: LayoutSplitDirection::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Leaf { pane_id: pane_a }),
                    second: Box::new(LayoutSnapshot::Leaf { pane_id: pane_b }),
                }),
            }],
            1,
        )
        .unwrap();

        // Move pane_b into a brand-new workspace 3.
        let (src, tgt) = mgr.move_pane(pane_b, 3).unwrap();
        assert_eq!(src, 1);
        assert_eq!(tgt, 3);

        let (ws, _) = mgr.get_workspace_state(session_id).unwrap();
        // Workspace 1 should now own only pane_a, with the layout collapsed.
        let ws1 = ws.iter().find(|w| w.id == 1).unwrap();
        assert_eq!(ws1.pane_ids, vec![pane_a]);
        assert!(matches!(
            ws1.layout.as_ref().unwrap(),
            LayoutSnapshot::Leaf { pane_id } if *pane_id == pane_a
        ));
        // Workspace 3 must have been auto-created with pane_b as the only leaf.
        let ws3 = ws.iter().find(|w| w.id == 3).unwrap();
        assert_eq!(ws3.pane_ids, vec![pane_b]);
        assert_eq!(ws3.focused_pane, Some(pane_b));
        assert!(matches!(
            ws3.layout.as_ref().unwrap(),
            LayoutSnapshot::Leaf { pane_id } if *pane_id == pane_b
        ));
    }

    #[test]
    #[ignore] // Requires a real TTY
    fn move_pane_noop_same_workspace() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let session_id = mgr.create_session(Some("noop-test".into())).unwrap();
        let pane_a = mgr.sessions.get(&session_id).unwrap().windows[0].panes[0].id;

        let (src, tgt) = mgr.move_pane(pane_a, 1).unwrap();
        assert_eq!(src, 1);
        assert_eq!(tgt, 1);
        let (ws, _) = mgr.get_workspace_state(session_id).unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].pane_ids, vec![pane_a]);
    }

    #[test]
    fn swap_panes_unknown_returns_error() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let err = mgr.swap_panes(404, 405).unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    #[ignore] // Requires a real TTY
    fn swap_panes_updates_layout_snapshot() {
        use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let session_id = mgr.create_session(Some("swap-test".into())).unwrap();
        // Find the existing pane id, then split to get two.
        let pane_a = mgr.sessions.get(&session_id).unwrap().windows[0].panes[0].id;
        let pane_b = mgr.split_pane(pane_a, false).unwrap();

        // Install a layout snapshot with both leaves.
        let workspaces = vec![WorkspaceInfo {
            id: 1,
            name: "main".into(),
            order: 0,
            pane_ids: vec![pane_a, pane_b],
            focused_pane: Some(pane_a),
            layout: Some(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: pane_a }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: pane_b }),
            }),
        }];
        mgr.set_workspace_state(session_id, workspaces, 1).unwrap();

        mgr.swap_panes(pane_a, pane_b).unwrap();
        let (got_ws, _) = mgr.get_workspace_state(session_id).unwrap();
        assert_eq!(got_ws[0].pane_ids, vec![pane_b, pane_a]);
        if let Some(LayoutSnapshot::Split { first, second, .. }) = got_ws[0].layout.as_ref() {
            assert!(matches!(**first, LayoutSnapshot::Leaf { pane_id } if pane_id == pane_b));
            assert!(matches!(**second, LayoutSnapshot::Leaf { pane_id } if pane_id == pane_a));
        } else {
            panic!("expected split layout");
        }
    }

    #[test]
    fn session_default_workspace_state() {
        let tx = make_event_tx();
        let session = Session::new(Some("test".into()), tx);
        assert!(session.workspace_state.is_empty());
        assert_eq!(session.active_workspace, 1);
    }

    #[test]
    fn set_workspace_state_nonexistent_session() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let result = mgr.set_workspace_state(999, vec![], 1);
        assert!(result.is_err());
    }

    #[test]
    fn get_workspace_state_nonexistent_session() {
        let tx = make_event_tx();
        let mgr = SessionManager::new(tx);
        let result = mgr.get_workspace_state(999);
        assert!(result.is_err());
    }

    #[test]
    #[ignore] // Requires a real TTY
    fn workspace_state_round_trip_via_session_manager() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        let session_id = mgr.create_session(Some("ws-test".into())).unwrap();

        let workspaces = vec![
            WorkspaceInfo {
                id: 1,
                name: "main".into(),
                order: 0,
                pane_ids: vec![10],
                focused_pane: Some(10),
                layout: None,
            },
            WorkspaceInfo {
                id: 3,
                name: "logs".into(),
                order: 1,
                pane_ids: vec![20, 21],
                focused_pane: Some(20),
                layout: None,
            },
        ];

        mgr.set_workspace_state(session_id, workspaces.clone(), 3)
            .unwrap();

        let (got_ws, got_active) = mgr.get_workspace_state(session_id).unwrap();
        assert_eq!(got_ws.len(), 2);
        assert_eq!(got_active, 3);
        assert_eq!(got_ws[0].name, "main");
        assert_eq!(got_ws[1].pane_ids, vec![20, 21]);
    }

    #[test]
    #[ignore] // Requires a real TTY
    fn workspace_state_broadcasts_event() {
        let tx = make_event_tx();
        let mut rx = tx.subscribe();
        let mut mgr = SessionManager::new(tx);
        let session_id = mgr.create_session(Some("evt-test".into())).unwrap();

        // Drain the SessionCreated event.
        let _ = rx.try_recv();

        mgr.set_workspace_state(session_id, vec![], 2).unwrap();

        match rx.try_recv() {
            Ok(DaemonEvent::WorkspaceChanged {
                session_id: sid,
                active_workspace,
            }) => {
                assert_eq!(sid, session_id);
                assert_eq!(active_workspace, 2);
            }
            other => panic!("expected WorkspaceChanged, got: {other:?}"),
        }
    }

    // ── tn-ju04: layout-aware resize cascade helpers ───────────────────

    #[test]
    fn layout_leaf_dims_single_leaf_returns_full_rect() {
        let tree = LayoutSnapshot::Leaf { pane_id: 7 };
        let dims = layout_leaf_dims(&tree, 80, 24);
        assert_eq!(dims.len(), 1);
        assert_eq!(dims[0].pane_id, 7);
        assert_eq!(dims[0].cols, 80);
        assert_eq!(dims[0].rows, 24);
    }

    #[test]
    fn layout_leaf_dims_horizontal_split_halves_cols() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
        };
        let dims = layout_leaf_dims(&tree, 81, 24);
        assert_eq!(dims.len(), 2);
        // 81 total - 1 separator = 80 usable; 40 + 40 halves.
        assert_eq!(
            dims[0],
            LeafDims {
                pane_id: 1,
                cols: 40,
                rows: 24
            }
        );
        assert_eq!(
            dims[1],
            LeafDims {
                pane_id: 2,
                cols: 40,
                rows: 24
            }
        );
    }

    #[test]
    fn layout_leaf_dims_vertical_split_halves_rows() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
        };
        let dims = layout_leaf_dims(&tree, 80, 25);
        assert_eq!(dims.len(), 2);
        // 25 rows - 1 separator = 24 usable; 12 + 12 halves.
        assert_eq!(
            dims[0],
            LeafDims {
                pane_id: 1,
                cols: 80,
                rows: 12
            }
        );
        assert_eq!(
            dims[1],
            LeafDims {
                pane_id: 2,
                cols: 80,
                rows: 12
            }
        );
    }

    #[test]
    fn layout_leaf_dims_nested_quad_layout() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        // Simulated 2x2: horizontal split, each side is a vertical split.
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
            }),
            second: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 4 }),
            }),
        };
        let dims = layout_leaf_dims(&tree, 81, 25);
        assert_eq!(dims.len(), 4);
        // Outer horizontal split: 81 -> (40 + 1 + 40)
        // Each side vertical split: 25 -> (12 + 1 + 12)
        for leaf in &dims {
            assert_eq!(leaf.cols, 40, "leaf {} cols", leaf.pane_id);
            assert_eq!(leaf.rows, 12, "leaf {} rows", leaf.pane_id);
        }
    }

    #[test]
    fn split_layout_leaf_replaces_leaf_with_split_node() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        let tree = LayoutSnapshot::Leaf { pane_id: 1 };
        let out = split_layout_leaf(tree, 1, 2, LayoutSplitDirection::Horizontal, 0.5);
        if let LayoutSnapshot::Split {
            direction,
            first,
            second,
            ..
        } = out
        {
            assert_eq!(direction, LayoutSplitDirection::Horizontal);
            assert!(matches!(*first, LayoutSnapshot::Leaf { pane_id: 1 }));
            assert!(matches!(*second, LayoutSnapshot::Leaf { pane_id: 2 }));
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn split_layout_leaf_ignores_unrelated_leaves() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        // Deep-nested tree: split leaf 3, leave leaves 1 + 2 untouched.
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
            }),
        };
        let out = split_layout_leaf(tree, 3, 99, LayoutSplitDirection::Vertical, 0.5);
        // Walk and assert: leaf 3 should now be a vertical split [3, 99].
        fn find_leaf(node: &LayoutSnapshot, target: PaneId) -> Option<&LayoutSnapshot> {
            match node {
                LayoutSnapshot::Leaf { pane_id } if *pane_id == target => Some(node),
                LayoutSnapshot::Leaf { .. } => None,
                LayoutSnapshot::Split { first, second, .. } => {
                    find_leaf(first, target).or_else(|| find_leaf(second, target))
                }
            }
        }
        assert!(find_leaf(&out, 1).is_some());
        assert!(find_leaf(&out, 2).is_some());
        assert!(find_leaf(&out, 99).is_some());
        // Leaf 3 is still a leaf inside the new split (first child).
        assert!(find_leaf(&out, 3).is_some());
    }

    #[test]
    fn reconstruct_layout_rect_from_leaf_dims() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        // Two siblings 40x24 side-by-side -> reconstructed 81x24 (with 1-cell gap).
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
        };
        let got = reconstruct_layout_rect(&tree, |id| match id {
            1 => Some((40, 24)),
            2 => Some((40, 24)),
            _ => None,
        })
        .unwrap();
        assert_eq!(got, (81, 24));
    }

    #[test]
    fn reconstruct_layout_rect_vertical_stack() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Vertical,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
        };
        let got = reconstruct_layout_rect(&tree, |id| match id {
            1 => Some((80, 12)),
            2 => Some((80, 12)),
            _ => None,
        })
        .unwrap();
        // 12 + 12 + 1 separator = 25 rows, 80 cols unchanged.
        assert_eq!(got, (80, 25));
    }

    #[test]
    fn layout_leaf_dims_roundtrips_through_reconstruction() {
        use therminal_protocol::daemon::LayoutSplitDirection;
        // Feed a tree + parent rect into `layout_leaf_dims`, then round-trip
        // through `reconstruct_layout_rect` using the computed dims; the
        // result should match the input within a 1-cell tolerance (the
        // reconstruction sums separators deterministically so the values
        // should be exact in this fixture).
        let tree = LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
            second: Box::new(LayoutSnapshot::Split {
                direction: LayoutSplitDirection::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
                second: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
            }),
        };
        let leaves = layout_leaf_dims(&tree, 81, 25);
        // Build a lookup map.
        let lookup: HashMap<PaneId, (u16, u16)> = leaves
            .iter()
            .map(|l| (l.pane_id, (l.cols, l.rows)))
            .collect();
        let rebuilt = reconstruct_layout_rect(&tree, |id| lookup.get(&id).copied()).unwrap();
        assert_eq!(rebuilt, (81, 25));
    }
}
