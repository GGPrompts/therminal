//! Sequence interceptor for therminal AI-awareness.
//!
//! [`TherminalInterceptor`] implements the VTE [`SequenceInterceptor`] trait to
//! catch AI-agent and shell-integration escape sequences before they reach the
//! terminal handler. This is the core abstraction that makes therminal
//! AI-aware without forking alacritty_terminal.
//!
//! ## Handled sequences
//!
//! | Code      | Protocol            | Purpose                        |
//! |-----------|---------------------|--------------------------------|
//! | OSC 133   | FinalTerm           | Shell integration (prompt/cmd) |
//! | OSC 633   | VS Code             | Shell integration (extended)   |
//! | OSC 7     | Standard            | Current working directory      |
//! | OSC 9     | ConEmu/mintty       | Desktop notifications          |
//! | OSC 9;4   | ConEmu/WT           | Taskbar progress (consumed)    |
//! | OSC 9;9   | Windows Terminal    | WSL cwd (Windows-native path)  |
//! | OSC 1337  | iTerm2              | Various (used by some agents)  |
//! | OSC 7337  | Therminal           | WSL-side shell PID (tn-ttie)   |
//! | OSC 7777  | Therminal           | Cooperative agent self-report   |
//!
//! Any other OSC code is consulted against the shared
//! [`OscHandlerRegistry`] (see [`crate::osc_registry`]) so harness crates
//! can claim their own codes without modifying core.
//!
//! [`SequenceInterceptor`]: alacritty_terminal::vte::SequenceInterceptor

use std::sync::{Arc, Mutex, mpsc};

use tracing::{debug, trace, warn};

use crate::graphics::KittyGraphicsParser;
use crate::osc_registry::{
    HarnessOscHandler, OscHandlerRegistry, OscRegistrationError, TaggedHarnessEvent,
};
use crate::osc633::{CommandTracker, Osc633Mark};
use crate::terminal::GraphicsEvent;

/// Events produced by the interceptor for consumption by the terminal or daemon.
#[derive(Debug, Clone)]
pub enum InterceptedEvent {
    /// An OSC 633 mark was detected (VS Code shell integration).
    Osc633(Osc633Mark),
    /// An OSC 133 (FinalTerm) mark was detected. Same semantics as 633 for
    /// the marks that overlap (A, B, C, D).
    Osc133(Osc633Mark),
    /// The shell reported its current working directory via OSC 7.
    CurrentDirectory(String),
    /// An iTerm2-style (OSC 1337) key=value pair was detected.
    Iterm2 { key: String, value: String },
    /// A desktop notification was requested via OSC 9.
    DesktopNotification(String),
    /// The shell reported its current working directory as a Windows-native
    /// path via OSC 9;9 (tn-kkr8).
    ///
    /// Emitted by the shell integration scripts when `WSL_DISTRO_NAME` is
    /// set. The payload is a Windows path (e.g. `\\wsl.localhost\Ubuntu\home\user`)
    /// produced by `wslpath -w "$PWD"`, so the daemon does not need to run
    /// `linux_to_unc()` — the path is usable on the Windows host as-is.
    WslCwd(String),
    /// The WSL-side shell reported its PID via OSC 7337 (tn-ttie).
    ///
    /// Emitted once at shell startup by the therminal bash rcfile running
    /// inside WSL. The daemon captures this to scope the WSL process
    /// detector probe to the pane's subtree instead of scanning the entire
    /// distro.
    WslShellPid(u32),
    /// A cooperative agent self-reported its state via OSC 7777.
    ///
    /// See [`TherminalInterceptor::handle_osc_7777`] for the full protocol spec.
    AgentReport {
        /// Agent identity (required). E.g. "claude-code", "copilot", "aider".
        agent: String,
        /// Current agent state (optional). One of "idle", "thinking",
        /// "streaming", "tool_use".
        state: Option<String>,
        /// Name of the tool currently being used (optional).
        tool: Option<String>,
        /// Cumulative token count for the current session (optional).
        tokens: Option<u64>,
        /// Model name the agent is using (optional). E.g. "opus-4", "gpt-4o".
        model: Option<String>,
    },
    /// A Kitty graphics APC string was fully parsed. See
    /// [`crate::terminal::GraphicsEvent`] and [`crate::graphics`] for the
    /// protocol surface.
    Graphics(GraphicsEvent),
}

/// Configuration for which sequence families to intercept.
#[derive(Debug, Clone)]
pub struct InterceptorConfig {
    /// Intercept OSC 633 (VS Code shell integration).
    pub osc_633: bool,
    /// Intercept OSC 133 (FinalTerm shell integration).
    pub osc_133: bool,
    /// Intercept OSC 7 (current working directory).
    pub osc_7: bool,
    /// Intercept OSC 1337 (iTerm2).
    pub osc_1337: bool,
    /// Intercept OSC 9 (desktop notifications).
    pub osc_9: bool,
    /// Intercept OSC 7777 (cooperative agent self-reporting).
    pub osc_7777: bool,
    /// Intercept OSC 7337 (WSL-side shell PID reporting).
    pub osc_7337: bool,
}

impl Default for InterceptorConfig {
    fn default() -> Self {
        Self {
            osc_633: true,
            osc_133: true,
            osc_7: true,
            osc_9: true,
            osc_1337: true,
            osc_7777: true,
            osc_7337: true,
        }
    }
}

/// The therminal-specific sequence interceptor.
///
/// Sits between the VTE parser and the terminal handler, consuming
/// AI-agent–relevant escape sequences and emitting [`InterceptedEvent`]s.
/// Sequences that are consumed here are *not* forwarded to the terminal
/// handler (the interceptor returns `true`).
///
/// Some sequences (like OSC 7) are *observed but not consumed* so that
/// the terminal can still update its own state while we also react.
pub struct TherminalInterceptor {
    config: InterceptorConfig,
    /// Channel for sending intercepted events.
    event_tx: mpsc::Sender<InterceptedEvent>,
    /// Shared command tracker for OSC 633/133 marks. Wrapped in
    /// `Arc<Mutex<...>>` so the daemon can hold a clone on its `Pane`
    /// and snapshot the tracker from a different thread.
    pub command_tracker: Arc<Mutex<CommandTracker>>,
    /// Shared OSC handler registry. Defaults to an empty per-interceptor
    /// registry so tests and standalone use-sites Just Work without extra
    /// wiring; the daemon replaces this with a shared `Arc` in
    /// [`Self::set_osc_registry`] before the first PTY is opened.
    osc_registry: Arc<OscHandlerRegistry>,
    /// Optional sink for [`TaggedHarnessEvent`]s produced by the OSC
    /// handler registry. Parallel to `event_tx` so daemon-side code can
    /// route harness events onto the (future) unified event bus without
    /// colliding with the existing `InterceptedEvent` stream.
    ///
    /// `None` by default — the dispatcher drops harness events on the
    /// floor if no sink is installed.
    harness_event_tx: Option<mpsc::Sender<TaggedHarnessEvent>>,
    /// Pane ID stamped onto every `TaggedHarnessEvent` before forwarding.
    /// Set via [`Self::set_pane_id`]; `None` by default (tests, standalone).
    pane_id: Option<u64>,
    /// Kitty graphics APC parser. Owns the chunk buffer so multi-chunk
    /// transmissions accumulate across APC strings.
    graphics_parser: KittyGraphicsParser,
    /// Optional sink for APC response bytes to write back through the PTY.
    /// Used for Kitty graphics replies (feature query, OK acks, error
    /// codes) and gated by the `q=` quiet level during parse.
    ///
    /// `None` by default — tests and harness-less sites silently drop
    /// responses, matching the existing pattern for harness events.
    graphics_response_tx: Option<mpsc::Sender<Vec<u8>>>,
}

