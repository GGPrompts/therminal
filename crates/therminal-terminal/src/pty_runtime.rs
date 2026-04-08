//! Shared PTY runtime: `PtyPaneCore` owns a `Term`, PTY master/writer, and reader thread.
//!
//! Both the app (with interceptor + process detector) and the daemon (with
//! event broadcast) implement `PtyReaderHandler` to customise the reader loop
//! without duplicating the boilerplate of Term creation, PTY spawn, and
//! thread management.

use std::io::Read as IoRead;
use std::io::Write as IoWrite;
use std::sync::Arc;
use std::thread;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi;
use portable_pty::MasterPty;
use tracing::{info, warn};

use crate::pty::{self, PtyError, SpawnOptions};

// ── Dimensions adapter ─────────────────────────────────────────────────

/// Generic dimensions adapter used by `PtyPaneCore` to create a `Term`.
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
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

// ── Reader handler trait ───────────────────────────────────────────────

/// Callback interface for the PTY reader thread.
///
/// Implementors decide how raw PTY bytes are fed to the `Term` (e.g. plain
/// `advance` vs `advance_with_interceptor`) and what side-effects happen
/// after each read (wake the event loop, broadcast events, etc.).
pub trait PtyReaderHandler: Send + 'static {
    /// The `EventListener` type used by the `Term` this handler works with.
    type Listener: EventListener;

    /// Process a chunk of bytes read from the PTY.
    ///
    /// The implementation receives:
    /// - `processor`: the VTE ANSI processor (for calling `advance` variants)
    /// - `term`: the locked terminal (caller must lock/unlock as needed)
    /// - `data`: raw bytes from the PTY
    fn process_bytes(
        &mut self,
        processor: &mut ansi::Processor<ansi::StdSyncHandler>,
        term: &Arc<FairMutex<Term<Self::Listener>>>,
        data: &[u8],
    );

    /// Called when the PTY reader encounters EOF (shell exited).
    fn on_eof(&mut self);

    /// Called when the PTY reader encounters a read error.
    fn on_error(&mut self, _error: &std::io::Error) {}
}

// ── PtyPaneCore ────────────────────────────────────────────────────────

/// Shared PTY + Term lifecycle owner.
///
/// Generic over `L: EventListener` so callers can use their own listener
/// (e.g. `PaneListener` in the app, `HeadlessListener` in the daemon).
pub struct PtyPaneCore<L: EventListener> {
    term: Arc<FairMutex<Term<L>>>,
    pty_writer: Box<dyn IoWrite + Send>,
    pty_master: Box<dyn MasterPty + Send>,
    /// PID of the spawned shell child, captured before the child handle
    /// was moved into the watcher thread. `None` if portable-pty did not
    /// expose a process id (e.g. some Windows backends).
    child_pid: Option<u32>,
    /// Kept alive so we can join on shutdown if needed in the future.
    #[allow(dead_code)]
    reader_handle: Option<thread::JoinHandle<()>>,
}

impl<L: EventListener> PtyPaneCore<L> {
    /// Spawn a new PTY with a `Term` and a reader thread.
    ///
    /// - `cols`, `rows`: initial terminal dimensions.
    /// - `scrollback_lines`: scrollback history size for the `Term`.
    /// - `listener`: the `EventListener` for the `Term`.
    /// - `spawn_options`: shell override and extra env vars.
    /// - `handler`: the `PtyReaderHandler` that will process bytes in the reader thread.
    pub fn spawn(
        cols: usize,
        rows: usize,
        scrollback_lines: usize,
        listener: L,
        spawn_options: &SpawnOptions,
        handler: impl PtyReaderHandler<Listener = L>,
    ) -> Result<Self, PtyError>
    where
        L: EventListener + Send + 'static,
    {
        let cols = cols.max(2);
        let rows = rows.max(1);

        // Create the terminal emulator.
        let term_config = TermConfig {
            scrolling_history: scrollback_lines,
            ..Default::default()
        };
        let term_size = TermSize {
            columns: cols,
            screen_lines: rows,
        };
        let term = Term::new(term_config, &term_size, listener);
        let term = Arc::new(FairMutex::new(term));

        // Spawn the PTY.
        let (pty_master, mut child) =
            pty::spawn_shell_with_options(cols as u16, rows as u16, spawn_options)?;

        // Capture the child PID before moving `child` into the watcher
        // thread. Used by the daemon-side `ProcessDetector` ticker to
        // walk the process tree below the spawned shell.
        let child_pid = child.process_id();

        let pty_reader = pty_master
            .try_clone_reader()
            .map_err(|e| PtyError::Open(anyhow::anyhow!("failed to clone PTY reader: {e}")))?;
        let pty_writer = pty_master
            .take_writer()
            .map_err(|e| PtyError::Open(anyhow::anyhow!("failed to get PTY writer: {e}")))?;

        // Spawn reader thread with child handle for exit detection.
        // On Windows/ConPTY, the PTY reader may not get EOF when the shell
        // exits (especially with wsl.exe). The reader loop uses a child
        // watcher thread that sets a flag, which the reader checks after
        // each read to detect exit even without EOF.
        let child_exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let child_exited_flag = Arc::clone(&child_exited);
        let _child_thread = thread::Builder::new()
            .name("child-watcher".into())
            .spawn(move || {
                let _status = child.wait();
                info!("child process exited");
                child_exited_flag.store(true, std::sync::atomic::Ordering::Release);
            });

        let term_for_reader = Arc::clone(&term);
        let reader_handle = thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                reader_loop(pty_reader, term_for_reader, handler, Some(child_exited));
            })
            .map_err(|e| {
                PtyError::Open(anyhow::anyhow!("failed to spawn PTY reader thread: {e}"))
            })?;

        Ok(Self {
            term,
            pty_writer,
            pty_master,
            child_pid,
            reader_handle: Some(reader_handle),
        })
    }

    /// PID of the spawned shell child, if portable-pty exposed one.
    /// Used by the daemon's process-tree agent detector to scan below
    /// the shell.
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Write bytes to the PTY (forward keystrokes).
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.pty_writer.write_all(data)?;
        self.pty_writer.flush()
    }

    /// Resize the PTY and terminal.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if let Err(e) = pty::resize(self.pty_master.as_ref(), cols as u16, rows as u16) {
            warn!(error = %e, "failed to resize PTY");
            return;
        }
        let size = TermSize {
            columns: cols,
            screen_lines: rows,
        };
        self.term.lock().resize(size);
    }

    /// Access the shared terminal state.
    pub fn term(&self) -> &Arc<FairMutex<Term<L>>> {
        &self.term
    }

    /// Access the PTY master (e.g. for external resize calls).
    pub fn pty_master(&self) -> &dyn MasterPty {
        self.pty_master.as_ref()
    }

    /// Take ownership of the PTY writer (for callers that need to store it separately).
    pub fn take_writer(&mut self) -> Box<dyn IoWrite + Send> {
        std::mem::replace(&mut self.pty_writer, Box::new(std::io::sink()))
    }

    /// Take ownership of the PTY master (for callers that need to store it separately).
    pub fn take_pty_master(&mut self) -> Box<dyn MasterPty + Send> {
        std::mem::replace(&mut self.pty_master, Box::new(TakenPtyMaster))
    }
}

