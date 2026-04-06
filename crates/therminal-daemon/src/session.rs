//! Session manager: persistent sessions with PTY workers.
//!
//! Hierarchy: `SessionManager` -> `Session` -> `Window` -> `Pane`.
//! Each pane owns a PTY + headless `alacritty_terminal::Term` running
//! in a dedicated reader thread via the shared `PtyPaneCore`.
//!
//! Attach/detach sends a state snapshot (grid + cursor + scrollback),
//! not a byte replay.

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi;
use portable_pty::MasterPty;
use therminal_terminal::pty_runtime::{PtyPaneCore, PtyReaderHandler, TermSize};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use therminal_protocol::daemon::DaemonEvent;
pub use therminal_protocol::{PaneId, SessionId, WindowId};

// ── ID generation ───────────────────────────────────────────────────────

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_PANE_ID: AtomicU64 = AtomicU64::new(1);

fn next_session_id() -> SessionId {
    NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_window_id() -> WindowId {
    NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed)
}

fn next_pane_id() -> PaneId {
    NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed)
}

// ── State snapshot (sent on attach) ─────────────────────────────────────

/// Maximum number of scrollback lines included in a snapshot to avoid huge payloads.
const MAX_SNAPSHOT_SCROLLBACK: usize = 10_000;

/// A snapshot of a pane's terminal state, sent to the client on attach.
#[derive(Debug, Clone)]
pub struct PaneSnapshot {
    pub pane_id: PaneId,
    pub title: String,
    /// Scrollback history above the visible screen (oldest first).
    /// Each row is a Vec of (character, bold flag). Capped at [`MAX_SNAPSHOT_SCROLLBACK`] lines.
    pub scrollback: Vec<Vec<(char, bool)>>,
    /// Visible grid contents: Vec of rows, each row a Vec of (character, bold flag).
    pub grid: Vec<Vec<(char, bool)>>,
    /// Cursor position (column, line) in the visible grid.
    pub cursor_col: usize,
    pub cursor_line: usize,
    /// Grid dimensions.
    pub cols: usize,
    pub rows: usize,
}

/// Snapshot of all panes in a session (sent on attach).
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub name: Option<String>,
    pub panes: Vec<PaneSnapshot>,
}

// ── Headless EventListener (no GUI) ─────────────────────────────────────

/// Minimal event listener for headless Term instances in the daemon.
/// We don't have a window, so most events are logged/ignored.
#[derive(Clone)]
struct HeadlessListener;

impl EventListener for HeadlessListener {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => debug!(title, "headless term title changed"),
            TermEvent::Wakeup => { /* PTY reader handles this */ }
            _ => debug!(?event, "headless term event"),
        }
    }
}

// ── Daemon-side PtyReaderHandler ───────────────────────────────────────

/// Handler that feeds bytes to the headless Term and broadcasts `PaneOutput` events.
struct DaemonPtyHandler {
    event_tx: broadcast::Sender<DaemonEvent>,
    session_id: SessionId,
    pane_id: PaneId,
}

impl PtyReaderHandler for DaemonPtyHandler {
    type Listener = HeadlessListener;

    fn process_bytes(
        &mut self,
        processor: &mut ansi::Processor<ansi::StdSyncHandler>,
        term: &Arc<FairMutex<Term<HeadlessListener>>>,
        data: &[u8],
    ) {
        // Feed bytes to the headless terminal.
        {
            let mut term_guard = term.lock();
            processor.advance(&mut *term_guard, data);
        }

        // Broadcast pane output event to subscribed clients.
        let _ = self.event_tx.send(DaemonEvent::PaneOutput {
            session_id: self.session_id,
            pane_id: self.pane_id,
            data: data.to_vec(),
        });
    }

    fn on_eof(&mut self) {
        info!(pane_id = %self.pane_id, "PTY closed (EOF)");
    }
}

// ── Pane ────────────────────────────────────────────────────────────────

/// A single pane: owns a headless Term + PTY via `PtyPaneCore`.
#[allow(dead_code)]
pub struct Pane {
    pub id: PaneId,
    term: Arc<FairMutex<Term<HeadlessListener>>>,
    pty_writer: Box<dyn IoWrite + Send>,
    _pty_master: Box<dyn MasterPty + Send>,
    cols: u16,
    rows: u16,
}

