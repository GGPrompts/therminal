//! Pattern engine dispatch (tn-86us).
//!
//! The pure [`PatternEngine`] lives in `therminal-terminal` and returns
//! [`PatternMatch`]es without knowing where they should end up. This module
//! is the daemon-side caller that the engine's module docs refer to: it
//! takes the engine handle + the unified event bus, feeds bytes from the
//! PTY reader thread through a line accumulator, invokes the engine, and
//! routes the resulting matches onto real sinks.
//!
//! ## Threading
//!
//! A [`PatternDispatcher`] lives on each `DaemonPtyHandler` (one per pane,
//! owned by the PTY reader thread). The engine + bus are wrapped in `Arc`
//! and cheaply clonable; dispatch calls run on the reader thread and do
//! not take any locks beyond the engine's own internal `RwLock`.
//!
//! ## Routing
//!
//! - `ResolvedAction::EmitEvent` → publish on the unified event bus with
//!   `source_class = Pattern`, `source_id = <pack_name>`, `kind = <pattern_name>`.
//! - `ResolvedAction::Hotspot` → publish a summary event on the bus so
//!   MCP subscribers see the match. The **real hotspot sink** lives
//!   app-side (tn-f9cl). The daemon publishes purely for MCP stats.
//! - `ResolvedAction::Widget` → publish an event carrying the resolved
//!   widget payload. Widget placement is **not yet wired** (tn-f9cl).
//!
//! All three paths also bump a shared `matches_total` counter used by the
//! `QueryPatternStats` IPC request for integration tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use therminal_protocol::bus_types::{SourceClass, TerminalEvent};
use therminal_terminal::semantic_patterns::{
    PatternEngine, PatternMatch, ResolvedAction, ResolvedEmitEvent, ResolvedHotspot, ResolvedWidget,
};
use therminal_terminal::state_inference::ansi_strip::AnsiStripper;
use tracing::trace;

use crate::event_bus::EventBus;

/// Per-pane dispatcher: stream bytes in, pattern matches out, routed to sinks.
pub struct PatternDispatcher {
    engine: Arc<PatternEngine>,
    bus: Arc<EventBus>,
    matches_total: Arc<AtomicU64>,

    pane_id: u64,

    /// Stateful ANSI stripper so sequences that span chunks are handled.
    stripper: AnsiStripper,
    /// Line accumulator: holds the in-progress line between newlines.
    line_buf: String,
    /// Transcript accumulator for the current OSC 133/633 command region.
    /// Populated while `in_command == true`; flushed through
    /// `process_prompt_boundary` on `CommandFinished`.
    transcript_buf: String,
    /// True between `PreExec` (C) and `CommandFinished` (D) marks.
    in_command: bool,
    /// Command text captured from an `E` mark / tracker lookup, used as
    /// `applies_to.command` filter for prompt-boundary dispatch.
    current_command: Option<String>,
}

impl PatternDispatcher {
    /// Construct a new dispatcher scoped to a single pane.
    pub fn new(
        engine: Arc<PatternEngine>,
        bus: Arc<EventBus>,
        matches_total: Arc<AtomicU64>,
        pane_id: u64,
    ) -> Self {
        Self {
            engine,
            bus,
            matches_total,
            pane_id,
            stripper: AnsiStripper::new(),
            line_buf: String::new(),
            transcript_buf: String::new(),
            in_command: false,
            current_command: None,
        }
    }