impl TherminalInterceptor {
    /// Create a new interceptor with the given config.
    ///
    /// Returns the interceptor and a receiver for intercepted events.
    pub fn new(config: InterceptorConfig) -> (Self, mpsc::Receiver<InterceptedEvent>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                config,
                event_tx: tx,
                command_tracker: Arc::new(Mutex::new(CommandTracker::new())),
                osc_registry: Arc::new(OscHandlerRegistry::new()),
                harness_event_tx: None,
                pane_id: None,
                graphics_parser: KittyGraphicsParser::new(),
                graphics_response_tx: None,
            },
            rx,
        )
    }

    /// Create with the given config and a shared command tracker. Used by
    /// callers that need to read the tracker from a different thread.
    pub fn new_with_tracker(
        config: InterceptorConfig,
        command_tracker: Arc<Mutex<CommandTracker>>,
    ) -> (Self, mpsc::Receiver<InterceptedEvent>) {
        let (tx, rx) = mpsc::channel();
        (
            Self {
                config,
                event_tx: tx,
                command_tracker,
                osc_registry: Arc::new(OscHandlerRegistry::new()),
                harness_event_tx: None,
                pane_id: None,
                graphics_parser: KittyGraphicsParser::new(),
                graphics_response_tx: None,
            },
            rx,
        )
    }

    /// Create with default config.
    pub fn with_defaults() -> (Self, mpsc::Receiver<InterceptedEvent>) {
        Self::new(InterceptorConfig::default())
    }

    /// Create with default config and a shared command tracker.
    pub fn with_defaults_and_tracker(
        command_tracker: Arc<Mutex<CommandTracker>>,
    ) -> (Self, mpsc::Receiver<InterceptedEvent>) {
        Self::new_with_tracker(InterceptorConfig::default(), command_tracker)
    }

    /// Install a shared [`OscHandlerRegistry`] into this interceptor.
    ///
    /// The daemon creates a single registry at startup, registers harness
    /// handlers on it, and passes the same `Arc` into every pane's
    /// interceptor via this method. Without this call the interceptor
    /// dispatches through an empty per-instance registry — harness events
    /// are silently dropped, which is the correct behaviour for tests and
    /// stand-alone use-sites that do not run any harness crates.
    pub fn set_osc_registry(&mut self, registry: Arc<OscHandlerRegistry>) {
        self.osc_registry = registry;
    }

    /// Install a channel sink for [`TaggedHarnessEvent`]s produced by the
    /// OSC handler registry.
    ///
    /// Daemon-side code should call this at pane-construction time so the
    /// harness-event stream can be routed onto whatever bus/broadcast
    /// channel the consuming layer wants. If no sink is installed the
    /// dispatcher logs at `trace` and drops the event.
    pub fn set_harness_event_sink(&mut self, tx: mpsc::Sender<TaggedHarnessEvent>) {
        self.harness_event_tx = Some(tx);
    }

    /// Set the pane ID stamped onto every outgoing [`TaggedHarnessEvent`].
    ///
    /// Called by the daemon at pane-construction time so marker events
    /// carry pane context through the shared `mpsc` channel to the drain
    /// thread. Without this, the drain thread has no way to route marker
    /// data into the correct `PaneCapacityCache` entry.
    pub fn set_pane_id(&mut self, pane_id: u64) {
        self.pane_id = Some(pane_id);
    }

    /// Install a channel sink for Kitty graphics APC response bytes.
    ///
    /// The parser emits response envelopes (see [`crate::graphics`]) for
    /// protocol replies that have to travel back through the PTY to the
    /// client (feature query, OK acks, error codes). Daemon / app wiring
    /// feeds these bytes into the PTY writer so the producing program sees
    /// the reply. Without a sink the responses are silently dropped — this
    /// is correct for tests and stand-alone callers that do not run a PTY.
    pub fn set_graphics_response_sink(&mut self, tx: mpsc::Sender<Vec<u8>>) {
        self.graphics_response_tx = Some(tx);
    }

    /// Emit APC response bytes via the optional `graphics_response_tx` sink.
    fn emit_graphics_response(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Some(tx) = &self.graphics_response_tx
            && tx.send(bytes).is_err()
        {
            trace!("interceptor graphics response receiver dropped");
        }
    }

    /// Claim an OSC code on this interceptor's shared handler registry.
    ///
    /// This is the spec-mandated registration surface (see
    /// `docs/osc-handler-registry.md` §1.1). It is a thin forward to
    /// [`OscHandlerRegistry::register`] so harness crates can write
    /// idiomatic code against `&mut TherminalInterceptor` without needing
    /// to reach into the registry type directly.
    ///
    /// # Errors
    ///
    /// Returns an [`OscRegistrationError`] if the code is reserved,
    /// already claimed, or if `owner` is not a valid identifier. Callers
    /// should `.expect()` the result at daemon startup — a duplicate claim
    /// is a programming mistake that must fail fast.
    pub fn register_osc_handler(
        &mut self,
        osc_code: u16,
        owner: &'static str,
        handler: HarnessOscHandler,
    ) -> Result<(), OscRegistrationError> {
        self.osc_registry.register(osc_code, owner, handler)
    }

    /// Borrow the shared OSC handler registry. Useful for diagnostics and
    /// introspection (owner lookup, disabled-flag checks).
    pub fn osc_registry(&self) -> &Arc<OscHandlerRegistry> {
        &self.osc_registry
    }

    /// Drive an OSC sequence through the interceptor without requiring
    /// callers to import `alacritty_terminal::vte::SequenceInterceptor`.
    ///
    /// Equivalent to calling
    /// `<Self as SequenceInterceptor>::intercept_osc(self, params, bell_terminated)`
    /// and returns the same boolean (`true` = consumed, `false` = pass
    /// through). Exposed as an inherent method so harness-crate
    /// integration tests can feed synthetic OSC sequences through the
    /// full interceptor pipeline without taking a direct dependency on
    /// the vte trait.
    pub fn dispatch_osc(&mut self, params: &[&[u8]], bell_terminated: bool) -> bool {
        <Self as alacritty_terminal::vte::SequenceInterceptor>::intercept_osc(
            self,
            params,
            bell_terminated,
        )
    }

    /// Send an event, logging if the receiver has been dropped.
    fn emit(&self, event: InterceptedEvent) {
        if self.event_tx.send(event).is_err() {
            trace!("interceptor event receiver dropped");
        }
    }

    /// Emit a tagged harness event on the optional `harness_event_tx`
    /// sink. Stamps the pane_id before forwarding. Silent if no sink is
    /// installed or the receiver has been dropped.
    fn emit_harness(&self, mut tagged: TaggedHarnessEvent) {
        tagged.pane_id = self.pane_id;
        if let Some(tx) = &self.harness_event_tx
            && tx.send(tagged).is_err()
        {
            trace!("interceptor harness-event receiver dropped");
        }
    }

    /// Handle an OSC 633 sequence. Returns `true` to consume.
    fn handle_osc_633(&mut self, params: &[&[u8]]) -> bool {
        // params[0] is "633", params[1..] are the mark and data.
        if params.len() < 2 {
            return false;
        }

        let mark = match parse_633_mark(params[1], params.get(2).copied()) {
            Some(m) => m,
            None => return false,
        };

        debug!("OSC 633: {:?}", mark);
        if let Ok(mut t) = self.command_tracker.lock() {
            t.apply(&mark);
        }
        self.emit(InterceptedEvent::Osc633(mark));

        // Consume: alacritty_terminal ignores OSC 633 anyway, but consuming
        // explicitly avoids the "unhandled osc_dispatch" debug log.
        true
    }

    /// Handle an OSC 133 (FinalTerm) sequence. Returns `true` to consume.
    fn handle_osc_133(&mut self, params: &[&[u8]]) -> bool {
        // FinalTerm format: OSC 133 ; <mark> [ ; <data> ] ST
        // params[0] = "133", params[1] = mark letter, params[2..] = optional data
        if params.len() < 2 {
            return false;
        }

        let mark = match parse_633_mark(params[1], params.get(2).copied()) {
            Some(m) => m,
            None => return false,
        };

        debug!("OSC 133 (FinalTerm): {:?}", mark);
        if let Ok(mut t) = self.command_tracker.lock() {
            t.apply(&mark);
        }
        self.emit(InterceptedEvent::Osc133(mark));

        // Consume: alacritty_terminal also ignores these.
        true
    }

    /// Handle OSC 9 (desktop notification / WSL cwd / taskbar progress).
    /// Returns `true` to consume.
    ///
    /// Sub-formats recognised:
    /// - `OSC 9 ; 4 ; <state> [; <pct>] ST` — ConEmu/Windows Terminal taskbar
    ///   progress. Emitted by PowerShell/PSReadLine and by Claude Code when
    ///   `preferredNotifChannel = auto` on Windows. Consumed silently; we do
    ///   not render taskbar progress yet.
    /// - `OSC 9 ; 9 ; <windows-path> ST` — Windows Terminal–style WSL cwd
    ///   report (tn-kkr8). The path is a Windows-native string produced by
    ///   `wslpath -w "$PWD"` inside WSL, so the daemon can use it directly
    ///   without `linux_to_unc()`.
    /// - `OSC 9 ; <text> ST` — ConEmu/mintty desktop notification.
    fn handle_osc_9(&mut self, params: &[&[u8]]) -> bool {
        // Format: OSC 9 ; <notification text> ST
        //     or: OSC 9 ; 4 ; <state> [; <pct>] ST
        //     or: OSC 9 ; 9 ; <windows-path> ST
        if params.len() < 2 {
            return false;
        }

        // ── OSC 9;4 — ConEmu/WT taskbar progress ──────────────────────
        // PowerShell (via PSReadLine) and Claude Code emit this to drive
        // the Windows taskbar progress indicator. Without this branch
        // params[1] = b"4" falls through to the plain-notification path
        // and surfaces as a toast with body "4".
        if params[1] == b"4" {
            trace!("OSC 9;4 (taskbar progress) consumed");
            return true;
        }

        // ── OSC 9;9 — WSL cwd (tn-kkr8) ────────────────────────────────
        // The VTE parser splits on ';', so OSC 9;9;<path> arrives as:
        //   params[0] = b"9", params[1] = b"9", params[2] = <path bytes>
        if params.len() >= 3 && params[1] == b"9" {
            let path = match std::str::from_utf8(params[2]) {
                Ok(s) if !s.is_empty() => s,
                _ => {
                    trace!("OSC 9;9: invalid or empty path payload, consuming");
                    return true;
                }
            };
            debug!("OSC 9;9 (WSL cwd): {}", path);
            self.emit(InterceptedEvent::WslCwd(path.to_string()));
            // Consume: alacritty_terminal doesn't handle OSC 9.
            return true;
        }

        // ── OSC 9 — desktop notification ────────────────────────────────
        let text = match std::str::from_utf8(params[1]) {
            Ok(s) => s,
            Err(_) => return false,
        };

        // Filter spurious ConPTY/PowerShell interop notifications.
        // When WSL panes tear down, ConPTY may inject an OSC 9 with a bare
        // shell name and exit code (e.g. "Powershell: 4", "cmd: 0").
        // These are not user-facing notifications — suppress them.
        if is_spurious_conpty_notification(text) {
            debug!("OSC 9 suppressed (ConPTY interop): {}", text);
            return true;
        }

        debug!("OSC 9 (notification): {}", text);
        self.emit(InterceptedEvent::DesktopNotification(text.to_string()));

        // Consume: alacritty_terminal doesn't handle OSC 9.
        true
    }

    /// Handle OSC 7 (current directory). Returns `false` to pass through.
    fn handle_osc_7(&mut self, params: &[&[u8]]) -> bool {
        // Format: OSC 7 ; <uri> ST
        // The URI is typically file://hostname/path
        if params.len() < 2 {
            return false;
        }

        let uri = match std::str::from_utf8(params[1]) {
            Ok(s) => s,
            Err(_) => return false,
        };

        debug!("OSC 7 (cwd): {}", uri);

        // Extract path from file:// URI.
        let path = if let Some(stripped) = uri.strip_prefix("file://") {
            // Skip hostname portion: file://hostname/path -> /path
            stripped
                .find('/')
                .map(|idx| &stripped[idx..])
                .unwrap_or(stripped)
        } else {
            uri
        };

        self.emit(InterceptedEvent::CurrentDirectory(path.to_string()));

        // Do NOT consume: let alacritty_terminal also process it.
        false
    }

    /// Handle OSC 1337 (iTerm2). Returns `true` to consume.
    fn handle_osc_1337(&mut self, params: &[&[u8]]) -> bool {
        // Format: OSC 1337 ; <key>=<value> ST
        if params.len() < 2 {
            return false;
        }

        let payload = match std::str::from_utf8(params[1]) {
            Ok(s) => s,
            Err(_) => return false,
        };

        if let Some((key, value)) = payload.split_once('=') {
            debug!("OSC 1337 (iTerm2): {}={}", key, value);
            self.emit(InterceptedEvent::Iterm2 {
                key: key.to_string(),
                value: value.to_string(),
            });
        } else {
            debug!("OSC 1337 (iTerm2): {}", payload);
            self.emit(InterceptedEvent::Iterm2 {
                key: payload.to_string(),
                value: String::new(),
            });
        }

        // Consume: alacritty_terminal doesn't handle these.
        true
    }

    /// Handle OSC 7777 (cooperative agent self-reporting). Returns `true` to consume.
    ///
    /// # OSC 7777 — Cooperative Agent Self-Reporting Protocol
    ///
    /// A custom Therminal extension that lets AI agents voluntarily report their
    /// identity and state to the terminal emulator, eliminating the need for
    /// heuristic-based detection (process tree scanning, cadence analysis) when
    /// the agent supports it.
    ///
    /// ## Wire format
    ///
    /// ```text
    /// ESC ] 7777 ; key=value ; key=value ... ST
    /// ```
    ///
    /// Where `ST` is the String Terminator (`ESC \` or `BEL`/`0x07`).
    /// The VTE parser splits on `;`, so the interceptor receives each
    /// `key=value` pair as a separate element in `params[1..]`.
    ///
    /// ## Keys
    ///
    /// | Key      | Required | Description                                         |
    /// |----------|----------|-----------------------------------------------------|
    /// | `agent`  | Yes      | Agent identity string (e.g. "claude-code", "aider") |
    /// | `state`  | No       | Current state: "idle", "thinking", "streaming", "tool_use" |
    /// | `tool`   | No       | Name of the tool currently being invoked             |
    /// | `tokens` | No       | Cumulative token count (decimal integer)             |
    /// | `model`  | No       | Model name (e.g. "opus-4", "gpt-4o")                |
    ///
    /// Unknown keys are silently ignored to allow forward-compatible extension.
    ///
    /// ## Examples
    ///
    /// Minimal (agent identity only):
    /// ```text
    /// \x1b]7777;agent=claude-code\x07
    /// ```
    ///
    /// Full report:
    /// ```text
    /// \x1b]7777;agent=claude-code;state=tool_use;tool=Edit;tokens=12345;model=opus-4\x07
    /// ```
    ///
    /// State transition (idle):
    /// ```text
    /// \x1b]7777;agent=claude-code;state=idle\x07
    /// ```
    ///
    /// ## Design rationale
    ///
    /// OSC 7777 was chosen because it does not conflict with any known terminal
    /// emulator extension (OSC 7 = cwd, OSC 133/633 = shell integration,
    /// OSC 1337 = iTerm2, OSC 52 = clipboard). The semicolon-delimited key=value
    /// format is consistent with iTerm2's OSC 1337 and is trivially extensible.
    ///
    /// Agents that emit OSC 7777 give the terminal authoritative identity and
    /// state information, which takes priority over heuristic detection in
    /// `ProcessDetector` and cadence analysis.
    fn handle_osc_7777(&mut self, params: &[&[u8]]) -> bool {
        // params[0] is "7777", params[1..] are semicolon-delimited key=value pairs.
        if params.len() < 2 {
            return false;
        }

        let mut agent: Option<String> = None;
        let mut state: Option<String> = None;
        let mut tool: Option<String> = None;
        let mut tokens: Option<u64> = None;
        let mut model: Option<String> = None;

        for param in &params[1..] {
            let s = match std::str::from_utf8(param) {
                Ok(s) => s,
                Err(_) => continue,
            };

            if let Some((key, value)) = s.split_once('=') {
                match key {
                    "agent" => agent = Some(value.to_string()),
                    "state" => state = Some(value.to_string()),
                    "tool" => tool = Some(value.to_string()),
                    "tokens" => tokens = value.parse::<u64>().ok(),
                    "model" => model = Some(value.to_string()),
                    _ => {
                        // Unknown keys are silently ignored for forward compatibility.
                        trace!("OSC 7777: unknown key {:?}", key);
                    }
                }
            }
        }

        // `agent` is required; drop the report if missing.
        let agent = match agent {
            Some(a) if !a.is_empty() => a,
            _ => {
                debug!("OSC 7777: missing or empty 'agent' key, ignoring");
                return true; // consume but don't emit
            }
        };

        debug!(
            "OSC 7777 (agent report): agent={}, state={:?}, tool={:?}, tokens={:?}, model={:?}",
            agent, state, tool, tokens, model,
        );

        self.emit(InterceptedEvent::AgentReport {
            agent,
            state,
            tool,
            tokens,
            model,
        });

        // Consume: no other handler knows about OSC 7777.
        true
    }

    /// Handle OSC 7337 (WSL-side shell PID). Returns `true` to consume.
    ///
    /// # OSC 7337 — WSL Shell PID Reporting (tn-ttie)
    ///
    /// Emitted once at shell startup by the therminal bash rcfile inside WSL:
    ///
    /// ```text
    /// ESC ] 7337 ; <pid> BEL
    /// ```
    ///
    /// Where `<pid>` is the decimal PID of the WSL-side shell process (`$$`).
    /// The daemon stores this on the `Pane` and passes it to
    /// `ProcessDetector` so the WSL probe can BFS-walk from this root
    /// instead of scanning every process in the distro.
    fn handle_osc_7337(&mut self, params: &[&[u8]]) -> bool {
        // params[0] is "7337", params[1] is the PID string.
        if params.len() < 2 {
            return false;
        }

        let pid_str = match std::str::from_utf8(params[1]) {
            Ok(s) => s.trim(),
            Err(_) => return false,
        };

        let pid: u32 = match pid_str.parse() {
            Ok(n) => n,
            Err(_) => {
                debug!("OSC 7337: invalid PID {:?}", pid_str);
                return true; // consume but don't emit
            }
        };

        debug!("OSC 7337 (WSL shell PID): {}", pid);
        self.emit(InterceptedEvent::WslShellPid(pid));

        // Consume: no other handler knows about OSC 7337.
        true
    }
}

