//! Session: a persistent session containing windows and panes.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use therminal_terminal::TaggedHarnessEvent;
use tokio::sync::broadcast;

use therminal_protocol::daemon::{DaemonEvent, WorkspaceInfo};
pub use therminal_protocol::{PaneId, SessionId, WorkspaceId};

use super::next_session_id;
use super::pane::{Pane, PaneDispatchCtx};
use super::snapshots::{PaneSnapshot, SessionSnapshot};
use super::window::Window;

/// A persistent session containing windows and panes.
pub struct Session {
    pub id: SessionId,
    pub name: Option<String>,
    pub windows: Vec<Window>,
    pub created_at_secs: u64,
    pub(super) event_tx: broadcast::Sender<DaemonEvent>,
    /// Workspace topology as reported by the app. The daemon stores this
    /// so MCP tools and reattaching clients can query it.
    pub workspace_state: Vec<WorkspaceInfo>,
    /// Which workspace the app is currently viewing.
    pub active_workspace: WorkspaceId,
}

impl Session {
    pub(super) fn new(name: Option<String>, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        let id = next_session_id();
        let created_at_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        Self {
            id,
            name,
            windows: Vec::new(),
            created_at_secs,
            event_tx,
            workspace_state: Vec::new(),
            active_workspace: 1,
        }
    }

    /// Create a default window with a single pane.
    pub fn create_default_pane(
        &mut self,
        cols: u16,
        rows: u16,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
        osc_registry: Arc<therminal_terminal::OscHandlerRegistry>,
        harness_event_tx: Option<std::sync::mpsc::Sender<TaggedHarnessEvent>>,
        pattern_ctx: PaneDispatchCtx,
    ) -> Result<&Pane, therminal_terminal::pty::PtyError> {
        let pane = Pane::spawn(
            cols,
            rows,
            self.event_tx.clone(),
            self.id,
            spawn_options,
            osc_registry,
            harness_event_tx,
            pattern_ctx,
        )?;
        let mut window = Window::new();
        let pane_id = pane.id;
        window.add_pane(pane);
        self.windows.push(window);
        // Return a reference to the newly created pane
        self.windows
            .last()
            .and_then(|w| w.pane(pane_id))
            .ok_or_else(|| {
                therminal_terminal::pty::PtyError::Integration(format!(
                    "pane {pane_id} vanished immediately after creation"
                ))
            })
    }

    /// Take a snapshot of the entire session for attach.
    pub fn snapshot(&self) -> SessionSnapshot {
        let panes: Vec<PaneSnapshot> = self
            .windows
            .iter()
            .flat_map(|w| w.panes.iter().map(|p| p.snapshot()))
            .collect();

        SessionSnapshot {
            session_id: self.id,
            name: self.name.clone(),
            panes,
        }
    }

    /// Find a mutable pane across all windows.
    pub fn find_pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Pane> {
        self.windows.iter_mut().find_map(|w| w.pane_mut(pane_id))
    }

    /// Total number of panes in this session.
    pub fn pane_count(&self) -> usize {
        self.windows.iter().map(|w| w.panes.len()).sum()
    }
}