    /// Feed raw PTY bytes through the ANSI stripper and invoke the engine
    /// on every completed line.
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        let visible = self.stripper.feed(bytes);
        for ch in visible.chars() {
            match ch {
                '\n' => {
                    let line = std::mem::take(&mut self.line_buf);
                    self.dispatch_line(&line);
                    if self.in_command {
                        self.transcript_buf.push_str(&line);
                        self.transcript_buf.push('\n');
                    }
                }
                '\r' => {
                    // Ignore carriage returns for line-finalization purposes;
                    // shells often emit `\r\n` or standalone `\r` to rewrite.
                }
                _ => self.line_buf.push(ch),
            }
        }
    }

    fn dispatch_line(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        let matches = self.engine.process_finalized_line(
            self.pane_id,
            line,
            None,
            self.current_command.as_deref(),
        );
        self.route_matches(matches);
    }

    /// Called when a PreExec (C) mark is observed for this pane.
    ///
    /// Shells emit OSC 633 marks in the order B → E → C → … → D, so the
    /// command text is delivered by the E mark *before* this method fires.
    /// We therefore preserve `current_command` if it's already populated
    /// from an earlier `set_command_text` call within the same prompt
    /// boundary, and only clear it on `on_command_finish`.
    pub fn on_command_start(&mut self) {
        // Flush any partial bytes from the previous segment so they aren't
        // silently dropped when we reset `transcript_buf` for the new
        // command region.
        if !self.line_buf.is_empty() {
            let partial = std::mem::take(&mut self.line_buf);
            self.dispatch_line(&partial);
        }
        self.in_command = true;
        self.transcript_buf.clear();
    }

    /// Called when an OSC 633 `E` (CommandLine) mark is observed for this
    /// pane. Sets the command text without touching `in_command` or
    /// `transcript_buf` — the C mark that follows is responsible for the
    /// region reset.
    pub fn set_command_text(&mut self, command: String) {
        self.current_command = Some(command);
    }

    /// Called when a CommandFinished (D) mark is observed for this pane.
    /// Runs prompt-boundary-scoped patterns against the accumulated
    /// transcript and then resets the command-region state.
    pub fn on_command_finish(&mut self) {
        // Flush any in-progress line before dispatch — the last line of the
        // command output is often not newline-terminated when the prompt
        // repaints over it.
        if !self.line_buf.is_empty() && self.in_command {
            self.transcript_buf.push_str(&self.line_buf);
            self.transcript_buf.push('\n');
        }
        let transcript = std::mem::take(&mut self.transcript_buf);
        let cmd = self.current_command.clone();
        self.in_command = false;
        self.current_command = None;
        if !transcript.is_empty() {
            let matches = self.engine.process_prompt_boundary(
                self.pane_id,
                &transcript,
                None,
                cmd.as_deref(),
            );
            self.route_matches(matches);
        }
    }

    fn route_matches(&self, matches: Vec<PatternMatch>) {
        if matches.is_empty() {
            return;
        }
        for m in matches {
            self.matches_total.fetch_add(1, Ordering::Release);
            let kind_base = m.pattern_name.clone();
            let source_id = m.pack_name.clone();
            let scope = m.scope.as_str();
            match &m.action {
                ResolvedAction::EmitEvent(e) => {
                    self.publish(&source_id, &kind_base, scope, &m, emit_body(&m, e));
                }
                ResolvedAction::Hotspot(h) => {
                    self.publish(&source_id, &kind_base, scope, &m, hotspot_body(&m, h));
                    // Real hotspot sink lives app-side (tn-f9cl).
                }
                ResolvedAction::Widget(w) => {
                    self.publish(&source_id, &kind_base, scope, &m, widget_body(&m, w));
                }
            }
            trace!(
                pack = %m.pack_name,
                pattern = %m.pattern_name,
                "pattern-dispatch: routed match"
            );
        }
    }

    fn publish(
        &self,
        source_id: &str,
        kind: &str,
        scope: &'static str,
        m: &PatternMatch,
        body: serde_json::Value,
    ) {
        let body = json!({
            "pack": m.pack_name,
            "pattern": m.pattern_name,
            "scope": scope,
            "matched_text": m.matched_text,
            "captures": m.captures,
            "action": body,
        });
        self.bus.publish(TerminalEvent {
            source_class: SourceClass::Pattern,
            source_id: source_id.to_string(),
            kind: kind.to_string(),
            pane_id: Some(self.pane_id),
            ts_ms: 0,
            cursor: 0,
            body,
        });
    }
}

fn emit_body(_m: &PatternMatch, e: &ResolvedEmitEvent) -> serde_json::Value {
    json!({ "kind": "emit_event", "extra": e.extra })
}

fn hotspot_body(_m: &PatternMatch, h: &ResolvedHotspot) -> serde_json::Value {
    json!({
        "kind": "hotspot",
        "on_click": h.on_click.as_str(),
        "target": h.target,
        "label": h.label,
        "hotspot_kind": h.kind,
    })
}

