//! Session manager: persistent sessions with PTY workers.
//!
//! Hierarchy: `SessionManager` -> `Session` -> `Window` -> `Pane`.
//! Each pane owns a PTY + headless `alacritty_terminal::Term` running
//! in a dedicated reader thread via the shared `PtyPaneCore`.
//!
//! Attach sends a structured `PaneStateSnapshot` (mode flags, cursor,
//! visible grid) that the GUI replays via synthesized escape sequences
//! onto a freshly-constructed local `Term`. See tn-zamd.

use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi;
use portable_pty::MasterPty;
use therminal_terminal::agent_registry::{AgentEntry, AgentRegistry, AgentStatus};
use therminal_terminal::event_log::{DEFAULT_MAX_ENTRIES, EventLog, StoredEvent};
use therminal_terminal::interceptor::{InterceptedEvent, TherminalInterceptor};
use therminal_terminal::osc633::{CommandBlock, CommandTracker};
use therminal_terminal::pty_runtime::{PtyPaneCore, PtyReaderHandler, TermSize};
use therminal_terminal::region_index::RegionIndex;
use therminal_terminal::state_inference::{
    AgentDetailsSnapshot, AgentStateInference, InferenceConfig,
};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use therminal_protocol::daemon::{DaemonEvent, LayoutSnapshot, WorkspaceInfo};
pub use therminal_protocol::{PaneId, SessionId, WindowId, WorkspaceId};

#[cfg(unix)]
use std::os::unix::io::FromRawFd;

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
///
/// Also runs a `TherminalInterceptor` to capture OSC shell-integration marks
/// and feeds them into a shared `RegionIndex` for semantic history queries.
struct DaemonPtyHandler {
    event_tx: broadcast::Sender<DaemonEvent>,
    session_id: SessionId,
    pane_id: PaneId,
    interceptor: TherminalInterceptor,
    interceptor_rx: std::sync::mpsc::Receiver<InterceptedEvent>,
    region_index: Arc<Mutex<RegionIndex>>,
    /// Shared cwd updated from OSC 7 events.
    cwd: Arc<Mutex<String>>,
    /// Shared agent state inference engine. Reader thread feeds bytes;
    /// MCP handlers snapshot state on demand.
    inference: Arc<Mutex<AgentStateInference>>,
}

impl PtyReaderHandler for DaemonPtyHandler {
    type Listener = HeadlessListener;

