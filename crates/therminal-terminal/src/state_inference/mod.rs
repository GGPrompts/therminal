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

pub mod ansi_strip;
mod cadence;
mod matchers;
mod patterns;
mod persistence;
mod state_machine;
mod types;

// Re-expose free helpers from state_machine so the test module (which uses
// `use super::*`) can call them as `now_rfc3339()` / `days_to_ymd(...)`.
#[cfg(test)]
use state_machine::{days_to_ymd, now_rfc3339};

// Re-export all public types so they remain importable from
// `therminal_terminal::state_inference::*`.
pub use cadence::{ByteChunkStats, OutputCadence};
pub use types::{AgentType, InferenceConfig, InferredStatus, StateChangeNotification};

/// Plain-data snapshot of the inference engine's externally-observable state.
///
/// Returned by [`AgentStateInference::snapshot`] so consumers (e.g. the
/// daemon's MCP `terminal.agents.get_details` handler) can read engine state
/// under a short lock and then drop it before serialising. Contains only
/// owned plain types -- no references or internal handles -- so it can cross
/// thread boundaries freely.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentDetailsSnapshot {
    /// Detected/configured agent type as a lowercase string ("claude",
    /// "codex", "copilot", "aider"). `None` if no agent has been identified.
    pub agent_type: Option<String>,
    /// Detected model name from output (e.g. "claude-sonnet-4-20250514").
    pub model: Option<String>,
    /// Context-window usage percentage (0.0..100.0) if detected.
    pub context_percent: Option<f32>,
    /// Count of consecutive non-zero exit codes.
    pub consecutive_failures: i64,
    /// Most recent command line captured via OSC 633 ;E.
    pub last_command: Option<String>,
    /// Exit code of the most recent finished command (OSC 633 ;D).
    pub last_exit_code: Option<i32>,
    /// Duration in milliseconds of the most recent finished command.
    pub last_command_duration_ms: Option<i64>,
}

/// Plain-data snapshot of the inference engine's output cadence window.
///
/// Returned by [`AgentStateInference::cadence_snapshot`] so the daemon's MCP
/// `terminal.agents.get_cadence` handler can serve cadence metrics without
/// exposing the internal `VecDeque<ByteChunkStats>` or holding the
/// inference lock during serialisation. Sample timestamps are converted from
/// monotonic `Instant` to wall-clock Unix epoch seconds at snapshot time so
/// the result is meaningful when crossing process boundaries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentCadenceSnapshot {
    /// Number of chunks currently in the sliding window.
    pub chunk_count: usize,
    /// Average inter-chunk arrival interval in milliseconds. `0.0` when
    /// fewer than two chunks have been observed.
    pub avg_arrival_ms: f32,
    /// Largest gap between consecutive chunks in milliseconds. `0.0` when
    /// fewer than two chunks have been observed.
    pub max_gap_ms: f32,
    /// True when recent output looks like a spinner (cursor-control-heavy,
    /// low visible text per chunk).
    pub is_spinner: bool,
    /// True when recent output is a sustained high-throughput stream
    /// (>500 visible chars/sec for at least 2 seconds, no backspaces).
    pub is_streaming: bool,
    /// Most recent samples (oldest first), capped at
    /// [`AgentCadenceSnapshot::MAX_RECENT_SAMPLES`].
    pub recent_samples: Vec<CadenceSampleSnapshot>,
}

impl AgentCadenceSnapshot {
    /// Maximum number of recent samples returned in a snapshot. The internal
    /// chunk-stats window is capped at 20 today, but this constant exists so
    /// the public DTO contract does not change if the internal cap grows.
    pub const MAX_RECENT_SAMPLES: usize = 50;
}

/// One chunk-arrival sample as exposed via [`AgentCadenceSnapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CadenceSampleSnapshot {
    /// Wall-clock arrival time in Unix epoch seconds, computed at snapshot
    /// time from the chunk's monotonic `Instant`.
    pub timestamp_secs: u64,
    /// Number of bytes in the chunk.
    pub bytes: usize,
    /// Gap from the previous chunk in milliseconds. `0.0` for the first
    /// sample in the window.
    pub gap_ms: f32,
}

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use crate::event_log::EventLog;
use crate::osc633::CommandState;

