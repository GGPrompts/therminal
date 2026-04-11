//! Session CRUD: create / list / iter / get / attach / destroy /
//! write_to_pane / send_keys_to_pane / resize_pane / capture_pane*.

use std::sync::Arc;

use tracing::info;

use therminal_protocol::daemon::{DaemonEvent, LayoutSnapshot, WorkspaceInfo};
use therminal_protocol::{PaneId, SessionId};

use super::SessionManager;
use crate::session::base::Session;
use crate::session::snapshots::{PaneSnapshot, SessionSnapshot};

impl SessionManager {
    /// Create a new session with a default window/pane.
    pub fn create_session(
        &mut self,
        name: Option<String>,
    ) -> Result<SessionId, therminal_terminal::pty::PtyError> {
        self.create_session_with_options(name, &therminal_terminal::pty::SpawnOptions::default())
    }

    /// Create a new session with a default window/pane and custom spawn options.
    pub fn create_session_with_options(
        &mut self,
        name: Option<String>,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
    ) -> Result<SessionId, therminal_terminal::pty::PtyError> {
        let mut session = Session::new(name, self.event_tx.clone());
        let default_pane_id = session
            .create_default_pane(
                self.default_cols,
                self.default_rows,
                spawn_options,
                Arc::clone(&self.osc_registry),
                self.harness_event_tx.clone(),
                self.pattern_ctx(),
            )?
            .id;

        // Seed workspace_state with a single default workspace containing the
        // newly-spawned pane. Without this, GetWorkspaces on a fresh session
        // returns an empty vec, which broke the GUI attach flow in tn-ytw2
        // (remote_spawn.rs couldn't discover the initial pane id).
        session.workspace_state = vec![WorkspaceInfo {
            id: 1,
            name: "1".to_string(),
            order: 0,
            pane_ids: vec![default_pane_id],
            focused_pane: Some(default_pane_id),
            layout: Some(LayoutSnapshot::Leaf {
                pane_id: default_pane_id,
            }),
        }];
        session.active_workspace = 1;

        let session_id = session.id;
        info!(session_id = session_id, "session created");

        // Broadcast creation event
        let _ = self
            .event_tx
            .send(DaemonEvent::SessionCreated { session_id });

        self.sessions.insert(session_id, session);
        self.mark_dirty();
        Ok(session_id)
    }

    /// Iterate over all sessions.
    pub fn iter_sessions(&self) -> impl Iterator<Item = (&SessionId, &Session)> {
        self.sessions.iter()
    }

    /// List all session IDs.
    pub fn list_sessions(&self) -> Vec<SessionId> {
        self.sessions.keys().copied().collect()
    }

    /// Get session info (id, name, created_at).
    pub fn get_session_info(
        &self,
        session_id: SessionId,
    ) -> Option<(SessionId, Option<String>, u64)> {
        self.sessions
            .get(&session_id)
            .map(|s| (s.id, s.name.clone(), s.created_at_secs))
    }

    /// Attach to a session: returns a snapshot of the current terminal state.
    pub fn attach(&self, session_id: SessionId) -> Option<SessionSnapshot> {
        self.sessions.get(&session_id).map(|s| s.snapshot())
    }

    /// Write input data to a specific pane in a session.
    pub fn write_to_pane(
        &mut self,
        session_id: SessionId,
        pane_id: PaneId,
        data: &[u8],
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let pane = session
            .find_pane_mut(pane_id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;
        pane.write(data).map_err(|e| format!("write error: {e}"))
    }

    /// Destroy a session and all its panes.
    pub fn destroy_session(&mut self, session_id: SessionId) -> bool {
        if let Some(session) = self.sessions.remove(&session_id) {
            // Unregister all agents from panes in this session.
            for window in &session.windows {
                for pane in &window.panes {
                    self.agent_registry.unregister(pane.id);
                }
            }
            info!(session_id = session_id, "session destroyed");
            let _ = self
                .event_tx
                .send(DaemonEvent::SessionDestroyed { session_id });
            self.mark_dirty();
            true
        } else {
            false
        }
    }

    /// Number of active sessions.
    pub fn session_count(&self) -> u32 {
        self.sessions.len() as u32
    }

    /// Send keys to a pane by pane ID (searches all sessions).
    pub fn send_keys_to_pane(&mut self, pane_id: PaneId, keys: &[u8]) -> Result<(), String> {
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                return pane.write(keys).map_err(|e| format!("write error: {e}"));
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Resize a pane's PTY by pane ID (searches all sessions).
    ///
    /// tn-ju04: also broadcasts `DaemonEvent::PaneResized` so CLI /
    /// subscription watchers re-read geometry whenever the GUI (or MCP)
    /// drives a resize.
    pub fn resize_pane(&mut self, pane_id: PaneId, cols: u16, rows: u16) -> Result<(), String> {
        let mut found_session: Option<SessionId> = None;
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                pane.resize(cols, rows);
                found_session = Some(session.id);
                break;
            }
        }
        match found_session {
            Some(session_id) => {
                self.broadcast_event(DaemonEvent::PaneResized {
                    session_id,
                    pane_id,
                    cols,
                    rows,
                });
                Ok(())
            }
            None => Err(format!("pane not found: {pane_id}")),
        }
    }

    /// Capture structured pane state (mode flags, cursor, visible grid)
    /// for tn-zamd replay on attach. See `Pane::snapshot_state`.
    pub fn capture_pane_state(
        &self,
        pane_id: PaneId,
    ) -> Result<therminal_protocol::daemon::PaneStateSnapshot, String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Ok(pane.snapshot_state());
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Capture pane content by pane ID (searches all sessions).
    pub fn capture_pane(&self, pane_id: PaneId) -> Result<PaneSnapshot, String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Ok(pane.snapshot());
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }
}
