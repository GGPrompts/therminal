//! Pattern-matching and inference-query helpers for `AgentStateInference`.
//!
//! These methods classify bytes or recent output lines into pattern
//! categories (agent type, model, context %, structured JSON, cursor
//! control, tool calls, spinners) and compute derived inference results
//! without mutating the state machine beyond dirty-flagging detected
//! fields. The state-transition side (command lifecycle, writes) lives in
//! `state_machine.rs`; the orchestrator and public API live in `mod.rs`.

use tracing::{debug, trace};

use crate::osc633::CommandState;

use super::persistence::update_state_file_path;
use super::types::{AgentType, InferredStatus, StateChangeNotification};
use super::{AgentStateInference, cadence::is_streaming_cadence};

impl AgentStateInference {
    /// Scan raw bytes for CSI cursor control sequences (movement, erase).
    ///
    /// Looks for ESC \[ ... followed by a final byte that indicates cursor
    /// movement (A-H, J, K) rather than graphics (m) or other attributes.
    pub(super) fn scan_cursor_control(bytes: &[u8]) -> bool {
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

    /// Sniff a line to detect structured JSON output mode.
    ///
    /// CC `--output-format json` emits JSONL where each line is a JSON object
    /// with a `"type"` field. We require 3 consecutive JSON object lines with
    /// a `"type"` key to confirm detection (avoids false positives from
    /// occasional JSON in normal terminal output).
    pub(super) fn sniff_structured_json(&mut self, line: &str) {
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
    pub(super) fn detect_agent_from_output(&mut self) {
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

    /// Infer the current agent status from command tracker state + output patterns.
    pub(super) fn infer_status(&self) -> InferredStatus {
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
        if is_streaming_cadence(&self.chunk_stats) {
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
        if is_streaming_cadence(&self.chunk_stats) {
            return InferredStatus::Streaming;
        }

        default
    }

    /// Detect context percentage from recent output.
    pub(super) fn detect_context_percent(&mut self) {
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
