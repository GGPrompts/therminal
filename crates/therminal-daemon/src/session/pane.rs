//! Pane: owns a headless Term + PTY via `PtyPaneCore`.
//!
//! Contains `HeadlessListener`, `DaemonPtyHandler`, `FdPtyMaster`,
//! `PaneDispatchCtx`, and the `Pane` struct with its impl.

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi;
use portable_pty::MasterPty;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::{Arc, Mutex};
use therminal_terminal::TaggedHarnessEvent;
use therminal_terminal::event_log::{DEFAULT_MAX_ENTRIES, EventLog, StoredEvent};
use therminal_terminal::interceptor::{InterceptedEvent, TherminalInterceptor};
use therminal_terminal::osc633::{CommandBlock, CommandTracker};
use therminal_terminal::pty_runtime::{PtyPaneCore, PtyReaderHandler, TermSize};
use therminal_terminal::region_index::RegionIndex;
use therminal_terminal::state_inference::{
    AgentCadenceSnapshot, AgentDetailsSnapshot, AgentStateInference, InferenceConfig,
};
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use therminal_protocol::daemon::DaemonEvent;
pub use therminal_protocol::{PaneId, SessionId};

#[cfg(unix)]
use std::os::unix::io::FromRawFd;

use super::next_pane_id;
use super::snapshots::{MAX_SNAPSHOT_SCROLLBACK, PaneSnapshot};

// ── Headless EventListener (no GUI) ─────────────────────────────────────