impl alacritty_terminal::vte::SequenceInterceptor for TherminalInterceptor {
    fn intercept_osc(&mut self, params: &[&[u8]], _bell_terminated: bool) -> bool {
        if params.is_empty() {
            return false;
        }

        // Native handlers take precedence over the registry. The registry
        // is only consulted for codes that none of the native arms claim,
        // so harness crates cannot shadow core OSC handling regardless of
        // registration order. See `docs/osc-handler-registry.md` §4.
        let native_consumed = match params[0] {
            b"633" if self.config.osc_633 => Some(self.handle_osc_633(params)),
            b"133" if self.config.osc_133 => Some(self.handle_osc_133(params)),
            b"7" if self.config.osc_7 => Some(self.handle_osc_7(params)),
            b"9" if self.config.osc_9 => Some(self.handle_osc_9(params)),
            b"1337" if self.config.osc_1337 => Some(self.handle_osc_1337(params)),
            b"7777" if self.config.osc_7777 => Some(self.handle_osc_7777(params)),
            b"7337" if self.config.osc_7337 => Some(self.handle_osc_7337(params)),
            _ => None,
        };

        if let Some(consumed) = native_consumed {
            return consumed;
        }

        // Unknown-to-core code: dispatch through the shared registry.
        if let Some(tagged) = self.osc_registry.dispatch(params) {
            debug!(
                owner = tagged.source_id,
                kind = %tagged.event.kind,
                "harness OSC handler emitted event"
            );
            self.emit_harness(tagged);
            // Consume so alacritty_terminal does not log "unhandled OSC".
            return true;
        }

        false
    }

