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
//! | OSC 1337  | iTerm2              | Various (used by some agents)  |
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
}

impl Default for InterceptorConfig {
    fn default() -> Self {
        Self {
            osc_633: true,
            osc_133: true,
            osc_7: true,
            osc_1337: true,
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
            b"1337" if self.config.osc_1337 => self.handle_osc_1337(params),
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
            osc_1337: false,
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
}
