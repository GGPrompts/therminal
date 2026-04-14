//! JSONL tail backend — watches a file for appended lines and renders
//! structured, color-coded rows in the terminal grid.
//!
//! Replaces the previous approach of spawning `tail -F` in a shell pane
//! which was fragile on Windows (no `tail` without WSL), wasted a shell
//! process per pane, and rendered raw JSON without structure.
//!
//! ## Design
//!
//! A `notify` file watcher detects modifications. On each modification
//! event the file is re-read from the last known offset, and new lines
//! are parsed as JSON and appended to a ring buffer. Callers retrieve
//! formatted output via [`JsonlTailState::formatted_content`].
//!
//! ## Format awareness
//!
//! The parser detects Claude agent event schemas (`type`, `message`,
//! `subagent` fields) and renders them with structured columns.
//! Unknown JSON lines are pretty-printed with basic key highlighting.

use std::collections::VecDeque;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use tracing::{debug, warn};

/// Maximum number of parsed rows retained in the ring buffer.
const DEFAULT_MAX_ROWS: usize = 1000;

/// A single parsed JSONL row with its formatted representation.
#[derive(Debug, Clone)]
pub struct JsonlRow {
    /// The original JSON value (retained for search / MCP queries).
    pub value: Value,
    /// Pre-formatted single-line string for display in the grid.
    /// Includes ANSI color codes for terminal rendering.
    pub formatted: String,
}

/// Shared state between the file-watcher callback and the pane backend.
pub struct JsonlTailState {
    /// Path being watched.
    pub path: PathBuf,
    /// Ring buffer of parsed rows.
    pub rows: VecDeque<JsonlRow>,
    /// Maximum rows to retain.
    pub max_rows: usize,
    /// Current column width for formatting.
    pub cols: usize,
    /// Byte offset into the file (next read starts here).
    pub file_offset: u64,
}

impl JsonlTailState {
    fn new(path: PathBuf, cols: usize) -> Self {
        Self {
            path,
            rows: VecDeque::with_capacity(DEFAULT_MAX_ROWS),
            max_rows: DEFAULT_MAX_ROWS,
            cols: cols.max(20),
            file_offset: 0,
        }
    }

    /// Read new lines from the file starting at `file_offset`, parse them,
    /// and append to the ring buffer.
    pub fn poll_file(&mut self) {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) => {
                debug!(path = %self.path.display(), error = %e, "jsonl_tail: cannot open file");
                return;
            }
        };

        // If the file shrank (truncation), reset to the beginning.
        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if file_len < self.file_offset {
            debug!(
                path = %self.path.display(),
                old_offset = self.file_offset,
                new_len = file_len,
                "jsonl_tail: file truncated, resetting offset"
            );
            self.file_offset = 0;
            self.rows.clear();
        }

        if let Err(e) = file.seek(SeekFrom::Start(self.file_offset)) {
            warn!(error = %e, "jsonl_tail: seek failed");
            return;
        }

        let reader = std::io::BufReader::new(file);
        let mut bytes_read: u64 = 0;
        let mut new_rows = 0u32;

        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    // Account for the line plus its newline delimiter.
                    bytes_read += line.len() as u64 + 1;

                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    let row = match serde_json::from_str::<Value>(trimmed) {
                        Ok(val) => {
                            let formatted = format_json_row(&val, self.cols);
                            JsonlRow {
                                value: val,
                                formatted,
                            }
                        }
                        Err(_) => {
                            // Not valid JSON — render as raw text.
                            let formatted = format_raw_line(trimmed, self.cols);
                            JsonlRow {
                                value: Value::String(trimmed.to_string()),
                                formatted,
                            }
                        }
                    };

                    self.rows.push_back(row);
                    new_rows += 1;

                    // Enforce ring buffer cap.
                    while self.rows.len() > self.max_rows {
                        self.rows.pop_front();
                    }
                }
                Err(e) => {
                    debug!(error = %e, "jsonl_tail: read error (partial line?)");
                    break;
                }
            }
        }

        self.file_offset += bytes_read;
        if new_rows > 0 {
            debug!(
                path = %self.path.display(),
                new_rows,
                total = self.rows.len(),
                offset = self.file_offset,
                "jsonl_tail: appended rows"
            );
        }
    }

    /// Re-format all rows (e.g. after a column width change).
    pub fn reformat_all(&mut self) {
        for row in &mut self.rows {
            row.formatted = format_json_row(&row.value, self.cols);
        }
    }

    /// Return the formatted content for display, joining rows with newlines.
    pub fn formatted_content(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&row.formatted);
            out.push('\n');
        }
        out
    }
}