    fn intercept_apc_byte(&mut self, byte: u8) {
        // The APC state machine in VTE is shared by SOS, PM, and APC strings,
        // so the parser itself validates the Kitty prefix (`G`) before
        // interpreting the body. See `graphics::KittyGraphicsParser`.
        self.graphics_parser.push_byte(byte);
    }

    fn intercept_apc_end(&mut self) -> bool {
        let out = self.graphics_parser.finalize();

        if let Some(event) = out.event {
            debug!(?event, "kitty graphics APC parsed");
            self.emit(InterceptedEvent::Graphics(event));
        } else if !out.response.is_empty() {
            // Parser produced an error-only response (malformed command).
            warn!("kitty graphics APC: malformed, replying with error envelope");
        }

        if !out.response.is_empty() {
            self.emit_graphics_response(out.response);
        }

        // Consume whenever the APC body started with the Kitty prefix, even
        // if it was a mid-chunk that produced neither an event nor a
        // response. Non-Kitty APCs fall through for other interceptors (or
        // the vte default-drop behaviour).
        out.consumed
    }
}

// -- Helpers ------------------------------------------------------------------

/// Parse a 633/133 mark letter and optional data into an [`Osc633Mark`].
fn parse_633_mark(mark_param: &[u8], data_param: Option<&[u8]>) -> Option<Osc633Mark> {
    if mark_param.is_empty() {
        return None;
    }

    let mark_byte = mark_param[0];
    let rest = data_param.unwrap_or(b"");

    match mark_byte {
        b'A' => Some(Osc633Mark::PromptStart),
        b'B' => Some(Osc633Mark::PromptEnd),
        b'C' => Some(Osc633Mark::PreExec),
        b'D' => {
            let exit_code = if rest.is_empty() {
                None
            } else {
                std::str::from_utf8(rest)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
            };
            Some(Osc633Mark::CommandFinished { exit_code })
        }
        b'E' => {
            let command = std::str::from_utf8(rest).ok()?.to_owned();
            Some(Osc633Mark::CommandLine { command })
        }
        _ => None,
    }
}

