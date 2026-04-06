//! Agent state inference from terminal output.
//!
//! Maps PTY output patterns and OSC 633 command tracker transitions to agent
//! session states, then writes state files in the format consumed by
//! [`therminal_core::claude_state::ClaudeStatePoller`].
//!
//! This replaces the need for external hook scripts that write to
//! `/tmp/claude-code-state/`, `/tmp/codex-state/`, and `/tmp/copilot-state/`.
//!
//! # Architecture
//!
//! ```text
//! PTY output bytes
//!   -> byte_processor (existing)
//!     -> OSC 633 -> CommandTracker (existing)
//!     -> AgentStateInference (this module)
//!       -> pattern matching on recent output lines
//!       -> state file writes to /tmp/{agent}-state/
//! ```

mod ansi_strip;
mod cadence;
mod patterns;
mod persistence;
mod types;

// Re-export all public types so they remain importable from
// `therminal_terminal::state_inference::*`.
pub use cadence::{ByteChunkStats, OutputCadence};
pub use types::{AgentType, InferenceConfig, InferredStatus, StateChangeNotification};

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use tracing::{debug, trace};

use crate::event_log::{EventLog, SessionEvent};
use crate::osc633::CommandState;

use ansi_strip::AnsiStripper;
use cadence::{MAX_CHUNK_STATS, classify_output_cadence, is_spinner_pattern, is_streaming_cadence};
use patterns::Patterns;
use persistence::{cleanup as do_cleanup, update_state_file_path, write_state_file};
use types::StateFile;

/// Maximum number of recent lines to keep in the ring buffer.
const MAX_RECENT_LINES: usize = 50;

/// Minimum interval between state file writes.
const MIN_WRITE_INTERVAL: Duration = Duration::from_millis(500);

/// Agent state inference engine.
///
/// Tracks recent terminal output lines and command tracker state to infer
/// the agent's current status. Writes state files atomically when the
/// inferred state changes.
pub struct AgentStateInference {
    config: InferenceConfig,
    /// Recent output lines (ring buffer, newest at back).
    recent_lines: VecDeque<String>,
    /// Current accumulated line (not yet terminated by newline).
    current_line: String,
    /// Last inferred status.
    last_status: InferredStatus,
    /// Last OSC 633 command state we saw.
    last_command_state: Option<CommandState>,
    /// Detected agent type from output (may override config).
    detected_agent_type: Option<AgentType>,
    /// Detected model name from output.
    detected_model: Option<String>,
    /// Last detected context percentage.
    context_percent: Option<f32>,
    /// Timestamp of last state file write (throttle writes).
    last_write: Instant,
    /// Path to the state file we manage.
    state_file_path: Option<PathBuf>,
    /// Compiled regex patterns (shared across calls).
    patterns: Patterns,
    /// Stateful ANSI escape sequence stripper (carries parse state across
    /// `feed_bytes` calls so split sequences don't leak into visible text).
    ansi_stripper: AnsiStripper,
    /// Sliding window of recent chunk statistics for cadence analysis.
    chunk_stats: VecDeque<ByteChunkStats>,
    /// Dirty flag -- set when any exported field changes, cleared on write.
    /// Persists across `infer_and_write()` calls so throttled writes are
    /// retried instead of lost.
    dirty: bool,
    // -- Command telemetry (populated from CommandTracker) --------------------
    /// The most recent command string (from OSC 633;E).
    last_command: Option<String>,
    /// Exit code of the most recent finished command (from OSC 633;D).
    last_exit_code: Option<i32>,
    /// Wall-clock instant when the current command started executing.
    command_started_at: Option<Instant>,
    /// ISO 8601 timestamp string when the current command started executing.
    command_started_at_iso: Option<String>,
    /// Duration in milliseconds from start to finish of the last command.
    last_command_duration_ms: Option<i64>,
    /// Count of consecutive non-zero exit codes.
    consecutive_failures: i64,
    /// Optional per-session JSONL event log for structured diagnostics.
    event_log: Option<EventLog>,
    /// Optional channel for emitting state change notifications to the daemon.
    change_tx: Option<std_mpsc::Sender<StateChangeNotification>>,
    /// Whether structured JSON output mode has been detected.
    structured_json_detected: bool,
    /// Number of consecutive JSON object lines seen (for sniffing).
    json_line_streak: u8,
}

impl AgentStateInference {
    /// Create a new inference engine with the given configuration.
    pub fn new(config: InferenceConfig) -> Self {
        let state_file_path = config
            .agent_type
            .map(|at| PathBuf::from(at.state_dir()).join(format!("{}.json", config.session_id)));

        Self {
            config,
            recent_lines: VecDeque::with_capacity(MAX_RECENT_LINES + 1),
            current_line: String::new(),
            last_status: InferredStatus::Idle,
            last_command_state: None,
            detected_agent_type: None,
            detected_model: None,
            context_percent: None,
            last_write: Instant::now() - MIN_WRITE_INTERVAL, // allow immediate first write
            state_file_path,
            patterns: Patterns::new(),
            ansi_stripper: AnsiStripper::new(),
            chunk_stats: VecDeque::with_capacity(MAX_CHUNK_STATS + 1),
            dirty: false,
            last_command: None,
            last_exit_code: None,
            command_started_at: None,
            command_started_at_iso: None,
            last_command_duration_ms: None,
            consecutive_failures: 0,
            event_log: None,
            change_tx: None,
            structured_json_detected: false,
            json_line_streak: 0,
        }
    }

    /// Attach a per-session event log.
    ///
    /// Must be called after construction to enable event logging. If not
    /// called, no events are recorded.
    pub fn set_event_log(&mut self, log: EventLog) {
        self.event_log = Some(log);
    }

    /// Attach a state change notification sender.
    ///
    /// When set, the inference engine emits [`StateChangeNotification`]s on
    /// every detected state transition. The daemon uses this channel to
    /// bridge into semantic events without polling.
    pub fn set_change_tx(&mut self, tx: std_mpsc::Sender<StateChangeNotification>) {
        self.change_tx = Some(tx);
    }

