//! `Pane` struct definition + spawn / from_raw_fd constructors + Drop.

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use portable_pty::MasterPty;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::sync::{Arc, Mutex};
use therminal_terminal::TaggedHarnessEvent;
use therminal_terminal::event_log::{DEFAULT_MAX_ENTRIES, EventLog};
use therminal_terminal::interceptor::TherminalInterceptor;
use therminal_terminal::osc633::CommandTracker;
use therminal_terminal::pty_runtime::PtyPaneCore;
use therminal_terminal::region_index::RegionIndex;
use therminal_terminal::state_inference::{AgentStateInference, InferenceConfig};
use tokio::sync::broadcast;
use tracing::{debug, info};

use therminal_protocol::daemon::DaemonEvent;
use therminal_protocol::{PaneId, SessionId};

use super::dispatch_ctx::PaneDispatchCtx;
use super::headless::{DaemonPtyHandler, HeadlessListener};
use crate::session::next_pane_id;

/// A single pane: owns a headless Term + PTY via `PtyPaneCore`.
#[allow(dead_code)]
pub struct Pane {
    pub id: PaneId,
    pub(super) term: Arc<FairMutex<Term<HeadlessListener>>>,
    pub(super) pty_writer: Box<dyn IoWrite + Send>,
    pub(crate) _pty_master: Box<dyn MasterPty + Send>,
    pub(super) region_index: Arc<Mutex<RegionIndex>>,
    pub(super) cols: u16,
    pub(super) rows: u16,
    /// Current working directory, updated from OSC 7 events.
    pub(super) cwd: Arc<Mutex<String>>,
    /// Shell command used when this pane was spawned.
    pub(super) shell: String,
    /// Shared agent state inference engine. Cloned into the reader thread's
    /// `DaemonPtyHandler` so PTY bytes feed it; daemon-side accessors
    /// snapshot it for MCP `terminal.agents.get_details`.
    pub(super) inference: Arc<Mutex<AgentStateInference>>,
    /// Shared OSC 633 command tracker. The reader thread's interceptor
    /// holds the same `Arc` so PTY bytes feed it; daemon-side accessors
    /// snapshot it for MCP `terminal.semantic.query_commands`.
    pub(super) command_tracker: Arc<Mutex<CommandTracker>>,
    /// Per-pane in-memory structured event log. Backs the
    /// `terminal.panes.query_events` MCP tool. The daemon does not write
    /// JSONL to disk for these — only the rolling in-memory ring is used,
    /// capped at `DEFAULT_MAX_ENTRIES` (5000) events.
    pub(super) event_log: Arc<Mutex<EventLog>>,
    /// Opaque key/value tags for binding the pane to external concepts
    /// (issue ids, branch names, conductor worker ids, ...). Therminal
    /// does not interpret these — see tn-bbvf.
    pub(super) tags: HashMap<String, String>,
    /// PID of the spawned shell child. Used by the daemon-side
    /// `ProcessDetector` ticker (tn-pehl) to scan the process tree below
    /// the shell and populate the central `AgentRegistry` even when no
    /// GUI is attached. `None` for handoff-restored panes (the shell PID
    /// is not transmitted with the SCM_RIGHTS FD payload) and on backends
    /// where portable-pty does not surface a process id.
    pub(super) shell_pid: Option<u32>,
}

impl Pane {
    /// Spawn a new pane with a PTY and headless terminal.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
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
    pub(crate) fn from_raw_fd(
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
        use super::fd_master::FdPtyMaster;

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
