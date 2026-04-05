//! Session manager: persistent sessions with PTY workers.
//!
//! Hierarchy: `SessionManager` -> `Session` -> `Window` -> `Pane`.
//! Each pane owns a PTY + headless `alacritty_terminal::Term` running
//! in a dedicated reader thread.
//!
//! Attach/detach sends a state snapshot (grid + cursor + scrollback),
//! not a byte replay.

use std::collections::HashMap;
use std::io::{Read as IoRead, Write as IoWrite};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;
use portable_pty::MasterPty;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use therminal_protocol::daemon::DaemonEvent;

// ── IDs ──────────────────────────────────────────────────────────────────

pub type SessionId = String;
pub type WindowId = String;
pub type PaneId = String;

fn gen_id(prefix: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros();
    format!("{prefix}-{ts:x}")
}

// ── State snapshot (sent on attach) ─────────────────────────────────────

/// A snapshot of a pane's terminal state, sent to the client on attach.
#[derive(Debug, Clone)]
pub struct PaneSnapshot {
    pub pane_id: PaneId,
    pub title: String,
    /// Grid contents: Vec of rows, each row a Vec of (character, bold flag).
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

// ── Dimensions adapter ──────────────────────────────────────────────────

struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

// ── Pane ────────────────────────────────────────────────────────────────

/// A single pane: owns a headless Term + PTY.
#[allow(dead_code)]
pub struct Pane {
    pub id: PaneId,
    term: Arc<FairMutex<Term<HeadlessListener>>>,
    pty_writer: Box<dyn IoWrite + Send>,
    _pty_master: Box<dyn MasterPty + Send>,
    reader_handle: Option<thread::JoinHandle<()>>,
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
        let id = gen_id("pane");

        // Create headless alacritty_terminal::Term
        let term_config = TermConfig {
            scrolling_history: 10_000,
            ..Default::default()
        };
        let term_size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(term_config, &term_size, HeadlessListener);
        let term = Arc::new(FairMutex::new(term));

        // Spawn PTY
        let (pty_master, _child) = therminal_terminal::pty::spawn_shell(cols, rows)?;

        let pty_reader = pty_master
            .try_clone_reader()
            .expect("failed to clone PTY reader");
        let pty_writer = pty_master.take_writer().expect("failed to get PTY writer");

        // Spawn reader thread
        let term_for_reader = Arc::clone(&term);
        let pane_id = id.clone();
        let sess_id = session_id.clone();
        let reader_handle = thread::Builder::new()
            .name(format!("pty-reader-{}", &id))
            .spawn(move || {
                pane_reader_loop(pty_reader, term_for_reader, event_tx, sess_id, pane_id);
            })
            .expect("failed to spawn PTY reader thread");

        Ok(Self {
            id,
            term,
            pty_writer,
            _pty_master: pty_master,
            reader_handle: Some(reader_handle),
            cols,
            rows,
        })
    }