impl<L: EventListener> Drop for PtyPaneCore<L> {
    fn drop(&mut self) {
        // PTY master drop closes the PTY, causing the reader thread EOF.
        // We don't join to avoid blocking.
    }
}

// ── Reader loop ────────────────────────────────────────────────────────

/// Public entry point for running a PTY reader loop on an externally-provided
/// reader and `Term`. Used by the daemon to re-attach reader threads to PTY
/// FDs received via SCM_RIGHTS during handoff.
pub fn reader_loop_external<H: PtyReaderHandler>(
    reader: Box<dyn IoRead + Send>,
    term: Arc<FairMutex<Term<H::Listener>>>,
    handler: H,
) {
    reader_loop(reader, term, handler, None);
}

fn reader_loop<H: PtyReaderHandler>(
    mut reader: Box<dyn IoRead + Send>,
    term: Arc<FairMutex<Term<H::Listener>>>,
    mut handler: H,
    child_exited: Option<Arc<std::sync::atomic::AtomicBool>>,
) {
    let mut processor = ansi::Processor::<ansi::StdSyncHandler>::new();
    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                info!("PTY closed (EOF)");
                handler.on_eof();
                break;
            }
            Ok(n) => {
                handler.process_bytes(&mut processor, &term, &buf[..n]);
                // After processing, check if the child has exited.
                // On Windows/ConPTY with wsl.exe, the reader may never get
                // EOF even after the shell exits. This check catches that.
                if let Some(ref flag) = child_exited
                    && flag.load(std::sync::atomic::Ordering::Acquire)
                {
                    info!("child exited, treating as EOF");
                    handler.on_eof();
                    break;
                }
            }
            Err(e) => {
                // If child already exited, this is expected — treat as EOF.
                if let Some(ref flag) = child_exited
                    && flag.load(std::sync::atomic::Ordering::Acquire)
                {
                    info!("PTY read error after child exit, treating as EOF");
                    handler.on_eof();
                    break;
                }
                warn!(error = %e, "PTY read error");
                handler.on_error(&e);
                break;
            }
        }
    }
}

// ── Sentinel for taken PTY master ──────────────────────────────────────

/// Placeholder inserted when the real PTY master is taken via `take_pty_master`.
struct TakenPtyMaster;

impl MasterPty for TakenPtyMaster {
    fn resize(&self, _size: portable_pty::PtySize) -> Result<(), anyhow::Error> {
        Err(anyhow::anyhow!("PTY master has been taken"))
    }

    fn get_size(&self) -> Result<portable_pty::PtySize, anyhow::Error> {
        Err(anyhow::anyhow!("PTY master has been taken"))
    }

    fn try_clone_reader(&self) -> Result<Box<dyn IoRead + Send>, anyhow::Error> {
        Err(anyhow::anyhow!("PTY master has been taken"))
    }

    fn take_writer(&self) -> Result<Box<dyn IoWrite + Send>, anyhow::Error> {
        Err(anyhow::anyhow!("PTY master has been taken"))
    }

    #[cfg(unix)]
    fn process_group_leader(&self) -> Option<libc::pid_t> {
        None
    }

    #[cfg(unix)]
    fn as_raw_fd(&self) -> Option<std::os::unix::io::RawFd> {
        None
    }

    #[cfg(unix)]
    fn tty_name(&self) -> Option<std::path::PathBuf> {
        None
    }
}