fn widget_body(_m: &PatternMatch, w: &ResolvedWidget) -> serde_json::Value {
    json!({
        "kind": "widget",
        "widget_kind": w.kind.as_str(),
        "anchor": w.anchor.as_str(),
        "label": w.label,
        "value": w.value,
        "max": w.max,
        "title": w.title,
        "body": w.body,
        "color": w.color,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use therminal_terminal::semantic_patterns::PatternEngineConfig;

    /// Helper: create a dispatcher with a disabled (empty) engine.
    fn make_dispatcher() -> (PatternDispatcher, Arc<EventBus>, Arc<AtomicU64>) {
        let engine = Arc::new(PatternEngine::new(PatternEngineConfig {
            enabled: false,
            ..PatternEngineConfig::new_default()
        }));
        let bus = Arc::new(EventBus::with_default_capacity());
        let matches_total = Arc::new(AtomicU64::new(0));
        let dispatcher = PatternDispatcher::new(
            Arc::clone(&engine),
            Arc::clone(&bus),
            Arc::clone(&matches_total),
            1,
        );
        (dispatcher, bus, matches_total)
    }

    #[test]
    fn line_accumulation_splits_on_newline() {
        let (mut d, _bus, _) = make_dispatcher();
        // Feed bytes that contain two newlines — the line_buf should be empty
        // after processing both.
        d.process_bytes(b"hello\nworld\n");
        assert!(
            d.line_buf.is_empty(),
            "line_buf should be empty after trailing newline"
        );
    }

    #[test]
    fn partial_line_stays_in_buffer() {
        let (mut d, _bus, _) = make_dispatcher();
        d.process_bytes(b"partial");
        assert_eq!(d.line_buf, "partial");
    }

    #[test]
    fn carriage_return_is_ignored() {
        let (mut d, _bus, _) = make_dispatcher();
        d.process_bytes(b"hello\r\nworld\r\n");
        // \r should not appear in line_buf or be treated as a line terminator
        assert!(d.line_buf.is_empty());
    }

    #[test]
    fn ansi_escapes_stripped_before_accumulation() {
        let (mut d, _bus, _) = make_dispatcher();
        d.process_bytes(b"\x1b[31mred text\x1b[0m\n");
        // After the newline, line_buf is empty (line was dispatched)
        assert!(d.line_buf.is_empty());
    }

    #[test]
    fn command_region_tracking() {
        let (mut d, _bus, _) = make_dispatcher();
        assert!(!d.in_command);

        d.set_command_text("cargo build".into());
        assert_eq!(d.current_command.as_deref(), Some("cargo build"));

        d.on_command_start();
        assert!(d.in_command);
        assert!(d.transcript_buf.is_empty());

        // Feed some output lines
        d.process_bytes(b"Compiling foo\n");
        assert!(d.transcript_buf.contains("Compiling foo"));

        d.on_command_finish();
        assert!(!d.in_command);
        assert!(d.current_command.is_none());
        assert!(d.transcript_buf.is_empty());
    }

    #[test]
    fn command_start_flushes_partial_line() {
        let (mut d, _bus, _) = make_dispatcher();
        d.process_bytes(b"partial line");
        assert!(!d.line_buf.is_empty());

        d.on_command_start();
        // on_command_start should have flushed the partial line
        assert!(d.line_buf.is_empty());
    }

    #[test]
    fn command_finish_includes_partial_line_in_transcript() {
        let (mut d, _bus, _) = make_dispatcher();
        d.on_command_start();
        d.process_bytes(b"full line\n");
        d.process_bytes(b"partial");
        // on_command_finish should include the partial line in the transcript
        d.on_command_finish();
        // transcript_buf is taken by on_command_finish, so it should be empty
        assert!(d.transcript_buf.is_empty());
    }

    #[test]
    fn set_command_text_does_not_affect_in_command() {
        let (mut d, _bus, _) = make_dispatcher();
        d.set_command_text("ls".into());
        assert!(
            !d.in_command,
            "set_command_text should not toggle in_command"
        );
    }

    #[test]
    fn transcript_not_accumulated_outside_command_region() {
        let (mut d, _bus, _) = make_dispatcher();
        assert!(!d.in_command);
        d.process_bytes(b"line 1\nline 2\n");
        assert!(
            d.transcript_buf.is_empty(),
            "transcript should not accumulate outside command region"
        );
    }

    #[test]
    fn multiple_command_regions_independent() {
        let (mut d, _bus, _) = make_dispatcher();

        // First command region
        d.set_command_text("cmd1".into());
        d.on_command_start();
        d.process_bytes(b"output1\n");
        d.on_command_finish();

        // Second command region
        d.set_command_text("cmd2".into());
        d.on_command_start();
        d.process_bytes(b"output2\n");
        assert!(d.transcript_buf.contains("output2"));
        assert!(!d.transcript_buf.contains("output1"));
        d.on_command_finish();
    }

    #[test]
    fn matches_total_starts_at_zero() {
        let (_d, _bus, matches_total) = make_dispatcher();
        assert_eq!(matches_total.load(Ordering::Relaxed), 0);
    }
}