    fn process_bytes(
        &mut self,
        processor: &mut ansi::Processor<ansi::StdSyncHandler>,
        term: &Arc<FairMutex<Term<HeadlessListener>>>,
        data: &[u8],
    ) {
        // Feed bytes to the headless terminal via the interceptor so we
        // capture OSC shell-integration marks for the RegionIndex.
        {
            let mut term_guard = term.lock();
            processor.advance_with_interceptor(&mut *term_guard, &mut self.interceptor, data);
        }

        // Feed the same bytes into the agent state inference engine. The
        // lock is held only for the duration of `feed_bytes`, which is
        // cheap (line buffer + ANSI strip + pattern match on recent lines).
        if let Ok(mut inf) = self.inference.lock() {
            inf.feed_bytes(data);
        }

        // Drain intercepted events into the region index and update cwd.
        while let Ok(event) = self.interceptor_rx.try_recv() {
            if let InterceptedEvent::CurrentDirectory(ref path) = event
                && let Ok(mut cwd) = self.cwd.lock()
            {
                *cwd = path.clone();
            }
            if let Ok(mut idx) = self.region_index.lock() {
                idx.push_event(&event);
            }
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
        // Broadcast PaneExited so RemotePty backends in the GUI can tear
        // down their local mirror. portable_pty does not surface a child
        // exit code through the reader thread today (tn-5rm0 follow-up),
        // so exit_code is None for now.
        let _ = self.event_tx.send(DaemonEvent::PaneExited {
            session_id: self.session_id,
            pane_id: self.pane_id,
            exit_code: None,
        });
    }
}

// ── FdPtyMaster (Unix-only) ─────────────────────────────────────────────

/// A `MasterPty` implementation backed by a raw file descriptor received
/// via SCM_RIGHTS during daemon handoff.
///
/// Owns the FD and closes it on drop. Provides reader/writer cloning via
/// `dup()` so the PTY reader thread and writer can operate independently.
#[cfg(unix)]
struct FdPtyMaster {
    fd: std::os::unix::io::RawFd,
    took_writer: std::cell::RefCell<bool>,
}

#[cfg(unix)]
impl FdPtyMaster {
    fn new(fd: std::os::unix::io::RawFd) -> Self {
        Self {
            fd,
            took_writer: std::cell::RefCell::new(false),
        }
    }
}

#[cfg(unix)]
impl MasterPty for FdPtyMaster {
    fn resize(&self, size: portable_pty::PtySize) -> Result<(), anyhow::Error> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        let ret = unsafe { libc::ioctl(self.fd, libc::TIOCSWINSZ, &ws as *const _) };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(())
        }
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ret = unsafe { libc::ioctl(self.fd, libc::TIOCGWINSZ, &mut ws as *mut _) };
        if ret < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(portable_pty::PtySize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                pixel_width: ws.ws_xpixel,
                pixel_height: ws.ws_ypixel,
            })
        }
    }

    fn try_clone_reader(&self) -> Result<Box<dyn std::io::Read + Send>, anyhow::Error> {
        let dup_fd = unsafe { libc::dup(self.fd) };
        if dup_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        Ok(Box::new(file))
    }

    fn take_writer(&self) -> Result<Box<dyn std::io::Write + Send>, anyhow::Error> {
        if *self.took_writer.borrow() {
            anyhow::bail!("cannot take writer more than once");
        }
        *self.took_writer.borrow_mut() = true;
        let dup_fd = unsafe { libc::dup(self.fd) };
        if dup_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        Ok(Box::new(file))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        match unsafe { libc::tcgetpgrp(self.fd) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        Some(self.fd)
    }

    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}

#[cfg(unix)]
impl Drop for FdPtyMaster {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
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
    region_index: Arc<Mutex<RegionIndex>>,
    cols: u16,
    rows: u16,
    /// Current working directory, updated from OSC 7 events.
    cwd: Arc<Mutex<String>>,
    /// Shell command used when this pane was spawned.
    shell: String,
    /// Shared agent state inference engine. Cloned into the reader thread's
    /// `DaemonPtyHandler` so PTY bytes feed it; daemon-side accessors
    /// snapshot it for MCP `terminal.agents.get_details`.
    inference: Arc<Mutex<AgentStateInference>>,
    /// Shared OSC 633 command tracker. The reader thread's interceptor
    /// holds the same `Arc` so PTY bytes feed it; daemon-side accessors
    /// snapshot it for MCP `terminal.semantic.query_commands`.
    command_tracker: Arc<Mutex<CommandTracker>>,
    /// Per-pane in-memory structured event log. Backs the
    /// `terminal.panes.query_events` MCP tool. The daemon does not write
    /// JSONL to disk for these — only the rolling in-memory ring is used,
    /// capped at `DEFAULT_MAX_ENTRIES` (5000) events.
    event_log: Arc<Mutex<EventLog>>,
}

