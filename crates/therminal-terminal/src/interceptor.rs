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
//! | OSC 1337  | iTerm2              | Various (used by some agents)  |
//! | OSC 7777  | Therminal           | Cooperative agent self-report   |
//!
//! [`SequenceInterceptor`]: alacritty_terminal::vte::SequenceInterceptor

use std::sync::mpsc;

use tracing::{debug, trace};

use crate::osc633::{CommandTracker, Osc633Mark};

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
    /// Inline command tracker for OSC 633 marks.
    pub command_tracker: CommandTracker,
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
                command_tracker: CommandTracker::new(),
            },
            rx,
        )
    }

    /// Create with default config.
    pub fn with_defaults() -> (Self, mpsc::Receiver<InterceptedEvent>) {
        Self::new(InterceptorConfig::default())
    }

    /// Send an event, logging if the receiver has been dropped.
    fn emit(&self, event: InterceptedEvent) {
        if self.event_tx.send(event).is_err() {
            trace!("interceptor event receiver dropped");
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
        self.command_tracker.apply(&mark);
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
        self.command_tracker.apply(&mark);
        self.emit(InterceptedEvent::Osc133(mark));

        // Consume: alacritty_terminal also ignores these.
        true
    }

    /// Handle OSC 9 (desktop notification). Returns `true` to consume.
    fn handle_osc_9(&mut self, params: &[&[u8]]) -> bool {
        // Format: OSC 9 ; <notification text> ST
        if params.len() < 2 {
            return false;
        }

        let text = match std::str::from_utf8(params[1]) {
            Ok(s) => s,
            Err(_) => return false,
        };

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
}

impl alacritty_terminal::vte::SequenceInterceptor for TherminalInterceptor {
    fn intercept_osc(&mut self, params: &[&[u8]], _bell_terminated: bool) -> bool {
        if params.is_empty() {
            return false;
        }

        match params[0] {
            b"633" if self.config.osc_633 => self.handle_osc_633(params),
            b"133" if self.config.osc_133 => self.handle_osc_133(params),
            b"7" if self.config.osc_7 => self.handle_osc_7(params),
            b"9" if self.config.osc_9 => self.handle_osc_9(params),
            b"1337" if self.config.osc_1337 => self.handle_osc_1337(params),
            b"7777" if self.config.osc_7777 => self.handle_osc_7777(params),
            _ => false,
        }
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

        assert_eq!(interceptor.command_tracker.blocks.len(), 1);
        let block = &interceptor.command_tracker.blocks[0];
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
        if consumed
            && let Ok(InterceptedEvent::DesktopNotification(t)) = rx.try_recv()
        {
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
}