impl Pane {
    /// Spawn a new pane with a PTY and headless terminal.
    fn spawn(
        cols: u16,
        rows: u16,
        event_tx: broadcast::Sender<DaemonEvent>,
        session_id: SessionId,
    ) -> Result<Self, therminal_terminal::pty::PtyError> {
        let id = next_pane_id();

        let handler = DaemonPtyHandler {
            event_tx,
            session_id,
            pane_id: id,
        };

        let mut core = PtyPaneCore::spawn(
            cols as usize,
            rows as usize,
            10_000,
            HeadlessListener,
            &therminal_terminal::pty::SpawnOptions::default(),
            handler,
        )?;

        let term = Arc::clone(core.term());
        let pty_writer = core.take_writer();
        let pty_master = core.take_pty_master();

        Ok(Self {
            id,
            term,
            pty_writer,
            _pty_master: pty_master,
            cols,
            rows,
        })
    }

    /// Get the number of columns.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Get the number of rows.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Write bytes to the pane's PTY (forwarding keystrokes).
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.pty_writer.write_all(data)?;
        self.pty_writer.flush()
    }

    /// Take a snapshot of the current terminal state, including scrollback history.
    pub fn snapshot(&self) -> PaneSnapshot {
        let term = self.term.lock();
        let grid = term.grid();
        let cols = term.columns();
        let rows = term.screen_lines();
        let cursor_point = grid.cursor.point;
        let history_size = grid.history_size();

        // Collect scrollback rows (oldest first), capped at MAX_SNAPSHOT_SCROLLBACK.
        let scrollback_lines = history_size.min(MAX_SNAPSHOT_SCROLLBACK);
        let mut scrollback = Vec::with_capacity(scrollback_lines);
        let start = -(scrollback_lines as i32);
        for line_idx in start..0 {
            let line = alacritty_terminal::index::Line(line_idx);
            let mut row = Vec::with_capacity(cols);
            for col_idx in 0..cols {
                let col = alacritty_terminal::index::Column(col_idx);
                let cell = &grid[line][col];
                row.push((cell.c, cell.flags.contains(CellFlags::BOLD)));
            }
            scrollback.push(row);
        }

        // Collect visible grid rows.
        let mut visible = Vec::with_capacity(rows);
        for line_idx in 0..rows {
            let line = alacritty_terminal::index::Line(line_idx as i32);
            let mut row = Vec::with_capacity(cols);
            for col_idx in 0..cols {
                let col = alacritty_terminal::index::Column(col_idx);
                let cell = &grid[line][col];
                row.push((cell.c, cell.flags.contains(CellFlags::BOLD)));
            }
            visible.push(row);
        }

        PaneSnapshot {
            pane_id: self.id,
            title: String::new(),
            scrollback,
            grid: visible,
            cursor_col: cursor_point.column.0,
            cursor_line: cursor_point.line.0 as usize,
            cols,
            rows,
        }
    }

    /// Resize the pane's PTY and terminal.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if let Err(e) = therminal_terminal::pty::resize(self._pty_master.as_ref(), cols, rows) {
            warn!(pane_id = %self.id, error = %e, "failed to resize PTY");
            return;
        }
        let mut term = self.term.lock();
        let size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        term.resize(size);
        self.cols = cols;
        self.rows = rows;
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        // The PTY master drop will close the PTY, causing the reader thread
        // to get EOF and exit. We don't join here to avoid blocking.
        debug!(pane_id = %self.id, "pane dropped");
    }
}

// ── Window ──────────────────────────────────────────────────────────────

/// A window within a session, containing one or more panes.
pub struct Window {
    pub id: WindowId,
    pub panes: Vec<Pane>,
}

impl Window {
    fn new() -> Self {
        Self {
            id: next_window_id(),
            panes: Vec::new(),
        }
    }

    /// Add a pane to this window.
    fn add_pane(&mut self, pane: Pane) {
        self.panes.push(pane);
    }

    /// Find a pane by ID.
    pub fn pane(&self, pane_id: PaneId) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == pane_id)
    }

    /// Find a mutable pane by ID.
    pub fn pane_mut(&mut self, pane_id: PaneId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == pane_id)
    }
}

// ── Session ─────────────────────────────────────────────────────────────

/// A persistent session containing windows and panes.
pub struct Session {
    pub id: SessionId,
    pub name: Option<String>,
    pub windows: Vec<Window>,
    pub created_at_secs: u64,
    event_tx: broadcast::Sender<DaemonEvent>,
}