use ansi_strip::AnsiStripper;
use cadence::{MAX_CHUNK_STATS, classify_output_cadence, is_spinner_pattern, is_streaming_cadence};
use patterns::Patterns;
use persistence::cleanup as do_cleanup;

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
    pub(super) fn emit(&self, notification: StateChangeNotification) {
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

    /// Build a plain-data snapshot of the inference engine's externally-
    /// observable state. Cheap (clones a few small `Option<String>`s) and
    /// intended to be called under a short lock from the daemon.
    pub fn snapshot(&self) -> AgentDetailsSnapshot {
        AgentDetailsSnapshot {
            agent_type: self
                .effective_agent_type()
                .map(|at| at.as_str().to_string()),
            model: self.detected_model.clone(),
            context_percent: self.context_percent,
            consecutive_failures: self.consecutive_failures,
            last_command: self.last_command.clone(),
            last_exit_code: self.last_exit_code,
            last_command_duration_ms: self.last_command_duration_ms,
        }
    }

    /// Build a plain-data snapshot of the output cadence window.
    ///
    /// Computes summary metrics (avg arrival, max gap, spinner / streaming
    /// classification) from the internal `chunk_stats` window and returns a
    /// trimmed list of recent samples capped at
    /// [`AgentCadenceSnapshot::MAX_RECENT_SAMPLES`]. Sample timestamps are
    /// converted from monotonic `Instant`s to wall-clock Unix-epoch seconds
    /// using the supplied `now_*` references — taking them as parameters
    /// keeps this function deterministic for tests and avoids syscalls under
    /// the inference lock when the daemon already holds wall-clock state.
    ///
    /// The DTO is intentionally plain (no internal `VecDeque` exposure) so
    /// it can be sent over IPC / serialised by MCP without dragging private
    /// types across the crate boundary.
    pub fn cadence_snapshot_at(
        &self,
        now_instant: Instant,
        now_unix_secs: u64,
    ) -> AgentCadenceSnapshot {
        let chunk_count = self.chunk_stats.len();

        // Inter-chunk gaps in milliseconds. `intervals[i]` is the gap
        // between `chunk_stats[i]` and `chunk_stats[i + 1]`. Empty when
        // fewer than two chunks have been observed.
        let intervals: Vec<f32> = self
            .chunk_stats
            .iter()
            .zip(self.chunk_stats.iter().skip(1))
            .map(|(a, b)| b.timestamp.duration_since(a.timestamp).as_secs_f32() * 1000.0)
            .collect();

        let avg_arrival_ms = if intervals.is_empty() {
            0.0
        } else {
            intervals.iter().sum::<f32>() / intervals.len() as f32
        };
        let max_gap_ms = intervals.iter().copied().fold(0.0_f32, f32::max);

        // Build the recent-sample list. Each sample's wall-clock seconds is
        // computed by subtracting the chunk's age (now_instant - chunk
        // timestamp) from the supplied `now_unix_secs`. The first sample's
        // gap is 0.0; every subsequent sample's gap mirrors `intervals`.
        let take_n = chunk_count.min(AgentCadenceSnapshot::MAX_RECENT_SAMPLES);
        let skip_n = chunk_count.saturating_sub(take_n);
        let mut recent_samples: Vec<CadenceSampleSnapshot> = Vec::with_capacity(take_n);
        let mut prev_ts: Option<Instant> = None;
        for chunk in self.chunk_stats.iter().skip(skip_n) {
            let age_secs = now_instant
                .checked_duration_since(chunk.timestamp)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let timestamp_secs = now_unix_secs.saturating_sub(age_secs);
            let gap_ms = match prev_ts {
                Some(prev) => chunk.timestamp.duration_since(prev).as_secs_f32() * 1000.0,
                None => 0.0,
            };
            recent_samples.push(CadenceSampleSnapshot {
                timestamp_secs,
                bytes: chunk.byte_count,
                gap_ms,
            });
            prev_ts = Some(chunk.timestamp);
        }

        AgentCadenceSnapshot {
            chunk_count,
            avg_arrival_ms,
            max_gap_ms,
            is_spinner: is_spinner_pattern(&self.chunk_stats),
            is_streaming: is_streaming_cadence(&self.chunk_stats),
            recent_samples,
        }
    }

    /// Convenience wrapper around [`Self::cadence_snapshot_at`] that uses
    /// the system clock at the moment of the call. Suitable for production
    /// callers (e.g. the daemon's MCP handler); tests prefer the explicit
    /// `_at` form so they can pin both clocks.
    pub fn cadence_snapshot(&self) -> AgentCadenceSnapshot {
        let now_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.cadence_snapshot_at(Instant::now(), now_unix_secs)
    }

    /// Get the effective agent type (config override > detected > None).
    pub(super) fn effective_agent_type(&self) -> Option<AgentType> {
        self.config.agent_type.or(self.detected_agent_type)
    }

    // feed_bytes / feed_bytes_at / scan_cursor_control / update_command_state
    // live in state_machine.rs (multiple `impl AgentStateInference` blocks).

    #[doc(hidden)]
    /// Clean up the state file on session exit.
    pub fn cleanup(&self) {
        do_cleanup(self.state_file_path.as_ref());
    }
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

    /// Regression for tn-3pkv: a Bubble Tea TUI (TFE) that happens to print
    /// the bare word "copilot" anywhere in its rendering must not be classified
    /// as a Copilot agent session. The copilot identifier regex is anchored to
    /// product-name contexts ("GitHub Copilot", "copilot cli", etc.).
    #[test]
    fn bubble_tea_tui_does_not_false_positive_as_copilot() {
        let bubbletea_lines = [
            "TFE  Terraform File Explorer",
            "  workspace: prod-copilot-experiments",
            "  [n]ew  [d]elete  [q]uit",
            "press ? for help",
            "see also: copilot-style autocomplete (planned)",
        ];
        for line in bubbletea_lines {
            let mut engine = make_engine(None);
            engine.push_line(line.to_string());
            engine.detect_agent_from_output();
            assert_eq!(
                engine.agent_type(),
                None,
                "TFE/Bubble-Tea line should not classify as any agent: {line}"
            );
        }
    }

    /// Regression for tn-97ag: the Claude detection regex used to match any
    /// occurrence of the bare word "claude", so a file manager (TFE) rendering
    /// a directory containing `CLAUDE.md` would flip the engine into "Claude
    /// session" mode and start writing `/tmp/claude-code-state/daemon-pane-*.json`.
    /// The identifier is now anchored to product-name contexts.
    #[test]
    fn file_manager_listing_claude_md_does_not_false_positive_as_claude() {
        let file_manager_lines = [
            "CLAUDE.md",
            "  CLAUDE.md                    12.3 KB",
            "drwxr-xr-x  claude/                    4096",
            "> cd ~/projects/claude-experiments",
            "see CLAUDE.md for project instructions",
            "// the claude integration is deprecated",
            "grepped: claude (2 matches)",
        ];
        for line in file_manager_lines {
            let mut engine = make_engine(None);
            engine.push_line(line.to_string());
            engine.detect_agent_from_output();
            assert_eq!(
                engine.agent_type(),
                None,
                "file-listing line should not classify as Claude: {line}"
            );
        }
    }

    /// Positive companion for tn-97ag: real Claude Code banners / model lines
    /// must still be detected. Covers the strings Claude Code actually writes
    /// to a TTY — startup banner, model line, footer URL, CLI token.
    #[test]
    fn claude_code_banners_still_detect_as_claude() {
        let banner_lines = [
            "Claude Code v1.0.42",
            "Claude Code",
            "Using model: Claude Sonnet 4",
            "Running claude-code in /home/user/repo",
            "Visit https://claude.ai/code for docs",
        ];
        for line in banner_lines {
            let mut engine = make_engine(None);
            engine.push_line(line.to_string());
            engine.detect_agent_from_output();
            assert_eq!(
                engine.agent_type(),
                Some(AgentType::Claude),
                "Claude banner line should classify as Claude: {line}"
            );
        }
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
    fn snapshot_reflects_engine_state_after_feed_bytes() {
        // End-to-end shape test: feed Claude-Code-style output through the
        // engine and assert that `snapshot()` returns the externally-
        // observable fields. This is the same call path the daemon uses
        // from `Pane::agent_details_snapshot()` for the MCP
        // `terminal.agents.get_details` tool.
        let mut engine = make_engine(Some(AgentType::Claude));

        // Model line — picked up by the patterns module.
        engine.feed_bytes(b"using: claude-sonnet-4-20250514\n");
        // Context-percent line.
        engine.push_line("37% context used".to_string());
        engine.infer_and_write();

        let snap = engine.snapshot();
        assert_eq!(snap.agent_type.as_deref(), Some("claude"));
        assert_eq!(snap.model.as_deref(), Some("claude-sonnet-4-20250514"));
        assert_eq!(snap.context_percent, Some(37.0));
        assert_eq!(snap.consecutive_failures, 0);
        // No OSC 633 ;E/;D fed, so command fields are None.
        assert_eq!(snap.last_command, None);
        assert_eq!(snap.last_exit_code, None);
        assert_eq!(snap.last_command_duration_ms, None);
    }

    #[test]
    fn snapshot_default_for_fresh_engine() {
        // A brand-new engine with no agent type configured and no bytes
        // fed should snapshot to all-None / zero — the same fallback the
        // daemon uses when locking fails.
        let engine = make_engine(None);
        let snap = engine.snapshot();
        assert_eq!(snap, AgentDetailsSnapshot::default());
    }

    #[test]
    fn cadence_snapshot_default_for_fresh_engine() {
        // A brand-new engine should yield zero defaults / empty samples.
        // Same fallback the daemon's MCP `terminal.agents.get_cadence`
        // handler relies on for panes that have not received any PTY
        // bytes yet.
        let engine = make_engine(None);
        let snap = engine.cadence_snapshot();
        assert_eq!(snap.chunk_count, 0);
        assert_eq!(snap.avg_arrival_ms, 0.0);
        assert_eq!(snap.max_gap_ms, 0.0);
        assert!(!snap.is_spinner);
        assert!(!snap.is_streaming);
        assert!(snap.recent_samples.is_empty());
    }

    #[test]
    fn cadence_snapshot_computes_avg_and_max_gap() {
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();
        // 4 chunks at +0ms / +50ms / +250ms / +300ms.
        engine.feed_bytes_at(b"abc", t0);
        engine.feed_bytes_at(b"defgh", t0 + Duration::from_millis(50));
        engine.feed_bytes_at(b"ij", t0 + Duration::from_millis(250));
        engine.feed_bytes_at(b"klmno", t0 + Duration::from_millis(300));

        // Snapshot at a fixed wall-clock for determinism.
        let snap = engine.cadence_snapshot_at(t0 + Duration::from_millis(310), 1_700_000_000);

        assert_eq!(snap.chunk_count, 4);
        // Intervals: 50, 200, 50  -> avg = 100ms, max = 200ms.
        assert!((snap.avg_arrival_ms - 100.0).abs() < 0.01);
        assert!((snap.max_gap_ms - 200.0).abs() < 0.01);
        assert_eq!(snap.recent_samples.len(), 4);
        assert_eq!(snap.recent_samples[0].bytes, 3);
        // First sample's gap is 0.0 by contract.
        assert_eq!(snap.recent_samples[0].gap_ms, 0.0);
        assert!((snap.recent_samples[1].gap_ms - 50.0).abs() < 0.01);
        assert!((snap.recent_samples[2].gap_ms - 200.0).abs() < 0.01);
        assert!((snap.recent_samples[3].gap_ms - 50.0).abs() < 0.01);
        // Wall-clock conversion: chunk t0 is 0ms behind now (310 - 0 = 310ms),
        // chunk t0+50 is 260ms behind, etc. Truncated to whole seconds, all
        // four chunks should report `now_unix_secs` (since 310ms < 1s).
        for sample in &snap.recent_samples {
            assert_eq!(sample.timestamp_secs, 1_700_000_000);
        }
    }

    #[test]
    fn cadence_snapshot_recent_samples_capped() {
        // Even though the internal chunk window is capped at 20 today, the
        // public DTO contract caps `recent_samples` at 50 — verify the cap
        // is honoured. We feed 30 chunks (a bit above the internal cap so
        // the engine evicts down to 20) and confirm the snapshot returns
        // at most 50 samples and does NOT exceed the internal window.
        let mut engine = make_engine(Some(AgentType::Claude));
        let t0 = Instant::now();
        for i in 0..30u64 {
            engine.feed_bytes_at(b"xx", t0 + Duration::from_millis(i * 10));
        }
        let snap = engine.cadence_snapshot_at(t0 + Duration::from_millis(310), 1_700_000_000);
        assert!(
            snap.recent_samples.len() <= AgentCadenceSnapshot::MAX_RECENT_SAMPLES,
            "recent_samples must not exceed MAX_RECENT_SAMPLES",
        );
        // Internal window cap is MAX_CHUNK_STATS = 20, so chunk_count
        // should be 20 here and the samples list should match.
        assert_eq!(snap.chunk_count, 20);
        assert_eq!(snap.recent_samples.len(), 20);
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