// -- OSC 9 filtering ----------------------------------------------------------

/// Known shell process names that ConPTY may inject as spurious OSC 9
/// notifications during pane teardown or interop transitions (e.g.
/// `"Powershell: 4"`, `"cmd: 0"`).
const CONPTY_SHELL_NAMES: &[&str] = &[
    "powershell",
    "pwsh",
    "cmd",
    "bash",
    "wsl",
    "conhost",
    "windows terminal",
    "windowsterminal",
    "openssh",
];

/// Returns `true` if the OSC 9 text looks like a spurious ConPTY notification.
///
/// ConPTY emits notifications shaped `"<ProcessName>: <number>"` when a child
/// process exits — these are status messages, not user-facing notifications.
/// We match case-insensitively against a list of known shell/terminal process
/// names.
fn is_spurious_conpty_notification(text: &str) -> bool {
    // Shape: "<name>: <integer>"  (single colon, space, integer with optional sign)
    let Some((name, rest)) = text.split_once(": ") else {
        return false;
    };

    // The "rest" after the colon-space must be a bare integer (the exit code).
    let rest = rest.trim();
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return false;
    }

    // Case-insensitive match against known ConPTY shell names.
    let name_lower = name.trim().to_ascii_lowercase();
    CONPTY_SHELL_NAMES.iter().any(|known| name_lower == *known)
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_all_enabled() {
        let config = InterceptorConfig::default();
        assert!(config.osc_633);
        assert!(config.osc_133);
        assert!(config.osc_7);
        assert!(config.osc_1337);
        assert!(config.osc_7777);
    }

    #[test]
    fn intercept_osc_633_prompt_start() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"633", b"A"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::Osc633(Osc633Mark::PromptStart) => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_osc_633_command_finished() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"633", b"D", b"0"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::Osc633(Osc633Mark::CommandFinished { exit_code: Some(0) }) => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_osc_133_prompt_end() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"133", b"B"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            false,
        );
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::Osc133(Osc633Mark::PromptEnd) => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_osc_7_passes_through() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7", b"file://localhost/home/user"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            false,
        );
        // OSC 7 should NOT be consumed (pass-through).
        assert!(!consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::CurrentDirectory(path) => {
                assert_eq!(path, "/home/user");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_osc_1337_key_value() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"1337", b"CurrentDir=/home/user"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::Iterm2 { key, value } => {
                assert_eq!(key, "CurrentDir");
                assert_eq!(value, "/home/user");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn unknown_osc_passes_through() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"2", b"window title"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(!consumed);
    }

    #[test]
    fn disabled_config_skips() {
        let config = InterceptorConfig {
            osc_633: false,
            osc_133: false,
            osc_7: false,
            osc_9: false,
            osc_1337: false,
            osc_7777: false,
            osc_7337: false,
        };
        let (mut interceptor, _rx) = TherminalInterceptor::new(config);
        let params: &[&[u8]] = &[b"633", b"A"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(!consumed);
    }

    #[test]
    fn command_tracker_updated_by_interceptor() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();

        // Simulate a full command flow through the interceptor.
        let sequences: &[&[&[u8]]] = &[
            &[b"633", b"A"],
            &[b"633", b"B"],
            &[b"633", b"E", b"ls -la"],
            &[b"633", b"C"],
            &[b"633", b"D", b"0"],
        ];

        for params in sequences {
            alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
                &mut interceptor,
                params,
                true,
            );
        }

        let blocks = interceptor.command_tracker.lock().unwrap().snapshot();
        assert_eq!(blocks.len(), 1);
        let block = &blocks[0];
        assert_eq!(block.command, Some("ls -la".to_string()));
        assert_eq!(block.exit_code, Some(0));
    }

    // -- OSC 9 tests -----------------------------------------------------------

    #[test]
    fn intercept_osc_9_notification() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"Build complete!"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::DesktopNotification(text) => {
                assert_eq!(text, "Build complete!");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_no_text_not_consumed() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(!consumed);
    }

    #[test]
    fn osc_9_bel_terminator() {
        // ESC ] 9 ; text BEL  -- bell_terminated = true
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"bel msg"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(t) => assert_eq!(t, "bel msg"),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_st_terminator() {
        // ESC ] 9 ; text ESC \  -- bell_terminated = false
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"st msg"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            false,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(t) => assert_eq!(t, "st msg"),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_empty_payload_does_not_crash() {
        // ESC ] 9 ; BEL  -- payload present but empty
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b""];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        // Should not panic. Whether consumed/emitted is implementation detail;
        // if an event is emitted it must be an empty-string notification.
        if consumed && let Ok(InterceptedEvent::DesktopNotification(t)) = rx.try_recv() {
            assert_eq!(t, "");
        }
    }

    #[test]
    fn osc_9_unicode_payload() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let msg = "Build done ✅ — 日本語 🚀";
        let params: &[&[u8]] = &[b"9", msg.as_bytes()];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(t) => assert_eq!(t, msg),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_disabled_config_not_consumed() {
        let config = InterceptorConfig {
            osc_9: false,
            ..InterceptorConfig::default()
        };
        let (mut interceptor, rx) = TherminalInterceptor::new(config);
        let params: &[&[u8]] = &[b"9", b"should be ignored"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(!consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_9_very_long_payload() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let msg = "A".repeat(8192);
        let params: &[&[u8]] = &[b"9", msg.as_bytes()];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(t) => {
                assert_eq!(t.len(), 8192);
                assert!(t.chars().all(|c| c == 'A'));
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    // -- OSC 9 ConPTY filter tests -----------------------------------------------

    #[test]
    fn osc_9_filters_powershell_exit_code() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"Powershell: 4"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        // Consumed (so alacritty_terminal ignores it) but no event emitted.
        assert!(consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_9_filters_cmd_exit_code() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"cmd: 0"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_9_filters_pwsh_exit_code() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"pwsh: 1"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_9_filters_case_insensitive() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"POWERSHELL: 4"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_9_passes_legitimate_notification() {
        // A real notification containing a colon should NOT be filtered.
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"Build complete: 42 tests passed"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(t) => {
                assert_eq!(t, "Build complete: 42 tests passed");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_passes_notification_with_unknown_process_name() {
        // A process name not in the known list should pass through.
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"myapp: 1"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(t) => {
                assert_eq!(t, "myapp: 1");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn spurious_conpty_filter_unit_tests() {
        // Positive matches (should be filtered).
        assert!(is_spurious_conpty_notification("Powershell: 4"));
        assert!(is_spurious_conpty_notification("powershell: 0"));
        assert!(is_spurious_conpty_notification("POWERSHELL: 1"));
        assert!(is_spurious_conpty_notification("cmd: 0"));
        assert!(is_spurious_conpty_notification("CMD: 255"));
        assert!(is_spurious_conpty_notification("pwsh: 1"));
        assert!(is_spurious_conpty_notification("bash: 127"));
        assert!(is_spurious_conpty_notification("wsl: 0"));
        assert!(is_spurious_conpty_notification("conhost: 0"));
        assert!(is_spurious_conpty_notification("Windows Terminal: 0"));

        // Negative matches (should NOT be filtered).
        assert!(!is_spurious_conpty_notification("Build complete!"));
        assert!(!is_spurious_conpty_notification(
            "Build complete: 42 tests passed",
        ));
        assert!(!is_spurious_conpty_notification("myapp: 1"));
        assert!(!is_spurious_conpty_notification("Powershell: running"));
        assert!(!is_spurious_conpty_notification("Powershell:4")); // no space after colon
        assert!(!is_spurious_conpty_notification(""));
        assert!(!is_spurious_conpty_notification("Powershell: ")); // empty after colon-space
    }

    // -- OSC 9;9 WSL cwd tests (tn-kkr8) ----------------------------------------

    #[test]
    fn osc_9_9_wsl_cwd_unc_path() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"9", b"\\\\wsl.localhost\\Ubuntu\\home\\marci"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::WslCwd(path) => {
                assert_eq!(path, "\\\\wsl.localhost\\Ubuntu\\home\\marci");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_9_wsl_cwd_windows_drive_path() {
        // wslpath -w on /mnt/c/Users/... returns a Windows drive path.
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"9", b"C:\\Users\\marci\\projects"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::WslCwd(path) => {
                assert_eq!(path, "C:\\Users\\marci\\projects");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_9_empty_path_consumed_no_event() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"9", b""];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        // Consumed (malformed) but no event emitted.
        assert!(consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_9_9_does_not_collide_with_plain_notification() {
        // A plain OSC 9 notification (not subtype 9) should still work.
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"Build complete!"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(text) => {
                assert_eq!(text, "Build complete!");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_9_disabled_config_not_consumed() {
        let config = InterceptorConfig {
            osc_9: false,
            ..InterceptorConfig::default()
        };
        let (mut interceptor, rx) = TherminalInterceptor::new(config);
        let params: &[&[u8]] = &[b"9", b"9", b"C:\\Users\\marci"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(!consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_9_9_unicode_path() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let path = "\\\\wsl.localhost\\Ubuntu\\home\\marci\\日本語";
        let params: &[&[u8]] = &[b"9", b"9", path.as_bytes()];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        match rx.try_recv().unwrap() {
            InterceptedEvent::WslCwd(p) => assert_eq!(p, path),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_9_9_missing_path_param() {
        // Only params[0]=9, params[1]=9 — no path. len < 3, falls through
        // to the plain notification path where params[1]=b"9" is just the
        // text "9" (not a known shell name, so it passes the ConPTY filter).
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"9"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        // Falls through to plain notification with text "9".
        match rx.try_recv().unwrap() {
            InterceptedEvent::DesktopNotification(t) => assert_eq!(t, "9"),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    // -- OSC 9;4 taskbar progress tests -----------------------------------------

    #[test]
    fn osc_9_4_clear_progress_consumed_no_event() {
        // `OSC 9 ; 4 ; 0 ST` — clear taskbar progress. This is what hit
        // the user's desktop as a bogus "Therminal / 4" toast before this
        // branch existed.
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"4", b"0"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        assert!(rx.try_recv().is_err(), "OSC 9;4 must not emit any event");
    }

    #[test]
    fn osc_9_4_set_progress_consumed_no_event() {
        // `OSC 9 ; 4 ; 1 ; 50 ST` — set default progress to 50%.
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"4", b"1", b"50"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        assert!(rx.try_recv().is_err(), "OSC 9;4 must not emit any event");
    }

    #[test]
    fn osc_9_4_bare_consumed_no_event() {
        // `OSC 9 ; 4 ST` — no state/pct. Still a progress sequence; must
        // not fall through to the desktop-notification path.
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"9", b"4"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);
        assert!(rx.try_recv().is_err(), "OSC 9;4 must not emit any event");
    }

    // -- OSC 7777 tests ---------------------------------------------------------

    /// Helper to dispatch an OSC 7777 sequence through the interceptor.
    fn dispatch_osc_7777(interceptor: &mut TherminalInterceptor, params: &[&[u8]]) -> bool {
        alacritty_terminal::vte::SequenceInterceptor::intercept_osc(interceptor, params, true)
    }

    #[test]
    fn osc_7777_full_report() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[
            b"7777",
            b"agent=claude-code",
            b"state=tool_use",
            b"tool=Edit",
            b"tokens=12345",
            b"model=opus-4",
        ];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::AgentReport {
                agent,
                state,
                tool,
                tokens,
                model,
            } => {
                assert_eq!(agent, "claude-code");
                assert_eq!(state.as_deref(), Some("tool_use"));
                assert_eq!(tool.as_deref(), Some("Edit"));
                assert_eq!(tokens, Some(12345));
                assert_eq!(model.as_deref(), Some("opus-4"));
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_7777_agent_only() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7777", b"agent=aider"];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::AgentReport {
                agent,
                state,
                tool,
                tokens,
                model,
            } => {
                assert_eq!(agent, "aider");
                assert!(state.is_none());
                assert!(tool.is_none());
                assert!(tokens.is_none());
                assert!(model.is_none());
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_7777_missing_agent_is_consumed_but_no_event() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        // No agent key at all.
        let params: &[&[u8]] = &[b"7777", b"state=thinking"];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(consumed); // consumed to suppress "unhandled" log
        assert!(rx.try_recv().is_err()); // no event emitted
    }

    #[test]
    fn osc_7777_empty_agent_is_consumed_but_no_event() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7777", b"agent="];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(consumed);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_7777_no_params_not_consumed() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        // Only the OSC code, no key=value pairs.
        let params: &[&[u8]] = &[b"7777"];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(!consumed); // nothing to parse
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn osc_7777_unknown_keys_ignored() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[
            b"7777",
            b"agent=copilot",
            b"future_key=future_value",
            b"state=streaming",
        ];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::AgentReport {
                agent, state, tool, ..
            } => {
                assert_eq!(agent, "copilot");
                assert_eq!(state.as_deref(), Some("streaming"));
                assert!(tool.is_none());
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_7777_invalid_tokens_silently_dropped() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7777", b"agent=test", b"tokens=not_a_number"];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::AgentReport { agent, tokens, .. } => {
                assert_eq!(agent, "test");
                assert!(tokens.is_none()); // parse failure -> None
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_7777_malformed_params_without_equals() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        // Params without '=' are skipped; agent is still found.
        let params: &[&[u8]] = &[b"7777", b"garbage", b"agent=ok"];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::AgentReport { agent, .. } => {
                assert_eq!(agent, "ok");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_7777_disabled_config_skips() {
        let config = InterceptorConfig {
            osc_7777: false,
            ..InterceptorConfig::default()
        };
        let (mut interceptor, _rx) = TherminalInterceptor::new(config);
        let params: &[&[u8]] = &[b"7777", b"agent=test"];
        let consumed = dispatch_osc_7777(&mut interceptor, params);
        assert!(!consumed);
    }

    // -- OSC handler registry integration tests -------------------------------

    #[test]
    fn registered_osc_handler_dispatches_through_interceptor() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
        let (harness_tx, harness_rx) = mpsc::channel();
        interceptor.set_harness_event_sink(harness_tx);

        interceptor
            .register_osc_handler(
                1341,
                "claude",
                Box::new(|params| {
                    let payload = params
                        .get(1)
                        .and_then(|b| std::str::from_utf8(b).ok())
                        .map(str::to_string)
                        .unwrap_or_default();
                    Some(crate::osc_registry::HarnessEvent {
                        kind: "claude.test".to_string(),
                        body: serde_json::json!({ "payload": payload }),
                    })
                }),
            )
            .expect("register 1341");

        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            &[b"1341", b"state=thinking"],
            true,
        );
        assert!(consumed, "registry handler should consume unknown OSC");

        let tagged = harness_rx.try_recv().expect("harness event emitted");
        assert_eq!(tagged.source_id, "claude");
        assert_eq!(tagged.event.kind, "claude.test");
        assert_eq!(
            tagged.event.body,
            serde_json::json!({ "payload": "state=thinking" })
        );
    }

    #[test]
    fn native_osc_takes_precedence_over_registry() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
        let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hit_clone = std::sync::Arc::clone(&hit);

        // Attempt to hijack OSC 7 (a core code) via a too-clever registry
        // claim. The reserved range rejects this — but even if we allowed
        // it, the dispatch loop short-circuits on native codes first, so
        // the native OSC 7 path always wins.
        let err = interceptor
            .register_osc_handler(
                7,
                "spoofer",
                Box::new(move |_| {
                    hit_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                    None
                }),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            crate::osc_registry::OscRegistrationError::ReservedCode { code: 7 }
        ));

        // Dispatch OSC 7 through the interceptor — native handler runs,
        // registry was never touched.
        let _ = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            &[b"7", b"file://localhost/tmp"],
            false,
        );
        assert!(!hit.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn unknown_osc_without_registered_handler_passes_through() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
        // Pre-existing behaviour: unknown OSC codes are not consumed.
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            &[b"1341", b"state=thinking"],
            true,
        );
        assert!(!consumed);
    }

    #[test]
    fn shared_registry_visible_through_multiple_interceptors() {
        // Daemon-style wiring: build a shared registry, register one
        // handler, then install it into two independent interceptors.
        let registry = std::sync::Arc::new(crate::osc_registry::OscHandlerRegistry::new());
        registry
            .register(
                1341,
                "claude",
                Box::new(|_| {
                    Some(crate::osc_registry::HarnessEvent {
                        kind: "claude.shared".to_string(),
                        body: serde_json::json!({}),
                    })
                }),
            )
            .expect("register");

        for _ in 0..2 {
            let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
            interceptor.set_osc_registry(std::sync::Arc::clone(&registry));
            let (tx, rx) = mpsc::channel();
            interceptor.set_harness_event_sink(tx);

            let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
                &mut interceptor,
                &[b"1341", b"hello"],
                true,
            );
            assert!(consumed);
            let tagged = rx.try_recv().expect("harness event");
            assert_eq!(tagged.source_id, "claude");
            assert_eq!(tagged.event.kind, "claude.shared");
        }
    }

    #[test]
    fn panicking_registry_handler_does_not_crash_interceptor() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
        let (tx, rx) = mpsc::channel();
        interceptor.set_harness_event_sink(tx);

        interceptor
            .register_osc_handler(1341, "buggy", Box::new(|_| panic!("oh no")))
            .expect("register");

        // First dispatch triggers a panic. The registry catches it and
        // disables the handler, returning `None` — the interceptor then
        // falls through to the "unknown OSC" path and returns `false`.
        // The critical invariant this test locks down is that the panic
        // does not escape and does not poison the interceptor: the handler
        // is marked disabled and the pane keeps processing PTY bytes.
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            &[b"1341", b"first"],
            true,
        );
        assert!(!consumed, "panicking handler should not consume the OSC");
        assert!(
            rx.try_recv().is_err(),
            "no event emitted for panicking handler"
        );
        assert!(interceptor.osc_registry().is_disabled(1341));

        // Second dispatch short-circuits inside the registry (the no-op
        // replacement closure is never called because `is_disabled`
        // returns early). The interceptor again falls through to "unknown
        // OSC" and does not consume.
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            &[b"1341", b"second"],
            true,
        );
        assert!(!consumed);
        assert!(rx.try_recv().is_err());
    }

    // -- OSC 7337 tests (tn-ttie) -------------------------------------------------

    #[test]
    fn intercept_osc_7337_valid_pid() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7337", b"12345"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::WslShellPid(pid) => {
                assert_eq!(pid, 12345);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_osc_7337_invalid_pid_consumed_but_no_event() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7337", b"not-a-number"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        // Consumed even on bad PID (we own the code).
        assert!(consumed);
        // But no event emitted.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn intercept_osc_7337_missing_param_not_consumed() {
        let (mut interceptor, _rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7337"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(!consumed);
    }

    #[test]
    fn intercept_osc_7337_whitespace_trimmed() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let params: &[&[u8]] = &[b"7337", b"  42  "];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        assert!(consumed);

        let event = rx.try_recv().unwrap();
        match event {
            InterceptedEvent::WslShellPid(pid) => {
                assert_eq!(pid, 42);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn osc_7337_disabled_config_skips() {
        let config = InterceptorConfig {
            osc_7337: false,
            ..InterceptorConfig::default()
        };
        let (mut interceptor, _rx) = TherminalInterceptor::new(config);
        let params: &[&[u8]] = &[b"7337", b"12345"];
        let consumed = alacritty_terminal::vte::SequenceInterceptor::intercept_osc(
            &mut interceptor,
            params,
            true,
        );
        // When disabled, falls through to registry → not consumed.
        assert!(!consumed);
    }

    // -- Kitty graphics APC tests (tn-7xme) --------------------------------------

    /// Feed an APC body byte-by-byte and call `intercept_apc_end`.
    fn drive_apc(interceptor: &mut TherminalInterceptor, body: &str) -> bool {
        use alacritty_terminal::vte::SequenceInterceptor as _;
        for b in body.as_bytes() {
            interceptor.intercept_apc_byte(*b);
        }
        interceptor.intercept_apc_end()
    }

    #[test]
    fn intercept_apc_feature_query_emits_ok() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let (resp_tx, resp_rx) = mpsc::channel::<Vec<u8>>();
        interceptor.set_graphics_response_sink(resp_tx);

        // Canonical Kitty feature query: `\x1b_Gi=1,a=q;\x1b\\`.
        let consumed = drive_apc(&mut interceptor, "Gi=1,a=q;");
        assert!(consumed, "APC should be consumed");

        // Event: GraphicsQuery
        let event = rx.try_recv().expect("event expected");
        match event {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsQuery {
                image_id: Some(1), ..
            }) => {}
            other => panic!("unexpected event: {:?}", other),
        }

        // Response: `\x1b_Gi=1;OK\x1b\\` — the required reply to the probe.
        let bytes = resp_rx.try_recv().expect("response expected");
        assert!(bytes.starts_with(b"\x1b_G"));
        assert!(bytes.ends_with(b"\x1b\\"));
        assert!(bytes.windows(2).any(|w| w == b"OK"));
    }

    #[test]
    fn intercept_apc_transmit_single_chunk() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();

        let consumed = drive_apc(&mut interceptor, "Ga=t,f=100,i=42;ZmFrZQ==");
        assert!(consumed);

        match rx.try_recv().unwrap() {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsTransmit {
                image_id,
                payload,
                display,
                ..
            }) => {
                assert_eq!(image_id, Some(42));
                assert!(!display);
                assert_eq!(payload, b"ZmFrZQ==");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_apc_transmit_multi_chunk_reassembles() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();

        assert!(drive_apc(&mut interceptor, "Ga=t,f=100,i=7,m=1;AAAA",));
        // Intermediate chunks don't emit an event.
        assert!(rx.try_recv().is_err());

        assert!(drive_apc(&mut interceptor, "Ga=t,i=7,m=1;BBBB"));
        assert!(rx.try_recv().is_err());

        assert!(drive_apc(&mut interceptor, "Ga=t,i=7,m=0;CCCC"));
        match rx.try_recv().unwrap() {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsTransmit { payload, .. }) => {
                assert_eq!(payload, b"AAAABBBBCCCC");
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_apc_display_emits_event() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        assert!(drive_apc(&mut interceptor, "Ga=p,i=3,r=5,c=10,z=1"));
        match rx.try_recv().unwrap() {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsDisplay {
                image_id: Some(3),
                rows: Some(5),
                cols: Some(10),
                z_index: Some(1),
                ..
            }) => {}
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_apc_delete_by_id_and_all() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();

        // a=d,i=N — delete by id.
        assert!(drive_apc(&mut interceptor, "Ga=d,i=8"));
        match rx.try_recv().unwrap() {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsDelete { scope, .. }) => {
                use crate::graphics::DeleteScope;
                match scope {
                    DeleteScope::ById { image_id, .. } => assert_eq!(image_id, Some(8)),
                    other => panic!("expected ById, got {:?}", other),
                }
            }
            other => panic!("unexpected event: {:?}", other),
        }

        // a=d — delete all.
        assert!(drive_apc(&mut interceptor, "Ga=d"));
        match rx.try_recv().unwrap() {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsDelete { scope, .. }) => {
                use crate::graphics::DeleteScope;
                assert_eq!(scope, DeleteScope::All);
            }
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[test]
    fn intercept_apc_quiet_level_2_suppresses_response() {
        let (mut interceptor, _event_rx) = TherminalInterceptor::with_defaults();
        let (resp_tx, resp_rx) = mpsc::channel::<Vec<u8>>();
        interceptor.set_graphics_response_sink(resp_tx);

        assert!(drive_apc(&mut interceptor, "Ga=t,f=100,i=5,q=2;Zg=="));
        assert!(
            resp_rx.try_recv().is_err(),
            "q=2 should suppress all responses"
        );
    }

    #[test]
    fn intercept_apc_quiet_level_0_emits_response() {
        let (mut interceptor, _event_rx) = TherminalInterceptor::with_defaults();
        let (resp_tx, resp_rx) = mpsc::channel::<Vec<u8>>();
        interceptor.set_graphics_response_sink(resp_tx);

        assert!(drive_apc(&mut interceptor, "Ga=t,f=100,i=5,q=0;Zg=="));
        let bytes = resp_rx.try_recv().expect("q=0 should emit response");
        assert!(bytes.windows(2).any(|w| w == b"OK"));
    }

    #[test]
    fn intercept_apc_malformed_does_not_panic_and_no_event() {
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        // Not a kitty APC at all (prefix mismatch).
        let _ = drive_apc(&mut interceptor, "NotKittyAtAll");
        assert!(rx.try_recv().is_err());

        // Malformed body with Kitty prefix — no event, but the parser still
        // flags the APC as handled (it emitted an error response envelope).
        let _ = drive_apc(&mut interceptor, "Ga=nope");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn intercept_apc_kitten_icat_fixture() {
        // Synthetic approximation of a `kitty +kitten icat` session:
        //   1. feature probe (q=0) → event Query, response OK
        //   2. multi-chunk PNG transmit-and-display
        //   3. implicit end triggers a single Transmit event with display=true
        let (mut interceptor, rx) = TherminalInterceptor::with_defaults();
        let (resp_tx, resp_rx) = mpsc::channel::<Vec<u8>>();
        interceptor.set_graphics_response_sink(resp_tx);

        // 1. Probe
        assert!(drive_apc(&mut interceptor, "Gi=1,a=q;"));
        match rx.try_recv().unwrap() {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsQuery { .. }) => {}
            other => panic!("probe: unexpected {:?}", other),
        }
        assert!(resp_rx.try_recv().unwrap().windows(2).any(|w| w == b"OK"));

        // 2. Chunks (icat uses a=T for inline display).
        assert!(drive_apc(
            &mut interceptor,
            "Ga=T,f=100,i=1,s=32,v=32,m=1;AAAA",
        ));
        assert!(rx.try_recv().is_err()); // mid-chunk, no event
        assert!(drive_apc(&mut interceptor, "Ga=T,i=1,m=1;BBBB"));
        assert!(drive_apc(&mut interceptor, "Ga=T,i=1,m=0;CCCC"));

        // 3. Terminal sees one final GraphicsTransmit with display=true.
        match rx.try_recv().unwrap() {
            InterceptedEvent::Graphics(GraphicsEvent::GraphicsTransmit {
                image_id: Some(1),
                display: true,
                width_px: Some(32),
                height_px: Some(32),
                payload,
                ..
            }) => {
                assert_eq!(payload, b"AAAABBBBCCCC");
            }
            other => panic!("icat: unexpected final event {:?}", other),
        }
    }
}