// ── Format helpers ─────────────────────────────────────────────────────

/// ANSI escape codes for structured rendering.
mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    // Colors
    pub const CYAN: &str = "\x1b[36m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const MAGENTA: &str = "\x1b[35m";
    pub const RED: &str = "\x1b[31m";
    pub const BLUE: &str = "\x1b[34m";
    pub const WHITE: &str = "\x1b[37m";
}

/// Detect whether a JSON value looks like a Claude agent event and
/// format it with structured columns. Falls back to generic JSON
/// pretty-printing.
fn format_json_row(val: &Value, cols: usize) -> String {
    // Try Claude agent event format first.
    if let Some(formatted) = try_format_claude_event(val, cols) {
        return formatted;
    }

    // Generic JSON: show key-highlighted summary.
    format_generic_json(val, cols)
}

/// Try to format as a Claude Code agent event.
///
/// Claude JSONL events typically have fields like:
/// - `type` (e.g. "assistant", "user", "result", "tool_use", "tool_result")
/// - `message` or `content`
/// - `timestamp` or `ts`
/// - `subagent` or `parentSessionId`
fn try_format_claude_event(val: &Value, cols: usize) -> Option<String> {
    let obj = val.as_object()?;

    // Must have a `type` field to be recognized as a Claude event.
    let event_type = obj.get("type").and_then(|v| v.as_str())?;

    let mut parts: Vec<String> = Vec::new();

    // Type badge with color coding.
    let type_color = match event_type {
        "assistant" => ansi::CYAN,
        "user" => ansi::GREEN,
        "tool_use" | "tool_call" => ansi::YELLOW,
        "tool_result" | "toolUseResult" => ansi::MAGENTA,
        "result" | "done" => ansi::BLUE,
        "error" => ansi::RED,
        _ => ansi::WHITE,
    };
    parts.push(format!(
        "{}{}{:<12}{}",
        ansi::BOLD,
        type_color,
        event_type,
        ansi::RESET
    ));

    // Timestamp if present.
    if let Some(ts) = obj
        .get("timestamp")
        .or_else(|| obj.get("ts"))
        .and_then(|v| v.as_str())
    {
        // Show only HH:MM:SS portion if it looks like an ISO timestamp.
        let short_ts = if ts.len() > 19 {
            &ts[11..19]
        } else if ts.len() >= 8 {
            &ts[..8]
        } else {
            ts
        };
        parts.push(format!("{}{}{}", ansi::DIM, short_ts, ansi::RESET));
    }

    // Tool name for tool_use events.
    if let Some(tool_name) = obj
        .get("tool")
        .or_else(|| obj.get("name"))
        .and_then(|v| v.as_str())
        && matches!(
            event_type,
            "tool_use" | "tool_call" | "tool_result" | "toolUseResult"
        )
    {
        parts.push(format!("{}{}{}", ansi::YELLOW, tool_name, ansi::RESET));
    }

    // Message content — truncated to fit available columns.
    let content_text = obj
        .get("message")
        .or_else(|| obj.get("content"))
        .or_else(|| obj.get("text"))
        .or_else(|| obj.get("output"));

    if let Some(content) = content_text {
        let text = match content {
            Value::String(s) => s.clone(),
            other => {
                // For arrays/objects, compact serialize.
                serde_json::to_string(other).unwrap_or_default()
            }
        };
        // Estimate used columns from the parts so far (rough, ignoring ANSI).
        let used: usize = parts.iter().map(|p| visible_len(p) + 1).sum();
        let avail = cols.saturating_sub(used + 2);
        if avail > 0 {
            let truncated = truncate_str(&text, avail);
            parts.push(format!("{}{}{}", ansi::DIM, truncated, ansi::RESET));
        }
    }

    Some(parts.join(" "))
}