    /// Write bytes to the pane's PTY (forwarding keystrokes).
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.pty_writer.write_all(data)?;
        self.pty_writer.flush()
    }

    /// Take a snapshot of the current terminal state.
    pub fn snapshot(&self) -> PaneSnapshot {
        let term = self.term.lock();
        let cols = term.columns();
        let rows = term.screen_lines();
        let cursor_point = term.grid().cursor.point;

        let mut grid = Vec::with_capacity(rows);
        for line_idx in 0..rows {
            let line = alacritty_terminal::index::Line(line_idx as i32);
            let mut row = Vec::with_capacity(cols);
            for col_idx in 0..cols {
                let col = alacritty_terminal::index::Column(col_idx);
                let cell = &term.grid()[line][col];
                let ch = cell.c;
                let bold = cell.flags.contains(CellFlags::BOLD);
                row.push((ch, bold));
            }
            grid.push(row);
        }

        PaneSnapshot {
            pane_id: self.id.clone(),
            title: String::new(), // Could extract from Term if needed
            grid,
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

// ── Pane reader loop (runs in a dedicated thread) ───────────────────────

fn pane_reader_loop(
    mut reader: Box<dyn IoRead + Send>,
    term: Arc<FairMutex<Term<HeadlessListener>>>,
    event_tx: broadcast::Sender<DaemonEvent>,
    session_id: SessionId,
    pane_id: PaneId,
) {
    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();
    let mut buf = [0u8; 4096];

    debug!(pane_id = %pane_id, "pane reader thread started");

    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                info!(pane_id = %pane_id, "PTY closed (EOF)");
                break;
            }
            Ok(n) => {
                // Feed bytes to the headless terminal
                {
                    let mut term_guard = term.lock();
                    processor.advance(&mut *term_guard, &buf[..n]);
                }

                // Broadcast pane output event to subscribed clients
                let _ = event_tx.send(DaemonEvent::PaneOutput {
                    session_id: session_id.clone(),
                    pane_id: pane_id.clone(),
                    data: buf[..n].to_vec(),
                });
            }
            Err(e) => {
                warn!(pane_id = %pane_id, error = %e, "PTY read error");
                break;
            }
        }
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
            id: gen_id("win"),
            panes: Vec::new(),
        }
    }

    /// Add a pane to this window.
    fn add_pane(&mut self, pane: Pane) {
        self.panes.push(pane);
    }

    /// Find a pane by ID.
    pub fn pane(&self, pane_id: &str) -> Option<&Pane> {
        self.panes.iter().find(|p| p.id == pane_id)
    }

    /// Find a mutable pane by ID.
    pub fn pane_mut(&mut self, pane_id: &str) -> Option<&mut Pane> {
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
        let id = gen_id("sess");
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
        let pane = Pane::spawn(cols, rows, self.event_tx.clone(), self.id.clone())?;
        let mut window = Window::new();
        let pane_id = pane.id.clone();
        window.add_pane(pane);
        self.windows.push(window);
        // Return a reference to the newly created pane
        Ok(self.windows.last().unwrap().pane(&pane_id).unwrap())
    }

    /// Take a snapshot of the entire session for attach.
    pub fn snapshot(&self) -> SessionSnapshot {
        let panes: Vec<PaneSnapshot> = self
            .windows
            .iter()
            .flat_map(|w| w.panes.iter().map(|p| p.snapshot()))
            .collect();

        SessionSnapshot {
            session_id: self.id.clone(),
            name: self.name.clone(),
            panes,
        }
    }

    /// Find a mutable pane across all windows.
    pub fn find_pane_mut(&mut self, pane_id: &str) -> Option<&mut Pane> {
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

        let session_id = session.id.clone();
        info!(session_id = %session_id, "session created");

        // Broadcast creation event
        let _ = self.event_tx.send(DaemonEvent::SessionCreated {
            session_id: session_id.clone(),
        });

        self.sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }

    /// List all session IDs.
    pub fn list_sessions(&self) -> Vec<SessionId> {
        self.sessions.keys().cloned().collect()
    }

    /// Get session info (id, name, created_at).
    pub fn get_session_info(&self, session_id: &str) -> Option<(SessionId, Option<String>, u64)> {
        self.sessions
            .get(session_id)
            .map(|s| (s.id.clone(), s.name.clone(), s.created_at_secs))
    }

    /// Attach to a session: returns a snapshot of the current terminal state.
    pub fn attach(&self, session_id: &str) -> Option<SessionSnapshot> {
        self.sessions.get(session_id).map(|s| s.snapshot())
    }

    /// Write input data to a specific pane in a session.
    pub fn write_to_pane(
        &mut self,
        session_id: &str,
        pane_id: &str,
        data: &[u8],
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let pane = session
            .find_pane_mut(pane_id)
            .ok_or_else(|| format!("pane not found: {pane_id}"))?;
        pane.write(data).map_err(|e| format!("write error: {e}"))
    }

    /// Destroy a session and all its panes.
    pub fn destroy_session(&mut self, session_id: &str) -> bool {
        if let Some(_session) = self.sessions.remove(session_id) {
            info!(session_id = %session_id, "session destroyed");
            let _ = self.event_tx.send(DaemonEvent::SessionDestroyed {
                session_id: session_id.to_string(),
            });
            true
        } else {
            false
        }
    }

    /// Number of active sessions.
    pub fn session_count(&self) -> u32 {
        self.sessions.len() as u32
    }

    /// Graceful shutdown: destroy all sessions.
    pub fn shutdown(&mut self) {
        let ids: Vec<SessionId> = self.sessions.keys().cloned().collect();
        for id in ids {
            self.destroy_session(&id);
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
    fn gen_id_has_prefix() {
        let id = gen_id("test");
        assert!(id.starts_with("test-"));
    }

    #[test]
    fn session_manager_create_and_list() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);

        // Create with a mock - we can't easily spawn real PTYs in unit tests
        // without a TTY, so test the non-PTY parts.
        assert_eq!(mgr.session_count(), 0);
        assert!(mgr.list_sessions().is_empty());
    }

    #[test]
    fn session_manager_destroy_nonexistent() {
        let tx = make_event_tx();
        let mut mgr = SessionManager::new(tx);
        assert!(!mgr.destroy_session("does-not-exist"));
    }

    #[test]
    fn window_new_has_id() {
        let w = Window::new();
        assert!(w.id.starts_with("win-"));
        assert!(w.panes.is_empty());
    }

    #[test]
    fn session_new_has_id_and_timestamp() {
        let tx = make_event_tx();
        let session = Session::new(Some("test".to_string()), tx);
        assert!(session.id.starts_with("sess-"));
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

        let info = mgr.get_session_info(&session_id).unwrap();
        assert_eq!(info.1.as_deref(), Some("test"));

        let snapshot = mgr.attach(&session_id).unwrap();
        assert_eq!(snapshot.session_id, session_id);
        assert!(!snapshot.panes.is_empty());

        assert!(mgr.destroy_session(&session_id));
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