    /// Emit a state change notification (non-blocking, best-effort).
    fn emit(&self, notification: StateChangeNotification) {
        if let Some(ref tx) = self.change_tx {
            let _ = tx.send(notification);
        }
    }

    /// Get a mutable reference to the event log (if attached).
    pub fn event_log_mut(&mut self) -> Option<&mut EventLog> {
        self.event_log.as_mut()
    }

    // -- Public getters for daemon-owned state bridging -----------------------

    /// Current inferred status.
    pub fn last_status(&self) -> &InferredStatus {
        &self.last_status
    }

    /// Session ID from config.
    pub fn session_id(&self) -> &str {
        &self.config.session_id
    }

    /// PID of the PTY child process.
    pub fn child_pid(&self) -> u32 {
        self.config.child_pid
    }

    /// Working directory from config.
    pub fn working_dir(&self) -> Option<&str> {
        self.config.working_dir.as_deref()
    }

    /// Detected or configured agent type.
    pub fn agent_type(&self) -> Option<AgentType> {
        self.effective_agent_type()
    }

    /// Detected model name from output.
    pub fn detected_model(&self) -> Option<&str> {
        self.detected_model.as_deref()
    }

    /// Last detected context percentage (0.0..100.0).
    pub fn context_percent(&self) -> Option<f32> {
        self.context_percent
    }

    /// Last command string (from OSC 633).
    pub fn last_command(&self) -> Option<&str> {
        self.last_command.as_deref()
    }

    /// Last command exit code.
    pub fn last_exit_code(&self) -> Option<i32> {
        self.last_exit_code
    }

    /// Duration of the last command in milliseconds.
    pub fn last_command_duration_ms(&self) -> Option<i64> {
        self.last_command_duration_ms
    }

    /// Count of consecutive non-zero exit codes.
    pub fn consecutive_failures(&self) -> i64 {
        self.consecutive_failures
    }

    /// Classify the output stream cadence from the recent chunk window.
    ///
    /// Distinguishes human typing, agent output, and burst dumps based on
    /// chunk sizes, inter-chunk intervals, and the presence of backspaces.
    pub fn classify_output_cadence(&self) -> OutputCadence {
        classify_output_cadence(&self.chunk_stats)
    }

    /// Check if recent output looks like a spinner pattern.
    ///
    /// Spinners are characterized by cursor-control-heavy output with low
    /// visible text content -- the terminal is being rewritten in place.
    pub fn is_spinner_pattern(&self) -> bool {
        is_spinner_pattern(&self.chunk_stats)
    }

    /// Check if recent output is a sustained high-throughput stream.
    ///
    /// Streaming is characterized by >500 visible chars/sec sustained over
    /// at least 2 seconds with no backspaces -- e.g., an agent writing a
    /// long code block or explanation.
    pub fn is_streaming_cadence(&self) -> bool {
        is_streaming_cadence(&self.chunk_stats)
    }

    /// Read-only access to the chunk stats window (for external analysis).
    pub fn chunk_stats(&self) -> &VecDeque<ByteChunkStats> {
        &self.chunk_stats
    }

    /// Get the effective agent type (config override > detected > None).
    fn effective_agent_type(&self) -> Option<AgentType> {
        self.config.agent_type.or(self.detected_agent_type)
    }