/// Format a generic JSON value as a single line with key highlighting.
fn format_generic_json(val: &Value, cols: usize) -> String {
    match val {
        Value::Object(map) => {
            let mut out = String::new();
            let mut first = true;
            for (key, value) in map {
                if !first {
                    out.push_str(&format!(" {}\u{2502}{} ", ansi::DIM, ansi::RESET));
                }
                first = false;

                let val_str = match value {
                    Value::String(s) => s.clone(),
                    Value::Null => "null".to_string(),
                    Value::Bool(b) => b.to_string(),
                    Value::Number(n) => n.to_string(),
                    other => serde_json::to_string(other).unwrap_or_default(),
                };

                out.push_str(&format!("{}{}{}={}", ansi::CYAN, key, ansi::RESET, val_str));

                // Stop if we've exceeded the column budget.
                if visible_len(&out) >= cols {
                    break;
                }
            }
            truncate_visible(&out, cols)
        }
        other => {
            let s = serde_json::to_string(other).unwrap_or_default();
            truncate_str(&s, cols)
        }
    }
}

/// Format a non-JSON line as dimmed raw text.
fn format_raw_line(line: &str, cols: usize) -> String {
    format!("{}{}{}", ansi::DIM, truncate_str(line, cols), ansi::RESET)
}

/// Compute visible length of a string (excluding ANSI escape sequences).
fn visible_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            len += 1;
        }
    }
    len
}

/// Truncate a plain string (no ANSI) to `max` visible characters,
/// appending an ellipsis if truncated.
fn truncate_str(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    // Replace newlines/tabs with spaces for single-line display.
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.chars().count() <= max {
        cleaned
    } else {
        let truncated: String = cleaned.chars().take(max.saturating_sub(1)).collect();
        format!("{}\u{2026}", truncated)
    }
}

/// Truncate a string that may contain ANSI escapes to `max` visible chars.
fn truncate_visible(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut visible = 0usize;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            out.push(ch);
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            out.push(ch);
            in_escape = true;
        } else {
            if visible >= max {
                out.push_str(ansi::RESET);
                return out;
            }
            out.push(ch);
            visible += 1;
        }
    }
    out
}

// ── Watcher lifecycle ──────────────────────────────────────────────────

/// Spawn a `notify` file watcher and return the shared state handle.
///
/// The watcher monitors the parent directory (because the file may not
/// exist yet or may be recreated). On each relevant event, it triggers
/// a poll of the file via the shared state.
///
/// `wake` is called after new rows are appended so the GUI can
/// request a redraw.
pub fn spawn_jsonl_watcher(
    path: PathBuf,
    cols: usize,
    wake: Box<dyn Fn() + Send + 'static>,
) -> Result<(Arc<Mutex<JsonlTailState>>, JsonlTailWatcher), anyhow::Error> {
    let state = Arc::new(Mutex::new(JsonlTailState::new(path.clone(), cols)));

    // Do an initial poll to pick up any existing content.
    if let Ok(mut s) = state.lock() {
        s.poll_file();
    }

    let state_for_watcher = Arc::clone(&state);
    let watch_path = path.clone();

    let mut watcher =
        notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            match res {
                Ok(event) => {
                    use notify::EventKind;
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            // Only react if our target file is among the affected paths.
                            let dominated = event.paths.iter().any(|p| p == &watch_path);
                            if dominated || event.paths.is_empty() {
                                if let Ok(mut s) = state_for_watcher.lock() {
                                    s.poll_file();
                                }
                                wake();
                            }
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    debug!(error = %e, "jsonl_tail: watcher error");
                }
            }
        })?;

    // Watch the parent directory so we catch file creation/rename.
    let watch_dir = path.parent().unwrap_or(Path::new("."));
    watcher.watch(watch_dir, RecursiveMode::NonRecursive)?;

    Ok((state, JsonlTailWatcher { _watcher: watcher }))
}

