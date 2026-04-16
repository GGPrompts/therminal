//! Debounced persistence of daemon session state.
//!
//! Saves a `PersistedState` snapshot to `<data_dir>/sessions.json` whenever
//! the session topology changes (create, destroy, split, kill). A 2-second
//! debounce timer coalesces rapid changes to avoid disk thrashing.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Notify, mpsc};
use tracing::{debug, info, warn};

use therminal_protocol::daemon::{PersistedPane, PersistedSession, PersistedState};

use crate::session::SessionManager;

/// Return the path to the persisted sessions file.
pub fn sessions_file() -> PathBuf {
    therminal_runtime::paths::data_dir().join("sessions.json")
}

/// Load persisted state from disk. Returns `None` if the file doesn't exist
/// or can't be parsed (log a warning in that case).
pub fn load() -> Option<PersistedState> {
    let path = sessions_file();
    match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str(&json) {
            Ok(state) => {
                info!(path = %path.display(), "loaded persisted session state");
                Some(state)
            }
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to parse persisted state, ignoring");
                None
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no persisted state file found");
            None
        }
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to read persisted state");
            None
        }
    }
}

/// Save persisted state to disk (synchronous, for use on shutdown).
pub fn save_sync(state: &PersistedState) {
    let path = sessions_file();
    if let Err(e) = therminal_runtime::paths::ensure_data_dir() {
        warn!(error = %e, "failed to create data directory for persistence");
        return;
    }
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            // Atomic write: write to .tmp then rename, so a crash mid-write
            // never corrupts the canonical sessions.json.
            let tmp_path = path.with_extension("json.tmp");
            if let Err(e) = std::fs::write(&tmp_path, json.as_bytes()) {
                warn!(path = %tmp_path.display(), error = %e, "failed to write temp state file");
                return;
            }
            if let Err(e) = std::fs::rename(&tmp_path, &path) {
                warn!(path = %path.display(), error = %e, "failed to rename persisted state file");
                // Clean up tmp file on rename failure.
                let _ = std::fs::remove_file(&tmp_path);
                return;
            }
            debug!(path = %path.display(), "persisted state saved");
        }
        Err(e) => {
            warn!(error = %e, "failed to serialize persisted state");
        }
    }
}

/// Snapshot the current session manager state into a `PersistedState`.
pub fn snapshot(mgr: &SessionManager) -> PersistedState {
    let mut sessions = Vec::new();
    for (_id, session) in mgr.iter_sessions() {
        let mut panes = Vec::new();
        for window in &session.windows {
            for pane in &window.panes {
                let tags = pane.tags();
                let pinned = tags.get("pinned").map(|v| v == "true").unwrap_or(false);
                panes.push(PersistedPane {
                    cwd: pane.cwd(),
                    shell: pane.shell().to_owned(),
                    cols: pane.cols(),
                    rows: pane.rows(),
                    tags,
                    pinned,
                });
            }
        }
        sessions.push(PersistedSession {
            name: session.name.clone(),
            panes,
            workspaces: session.workspace_state.clone(),
            active_workspace: session.active_workspace,
        });
    }
    PersistedState { sessions }
}

/// Handle to a running persistence task. Send a notification to trigger
/// a debounced save, or drop to stop the task.
#[derive(Clone)]
pub struct PersistenceHandle {
    /// Send on this channel to signal that state has changed.
    notify_tx: mpsc::UnboundedSender<()>,
}

impl PersistenceHandle {
    /// Signal that session state has changed and should be persisted.
    pub fn mark_dirty(&self) {
        let _ = self.notify_tx.send(());
    }
}