    /// Feed a chunk of raw PTY output bytes.
    ///
    /// Extracts visible text lines (stripping ANSI escape sequences) and
    /// updates the recent lines buffer. Call [`Self::update_command_state`]
    /// separately with the latest `CommandState` from the tracker.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.feed_bytes_at(bytes, Instant::now());
    }

    /// Feed bytes with an explicit timestamp (used by tests for mock timing).
    fn feed_bytes_at(&mut self, bytes: &[u8], timestamp: Instant) {
        // Collect chunk stats before stripping ANSI for cadence analysis.
        let has_backspace = bytes.iter().any(|&b| b == 0x08 || b == 0x7F);
        let has_cursor_control = Self::scan_cursor_control(bytes);

        // Stateful ANSI stripping: carries parse state across calls so that
        // escape sequences split across PTY read boundaries are consumed
        // correctly instead of leaking fragments into visible text.
        let text = self.ansi_stripper.feed(bytes);

        let visible_chars = text.chars().filter(|c| !c.is_control()).count();

        // Record chunk statistics for cadence analysis.
        if !bytes.is_empty() {
            self.chunk_stats.push_back(ByteChunkStats {
                timestamp,
                byte_count: bytes.len(),
                has_backspace,
                has_cursor_control,
                visible_chars,
            });
            if self.chunk_stats.len() > MAX_CHUNK_STATS {
                self.chunk_stats.pop_front();
            }
        }

        for ch in text.chars() {
            if ch == '\n' || ch == '\r' {
                if !self.current_line.is_empty() {
                    let line = std::mem::take(&mut self.current_line);
                    self.push_line(line);
                }
            } else if ch.is_control() {
                // Skip other control chars
            } else {
                self.current_line.push(ch);
            }
        }

        // Also try to detect agent type and model from the output.
        self.detect_agent_from_output();

        // Run inference and write if state changed.
        self.infer_and_write();
    }

    /// Scan raw bytes for CSI cursor control sequences (movement, erase).
    ///
    /// Looks for ESC \[ ... followed by a final byte that indicates cursor
    /// movement (A-H, J, K) rather than graphics (m) or other attributes.
    fn scan_cursor_control(bytes: &[u8]) -> bool {
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // Found CSI; skip to final byte.
                i += 2;
                while i < bytes.len() {
                    let b = bytes[i];
                    if (0x40..=0x7E).contains(&b) {
                        // Final byte: check if it's a cursor control command.
                        if matches!(
                            b,
                            b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'G' | b'H' | b'J' | b'K'
                        ) {
                            return true;
                        }
                        break;
                    }
                    i += 1;
                }
            }
            i += 1;
        }
        false
    }

    /// Update with the latest OSC 633 command state and telemetry from
    /// the [`CommandBlock`].
    ///
    /// Should be called after the command tracker processes marks.  The
    /// `command` and `exit_code` are extracted from the current
    /// [`crate::osc633::CommandBlock`] at the call site.
    pub fn update_command_state(
        &mut self,
        state: CommandState,
        command: Option<&str>,
        exit_code: Option<i32>,
    ) {
        // Track command telemetry on state transitions.
        match state {
            CommandState::Executing => {
                // Command just started executing -- record the start time.
                if self.command_started_at.is_none() {
                    self.command_started_at = Some(Instant::now());
                    self.command_started_at_iso = Some(now_rfc3339());
                    // Capture the command text (may have arrived via OSC 633;E
                    // before the 633;C mark).
                    if let Some(cmd) = command
                        && self.last_command.as_deref() != Some(cmd)
                    {
                        self.last_command = Some(cmd.to_string());
                        self.dirty = true;
                    }
                    // Log command start event.
                    if let Some(ref mut log) = self.event_log {
                        log.log(&SessionEvent::CommandStart {
                            command: command.unwrap_or("<unknown>").to_string(),
                        });
                    }
                    self.emit(StateChangeNotification::CommandStarted {
                        command: command.map(|s| s.to_string()),
                    });
                }
            }
            CommandState::Finished => {
                // Command finished -- compute duration, update exit code and
                // consecutive failure count.
                let duration_ms = if let Some(started) = self.command_started_at.take() {
                    let dur = started.elapsed().as_millis() as i64;
                    self.last_command_duration_ms = Some(dur);
                    self.dirty = true;
                    dur as u64
                } else {
                    0
                };
                // Capture command text if we didn't get it during Executing
                // (the E mark can arrive at any point before D).
                if let Some(cmd) = command
                    && self.last_command.as_deref() != Some(cmd)
                {
                    self.last_command = Some(cmd.to_string());
                    self.dirty = true;
                }
                if let Some(ec) = exit_code {
                    if self.last_exit_code != Some(ec) {
                        self.last_exit_code = Some(ec);
                        self.dirty = true;
                    }
                    if ec != 0 {
                        self.consecutive_failures += 1;
                    } else {
                        self.consecutive_failures = 0;
                    }
                } else {
                    // No exit code provided -- treat as unknown, don't reset
                    // the failure counter.
                    self.last_exit_code = None;
                    self.dirty = true;
                }
                // Log command finish event.
                let cmd_str = command
                    .map(|s| s.to_string())
                    .or_else(|| self.last_command.clone())
                    .unwrap_or_else(|| "<unknown>".to_string());
                if let Some(ref mut log) = self.event_log {
                    log.log(&SessionEvent::CommandFinish {
                        command: cmd_str.clone(),
                        exit_code,
                        duration_ms,
                    });
                }
                self.emit(StateChangeNotification::CommandFinished {
                    command: Some(cmd_str),
                    exit_code,
                    duration_ms,
                });
            }
            CommandState::PromptStart => {
                // New prompt cycle -- clear the command-started timestamp so
                // the next Executing transition records a fresh start.
                self.command_started_at = None;
                self.command_started_at_iso = None;
            }
            CommandState::Input => {
                // Nothing special for Input.
            }
        }

        if self.last_command_state.as_ref() != Some(&state) {
            self.last_command_state = Some(state);
            self.infer_and_write();
        }
    }

    /// Clean up the state file on session exit.
    pub fn cleanup(&self) {
        do_cleanup(self.state_file_path.as_ref());
    }

    fn push_line(&mut self, line: String) {
        // Sniff for structured JSON output before pushing.
        if !self.structured_json_detected {
            self.sniff_structured_json(&line);
        }
        self.recent_lines.push_back(line);
        if self.recent_lines.len() > MAX_RECENT_LINES {
            self.recent_lines.pop_front();
        }
    }

    /// Sniff a line to detect structured JSON output mode.
    ///
    /// CC `--output-format json` emits JSONL where each line is a JSON object
    /// with a `"type"` field. We require 3 consecutive JSON object lines with
    /// a `"type"` key to confirm detection (avoids false positives from
    /// occasional JSON in normal terminal output).
    fn sniff_structured_json(&mut self, line: &str) {
        let trimmed = line.trim();
        if trimmed.starts_with('{') && trimmed.ends_with('}') {
            // Quick check for `"type"` key without full JSON parsing.
            if trimmed.contains("\"type\"") {
                self.json_line_streak += 1;
                if self.json_line_streak >= 3 {
                    self.structured_json_detected = true;
                    debug!(
                        session = %self.config.session_id,
                        "Detected structured JSON output mode (3 consecutive JSON lines)"
                    );
                    self.emit(StateChangeNotification::StructuredJsonDetected);
                }
                return;
            }
        }
        // Non-JSON line resets the streak.
        self.json_line_streak = 0;
    }

    /// Whether structured JSON output mode has been detected.
    pub fn is_structured_json(&self) -> bool {
        self.structured_json_detected
    }

    /// Check `/proc/<PID>/cmdline` for `--output-format` flags.
    ///
    /// Called once after agent type detection to eagerly detect structured
    /// JSON mode from process arguments, before any output is parsed.
    pub fn check_proc_cmdline_for_json_mode(&mut self) {
        if self.structured_json_detected {
            return;
        }
        let pid = self.config.child_pid;
        let cmdline_path = format!("/proc/{pid}/cmdline");
        if let Ok(data) = std::fs::read(&cmdline_path) {
            // cmdline is NUL-separated.
            let args: Vec<&str> = data
                .split(|&b| b == 0)
                .filter_map(|s| std::str::from_utf8(s).ok())
                .filter(|s| !s.is_empty())
                .collect();
            // Look for --output-format json (either as one arg or two).
            for window in args.windows(2) {
                if window[0] == "--output-format" && window[1] == "json" {
                    self.structured_json_detected = true;
                    debug!(
                        session = %self.config.session_id,
                        pid = pid,
                        "Detected --output-format json from /proc cmdline"
                    );
                    self.emit(StateChangeNotification::StructuredJsonDetected);
                    return;
                }
            }
            // Also check for --output-format=json as a single arg.
            for arg in &args {
                if *arg == "--output-format=json" {
                    self.structured_json_detected = true;
                    debug!(
                        session = %self.config.session_id,
                        pid = pid,
                        "Detected --output-format=json from /proc cmdline"
                    );
                    self.emit(StateChangeNotification::StructuredJsonDetected);
                    return;
                }
            }
        }
    }

    /// Detect agent type from terminal output if not already configured.
    fn detect_agent_from_output(&mut self) {
        if self.config.agent_type.is_some() && self.detected_model.is_some() {
            return; // Already fully configured
        }

        // Scan recent lines for detection signals, collecting results
        // without mutating self (avoids borrow-checker conflict on the
        // `recent_lines` iterator).
        let mut new_agent_type: Option<AgentType> = None;
        let mut new_model: Option<String> = None;

        let need_agent = self.config.agent_type.is_none() && self.detected_agent_type.is_none();
        let need_model = self.detected_model.is_none();

        for line in self.recent_lines.iter().rev().take(10) {
            if need_agent && new_agent_type.is_none() {
                if self.patterns.agent_ident_claude.is_match(line) {
                    new_agent_type = Some(AgentType::Claude);
                } else if self.patterns.agent_ident_copilot.is_match(line) {
                    new_agent_type = Some(AgentType::Copilot);
                } else if self.patterns.agent_ident_codex.is_match(line) {
                    new_agent_type = Some(AgentType::Codex);
                }
            }

            if need_model
                && new_model.is_none()
                && let Some(caps) = self.patterns.model_pattern.captures(line)
                && let Some(m) = caps.get(1)
            {
                new_model = Some(m.as_str().to_string());
            }

            // Stop early if we found everything we need.
            if (!need_agent || new_agent_type.is_some()) && (!need_model || new_model.is_some()) {
                break;
            }
        }

        // Apply detected values.
        if let Some(at) = new_agent_type {
            debug!(agent = at.as_str(), "Detected agent type from output");
            trace!(field = "agent_type", value = at.as_str(), "state dirty");
            self.detected_agent_type = Some(at);
            self.dirty = true;
            self.state_file_path =
                update_state_file_path(self.effective_agent_type(), &self.config.session_id);
            self.emit(StateChangeNotification::AgentDetected { agent_type: at });
        }
        if let Some(ref model) = new_model {
            debug!(model = %model, "Detected model from output");
            trace!(field = "model", value = %model, "state dirty");
            self.emit(StateChangeNotification::ModelDetected {
                model: model.clone(),
            });
        }
        if let Some(model) = new_model {
            self.detected_model = Some(model);
            self.dirty = true;
        }
    }

    /// Run the inference pipeline and write state if anything changed.
    ///
    /// The `dirty` flag is set by any exported-field mutation (status, model,
    /// agent_type, context_percent) and persists across calls. If a write is
    /// throttled, the flag stays set so the next call retries instead of
    /// silently dropping the update.
    fn infer_and_write(&mut self) {
        let new_status = self.infer_status();

        // Detect context percentage from recent output (may set dirty).
        self.detect_context_percent();

        if new_status != self.last_status {
            let old_str = self.last_status.status_str().to_string();
            let new_str = new_status.status_str().to_string();
            trace!(
                old = %old_str,
                new = %new_str,
                field = "status",
                "state dirty"
            );
            if let Some(ref mut log) = self.event_log {
                log.log(&SessionEvent::StatusChange {
                    old: old_str,
                    new: new_str,
                });
            }
            // Emit tool lifecycle notifications when transitioning to/from ToolUse.
            if let InferredStatus::ToolUse { ref tool_name } = new_status {
                self.emit(StateChangeNotification::ToolStarted {
                    tool_name: tool_name.clone(),
                });
            }
            if let InferredStatus::ToolUse { ref tool_name } = self.last_status
                && !matches!(new_status, InferredStatus::ToolUse { .. })
            {
                self.emit(StateChangeNotification::ToolCompleted {
                    tool_name: tool_name.clone(),
                });
            }
            // Emit the general status change notification.
            self.emit(StateChangeNotification::StatusChanged {
                old: self.last_status.clone(),
                new: new_status.clone(),
            });
            self.last_status = new_status;
            self.dirty = true;
        }

        if self.dirty && self.last_write.elapsed() >= MIN_WRITE_INTERVAL {
            self.dirty = false;
            let start = Instant::now();
            self.do_write_state_file();
            let elapsed = start.elapsed();
            debug!(
                elapsed_ms = elapsed.as_millis() as u64,
                status = %self.last_status.status_str(),
                "write_state_file"
            );
        } else if self.dirty {
            debug!(
                status = %self.last_status.status_str(),
                throttle_remaining_ms = (MIN_WRITE_INTERVAL.saturating_sub(self.last_write.elapsed())).as_millis() as u64,
                "state write throttled"
            );
        }
    }

    /// Build a `StateFile` from current state and delegate to persistence.
    fn do_write_state_file(&mut self) {
        let Some(agent_type) = self.effective_agent_type() else {
            trace!("No agent type detected, skipping state file write");
            return;
        };

        let state_dir = std::path::Path::new(agent_type.state_dir());

        let state_file = StateFile {
            session_id: self.config.session_id.clone(),
            agent_type: Some(agent_type.as_str().to_string()),
            status: self.last_status.status_str().to_string(),
            current_tool: self.last_status.tool_name().map(|s| s.to_string()),
            working_dir: self.config.working_dir.clone(),
            last_updated: Some(now_rfc3339()),
            pid: Some(self.config.child_pid as i64),
            model: self.detected_model.clone(),
            context_percent: self.context_percent.map(|v| v as f64),
            source: Some("terminal_inference".to_string()),
            last_command: self.last_command.clone(),
            last_exit_code: self.last_exit_code.map(|c| c as i64),
            last_command_started_at: self.command_started_at_iso.clone(),
            last_command_duration_ms: self.last_command_duration_ms,
            consecutive_failures: if self.consecutive_failures > 0 {
                Some(self.consecutive_failures)
            } else {
                None
            },
        };

        if let Some(path) = write_state_file(&state_file, &self.config.session_id, state_dir) {
            self.last_write = Instant::now();
            self.state_file_path = Some(path);
        }
    }

    /// Infer the current agent status from command tracker state + output patterns.
    fn infer_status(&self) -> InferredStatus {
        // OSC 633 command state takes priority -- it's the most reliable signal.
        if let Some(ref cmd_state) = self.last_command_state {
            match cmd_state {
                CommandState::PromptStart | CommandState::Input => {
                    // Shell is at a prompt -- but the agent may be showing its
                    // own prompt, not the shell prompt. Check output patterns.
                    return self.infer_from_output_or(InferredStatus::AwaitingInput);
                }
                CommandState::Executing => {
                    // A command is running. Check if we can identify a tool.
                    return self.infer_executing_status();
                }
                CommandState::Finished => {
                    // Command just finished. Back to idle/awaiting.
                    return self.infer_from_output_or(InferredStatus::Idle);
                }
            }
        }

        // No OSC 633 data -- rely purely on output heuristics.
        self.infer_from_output_or(InferredStatus::Idle)
    }

    /// During command execution, check output for tool use or processing patterns.
    fn infer_executing_status(&self) -> InferredStatus {
        // Check the live unterminated line first -- it has the most recent content
        // (prompts and spinners are often rendered without a trailing newline).
        let lines_iter = std::iter::once(&self.current_line)
            .filter(|l| !l.is_empty())
            .chain(self.recent_lines.iter().rev());

        // Check for tool call patterns.
        for line in lines_iter.clone().take(16) {
            if let Some(caps) = self.patterns.tool_call.captures(line)
                && let Some(m) = caps.get(1)
            {
                return InferredStatus::ToolUse {
                    tool_name: m.as_str().to_string(),
                };
            }
        }

        // Check for spinner/thinking patterns -- these indicate the agent is
        // "thinking" (no substantive output, just an animation).
        for line in lines_iter.take(6) {
            if self.patterns.spinner.is_match(line) {
                return InferredStatus::Thinking;
            }
        }

        // Check cadence: sustained high-throughput monotonic output = Streaming.
        if self.is_streaming_cadence() {
            return InferredStatus::Streaming;
        }

        // Default during execution: processing.
        InferredStatus::Processing
    }

    /// Check output patterns, falling back to the given default.
    fn infer_from_output_or(&self, default: InferredStatus) -> InferredStatus {
        // Include the live unterminated line -- prompts like `>` and spinners
        // are often rendered without a trailing newline.
        let lines_iter = std::iter::once(&self.current_line)
            .filter(|l| !l.is_empty())
            .chain(self.recent_lines.iter().rev());

        // Check for awaiting input patterns in very recent lines.
        for line in lines_iter.clone().take(6) {
            if self.patterns.awaiting_input.is_match(line) {
                return InferredStatus::AwaitingInput;
            }
        }

        // Check for tool use patterns (agent might be showing tool output).
        for line in lines_iter.clone().take(11) {
            if let Some(caps) = self.patterns.tool_call.captures(line)
                && let Some(m) = caps.get(1)
            {
                return InferredStatus::ToolUse {
                    tool_name: m.as_str().to_string(),
                };
            }
        }

        // Check for spinner/thinking.
        for line in lines_iter.take(4) {
            if self.patterns.spinner.is_match(line) {
                return InferredStatus::Thinking;
            }
        }

        // Check cadence: sustained high-throughput monotonic output = Streaming.
        if self.is_streaming_cadence() {
            return InferredStatus::Streaming;
        }

        default
    }

    /// Detect context percentage from recent output.
    fn detect_context_percent(&mut self) {
        for line in self.recent_lines.iter().rev().take(10) {
            if let Some(caps) = self.patterns.context_percent.captures(line)
                && let Some(m) = caps.get(1)
                && let Ok(pct) = m.as_str().parse::<f32>()
                && (0.0..=100.0).contains(&pct)
            {
                if self.context_percent != Some(pct) {
                    trace!(field = "context_percent", value = pct, "state dirty");
                    self.context_percent = Some(pct);
                    self.dirty = true;
                    self.emit(StateChangeNotification::ContextUpdated { percent: pct });
                }
                return;
            }
        }
    }
}

