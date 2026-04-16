//! Headless `EventListener` (no GUI) and the daemon-side
//! `PtyReaderHandler` that drives every PTY reader thread in the daemon.

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi;
use std::sync::{Arc, Mutex};
use therminal_terminal::interceptor::{InterceptedEvent, TherminalInterceptor};
use therminal_terminal::pty_runtime::PtyReaderHandler;
use therminal_terminal::region_index::RegionIndex;
use therminal_terminal::state_inference::AgentStateInference;
use tokio::sync::broadcast;
use tracing::{debug, info};

use therminal_protocol::daemon::DaemonEvent;
use therminal_protocol::{PaneId, SessionId};

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
pub(super) struct DaemonPtyHandler {
    pub(super) event_tx: broadcast::Sender<DaemonEvent>,
    pub(super) session_id: SessionId,
    pub(super) pane_id: PaneId,
    pub(super) interceptor: TherminalInterceptor,
    pub(super) interceptor_rx: std::sync::mpsc::Receiver<InterceptedEvent>,
    pub(super) region_index: Arc<Mutex<RegionIndex>>,
    /// Shared cwd updated from OSC 7 events.
    pub(super) cwd: Arc<Mutex<String>>,
    /// Shared agent state inference engine. Reader thread feeds bytes;
    /// MCP handlers snapshot state on demand.
    pub(super) inference: Arc<Mutex<AgentStateInference>>,
    /// Pattern engine dispatcher (tn-86us). `None` when the daemon was
    /// built without a pattern engine installed on the session manager
    /// (e.g. unit tests).
    pub(super) pattern_dispatch: Option<crate::pattern_dispatch::PatternDispatcher>,
    /// WSL-side shell PID shared with the Pane struct (tn-ttie). Updated
    /// from OSC 7337 events in `process_bytes`.
    pub(super) wsl_shell_pid: Arc<Mutex<Option<u32>>>,
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
            // OSC 7 only — OSC 9;9 (WslCwd) carries the same location as
            // a Windows-native path, and overwriting the pane cwd with it
            // breaks WSL-aware consumers (tn-5o34): worktree resolution
            // via `git -C`, split inheritance for WSL children, and
            // `pane.list` cwd reporting. `linux_to_unc` recomputes the
            // Windows form on demand wherever it's actually needed.
            if let InterceptedEvent::CurrentDirectory(path) = &event
                && let Ok(mut cwd) = self.cwd.lock()
            {
                *cwd = path.clone();
            }
            // tn-ttie: capture WSL-side shell PID from OSC 7337.
            if let InterceptedEvent::WslShellPid(pid) = &event
                && let Ok(mut slot) = self.wsl_shell_pid.lock()
            {
                *slot = Some(*pid);
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
