//! State-transition side of `AgentStateInference`.
//!
//! Handles byte ingestion (cadence bookkeeping, ANSI stripping, line
//! buffering), OSC 633 command lifecycle transitions, the
//! infer-and-write orchestrator, and the atomic state file write path.
//! Pure pattern/classification helpers live in `matchers.rs`; the struct
//! definition and public getters live in `mod.rs`.

use std::time::Instant;

use tracing::{debug, trace};

use crate::event_log::SessionEvent;
use crate::osc633::CommandState;

use super::cadence::{ByteChunkStats, MAX_CHUNK_STATS};
use super::persistence::write_state_file;
use super::types::{InferredStatus, StateChangeNotification, StateFile};
use super::{AgentStateInference, MAX_RECENT_LINES, MIN_WRITE_INTERVAL};

impl AgentStateInference {
    /// Feed a chunk of raw PTY output bytes.
    ///
    /// Extracts visible text lines (stripping ANSI escape sequences) and
    /// updates the recent lines buffer. Call [`Self::update_command_state`]
    /// separately with the latest `CommandState` from the tracker.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.feed_bytes_at(bytes, Instant::now());
    }

    /// Feed bytes with an explicit timestamp (used by tests for mock timing).
    pub(super) fn feed_bytes_at(&mut self, bytes: &[u8], timestamp: Instant) {
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

    pub(super) fn push_line(&mut self, line: String) {
        // Sniff for structured JSON output before pushing.
        if !self.structured_json_detected {
            self.sniff_structured_json(&line);
        }
        self.recent_lines.push_back(line);
        if self.recent_lines.len() > MAX_RECENT_LINES {
            self.recent_lines.pop_front();
        }
    }

    /// Run the inference pipeline and write state if anything changed.
    ///
    /// The `dirty` flag is set by any exported-field mutation (status, model,
    /// agent_type, context_percent) and persists across calls. If a write is
    /// throttled, the flag stays set so the next call retries instead of
    /// silently dropping the update.
    pub(super) fn infer_and_write(&mut self) {
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
}

/// Get the current time as an RFC 3339 string.
pub(super) fn now_rfc3339() -> String {
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
pub(super) fn days_to_ymd(days: u64) -> (u64, u64, u64) {
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