/// Get the current time as an RFC 3339 string.
fn now_rfc3339() -> String {
    // Use a simple manual approach to avoid pulling in the `time` crate
    // (which therminal-terminal doesn't depend on).
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    // Convert to UTC components.
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Convert days since epoch to year-month-day.
    // Simplified: use a basic algorithm for dates from 1970 onward.
    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Civil calendar algorithm from Howard Hinnant.
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// -- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine(agent_type: Option<AgentType>) -> AgentStateInference {
        AgentStateInference::new(InferenceConfig {
            session_id: "test-session-1".to_string(),
            child_pid: 12345,
            agent_type,
            working_dir: Some("/home/user/project".to_string()),
        })
    }

    // -- Pattern matching ----------------------------------------------------

    #[test]
    fn detect_spinner_thinking() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("\u{280b} Processing your request...".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Thinking);
    }

    #[test]
    fn detect_thinking_dots() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("Thinking...".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Thinking);
    }

    #[test]
    fn detect_tool_use_read() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Read(/home/user/file.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Read".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_bash() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Bash(cargo build --release)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Bash".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_edit() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("  Edit(src/main.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Edit".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_write() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Write(new_file.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Write".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_glob() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Glob(**/*.rs)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Glob".to_string()
            }
        );
    }

    #[test]
    fn detect_tool_use_grep() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Grep(pattern)".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Grep".to_string()
            }
        );
    }

    #[test]
    fn detect_agent_from_output_prefers_copilot_over_codex() {
        let mut engine = make_engine(None);
        engine.push_line("GitHub Copilot using gpt-4.1; comparing with Codex output".to_string());
        engine.detect_agent_from_output();
        assert_eq!(engine.agent_type(), Some(AgentType::Copilot));
    }

    #[test]
    fn detect_awaiting_input_prompt() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("> ".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::AwaitingInput);
    }

    #[test]
    fn detect_awaiting_input_esc() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("esc to interrupt".to_string());
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::AwaitingInput);
    }

    #[test]
    fn detect_context_percent() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("42% context used".to_string());
        engine.detect_context_percent();
        assert_eq!(engine.context_percent, Some(42.0));
    }

    #[test]
    fn detect_context_percent_with_decimal() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("87.5% context remaining".to_string());
        engine.detect_context_percent();
        assert_eq!(engine.context_percent, Some(87.0));
    }

    // -- OSC 633 state integration -------------------------------------------

    #[test]
    fn command_state_executing_default_processing() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Processing);
    }

    #[test]
    fn command_state_input_awaiting() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Input);
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::AwaitingInput);
    }

    #[test]
    fn command_state_finished_idle() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Finished);
        let status = engine.infer_status();
        assert_eq!(status, InferredStatus::Idle);
    }

    // -- RFC 3339 timestamp --------------------------------------------------

    #[test]
    fn now_rfc3339_format() {
        let ts = now_rfc3339();
        assert!(ts.len() == 20, "Unexpected timestamp length: {ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "T");
        assert_eq!(&ts[13..14], ":");
        assert_eq!(&ts[16..17], ":");
    }

    // -- Days to YMD ---------------------------------------------------------

    #[test]
    fn epoch_day_zero() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn known_date() {
        assert_eq!(days_to_ymd(10957), (2000, 1, 1));
    }

    // -- Feed bytes integration ----------------------------------------------

    #[test]
    fn feed_bytes_extracts_lines() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"Hello world\nSecond line\n");
        assert_eq!(engine.recent_lines.len(), 2);
        assert_eq!(engine.recent_lines[0], "Hello world");
        assert_eq!(engine.recent_lines[1], "Second line");
    }

    #[test]
    fn feed_bytes_with_ansi() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"\x1b[32mGreen text\x1b[0m\n");
        assert_eq!(engine.recent_lines.len(), 1);
        assert_eq!(engine.recent_lines[0], "Green text");
    }

    #[test]
    fn feed_bytes_accumulates_partial_lines() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"partial");
        assert_eq!(engine.recent_lines.len(), 0);
        assert_eq!(engine.current_line, "partial");
        engine.feed_bytes(b" line\n");
        assert_eq!(engine.recent_lines.len(), 1);
        assert_eq!(engine.recent_lines[0], "partial line");
    }

    #[test]
    fn ring_buffer_eviction() {
        let mut engine = make_engine(Some(AgentType::Claude));
        for i in 0..60 {
            engine.push_line(format!("line {i}"));
        }
        assert_eq!(engine.recent_lines.len(), MAX_RECENT_LINES);
        assert_eq!(engine.recent_lines[0], "line 10");
        assert_eq!(engine.recent_lines[MAX_RECENT_LINES - 1], "line 59");
    }

    // -- Stateful ANSI stripper integration ----------------------------------

    #[test]
    fn feed_bytes_split_csi_no_leak() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"clean text\x1b");
        engine.feed_bytes(b"[0m more text\n");
        assert_eq!(engine.recent_lines.len(), 1);
        assert_eq!(engine.recent_lines[0], "clean text more text");
    }

    #[test]
    fn feed_bytes_split_osc_no_leak() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"before\x1b]2;title");
        engine.feed_bytes(b"\x07after\n");
        assert_eq!(engine.recent_lines.len(), 1);
        assert_eq!(engine.recent_lines[0], "beforeafter");
    }

    // -- Dirty-flag write semantics ------------------------------------------

    #[test]
    fn model_detection_triggers_write_without_status_change() {
        let mut engine = make_engine(Some(AgentType::Claude));
        assert!(!engine.dirty);

        engine.feed_bytes(b"using: claude-sonnet-4-20250514\n");
        assert_eq!(
            engine.detected_model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );

        assert!(!engine.dirty, "dirty should be cleared after write");
        assert_eq!(engine.last_status, InferredStatus::Idle);
    }

    #[test]
    fn context_percent_change_triggers_dirty() {
        let mut engine = make_engine(Some(AgentType::Claude));
        assert!(!engine.dirty);

        engine.push_line("42% context used".to_string());
        engine.infer_and_write();
        assert_eq!(engine.context_percent, Some(42.0));
        assert!(!engine.dirty);

        engine.last_write = Instant::now() - MIN_WRITE_INTERVAL;
        engine.push_line("42% context used".to_string());
        engine.infer_and_write();
        assert!(!engine.dirty);

        engine.last_write = Instant::now() - MIN_WRITE_INTERVAL;
        engine.push_line("58% context remaining".to_string());
        engine.infer_and_write();
        assert_eq!(engine.context_percent, Some(58.0));
        assert!(!engine.dirty);
    }

    #[test]
    fn throttled_status_change_flushed_on_next_call() {
        let mut engine = make_engine(Some(AgentType::Claude));

        engine.last_write = Instant::now();
        let initial_write_time = engine.last_write;

        engine.push_line("\u{280b} Processing...".to_string());
        engine.infer_and_write();
        assert_eq!(engine.last_status, InferredStatus::Thinking);
        assert!(engine.dirty, "dirty flag should persist when throttled");
        assert_eq!(
            engine.last_write, initial_write_time,
            "last_write unchanged -- write was throttled"
        );

        engine.last_write = Instant::now() - MIN_WRITE_INTERVAL - Duration::from_millis(1);

        engine.infer_and_write();
        assert!(
            !engine.dirty,
            "dirty should be cleared after deferred flush"
        );
        assert_ne!(
            engine.last_write, initial_write_time,
            "last_write should be updated after flush"
        );
    }

    #[test]
    fn no_changes_no_write() {
        let mut engine = make_engine(Some(AgentType::Claude));

        let initial_write_time = engine.last_write;

        engine.feed_bytes(b"some random text\n");

        assert_eq!(engine.last_status, InferredStatus::Idle);
        assert!(!engine.dirty);
        assert_eq!(engine.last_write, initial_write_time);
    }

    // -- Output cadence analysis -----------------------------------------------

    #[test]
    fn cadence_unknown_insufficient_data() {
        let engine = make_engine(Some(AgentType::Claude));
        assert_eq!(engine.classify_output_cadence(), OutputCadence::Unknown);
    }

    #[test]
    fn cadence_unknown_two_chunks() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();
        engine.feed_bytes_at(b"ab", t0);
        engine.feed_bytes_at(b"cd", t0 + Duration::from_millis(100));
        assert_eq!(engine.classify_output_cadence(), OutputCadence::Unknown);
    }

    #[test]
    fn cadence_human_typing() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        engine.feed_bytes_at(b"h", t0);
        engine.feed_bytes_at(b"el", t0 + Duration::from_millis(80));
        engine.feed_bytes_at(b"l", t0 + Duration::from_millis(150));
        engine.feed_bytes_at(b"\x08", t0 + Duration::from_millis(300));
        engine.feed_bytes_at(b"lo", t0 + Duration::from_millis(420));
        engine.feed_bytes_at(b" w", t0 + Duration::from_millis(550));
        engine.feed_bytes_at(b"or", t0 + Duration::from_millis(700));
        engine.feed_bytes_at(b"\x08", t0 + Duration::from_millis(800));
        engine.feed_bytes_at(b"rl", t0 + Duration::from_millis(950));
        engine.feed_bytes_at(b"d", t0 + Duration::from_millis(1100));

        assert_eq!(engine.classify_output_cadence(), OutputCadence::Human);
    }

    #[test]
    fn cadence_human_typing_no_backspace() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        for i in 0..10 {
            engine.feed_bytes_at(b"ab", t0 + Duration::from_millis(i * 120));
        }

        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Human,
            "Small chunks at human speed should classify as Human even without backspaces"
        );
    }

    #[test]
    fn cadence_agent_sustained_large_output() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        let chunk = b"This is a line of agent output that is about one hundred characters long roughly speaking yes.\n";
        for i in 0..20 {
            engine.feed_bytes_at(chunk, t0 + Duration::from_millis(i * 150));
        }

        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Agent,
            "Sustained large output over 2.85s should be classified as Agent"
        );
    }

    #[test]
    fn cadence_burst_large_single_dump() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        let big_chunk: Vec<u8> = b"x".repeat(2000);
        engine.feed_bytes_at(&big_chunk, t0);
        engine.feed_bytes_at(&big_chunk, t0 + Duration::from_millis(5));
        engine.feed_bytes_at(&big_chunk, t0 + Duration::from_millis(10));

        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Burst,
            "3 large chunks in 10ms should be Burst (not sustained)"
        );
    }

    #[test]
    fn cadence_burst_short_window() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        let chunk: Vec<u8> = b"x".repeat(200);
        for i in 0..5 {
            engine.feed_bytes_at(&chunk, t0 + Duration::from_millis(i * 50));
        }

        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Burst,
            "Large output over 200ms should be Burst"
        );
    }

    #[test]
    fn cadence_transition_human_to_agent_to_human() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        for i in 0..10 {
            let b: &[u8] = if i % 4 == 3 { b"\x08" } else { b"ab" };
            engine.feed_bytes_at(b, t0 + Duration::from_millis(i * 100));
        }
        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Human,
            "Phase 1 should be Human"
        );

        engine.chunk_stats.clear();
        let t1 = t0 + Duration::from_secs(5);
        let agent_chunk = b"Agent is writing a long response with many characters per chunk and this continues on.\n";
        for i in 0..20 {
            engine.feed_bytes_at(agent_chunk, t1 + Duration::from_millis(i * 150));
        }
        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Agent,
            "Phase 2 should be Agent"
        );

        engine.chunk_stats.clear();
        let t2 = t1 + Duration::from_secs(10);
        for i in 0..10 {
            let b: &[u8] = if i % 3 == 2 { b"\x7F" } else { b"k" };
            engine.feed_bytes_at(b, t2 + Duration::from_millis(i * 120));
        }
        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Human,
            "Phase 3 should be Human"
        );
    }

    #[test]
    fn cadence_burst_vs_agent() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();
        let big = b"x".repeat(5000);
        engine.feed_bytes_at(&big, t0);
        engine.feed_bytes_at(&big, t0 + Duration::from_millis(2));
        engine.feed_bytes_at(&big, t0 + Duration::from_millis(4));
        engine.feed_bytes_at(&big, t0 + Duration::from_millis(6));
        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Burst,
            "Large output over 6ms should be Burst"
        );

        engine.chunk_stats.clear();
        let chunk = b"x".repeat(200);
        for i in 0..20 {
            engine.feed_bytes_at(&chunk, t0 + Duration::from_millis(i * 150));
        }
        assert_eq!(
            engine.classify_output_cadence(),
            OutputCadence::Agent,
            "Large output sustained over 2.85s should be Agent"
        );
    }

    // -- Spinner detection -----------------------------------------------------

    #[test]
    fn spinner_pattern_detected() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        for i in 0..10 {
            let mut raw = b"\x1b[H".to_vec();
            raw.push(b"-/|\\"[i as usize % 4]);
            engine.feed_bytes_at(&raw, t0 + Duration::from_millis(i * 80));
        }

        assert!(
            engine.is_spinner_pattern(),
            "Cursor-control-heavy output with small visible content should be spinner"
        );
    }

    #[test]
    fn spinner_not_detected_for_normal_output() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();

        for i in 0..10 {
            let line = format!("This is line {} of normal output\n", i);
            engine.feed_bytes_at(line.as_bytes(), t0 + Duration::from_millis(i * 50));
        }

        assert!(
            !engine.is_spinner_pattern(),
            "Normal text output should not be detected as spinner"
        );
    }

    #[test]
    fn spinner_not_detected_insufficient_data() {
        let engine = make_engine(Some(AgentType::Claude));
        assert!(
            !engine.is_spinner_pattern(),
            "Empty chunk window should not be spinner"
        );
    }

    // -- Chunk stats collection ------------------------------------------------

    #[test]
    fn chunk_stats_populated_by_feed_bytes() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"hello world\n");
        assert_eq!(engine.chunk_stats.len(), 1);
        assert_eq!(engine.chunk_stats[0].byte_count, 12);
        assert!(!engine.chunk_stats[0].has_backspace);
        assert!(!engine.chunk_stats[0].has_cursor_control);
    }

    #[test]
    fn chunk_stats_backspace_detected() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"helo\x08lo\n");
        assert_eq!(engine.chunk_stats.len(), 1);
        assert!(engine.chunk_stats[0].has_backspace);
    }

    #[test]
    fn chunk_stats_del_detected() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"helo\x7Flo\n");
        assert!(engine.chunk_stats[0].has_backspace);
    }

    #[test]
    fn chunk_stats_cursor_control_detected() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"\x1b[Ahello\n");
        assert!(engine.chunk_stats[0].has_cursor_control);
    }

    #[test]
    fn chunk_stats_no_cursor_control_for_sgr() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"\x1b[31mred\x1b[0m\n");
        assert!(!engine.chunk_stats[0].has_cursor_control);
    }

    #[test]
    fn chunk_stats_window_eviction() {
        let mut engine = make_engine(Some(AgentType::Claude));
        for i in 0..25 {
            engine.feed_bytes(format!("line {}\n", i).as_bytes());
        }
        assert_eq!(engine.chunk_stats.len(), MAX_CHUNK_STATS);
    }

    #[test]
    fn chunk_stats_visible_chars_count() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.feed_bytes(b"\x1b[31mhello\x1b[0m\n");
        assert_eq!(engine.chunk_stats[0].visible_chars, 5);
    }

    // -- Streaming state -------------------------------------------------------

    #[test]
    fn streaming_cadence_high_throughput() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let base = Instant::now();
        for i in 0..10 {
            let chunk = "a".repeat(200);
            let ts = base + Duration::from_millis(300 * i);
            engine.feed_bytes_at(format!("{chunk}\n").as_bytes(), ts);
        }
        assert!(
            engine.is_streaming_cadence(),
            "Sustained >500 chars/sec without backspaces should be streaming"
        );
    }

    #[test]
    fn streaming_not_detected_with_backspaces() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let base = Instant::now();
        for i in 0..10 {
            let mut chunk = "a".repeat(200);
            chunk.push('\x08');
            let ts = base + Duration::from_millis(300 * i);
            engine.feed_bytes_at(chunk.as_bytes(), ts);
        }
        assert!(
            !engine.is_streaming_cadence(),
            "Output with backspaces should not be classified as streaming"
        );
    }

    #[test]
    fn streaming_not_detected_low_throughput() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let base = Instant::now();
        for i in 0..5 {
            let ts = base + Duration::from_millis(1000 * i);
            engine.feed_bytes_at(b"hi\n", ts);
        }
        assert!(
            !engine.is_streaming_cadence(),
            "Low throughput should not be classified as streaming"
        );
    }

    #[test]
    fn streaming_not_detected_short_window() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let base = Instant::now();
        for i in 0..3 {
            let chunk = "a".repeat(500);
            let ts = base + Duration::from_millis(100 * i);
            engine.feed_bytes_at(format!("{chunk}\n").as_bytes(), ts);
        }
        assert!(
            !engine.is_streaming_cadence(),
            "Short window should not be classified as streaming even at high throughput"
        );
    }

    #[test]
    fn infer_streaming_during_execution() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        let base = Instant::now();
        for i in 0..10 {
            let chunk = "x".repeat(200);
            let ts = base + Duration::from_millis(300 * i);
            engine.feed_bytes_at(format!("{chunk}\n").as_bytes(), ts);
        }
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::Streaming,
            "High-throughput cadence during execution should infer Streaming"
        );
    }

    #[test]
    fn infer_streaming_from_output_fallback() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let base = Instant::now();
        for i in 0..10 {
            let chunk = "y".repeat(200);
            let ts = base + Duration::from_millis(300 * i);
            engine.feed_bytes_at(format!("{chunk}\n").as_bytes(), ts);
        }
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::Streaming,
            "High-throughput cadence without OSC 633 should infer Streaming"
        );
    }

    // -- Thinking state --------------------------------------------------------

    #[test]
    fn infer_thinking_during_execution_spinner() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("\u{2819} Working on it...".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::Thinking,
            "Spinner during execution should infer Thinking"
        );
    }

    #[test]
    fn infer_thinking_from_output_fallback() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.push_line("Thinking...".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::Thinking,
            "Spinner pattern without OSC 633 should infer Thinking"
        );
    }

    #[test]
    fn thinking_does_not_override_tool_use() {
        let mut engine = make_engine(Some(AgentType::Claude));
        engine.last_command_state = Some(CommandState::Executing);
        engine.push_line("Read(/home/user/file.rs)".to_string());
        engine.push_line("\u{280b} reading...".to_string());
        let status = engine.infer_status();
        assert_eq!(
            status,
            InferredStatus::ToolUse {
                tool_name: "Read".to_string()
            },
            "Tool use should take priority over thinking"
        );
    }
}