/// Spawn a background task that debounces save requests.
///
/// Returns a `PersistenceHandle` the caller uses to signal changes, and
/// the `JoinHandle` for the spawned task.
///
/// The `shutdown` notify is used to trigger a final save on daemon exit.
pub fn spawn_persistence_task(
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    shutdown: Arc<Notify>,
) -> (PersistenceHandle, tokio::task::JoinHandle<()>) {
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<()>();

    let handle = PersistenceHandle { notify_tx };

    let task = tokio::spawn(async move {
        let debounce = std::time::Duration::from_secs(2);

        loop {
            tokio::select! {
                recv = notify_rx.recv() => {
                    if recv.is_none() {
                        // Channel closed, do a final save and exit.
                        break;
                    }
                    // Got a dirty signal -- wait for the debounce period,
                    // draining any additional signals that arrive.
                    tokio::time::sleep(debounce).await;
                    // Drain any queued signals.
                    while notify_rx.try_recv().is_ok() {}
                    // Save.
                    let mgr = session_mgr.lock().await;
                    let state = snapshot(&mgr);
                    drop(mgr);
                    save_sync(&state);
                }
                _ = shutdown.notified() => {
                    // Final save on shutdown.
                    break;
                }
            }
        }

        // Final save.
        let mgr = session_mgr.lock().await;
        let state = snapshot(&mgr);
        drop(mgr);
        save_sync(&state);
        info!("persistence task shut down, final state saved");
    });

    (handle, task)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_state_round_trip() {
        let state = PersistedState {
            sessions: vec![PersistedSession {
                name: Some("main".into()),
                panes: vec![
                    PersistedPane {
                        cwd: "/home/user".into(),
                        shell: String::new(),
                        cols: 120,
                        rows: 40,
                        tags: Default::default(),
                        pinned: false,
                    },
                    PersistedPane {
                        cwd: "/tmp".into(),
                        shell: "/bin/zsh".into(),
                        cols: 80,
                        rows: 24,
                        tags: Default::default(),
                        pinned: false,
                    },
                ],
                workspaces: vec![],
                active_workspace: 1,
            }],
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: PersistedState = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.sessions.len(), 1);
        assert_eq!(parsed.sessions[0].name.as_deref(), Some("main"));
        assert_eq!(parsed.sessions[0].panes.len(), 2);
        assert_eq!(parsed.sessions[0].panes[0].cwd, "/home/user");
        assert_eq!(parsed.sessions[0].panes[1].shell, "/bin/zsh");
    }

    #[test]
    fn load_nonexistent_returns_none() {
        // Verify that valid JSON parses correctly.
        let result: Option<PersistedState> = serde_json::from_str(r#"{"sessions":[]}"#).ok();
        assert!(result.is_some());
        assert!(result.unwrap().sessions.is_empty());
    }

    #[test]
    fn save_and_load_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions.json");

        let state = PersistedState {
            sessions: vec![PersistedSession {
                name: Some("test".into()),
                panes: vec![PersistedPane {
                    cwd: "/home/test".into(),
                    shell: String::new(),
                    cols: 80,
                    rows: 24,
                    tags: Default::default(),
                    pinned: false,
                }],
                workspaces: vec![],
                active_workspace: 1,
            }],
        };

        // Write manually to the temp path.
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&path, &json).unwrap();

        // Read back.
        let loaded: PersistedState =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.sessions.len(), 1);
        assert_eq!(loaded.sessions[0].panes[0].cols, 80);
    }

    #[test]
    fn snapshot_empty_manager() {
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mgr = SessionManager::new(tx);
        let state = snapshot(&mgr);
        assert!(state.sessions.is_empty());
    }

    /// Verify that atomic write cleans up the temp file after success.
    #[test]
    fn atomic_write_cleans_up_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sessions.json");
        let tmp_path = path.with_extension("json.tmp");

        let state = PersistedState {
            sessions: vec![PersistedSession {
                name: Some("atomic-test".into()),
                panes: vec![PersistedPane {
                    cwd: "/home/test".into(),
                    shell: String::new(),
                    cols: 80,
                    rows: 24,
                    tags: Default::default(),
                    pinned: false,
                }],
                workspaces: vec![],
                active_workspace: 1,
            }],
        };

        // Manually perform the atomic write pattern against our temp dir.
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&tmp_path, json.as_bytes()).unwrap();
        assert!(tmp_path.exists(), "tmp file should exist before rename");

        std::fs::rename(&tmp_path, &path).unwrap();
        assert!(path.exists(), "final file should exist after rename");
        assert!(!tmp_path.exists(), "tmp file should be gone after rename");

        // Verify contents survived.
        let loaded: PersistedState =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.sessions[0].name.as_deref(), Some("atomic-test"));
    }

    /// Verify that snapshot -> restore -> snapshot preserves cwd and shell values.
    ///
    /// Creates a persisted state with specific cwd and shell, restores it
    /// (spawning real PTYs), then snapshots and checks the values survived.
    #[test]
    fn snapshot_preserves_cwd_and_shell() {
        let persisted = PersistedState {
            sessions: vec![PersistedSession {
                name: Some("persist-test".into()),
                panes: vec![
                    PersistedPane {
                        cwd: "/tmp".into(),
                        shell: "/bin/sh".into(),
                        cols: 80,
                        rows: 24,
                        tags: Default::default(),
                        pinned: false,
                    },
                    PersistedPane {
                        cwd: "/var".into(),
                        shell: String::new(),
                        cols: 120,
                        rows: 40,
                        tags: Default::default(),
                        pinned: false,
                    },
                ],
                workspaces: vec![],
                active_workspace: 1,
            }],
        };

        let (tx, _) = tokio::sync::broadcast::channel(16);
        let mut mgr = SessionManager::new(tx);
        let restored = mgr.restore_from_persisted(&persisted);
        assert_eq!(restored, 2, "should restore two panes");

        let state = snapshot(&mgr);
        assert_eq!(state.sessions.len(), 1);
        let session = &state.sessions[0];
        assert_eq!(session.name.as_deref(), Some("persist-test"));
        assert_eq!(session.panes.len(), 2);

        // The first pane was spawned with cwd="/tmp" and shell="/bin/sh".
        assert_eq!(session.panes[0].cwd, "/tmp");
        assert_eq!(session.panes[0].shell, "/bin/sh");

        // The second pane was spawned with cwd="/var" and the default
        // shell. tn-x1h9: `Pane.shell` now records the *resolved* shell
        // (`resolve_shell(&spawn_options)`), not the caller's empty-string
        // request. This is load-bearing for `process_detector_task`'s
        // WSL-probe activation on Windows native; see
        // `resolved_default_shell_is_never_empty` in process_detector_task.
        assert_eq!(session.panes[1].cwd, "/var");
        assert!(
            !session.panes[1].shell.is_empty(),
            "restored pane must record a resolved (non-empty) shell command so downstream detectors can classify it"
        );
        assert_eq!(
            session.panes[1].shell,
            therminal_terminal::pty::get_default_shell(),
            "restored pane's shell should match the resolved default"
        );
    }
}