impl Pane {
    /// Spawn a new pane with a PTY and headless terminal.
    fn spawn(
        cols: u16,
        rows: u16,
        event_tx: broadcast::Sender<DaemonEvent>,
        session_id: SessionId,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
    ) -> Result<Self, therminal_terminal::pty::PtyError> {
        let id = next_pane_id();

        let region_index = Arc::new(Mutex::new(RegionIndex::new()));
        let command_tracker = Arc::new(Mutex::new(CommandTracker::new()));
        let (interceptor, interceptor_rx) =
            TherminalInterceptor::with_defaults_and_tracker(Arc::clone(&command_tracker));

        // Initialize cwd from spawn options; OSC 7 events will update it later.
        let cwd = Arc::new(Mutex::new(spawn_options.cwd.clone()));

        let inference = Arc::new(Mutex::new(AgentStateInference::new(InferenceConfig {
            session_id: format!("daemon-pane-{id}"),
            child_pid: 0,
            agent_type: None,
            working_dir: if spawn_options.cwd.is_empty() {
                None
            } else {
                Some(spawn_options.cwd.clone())
            },
        })));

        let handler = DaemonPtyHandler {
            event_tx,
            session_id,
            pane_id: id,
            interceptor,
            interceptor_rx,
            region_index: Arc::clone(&region_index),
            cwd: Arc::clone(&cwd),
            inference: Arc::clone(&inference),
        };

        let mut core = PtyPaneCore::spawn(
            cols as usize,
            rows as usize,
            10_000,
            HeadlessListener,
            spawn_options,
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
            region_index,
            cols,
            rows,
            cwd,
            shell: spawn_options.shell.clone(),
            inference,
            command_tracker,
            event_log: Arc::new(Mutex::new(EventLog::in_memory(DEFAULT_MAX_ENTRIES))),
        })
    }

    /// Reconstruct a pane from a raw PTY master FD received via SCM_RIGHTS (Unix only).
    ///
    /// Creates a new headless `Term`, wraps the FD in a `FdPtyMaster`, clones a
    /// reader/writer from it, and spawns a reader thread -- mirroring `Pane::spawn`
    /// but without spawning a new shell.
    #[cfg(unix)]
    fn from_raw_fd(
        pane_id: PaneId,
        cols: u16,
        rows: u16,
        raw_fd: std::os::unix::io::RawFd,
        event_tx: broadcast::Sender<DaemonEvent>,
        session_id: SessionId,
    ) -> Result<Self, therminal_terminal::pty::PtyError> {
        let pty_master = Box::new(FdPtyMaster::new(raw_fd));

        let region_index = Arc::new(Mutex::new(RegionIndex::new()));
        let command_tracker = Arc::new(Mutex::new(CommandTracker::new()));
        let (interceptor, interceptor_rx) =
            TherminalInterceptor::with_defaults_and_tracker(Arc::clone(&command_tracker));

        // FD handoff panes don't have spawn options; cwd will be updated by OSC 7.
        let cwd = Arc::new(Mutex::new(String::new()));

        let inference = Arc::new(Mutex::new(AgentStateInference::new(InferenceConfig {
            session_id: format!("daemon-pane-{pane_id}"),
            child_pid: 0,
            agent_type: None,
            working_dir: None,
        })));

        let handler = DaemonPtyHandler {
            event_tx: event_tx.clone(),
            session_id,
            pane_id,
            interceptor,
            interceptor_rx,
            region_index: Arc::clone(&region_index),
            cwd: Arc::clone(&cwd),
            inference: Arc::clone(&inference),
        };

        // Create headless Term.
        let term_config = alacritty_terminal::term::Config {
            scrolling_history: 10_000,
            ..Default::default()
        };
        let term_size = therminal_terminal::pty_runtime::TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = alacritty_terminal::term::Term::new(term_config, &term_size, HeadlessListener);
        let term = Arc::new(FairMutex::new(term));

        // Clone reader and writer from the FD.
        let pty_reader = pty_master
            .try_clone_reader()
            .map_err(therminal_terminal::pty::PtyError::Open)?;
        let pty_writer = pty_master
            .take_writer()
            .map_err(therminal_terminal::pty::PtyError::Open)?;

        // Spawn reader thread.
        let term_for_reader = Arc::clone(&term);
        std::thread::Builder::new()
            .name(format!("pty-reader-{pane_id}"))
            .spawn(move || {
                therminal_terminal::pty_runtime::reader_loop_external(
                    pty_reader,
                    term_for_reader,
                    handler,
                );
            })
            .map_err(|e| {
                therminal_terminal::pty::PtyError::Open(anyhow::anyhow!(
                    "failed to spawn reader thread: {e}"
                ))
            })?;

        info!(pane_id, cols, rows, "restored pane from handoff FD");

        Ok(Self {
            id: pane_id,
            term,
            pty_writer,
            _pty_master: pty_master,
            region_index,
            cols,
            rows,
            cwd,
            shell: String::new(), // Unknown for handoff panes
            inference,
            command_tracker,
            event_log: Arc::new(Mutex::new(EventLog::in_memory(DEFAULT_MAX_ENTRIES))),
        })
    }

    /// Snapshot the per-pane agent inference engine. Returns a plain DTO
    /// suitable for serialising into the MCP `terminal.agents.get_details`
    /// response. Holds the inference lock only for the duration of the
    /// snapshot clone.
    pub fn agent_details_snapshot(&self) -> AgentDetailsSnapshot {
        self.inference
            .lock()
            .map(|inf| inf.snapshot())
            .unwrap_or_default()
    }

    /// Snapshot the per-pane OSC 633 `CommandTracker`. Returns a plain
    /// `Vec<CommandBlock>` cloned under the lock so the daemon can serve
    /// `terminal.semantic.query_commands` without holding the lock for the
    /// duration of the request.
    pub fn command_tracker_snapshot(&self) -> Vec<CommandBlock> {
        self.command_tracker
            .lock()
            .map(|t| t.snapshot())
            .unwrap_or_default()
    }

    /// Test-only handle to the shared `CommandTracker` Arc. Lets unit
    /// tests inject OSC 633 marks (mirroring what the reader thread's
    /// interceptor does in production) and then read back via the public
    /// snapshot path.
    #[cfg(test)]
    pub fn command_tracker_arc(&self) -> Arc<Mutex<CommandTracker>> {
        Arc::clone(&self.command_tracker)
    }

    /// Snapshot the per-pane in-memory event log. Returns events filtered
    /// by `since_timestamp_secs` (inclusive) and capped at `limit`. Source
    /// is the rolling in-memory ring (`DEFAULT_MAX_ENTRIES` cap), no JSONL
    /// file is read. Returns oldest-first within the truncated window.
    pub fn event_log_snapshot(
        &self,
        since_timestamp_secs: Option<u64>,
        limit: usize,
    ) -> Vec<StoredEvent> {
        self.event_log
            .lock()
            .map(|log| log.snapshot(since_timestamp_secs, limit))
            .unwrap_or_default()
    }

    /// Test-only: shared event log Arc, so unit tests can directly inject
    /// `SessionEvent`s without driving them through the full pty pipeline.
    #[cfg(test)]
    pub fn event_log_arc(&self) -> Arc<Mutex<EventLog>> {
        Arc::clone(&self.event_log)
    }

    /// Access the pane's semantic region index.
    pub fn region_index(&self) -> &Arc<Mutex<RegionIndex>> {
        &self.region_index
    }

    /// Get the number of columns.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Get the number of rows.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Get the current working directory (from OSC 7 or initial spawn).
    pub fn cwd(&self) -> String {
        self.cwd.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Exit code of the most recently finished command, derived from OSC 633
    /// `D` marks captured in the region index. Returns `None` if no command
    /// has finished yet (or the shell isn't emitting OSC 633).
    ///
    /// Scans the region index backwards for the most recent `Output` or
    /// `Error` region that has an `exit_code` metadata entry.
    pub fn last_exit_code(&self) -> Option<i32> {
        use therminal_terminal::region_index::RegionKind;
        let idx = self.region_index.lock().ok()?;
        for region in idx.regions().iter().rev() {
            if matches!(region.kind, RegionKind::Output | RegionKind::Error)
                && let Some(code) = region.metadata.get("exit_code")
                && let Ok(n) = code.parse::<i32>()
            {
                return Some(n);
            }
        }
        None
    }

    /// Get the shell command used when this pane was spawned.
    pub fn shell(&self) -> &str {
        &self.shell
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
            cursor_line: (cursor_point.line.0.max(0) as usize).min(rows.saturating_sub(1)),
            cols,
            rows,
        }
    }

    /// Capture a structured state snapshot for tn-zamd replay.
    ///
    /// Reads TermMode bits, cursor position, dimensions, and the visible
    /// grid. The lock is held only for the copy into owned types and
    /// released before returning.
    pub fn snapshot_state(&self) -> therminal_protocol::daemon::PaneStateSnapshot {
        use alacritty_terminal::term::TermMode;
        use therminal_protocol::daemon::{PaneModeFlags, PaneStateSnapshot};

        let term = self.term.lock();
        let mode = *term.mode();
        let cols = term.columns();
        let rows = term.screen_lines();
        let grid = term.grid();
        let cursor_point = grid.cursor.point;

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

        let modes = PaneModeFlags {
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
        };

        PaneStateSnapshot {
            version: PaneStateSnapshot::CURRENT_VERSION,
            cols: cols as u16,
            rows: rows as u16,
            modes,
            cursor_col: cursor_point.column.0 as u16,
            cursor_line: (cursor_point.line.0.max(0) as usize).min(rows.saturating_sub(1)) as u16,
            grid_chars,
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
    /// Workspace topology as reported by the app. The daemon stores this
    /// so MCP tools and reattaching clients can query it.
    pub workspace_state: Vec<WorkspaceInfo>,
    /// Which workspace the app is currently viewing.
    pub active_workspace: WorkspaceId,
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
    ) -> Result<&Pane, therminal_terminal::pty::PtyError> {
        let pane = Pane::spawn(cols, rows, self.event_tx.clone(), self.id, spawn_options)?;
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
    /// Optional persistence handle for debounced state saving.
    persistence: Option<crate::persistence::PersistenceHandle>,
    /// Central registry of all detected agents across panes.
    agent_registry: AgentRegistry,
    /// Per-pane agent capacity cache fed by the Claude state poller.
    pane_capacity: Arc<crate::pane_capacity::PaneCapacityCache>,
}

impl SessionManager {
    /// Create a new empty session manager.
    pub fn new(event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self {
            sessions: HashMap::new(),
            event_tx,
            default_cols: 80,
            default_rows: 24,
            persistence: None,
            agent_registry: AgentRegistry::new(),
            pane_capacity: crate::pane_capacity::PaneCapacityCache::shared(),
        }
    }

    /// Shared handle to the per-pane capacity cache. Cloned by `ensure.rs`
    /// into the Claude state poller bridge task so it can write entries
    /// without holding the session manager mutex.
    pub fn pane_capacity_cache(&self) -> Arc<crate::pane_capacity::PaneCapacityCache> {
        Arc::clone(&self.pane_capacity)
    }

    /// Look up the most recent agent capacity entry for a pane. Returns a
    /// clone of the small DTO; the cache stays locked only briefly.
    pub fn pane_capacity(
        &self,
        pane_id: PaneId,
    ) -> Option<crate::pane_capacity::PaneCapacityEntry> {
        self.pane_capacity.get(pane_id)
    }

    /// Attach a persistence handle for debounced state saving.
    pub fn set_persistence(&mut self, handle: crate::persistence::PersistenceHandle) {
        self.persistence = Some(handle);
    }

    /// Notify the persistence layer that session state has changed.
    fn mark_dirty(&self) {
        if let Some(ref handle) = self.persistence {
            handle.mark_dirty();
        }
    }

    /// Subscribe to daemon events via the broadcast channel.
    ///
    /// Returns a new `broadcast::Receiver` that will receive all future
    /// `DaemonEvent`s (including `PaneOutput`). Used by long-running MCP
    /// tools like `wait_for_output` that need to watch the event stream.
    pub fn subscribe_events(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

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
            .create_default_pane(self.default_cols, self.default_rows, spawn_options)?
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
    pub fn resize_pane(&mut self, pane_id: PaneId, cols: u16, rows: u16) -> Result<(), String> {
        for session in self.sessions.values_mut() {
            if let Some(pane) = session.find_pane_mut(pane_id) {
                pane.resize(cols, rows);
                return Ok(());
            }
        }
        Err(format!("pane not found: {pane_id}"))
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

    /// Test-only: get the shared command tracker `Arc` for a pane so
    /// tests can inject OSC 633 marks bypassing the PTY reader thread.
    #[cfg(test)]
    pub fn pane_command_tracker_arc(&self, pane_id: PaneId) -> Option<Arc<Mutex<CommandTracker>>> {
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
    pub fn pane_event_log_arc(&self, pane_id: PaneId) -> Option<Arc<Mutex<EventLog>>> {
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

    /// Split a pane: creates a new sibling pane in the same window.
    /// Returns the new pane's ID. The `_horizontal` flag is accepted for
    /// future layout use but currently has no effect on the headless daemon.
    pub fn split_pane(&mut self, pane_id: PaneId, _horizontal: bool) -> Result<PaneId, String> {
        self.split_pane_with_options(
            pane_id,
            _horizontal,
            &therminal_terminal::pty::SpawnOptions::default(),
        )
    }

    /// Split a pane with custom spawn options for the new pane's PTY.
    pub fn split_pane_with_options(
        &mut self,
        pane_id: PaneId,
        _horizontal: bool,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
    ) -> Result<PaneId, String> {
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
            spawn_options,
        )
        .map_err(|e| format!("failed to spawn pane: {e}"))?;

        let new_id = new_pane.id;
        window.add_pane(new_pane);
        self.mark_dirty();
        Ok(new_id)
    }

    /// Kill (destroy) a single pane by ID. Removes it from its window.
    /// If the window becomes empty, removes the window. If the session
    /// becomes empty, destroys the session.
    pub fn kill_pane(&mut self, pane_id: PaneId) -> Result<(), String> {
        // Unregister any agent tracked for this pane.
        self.agent_registry.unregister(pane_id);

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
        // If no windows left, destroy session (which also marks dirty)
        if session.windows.is_empty() {
            self.destroy_session(session_id);
        } else {
            self.mark_dirty();
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
        let _ = self.event_tx.send(DaemonEvent::WorkspaceChanged {
            session_id,
            active_workspace,
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

    /// Return the ID of the first (default) session, if any.
    pub fn default_session_id(&self) -> Option<SessionId> {
        self.sessions.keys().next().copied()
    }

    // ── Agent registry ─────────────────────────────────────────────────

    /// Access the agent registry (read-only).
    pub fn agent_registry(&self) -> &AgentRegistry {
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
    pub fn update_agent_status(&mut self, pane_id: PaneId, status: AgentStatus) {
        self.agent_registry.update_status(pane_id, status);
    }

    /// Return a snapshot of all tracked agents.
    pub fn list_agents(&self) -> Vec<AgentEntry> {
        self.agent_registry.agents()
    }

    /// Return agents filtered by status string.
    pub fn list_agents_by_status(&self, status: &str) -> Vec<AgentEntry> {
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
                            cols: pane.cols,
                            rows: pane.rows,
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
            let mut session = Session::new(session_name, self.event_tx.clone());
            // Override the auto-generated ID with the original.
            session.id = session_id;

            let mut window = Window::new();

            for (meta, raw_fd) in pane_entries {
                match Pane::from_raw_fd(
                    meta.pane_id,
                    meta.cols,
                    meta.rows,
                    raw_fd,
                    self.event_tx.clone(),
                    session_id,
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
                info!(
                    session_id = session_id,
                    pane_count = session.pane_count(),
                    "restored session from handoff"
                );
                self.sessions.insert(session_id, session);
            }
        }

        // Update the ID counters so new sessions/panes don't collide.
        if let Some(max_session) = self.sessions.keys().max() {
            let current = NEXT_SESSION_ID.load(Ordering::Relaxed);
            if *max_session >= current {
                NEXT_SESSION_ID.store(max_session + 1, Ordering::Relaxed);
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
        let current_pane = NEXT_PANE_ID.load(Ordering::Relaxed);
        if max_pane >= current_pane {
            NEXT_PANE_ID.store(max_pane + 1, Ordering::Relaxed);
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

            let mut session = Session::new(persisted_session.name.clone(), self.event_tx.clone());
            match session.create_default_pane(first_pane.cols, first_pane.rows, &spawn_opts) {
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

            let session_id = session.id;

            // Spawn additional panes for multi-pane sessions.
            for pane_meta in &persisted_session.panes[1..] {
                let opts = therminal_terminal::pty::SpawnOptions {
                    cwd: pane_meta.cwd.clone(),
                    shell: pane_meta.shell.clone(),
                    ..Default::default()
                };
                match Pane::spawn(
                    pane_meta.cols,
                    pane_meta.rows,
                    self.event_tx.clone(),
                    session_id,
                    &opts,
                ) {
                    Ok(pane) => {
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
            info!(
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
}