impl Session {
    fn new(name: Option<String>, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
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
        }
    }

    /// Create a default window with a single pane.
    pub fn create_default_pane(
        &mut self,
        cols: u16,
        rows: u16,
    ) -> Result<&Pane, therminal_terminal::pty::PtyError> {
        let pane = Pane::spawn(cols, rows, self.event_tx.clone(), self.id)?;
        let mut window = Window::new();
        let pane_id = pane.id;
        window.add_pane(pane);
        self.windows.push(window);
        // Return a reference to the newly created pane
        Ok(self.windows.last().unwrap().pane(pane_id).unwrap())
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

// ── Session Manager ─────────────────────────────────────────────────────

/// Central registry of all sessions.
///
/// Owns the session map and provides CRUD + attach/detach operations.
/// Designed to be wrapped in `Arc<tokio::sync::Mutex<SessionManager>>`
/// for sharing across IPC handler tasks.
pub struct SessionManager {
    sessions: HashMap<SessionId, Session>,
    event_tx: broadcast::Sender<DaemonEvent>,
    /// Default pane dimensions for new sessions.
    default_cols: u16,
    default_rows: u16,
}

impl SessionManager {
    /// Create a new empty session manager.
    pub fn new(event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self {
            sessions: HashMap::new(),
            event_tx,
            default_cols: 80,
            default_rows: 24,
        }
    }

    /// Create a new session with a default window/pane.
    pub fn create_session(
        &mut self,
        name: Option<String>,
    ) -> Result<SessionId, therminal_terminal::pty::PtyError> {
        let mut session = Session::new(name, self.event_tx.clone());
        session.create_default_pane(self.default_cols, self.default_rows)?;

        let session_id = session.id;
        info!(session_id = session_id, "session created");

        // Broadcast creation event
        let _ = self
            .event_tx
            .send(DaemonEvent::SessionCreated { session_id });

        self.sessions.insert(session_id, session);
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
        if let Some(_session) = self.sessions.remove(&session_id) {
            info!(session_id = session_id, "session destroyed");
            let _ = self
                .event_tx
                .send(DaemonEvent::SessionDestroyed { session_id });
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

    /// Split a pane: creates a new sibling pane in the same window.
    /// Returns the new pane's ID. The `_horizontal` flag is accepted for
    /// future layout use but currently has no effect on the headless daemon.
    pub fn split_pane(&mut self, pane_id: PaneId, _horizontal: bool) -> Result<PaneId, String> {
        // Find which session and window this pane belongs to.
        let session_id = self
            .sessions
            .values()
            .find(|s| {
                s.windows
                    .iter()
                    .any(|w| w.panes.iter().any(|p| p.id == pane_id))
            })
            .map(|s| s.id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;

        let session = self.sessions.get_mut(&session_id).unwrap();
        let window = session
            .windows
            .iter_mut()
            .find(|w| w.panes.iter().any(|p| p.id == pane_id))
            .unwrap();

        let new_pane = Pane::spawn(
            self.default_cols,
            self.default_rows,
            self.event_tx.clone(),
            session_id,
        )
        .map_err(|e| format!("failed to spawn pane: {e}"))?;

        let new_id = new_pane.id;
        window.add_pane(new_pane);
        Ok(new_id)
    }

    /// Kill (destroy) a single pane by ID. Removes it from its window.
    /// If the window becomes empty, removes the window. If the session
    /// becomes empty, destroys the session.
    pub fn kill_pane(&mut self, pane_id: PaneId) -> Result<(), String> {
        let session_id = self
            .sessions
            .values()
            .find(|s| {
                s.windows
                    .iter()
                    .any(|w| w.panes.iter().any(|p| p.id == pane_id))
            })
            .map(|s| s.id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;

        let session = self.sessions.get_mut(&session_id).unwrap();
        for window in &mut session.windows {
            if let Some(pos) = window.panes.iter().position(|p| p.id == pane_id) {
                window.panes.remove(pos);
                break;
            }
        }
        // Remove empty windows
        session.windows.retain(|w| !w.panes.is_empty());
        // If no windows left, destroy session
        if session.windows.is_empty() {
            self.destroy_session(session_id);
        }
        Ok(())
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

    /// Graceful shutdown: destroy all sessions.
    pub fn shutdown(&mut self) {
        let ids: Vec<SessionId> = self.sessions.keys().copied().collect();
        for id in ids {
            self.destroy_session(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn session_manager_destroy_nonexistent() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        assert!(!mgr.destroy_session(999999));
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
}