/// Minimal event listener for headless Term instances in the daemon.
/// We don't have a window, so most events are logged/ignored.
#[derive(Clone)]
pub(crate) struct HeadlessListener;

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
    /// Pattern engine dispatcher (tn-86us). `None` when the daemon was
    /// built without a pattern engine installed on the session manager
    /// (e.g. unit tests).
    pattern_dispatch: Option<crate::pattern_dispatch::PatternDispatcher>,
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

        // Feed the pattern engine dispatcher (tn-86us). Runs the ANSI
        // stripper + line accumulator internally and invokes
        // `process_finalized_line` on every committed line.
        if let Some(ref mut pd) = self.pattern_dispatch {
            pd.process_bytes(data);
        }

        // Drain intercepted events into the region index and update cwd.
        while let Ok(event) = self.interceptor_rx.try_recv() {
            if let InterceptedEvent::CurrentDirectory(ref path) = event
                && let Ok(mut cwd) = self.cwd.lock()
            {
                *cwd = path.clone();
            }
            // Route OSC 133/633 marks into the pattern dispatcher so
            // `prompt_boundary`-scoped patterns run against the command
            // transcript at the D mark.
            if let Some(ref mut pd) = self.pattern_dispatch {
                match &event {
                    InterceptedEvent::Osc633(mark) | InterceptedEvent::Osc133(mark) => {
                        use therminal_terminal::osc633::Osc633Mark;
                        match mark {
                            Osc633Mark::PreExec => pd.on_command_start(),
                            Osc633Mark::CommandLine { command } => {
                                pd.set_command_text(command.clone());
                            }
                            Osc633Mark::CommandFinished { .. } => pd.on_command_finish(),
                            _ => {}
                        }
                    }
                    _ => {}
                }
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

/// Pattern-dispatch wiring handed to every new pane (tn-86us).
#[derive(Clone, Default)]
pub struct PaneDispatchCtx {
    pub engine: Option<Arc<therminal_terminal::semantic_patterns::PatternEngine>>,
    pub bus: Option<Arc<crate::event_bus::EventBus>>,
    pub matches_total: Arc<std::sync::atomic::AtomicU64>,
}

impl PaneDispatchCtx {
    fn build_dispatcher(
        &self,
        pane_id: PaneId,
    ) -> Option<crate::pattern_dispatch::PatternDispatcher> {
        let engine = self.engine.as_ref()?.clone();
        let bus = self.bus.as_ref()?.clone();
        Some(crate::pattern_dispatch::PatternDispatcher::new(
            engine,
            bus,
            Arc::clone(&self.matches_total),
            pane_id,
        ))
    }
}

/// A single pane: owns a headless Term + PTY via `PtyPaneCore`.
#[allow(dead_code)]
pub struct Pane {
    pub id: PaneId,
    term: Arc<FairMutex<Term<HeadlessListener>>>,
    pty_writer: Box<dyn IoWrite + Send>,
    pub(super) _pty_master: Box<dyn MasterPty + Send>,
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
    /// Opaque key/value tags for binding the pane to external concepts
    /// (issue ids, branch names, conductor worker ids, ...). Therminal
    /// does not interpret these — see tn-bbvf.
    tags: HashMap<String, String>,
    /// PID of the spawned shell child. Used by the daemon-side
    /// `ProcessDetector` ticker (tn-pehl) to scan the process tree below
    /// the shell and populate the central `AgentRegistry` even when no
    /// GUI is attached. `None` for handoff-restored panes (the shell PID
    /// is not transmitted with the SCM_RIGHTS FD payload) and on backends
    /// where portable-pty does not surface a process id.
    shell_pid: Option<u32>,
}

impl Pane {
    /// Spawn a new pane with a PTY and headless terminal.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn(
        cols: u16,
        rows: u16,
        event_tx: broadcast::Sender<DaemonEvent>,
        session_id: SessionId,
        spawn_options: &therminal_terminal::pty::SpawnOptions,
        osc_registry: Arc<therminal_terminal::OscHandlerRegistry>,
        harness_event_tx: Option<std::sync::mpsc::Sender<TaggedHarnessEvent>>,
        pattern_ctx: PaneDispatchCtx,
    ) -> Result<Self, therminal_terminal::pty::PtyError> {
        let id = next_pane_id();

        let region_index = Arc::new(Mutex::new(RegionIndex::new()));
        let command_tracker = Arc::new(Mutex::new(CommandTracker::new()));
        let (mut interceptor, interceptor_rx) =
            TherminalInterceptor::with_defaults_and_tracker(Arc::clone(&command_tracker));
        interceptor.set_osc_registry(osc_registry);
        // tn-gln6 #1: install the shared harness-event sink so OSC 1341
        // (and any future harness OSC codes) actually reach a daemon-side
        // consumer instead of being silently dropped by the registry
        // dispatch. The interceptor emits TaggedHarnessEvent over this
        // channel; ensure.rs owns the receiver side and logs/routes events.
        if let Some(tx) = harness_event_tx {
            interceptor.set_harness_event_sink(tx);
        }

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

        let pattern_dispatch = pattern_ctx.build_dispatcher(id);
        let handler = DaemonPtyHandler {
            event_tx,
            session_id,
            pane_id: id,
            interceptor,
            interceptor_rx,
            region_index: Arc::clone(&region_index),
            cwd: Arc::clone(&cwd),
            inference: Arc::clone(&inference),
            pattern_dispatch,
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
        let shell_pid = core.child_pid();
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
            tags: HashMap::new(),
            shell_pid,
        })
    }

    /// Reconstruct a pane from a raw PTY master FD received via SCM_RIGHTS (Unix only).
    ///
    /// Creates a new headless `Term`, wraps the FD in a `FdPtyMaster`, clones a
    /// reader/writer from it, and spawns a reader thread -- mirroring `Pane::spawn`
    /// but without spawning a new shell.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_raw_fd(
        pane_id: PaneId,
        cols: u16,
        rows: u16,
        raw_fd: std::os::unix::io::RawFd,
        event_tx: broadcast::Sender<DaemonEvent>,
        session_id: SessionId,
        osc_registry: Arc<therminal_terminal::OscHandlerRegistry>,
        harness_event_tx: Option<std::sync::mpsc::Sender<TaggedHarnessEvent>>,
        pattern_ctx: PaneDispatchCtx,
    ) -> Result<Self, therminal_terminal::pty::PtyError> {
        let pty_master = Box::new(FdPtyMaster::new(raw_fd));

        let region_index = Arc::new(Mutex::new(RegionIndex::new()));
        let command_tracker = Arc::new(Mutex::new(CommandTracker::new()));
        let (mut interceptor, interceptor_rx) =
            TherminalInterceptor::with_defaults_and_tracker(Arc::clone(&command_tracker));
        interceptor.set_osc_registry(osc_registry);
        // tn-gln6 #1: see Pane::spawn above.
        if let Some(tx) = harness_event_tx {
            interceptor.set_harness_event_sink(tx);
        }

        // FD handoff panes don't have spawn options; cwd will be updated by OSC 7.
        let cwd = Arc::new(Mutex::new(String::new()));

        let inference = Arc::new(Mutex::new(AgentStateInference::new(InferenceConfig {
            session_id: format!("daemon-pane-{pane_id}"),
            child_pid: 0,
            agent_type: None,
            working_dir: None,
        })));

        let pattern_dispatch = pattern_ctx.build_dispatcher(pane_id);
        let handler = DaemonPtyHandler {
            event_tx: event_tx.clone(),
            session_id,
            pane_id,
            interceptor,
            interceptor_rx,
            region_index: Arc::clone(&region_index),
            cwd: Arc::clone(&cwd),
            inference: Arc::clone(&inference),
            pattern_dispatch,
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
            tags: HashMap::new(),
            // Handoff payloads do not include the shell PID; the new
            // daemon will not run process-tree detection on this pane
            // until the next session restart. tn-pehl follow-up.
            shell_pid: None,
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

    /// Snapshot the per-pane output cadence window. Returns a plain DTO
    /// suitable for serialising into the MCP `terminal.agents.get_cadence`
    /// response. Holds the inference lock only for the duration of the
    /// snapshot build (sample timestamps converted to wall-clock seconds
    /// before the lock is released).
    pub fn agent_cadence_snapshot(&self) -> AgentCadenceSnapshot {
        self.inference
            .lock()
            .map(|inf| inf.cadence_snapshot())
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

    /// PID of the spawned shell child, if known. Used by the daemon-side
    /// `ProcessDetector` ticker (tn-pehl) to walk the process tree below
    /// the shell. Returns `None` for handoff-restored panes.
    pub fn shell_pid(&self) -> Option<u32> {
        self.shell_pid
    }

    /// Snapshot of the pane's opaque key/value tags (tn-bbvf).
    pub fn tags(&self) -> HashMap<String, String> {
        self.tags.clone()
    }

    /// Merge tags into the pane's tag set. Existing keys with the same
    /// name are overwritten; other keys are left untouched.
    pub fn merge_tags(&mut self, new_tags: HashMap<String, String>) {
        for (k, v) in new_tags {
            self.tags.insert(k, v);
        }
    }

    /// Remove specific tag keys from the pane. Keys not present are ignored.
    pub fn remove_tag_keys(&mut self, keys: &[String]) {
        for k in keys {
            self.tags.remove(k);
        }
    }

    /// Clear all tags on the pane.
    pub fn clear_tags(&mut self) {
        self.tags.clear();
    }

    /// Restore tags from persisted state.
    pub fn set_tags(&mut self, tags: HashMap<String, String>) {
        self.tags = tags;
    }

    /// Write bytes to the pane's PTY (forwarding keystrokes).
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.pty_writer.write_all(data)?;
        self.pty_writer.flush()
    }

    pub(super) fn has_seen_prompt_start(&self) -> bool {
        use therminal_terminal::osc633::CommandState;

        self.command_tracker
            .lock()
            .ok()
            .and_then(|tracker| tracker.current_block().map(|block| block.state.clone()))
            .is_some_and(|state| {
                matches!(
                    state,
                    CommandState::PromptStart
                        | CommandState::Input
                        | CommandState::Executing
                        | CommandState::Finished
                )
            })
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
            tags: self.tags.clone(),
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
        // Best-effort cleanup of the daemon-pane-*.json file the per-pane
        // state inference engine wrote into /tmp/<agent>-state. Without this,
        // restarting the daemon leaves stale state files lying around that
        // ClaudeStatePoller has to filter at boot. (tn-qfi0)
        if let Ok(inference) = self.inference.lock() {
            inference.cleanup();
        }
        // The PTY master drop will close the PTY, causing the reader thread
        // to get EOF and exit. We don't join here to avoid blocking.
        debug!(pane_id = %self.id, "pane dropped");
    }
}
