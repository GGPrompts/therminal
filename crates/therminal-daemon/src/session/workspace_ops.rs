//! Workspace, pane topology, and pane accessor operations on `SessionManager`.
//!
//! Extracted from `manager.rs` to keep the core CRUD module focused.
//! Contains: pane read accessors (command tracker, event log, agent details,
//! cadence, region index), workspace create/rename/switch/set, pane
//! swap/move/select, tag management, agent registry, handoff/restore, and
//! shutdown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use therminal_protocol::daemon::{LayoutSnapshot, WorkspaceInfo};
use tracing::warn;

use super::layout::{append_layout_leaf, remove_layout_leaf, swap_layout_leaves};
use super::manager::SessionManager;
use therminal_protocol::daemon::DaemonEvent;
pub use therminal_protocol::{PaneId, SessionId, WorkspaceId};

use therminal_terminal::event_log::StoredEvent;
use therminal_terminal::osc633::CommandBlock;
use therminal_terminal::region_index::RegionIndex;
use therminal_terminal::state_inference::{AgentCadenceSnapshot, AgentDetailsSnapshot};

impl SessionManager {
    /// Test-only: get the shared command tracker `Arc` for a pane so
    /// tests can inject OSC 633 marks bypassing the PTY reader thread.
    #[cfg(test)]
    pub fn pane_command_tracker_arc(
        &self,
        pane_id: PaneId,
    ) -> Option<Arc<Mutex<therminal_terminal::osc633::CommandTracker>>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.command_tracker_arc());
                }
            }
        }
        None
    }

    /// Snapshot a pane's OSC 633 command tracker by pane ID. Returns
    /// `None` if the pane does not exist.
    pub fn pane_command_blocks(&self, pane_id: PaneId) -> Option<Vec<CommandBlock>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.command_tracker_snapshot());
                }
            }
        }
        None
    }

    /// Snapshot a pane's in-memory event log by pane ID. Returns `None`
    /// if the pane does not exist; otherwise the (possibly empty) list of
    /// recent events filtered by the optional `since_timestamp_secs` and
    /// capped at `limit`.
    pub fn pane_event_log_snapshot(
        &self,
        pane_id: PaneId,
        since_timestamp_secs: Option<u64>,
        limit: usize,
    ) -> Option<Vec<StoredEvent>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.event_log_snapshot(since_timestamp_secs, limit));
                }
            }
        }
        None
    }

    /// Test-only: shared event log Arc for a pane.
    #[cfg(test)]
    pub fn pane_event_log_arc(
        &self,
        pane_id: PaneId,
    ) -> Option<Arc<Mutex<therminal_terminal::event_log::EventLog>>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.event_log_arc());
                }
            }
        }
        None
    }

    /// Snapshot a pane's agent inference state by pane ID. Returns `None`
    /// if the pane does not exist.
    pub fn pane_agent_details(&self, pane_id: PaneId) -> Option<AgentDetailsSnapshot> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.agent_details_snapshot());
                }
            }
        }
        None
    }

    /// Snapshot a pane's output cadence window by pane ID. Returns `None`
    /// if the pane does not exist. The DTO is plain owned data with sample
    /// timestamps already converted to wall-clock Unix seconds, so the
    /// caller can serialise it after the session-manager lock is released.
    pub fn pane_agent_cadence(&self, pane_id: PaneId) -> Option<AgentCadenceSnapshot> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.agent_cadence_snapshot());
                }
            }
        }
        None
    }

    /// Access a pane's region index by pane ID (searches all sessions).
    pub fn pane_region_index(&self, pane_id: PaneId) -> Result<Arc<Mutex<RegionIndex>>, String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Ok(Arc::clone(pane.region_index()));
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Select (focus) a pane. Currently a no-op since the daemon is headless,
    /// but validates the pane exists and can be extended with focus tracking.
    pub fn select_pane(&self, pane_id: PaneId) -> Result<(), String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if window.pane(pane_id).is_some() {
                    return Ok(());
                }
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Swap two panes' positions in the layout tree of their session.
    ///
    /// Both panes must currently belong to the same session — cross-session
    /// swaps are not expressible in the wire protocol and are rejected here.
    /// Updates `WorkspaceInfo::pane_ids` ordering and rewrites any
    /// `LayoutSnapshot::Leaf` nodes referencing either pane within all of
    /// the session's workspaces, so a follow-up `set_workspace_state` from
    /// the GUI will be a no-op.
    pub fn swap_panes(&mut self, a: PaneId, b: PaneId) -> Result<(), String> {
        if a == b {
            return Ok(());
        }
        let session_a = self
            .session_for_pane(a)
            .ok_or_else(|| format!("pane not found: {a}"))?;
        let session_b = self
            .session_for_pane(b)
            .ok_or_else(|| format!("pane not found: {b}"))?;
        if session_a != session_b {
            return Err(format!(
                "cross-session swap not supported: pane {a} in session {session_a}, pane {b} in session {session_b}"
            ));
        }

        let session = self
            .sessions
            .get_mut(&session_a)
            .ok_or_else(|| "session disappeared under concurrent request".to_string())?;

        for ws in session.workspace_state.iter_mut() {
            for pid in ws.pane_ids.iter_mut() {
                if *pid == a {
                    *pid = b;
                } else if *pid == b {
                    *pid = a;
                }
            }
            if let Some(layout) = ws.layout.as_mut() {
                swap_layout_leaves(layout, a, b);
            }
        }

        self.mark_dirty();
        Ok(())
    }

    /// Move a pane between workspaces inside its containing session
    /// (tn-fi1k). Metadata-only: the underlying PTY is not touched.
    ///
    /// Returns `(source_workspace_id, target_workspace_id)` on success.
    /// Errors if the pane does not exist anywhere in any session, or if
    /// it is somehow not present in any workspace's `pane_ids` (a corrupt
    /// state that should be loud).
    ///
    /// If the target workspace doesn't exist yet, it is created as a
    /// fresh single-pane workspace whose layout is just `Leaf { pane_id }`.
    ///
    /// If the move is a no-op (target == source), it succeeds with the
    /// source as both source and target.
    pub fn move_pane(
        &mut self,
        pane_id: PaneId,
        target_workspace_id: WorkspaceId,
    ) -> Result<(WorkspaceId, WorkspaceId), String> {
        let session_id = self
            .session_for_pane(pane_id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;

        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| "session disappeared under concurrent request".to_string())?;

        // 1. Find the workspace currently owning the pane.
        let source_idx = session
            .workspace_state
            .iter()
            .position(|ws| ws.pane_ids.contains(&pane_id))
            .ok_or_else(|| {
                format!(
                    "pane {pane_id} exists in session {session_id} but is not bound to any workspace"
                )
            })?;
        let source_workspace_id = session.workspace_state[source_idx].id;

        if source_workspace_id == target_workspace_id {
            // No-op move: nothing to do, but report it as success so callers
            // can keep the local <-> daemon state mirror in sync without
            // special-casing.
            return Ok((source_workspace_id, target_workspace_id));
        }

        // 2. Remove the pane from the source workspace's pane_ids and layout.
        {
            let src = &mut session.workspace_state[source_idx];
            src.pane_ids.retain(|p| *p != pane_id);
            if src.focused_pane == Some(pane_id) {
                src.focused_pane = src.pane_ids.first().copied();
            }
            if let Some(layout) = src.layout.as_mut() {
                let new_layout = remove_layout_leaf(layout.clone(), pane_id);
                src.layout = new_layout;
            }
        }

        // 3. Add the pane to the target workspace, creating it if missing.
        let target_idx_opt = session
            .workspace_state
            .iter()
            .position(|ws| ws.id == target_workspace_id);
        match target_idx_opt {
            Some(idx) => {
                let target = &mut session.workspace_state[idx];
                if !target.pane_ids.contains(&pane_id) {
                    target.pane_ids.push(pane_id);
                }
                target.layout = Some(append_layout_leaf(target.layout.take(), pane_id));
            }
            None => {
                // Create a fresh workspace tab for the target id with the
                // moved pane as its only leaf.
                let next_order = session
                    .workspace_state
                    .iter()
                    .map(|w| w.order)
                    .max()
                    .map(|m| m + 1)
                    .unwrap_or(0);
                session.workspace_state.push(WorkspaceInfo {
                    id: target_workspace_id,
                    name: target_workspace_id.to_string(),
                    order: next_order,
                    pane_ids: vec![pane_id],
                    focused_pane: Some(pane_id),
                    layout: Some(LayoutSnapshot::Leaf { pane_id }),
                });
            }
        }

        self.mark_dirty();
        Ok((source_workspace_id, target_workspace_id))
    }

    /// Merge opaque key/value tags into a pane (tn-bbvf). Returns the
    /// resulting full tag set on success.
    pub fn tag_pane(
        &mut self,
        pane_id: PaneId,
        tags: HashMap<String, String>,
    ) -> Result<HashMap<String, String>, String> {
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                pane.merge_tags(tags);
                let snap = pane.tags();
                self.mark_dirty();
                return Ok(snap);
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Remove tags from a pane. `keys = None` clears all tags. Returns
    /// the remaining tag set.
    pub fn untag_pane(
        &mut self,
        pane_id: PaneId,
        keys: Option<Vec<String>>,
    ) -> Result<HashMap<String, String>, String> {
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                match keys {
                    Some(ref ks) => pane.remove_tag_keys(ks),
                    None => pane.clear_tags(),
                }
                let snap = pane.tags();
                self.mark_dirty();
                return Ok(snap);
            }
        }
        Err(format!("pane not found: {pane_id}"))
    }

    /// Snapshot a pane's tags by ID. `None` if the pane does not exist.
    pub fn pane_tags(&self, pane_id: PaneId) -> Option<HashMap<String, String>> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.tags());
                }
            }
        }
        None
    }

    /// Snapshot a pane's current working directory (from OSC 7 / spawn).
    /// Returns `None` if the pane does not exist; returns `Some("")` if it
    /// exists but never published a cwd. Used by tn-h7tq's worktree
    /// resolution path to find the source pane's git repo.
    pub fn pane_cwd(&self, pane_id: PaneId) -> Option<String> {
        for session in self.sessions.values() {
            for window in &session.windows {
                if let Some(pane) = window.pane(pane_id) {
                    return Some(pane.cwd());
                }
            }
        }
        None
    }

    /// Find the session ID that contains a given pane.
    pub fn session_for_pane(&self, pane_id: PaneId) -> Option<SessionId> {
        self.sessions
            .values()
            .find(|s| {
                s.windows
                    .iter()
                    .any(|w| w.panes.iter().any(|p| p.id == pane_id))
            })
            .map(|s| s.id)
    }

    /// Set the workspace topology for a session.
    ///
    /// The app calls this whenever workspace state changes (switch, create,
    /// rename, pane move). The daemon stores it as the source of truth so
    /// MCP tools and reattaching clients can query it.
    pub fn set_workspace_state(
        &mut self,
        session_id: SessionId,
        workspaces: Vec<WorkspaceInfo>,
        active_workspace: WorkspaceId,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        session.workspace_state = workspaces;
        session.active_workspace = active_workspace;
        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace,
        });
        self.mark_dirty();
        Ok(())
    }

    /// Switch the active workspace for a session without touching the
    /// stored topology (tn-8ysl). Validates that the requested workspace
    /// exists in the session's `workspace_state` (or is `1` for legacy
    /// sessions that haven't populated their workspace state yet), then
    /// updates `active_workspace` and broadcasts `WorkspaceChanged`.
    pub fn set_active_workspace(
        &mut self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let exists = session.workspace_state.iter().any(|w| w.id == workspace_id)
            || (session.workspace_state.is_empty() && workspace_id == 1);
        if !exists {
            return Err(format!(
                "workspace {workspace_id} not found in session {session_id}"
            ));
        }
        session.active_workspace = workspace_id;
        // no subscribers is normal — events broadcast to whatever clients are attached
        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace: workspace_id,
        });
        self.mark_dirty();
        Ok(())
    }

    /// Get the workspace topology for a session.
    pub fn get_workspace_state(
        &self,
        session_id: SessionId,
    ) -> Result<(Vec<WorkspaceInfo>, WorkspaceId), String> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        Ok((session.workspace_state.clone(), session.active_workspace))
    }

    /// Create a new empty workspace in a session (tn-ceqw).
    ///
    /// Picks the lowest unused workspace ID in 1..=9, appends a
    /// `WorkspaceInfo` entry, sets the new workspace as active, and
    /// broadcasts `WorkspaceChanged`.
    pub fn create_workspace(
        &mut self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<WorkspaceId, String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;

        // Pick the lowest unused workspace slot in 1..=9.
        let used: std::collections::HashSet<WorkspaceId> =
            session.workspace_state.iter().map(|w| w.id).collect();
        let new_id = (1..=9u64)
            .find(|id| !used.contains(id))
            .ok_or_else(|| "all workspace slots 1-9 are occupied".to_string())?;

        let ws_name = name.unwrap_or_else(|| format!("Workspace {new_id}"));
        session.workspace_state.push(WorkspaceInfo {
            id: new_id,
            name: ws_name,
            order: new_id as u32,
            pane_ids: vec![],
            focused_pane: None,
            layout: None,
        });
        session.active_workspace = new_id;

        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace: new_id,
        });
        self.mark_dirty();
        Ok(new_id)
    }

    /// Rename an existing workspace (tn-ceqw).
    pub fn rename_workspace(
        &mut self,
        session_id: SessionId,
        workspace_id: WorkspaceId,
        name: String,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let ws = session
            .workspace_state
            .iter_mut()
            .find(|w| w.id == workspace_id)
            .ok_or_else(|| format!("workspace {workspace_id} not found in session {session_id}"))?;
        ws.name = name;
        let active_workspace = session.active_workspace;
        self.broadcast_event(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace,
        });
        self.mark_dirty();
        Ok(())
    }

    /// Return the ID of the first (default) session, if any.
    pub fn default_session_id(&self) -> Option<SessionId> {
        self.sessions.keys().next().copied()
    }

    /// Snapshot of `(pane_id, shell_pid)` pairs for every live pane across
    /// all sessions. Returned as plain owned values so callers (notably
    /// the daemon-side process-detector ticker — tn-pehl) can drop the
    /// `SessionManager` lock before performing the scan.
    pub fn pane_shell_pids(&self) -> Vec<(PaneId, Option<u32>)> {
        let mut out = Vec::new();
        for session in self.sessions.values() {
            for window in &session.windows {
                for pane in &window.panes {
                    out.push((pane.id, pane.shell_pid()));
                }
            }
        }
        out
    }

    // ── Agent registry ─────────────────────────────────────────────────

    /// Access the agent registry (read-only).
    pub fn agent_registry(&self) -> &therminal_terminal::agent_registry::AgentRegistry {
        &self.agent_registry
    }

    /// Install a broadcaster on the agent registry. Used by `ensure.rs` to
    /// forward lifecycle events into the MCP `therminal://agents/events`
    /// resource pipeline.
    pub fn set_agent_event_broadcaster(
        &mut self,
        broadcaster: therminal_terminal::agent_registry::AgentEventBroadcaster,
    ) {
        self.agent_registry.set_broadcaster(broadcaster);
    }

    /// Register an agent for a pane in the central registry.
    pub fn register_agent(
        &mut self,
        pane_id: PaneId,
        name: String,
        agent_type: therminal_terminal::state_inference::AgentType,
        pid: Option<u32>,
    ) {
        self.agent_registry.register(pane_id, name, agent_type, pid);
    }

    /// Unregister the agent for a pane.
    pub fn unregister_agent(&mut self, pane_id: PaneId) {
        self.agent_registry.unregister(pane_id);
    }

    /// Update the status of a tracked agent.
    pub fn update_agent_status(
        &mut self,
        pane_id: PaneId,
        status: therminal_terminal::agent_registry::AgentStatus,
    ) {
        self.agent_registry.update_status(pane_id, status);
    }

    /// Return a snapshot of all tracked agents.
    pub fn list_agents(&self) -> Vec<therminal_terminal::agent_registry::AgentEntry> {
        self.agent_registry.agents()
    }

    /// Return agents filtered by status string.
    pub fn list_agents_by_status(
        &self,
        status: &str,
    ) -> Vec<therminal_terminal::agent_registry::AgentEntry> {
        self.agent_registry.agents_by_status(status)
    }

    /// Collect handoff metadata and raw FDs for all panes (Unix only).
    ///
    /// Returns a `HandoffPayload` and a Vec of `RawFd` in matching order.
    /// The FDs are borrowed from the panes' PTY masters -- the caller must
    /// send them via SCM_RIGHTS before the panes are dropped.
    #[cfg(unix)]
    pub fn collect_handoff_fds(
        &self,
    ) -> (
        therminal_protocol::daemon::HandoffPayload,
        Vec<std::os::unix::io::RawFd>,
    ) {
        use therminal_protocol::daemon::{HandoffPaneMeta, HandoffPayload};

        let mut panes_meta = Vec::new();
        let mut fds = Vec::new();

        for session in self.sessions.values() {
            for window in &session.windows {
                for pane in &window.panes {
                    if let Some(raw_fd) = pane._pty_master.as_raw_fd() {
                        panes_meta.push(HandoffPaneMeta {
                            session_id: session.id,
                            session_name: session.name.clone(),
                            pane_id: pane.id,
                            cols: pane.cols(),
                            rows: pane.rows(),
                        });
                        fds.push(raw_fd);
                    } else {
                        warn!(
                            pane_id = %pane.id,
                            "pane has no raw FD, skipping in handoff"
                        );
                    }
                }
            }
        }

        (HandoffPayload { panes: panes_meta }, fds)
    }

    /// Reconstruct sessions from handoff metadata and received PTY master FDs (Unix only).
    ///
    /// Each received FD is wrapped in a `FdPtyMaster` that implements `MasterPty`,
    /// and a new reader thread is spawned to feed the headless `Term`. This is the
    /// counterpart to `collect_handoff_fds()`.
    #[cfg(unix)]
    pub fn restore_from_handoff(
        &mut self,
        payload: &therminal_protocol::daemon::HandoffPayload,
        fds: Vec<std::os::unix::io::RawFd>,
    ) -> usize {
        use std::collections::HashMap as StdHashMap;
        use std::sync::atomic::Ordering;

        type PaneEntry = (
            therminal_protocol::daemon::HandoffPaneMeta,
            std::os::unix::io::RawFd,
        );
        type SessionGroup = (Option<String>, Vec<PaneEntry>);

        let mut restored = 0usize;

        // Group panes by session_id so we can reconstruct session -> window -> pane.
        let mut session_groups: StdHashMap<SessionId, SessionGroup> = StdHashMap::new();

        for (meta, fd) in payload.panes.iter().zip(fds.into_iter()) {
            let entry = session_groups
                .entry(meta.session_id)
                .or_insert_with(|| (meta.session_name.clone(), Vec::new()));
            entry.1.push((meta.clone(), fd));
        }

        for (session_id, (session_name, pane_entries)) in session_groups {
            let mut session = super::base::Session::new(session_name, self.event_tx.clone());
            // Override the auto-generated ID with the original.
            session.id = session_id;

            let mut window = super::window::Window::new();

            for (meta, raw_fd) in pane_entries {
                match super::pane::Pane::from_raw_fd(
                    meta.pane_id,
                    meta.cols,
                    meta.rows,
                    raw_fd,
                    self.event_tx.clone(),
                    session_id,
                    std::sync::Arc::clone(&self.osc_registry),
                    self.harness_event_tx.clone(),
                    self.pattern_ctx(),
                ) {
                    Ok(pane) => {
                        window.add_pane(pane);
                        restored += 1;
                    }
                    Err(e) => {
                        warn!(
                            pane_id = meta.pane_id,
                            error = %e,
                            "failed to restore pane from FD, closing FD"
                        );
                        unsafe {
                            libc::close(raw_fd);
                        }
                    }
                }
            }

            if !window.panes.is_empty() {
                session.windows.push(window);
                tracing::info!(
                    session_id = session_id,
                    pane_count = session.pane_count(),
                    "restored session from handoff"
                );
                self.sessions.insert(session_id, session);
            }
        }

        // Update the ID counters so new sessions/panes don't collide.
        if let Some(max_session) = self.sessions.keys().max() {
            let current = super::NEXT_SESSION_ID.load(Ordering::Relaxed);
            if *max_session >= current {
                super::NEXT_SESSION_ID.store(max_session + 1, Ordering::Relaxed);
            }
        }
        let max_pane = self
            .sessions
            .values()
            .flat_map(|s| s.windows.iter())
            .flat_map(|w| w.panes.iter())
            .map(|p| p.id)
            .max()
            .unwrap_or(0);
        let current_pane = super::NEXT_PANE_ID.load(Ordering::Relaxed);
        if max_pane >= current_pane {
            super::NEXT_PANE_ID.store(max_pane + 1, Ordering::Relaxed);
        }

        restored
    }

    /// Restore sessions from persisted state.
    ///
    /// For each persisted session, spawns a new session with fresh PTYs using
    /// the saved cwd. Does not restore terminal grid content -- only layout
    /// and metadata.
    pub fn restore_from_persisted(
        &mut self,
        state: &therminal_protocol::daemon::PersistedState,
    ) -> usize {
        let mut restored = 0usize;
        for persisted_session in &state.sessions {
            if persisted_session.panes.is_empty() {
                continue;
            }

            // Use the first pane to create the session (which creates a default pane).
            let first_pane = &persisted_session.panes[0];
            let spawn_opts = therminal_terminal::pty::SpawnOptions {
                cwd: first_pane.cwd.clone(),
                shell: first_pane.shell.clone(),
                ..Default::default()
            };

            let mut session =
                super::base::Session::new(persisted_session.name.clone(), self.event_tx.clone());
            match session.create_default_pane(
                first_pane.cols,
                first_pane.rows,
                &spawn_opts,
                std::sync::Arc::clone(&self.osc_registry),
                self.harness_event_tx.clone(),
                self.pattern_ctx(),
            ) {
                Ok(_) => {}
                Err(e) => {
                    warn!(
                        name = ?persisted_session.name,
                        error = %e,
                        "failed to restore session from persisted state"
                    );
                    continue;
                }
            }

            // Restore tags onto the freshly-spawned default pane.
            if !first_pane.tags.is_empty()
                && let Some(window) = session.windows.first_mut()
                && let Some(pane) = window.panes.first_mut()
            {
                pane.set_tags(first_pane.tags.clone());
            }

            let session_id = session.id;

            // Spawn additional panes for multi-pane sessions.
            for pane_meta in &persisted_session.panes[1..] {
                let opts = therminal_terminal::pty::SpawnOptions {
                    cwd: pane_meta.cwd.clone(),
                    shell: pane_meta.shell.clone(),
                    ..Default::default()
                };
                match super::pane::Pane::spawn(
                    pane_meta.cols,
                    pane_meta.rows,
                    self.event_tx.clone(),
                    session_id,
                    &opts,
                    std::sync::Arc::clone(&self.osc_registry),
                    self.harness_event_tx.clone(),
                    self.pattern_ctx(),
                ) {
                    Ok(mut pane) => {
                        if !pane_meta.tags.is_empty() {
                            pane.set_tags(pane_meta.tags.clone());
                        }
                        // Add to the first (default) window.
                        if let Some(window) = session.windows.first_mut() {
                            window.add_pane(pane);
                        }
                    }
                    Err(e) => {
                        warn!(
                            session_id = session_id,
                            error = %e,
                            "failed to restore pane in persisted session"
                        );
                    }
                }
            }

            // Restore workspace topology if saved. If the persisted data
            // predates workspace_state (old format), seed a default workspace
            // from whatever panes were restored so GetWorkspaces returns
            // something usable to the GUI attach flow.
            if !persisted_session.workspaces.is_empty() {
                session.workspace_state = persisted_session.workspaces.clone();
                session.active_workspace = persisted_session.active_workspace;
            } else {
                let pane_ids: Vec<PaneId> = session
                    .windows
                    .iter()
                    .flat_map(|w| w.panes.iter().map(|p| p.id))
                    .collect();
                if let Some(&first_pane) = pane_ids.first() {
                    let layout = if pane_ids.len() == 1 {
                        Some(LayoutSnapshot::Leaf {
                            pane_id: first_pane,
                        })
                    } else {
                        // Multi-pane session with no stored layout — leave
                        // layout as None so the client falls back to a flat
                        // cascade rather than guessing at split ratios.
                        None
                    };
                    session.workspace_state = vec![WorkspaceInfo {
                        id: 1,
                        name: "1".to_string(),
                        order: 0,
                        pane_ids,
                        focused_pane: Some(first_pane),
                        layout,
                    }];
                    session.active_workspace = 1;
                }
            }

            let pane_count = session.pane_count();
            tracing::info!(
                session_id = session_id,
                name = ?persisted_session.name,
                pane_count,
                workspaces = persisted_session.workspaces.len(),
                "restored session from persisted state"
            );

            let _ = self
                .event_tx
                .send(DaemonEvent::SessionCreated { session_id });
            self.sessions.insert(session_id, session);
            restored += pane_count;
        }

        restored
    }

    /// Graceful shutdown: destroy all sessions.
    pub fn shutdown(&mut self) {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in ids {
            self.destroy_session(id);
        }
    }
}