/// RAII guard holding the `notify` watcher alive. Drop to stop watching.
pub struct JsonlTailWatcher {
    _watcher: RecommendedWatcher,
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_len_strips_ansi() {
        assert_eq!(visible_len("hello"), 5);
        assert_eq!(visible_len("\x1b[31mhello\x1b[0m"), 5);
        assert_eq!(visible_len("\x1b[1m\x1b[36mfoo\x1b[0m"), 3);
    }

    #[test]
    fn truncate_str_basic() {
        assert_eq!(truncate_str("hello world", 5), "hell\u{2026}");
        assert_eq!(truncate_str("hi", 5), "hi");
        assert_eq!(truncate_str("", 5), "");
    }

    #[test]
    fn truncate_str_zero_width() {
        assert_eq!(truncate_str("anything", 0), "");
    }

    #[test]
    fn format_claude_event_assistant() {
        let val: Value = serde_json::from_str(
            r#"{"type":"assistant","message":"thinking about it","timestamp":"2026-01-15T10:30:45Z"}"#,
        )
        .unwrap();
        let formatted = format_json_row(&val, 120);
        // Should contain the type and some content.
        assert!(visible_len(&formatted) > 0);
        // Should include the type badge.
        assert!(formatted.contains("assistant"));
    }

    #[test]
    fn format_claude_event_tool_use() {
        let val: Value = serde_json::from_str(
            r#"{"type":"tool_use","tool":"Read","content":"reading file.rs"}"#,
        )
        .unwrap();
        let formatted = format_json_row(&val, 80);
        assert!(formatted.contains("tool_use"));
        assert!(formatted.contains("Read"));
    }

    #[test]
    fn format_generic_json() {
        let val: Value = serde_json::from_str(r#"{"foo":"bar","count":42}"#).unwrap();
        let formatted = format_json_row(&val, 60);
        assert!(formatted.contains("foo"));
        assert!(formatted.contains("bar"));
    }

    #[test]
    fn format_raw_non_json() {
        let formatted = format_raw_line("this is not json", 40);
        assert!(formatted.contains("this is not json"));
    }

    #[test]
    fn poll_file_reads_incrementally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, "{\"type\":\"user\",\"message\":\"hello\"}\n").unwrap();

        let mut state = JsonlTailState::new(path.clone(), 80);
        state.poll_file();
        assert_eq!(state.rows.len(), 1);
        assert!(state.file_offset > 0);

        // Append a second line.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{{\"type\":\"assistant\",\"message\":\"world\"}}").unwrap();

        state.poll_file();
        assert_eq!(state.rows.len(), 2);
    }

    #[test]
    fn poll_file_handles_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n").unwrap();

        let mut state = JsonlTailState::new(path.clone(), 80);
        state.poll_file();
        assert_eq!(state.rows.len(), 3);

        // Truncate the file.
        std::fs::write(&path, "{\"b\":1}\n").unwrap();
        state.poll_file();
        // Should have reset and re-read.
        assert_eq!(state.rows.len(), 1);
    }

    #[test]
    fn ring_buffer_enforces_max_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..50 {
            content.push_str(&format!("{{\"i\":{i}}}\n"));
        }
        std::fs::write(&path, &content).unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.max_rows = 10;
        state.poll_file();
        assert_eq!(state.rows.len(), 10);
        // Should have the last 10 rows (i=40..49).
        let first_val = &state.rows[0].value;
        assert_eq!(first_val["i"].as_u64(), Some(40));
    }

    #[test]
    fn formatted_content_joins_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, "{\"x\":1}\n{\"x\":2}\n").unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.poll_file();
        let content = state.formatted_content();
        assert!(content.contains("x"));
        // Two rows means two trailing newlines at least.
        assert!(content.matches('\n').count() >= 2);
    }
}
