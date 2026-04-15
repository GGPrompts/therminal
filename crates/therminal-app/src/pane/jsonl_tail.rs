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
//! The parser detects Claude Code agent event schemas (nested
//! `message.content` arrays with `type` discriminators) and renders them
//! with structured columns, expandable tool sections, and visual hierarchy
//! ported from thermal-desktop's viewer (tn-bjvl).
//!
//! ## Keyboard interaction
//!
//! The pane supports navigation via `handle_input`:
//! - Arrow up/down, j/k: scroll one line
//! - PageUp/PageDown: scroll one page
//! - Enter/Space: toggle expand/collapse on tool entries
//! - `e`: toggle expand/collapse all
//! - `f`: toggle auto-follow mode
//! - `g`/Home: scroll to top
//! - `G`/End: scroll to bottom
//! - `t`/`T`: jump to next/previous tool call

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Term;
use alacritty_terminal::vte::ansi as vte_ansi;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, warn};

use super::PaneListener;

/// Maximum number of parsed rows retained in the ring buffer.
const DEFAULT_MAX_ROWS: usize = 1000;

/// Maximum structured events retained. Prevents unbounded memory
/// growth on long-running subagent tails (tn-kyr3).
const MAX_EVENTS: usize = 2000;

/// Default max lines shown for collapsed assistant text.
const COLLAPSED_ASSISTANT_LINES: usize = 4;

/// Default max lines shown for collapsed user messages.
const COLLAPSED_USER_LINES: usize = 5;

// ── Structured event types ────────────────────────────────────────────

/// Classification of a parsed JSONL event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    UserMessage,
    AssistantText,
    ToolUse,
    ToolResult,
    Thinking,
    Progress,
    SystemMessage,
}

/// A parsed, structured event from a JSONL line.
#[derive(Debug, Clone)]
pub struct StructuredEvent {
    /// Event classification.
    pub kind: EventKind,
    /// ISO 8601 timestamp string.
    pub timestamp: String,
    /// Primary content (message text, tool output, thinking, etc.).
    pub content: String,
    /// Tool name for tool-related events.
    pub tool_name: Option<String>,
    /// Tool use ID for pairing ToolUse <-> ToolResult.
    #[allow(dead_code)]
    pub tool_use_id: Option<String>,
    /// Whether a ToolResult was an error.
    pub is_error: bool,
}

/// A single display row with its formatted ANSI string and metadata.
#[derive(Debug, Clone)]
pub struct DisplayRow {
    /// Pre-formatted single-line string with ANSI color codes.
    pub formatted: String,
    /// Index of the source event in `events` that produced this row.
    pub event_index: usize,
    /// Whether this row is an expandable/collapsible indicator.
    pub is_expandable: bool,
}

/// A single parsed JSONL row with its formatted representation.
/// Kept for backward compatibility with `get_content()`.
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
    /// Ring buffer of parsed rows (legacy flat format).
    pub rows: VecDeque<JsonlRow>,
    /// Maximum rows to retain.
    pub max_rows: usize,
    /// Current column width for formatting.
    pub cols: usize,
    /// Current visible rows (viewport height).
    pub visible_rows: usize,
    /// Byte offset into the file (next read starts here).
    pub file_offset: u64,

    // ── Structured viewer state (tn-bjvl) ─────────────────────────────
    /// Parsed structured events.
    pub events: Vec<StructuredEvent>,
    /// Pre-rendered display rows.
    pub display: Vec<DisplayRow>,
    /// Scroll offset (display row index at top of viewport).
    pub scroll: usize,
    /// Whether auto-follow is active (scroll to bottom on new content).
    pub following: bool,
    /// Set of event indices that the user has manually expanded.
    pub expanded_events: HashSet<usize>,
    /// Whether all events are expanded (toggle-all state).
    pub all_expanded: bool,
}

impl JsonlTailState {
    fn new(path: PathBuf, cols: usize) -> Self {
        Self {
            path,
            rows: VecDeque::with_capacity(DEFAULT_MAX_ROWS),
            max_rows: DEFAULT_MAX_ROWS,
            cols: cols.max(20),
            visible_rows: 24,
            file_offset: 0,
            events: Vec::new(),
            display: Vec::new(),
            scroll: 0,
            following: true,
            expanded_events: HashSet::new(),
            all_expanded: false,
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
            self.events.clear();
            self.display.clear();
            self.expanded_events.clear();
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

                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(val) => {
                            // Parse structured events from the CC JSONL format.
                            let structured = parse_jsonl_event(trimmed);
                            if !structured.is_empty() {
                                for event in structured {
                                    let idx = self.events.len();
                                    let expanded =
                                        self.all_expanded || self.expanded_events.contains(&idx);
                                    let new_display_rows =
                                        render_event(&event, self.cols, expanded, idx);
                                    self.display.extend(new_display_rows);
                                    self.events.push(event);
                                }
                            } else {
                                // Fall back to generic JSON formatting.
                                let formatted = format_generic_json(&val, self.cols);
                                let idx = self.events.len();
                                self.display.push(DisplayRow {
                                    formatted,
                                    event_index: idx,
                                    is_expandable: false,
                                });
                                self.events.push(StructuredEvent {
                                    kind: EventKind::SystemMessage,
                                    timestamp: String::new(),
                                    content: serde_json::to_string(&val).unwrap_or_default(),
                                    tool_name: None,
                                    tool_use_id: None,
                                    is_error: false,
                                });
                            }

                            // Legacy row for get_content().
                            let formatted = format_json_row(&val, self.cols);
                            self.rows.push_back(JsonlRow {
                                value: val,
                                formatted,
                            });
                        }
                        Err(_) => {
                            // Not valid JSON — render as raw text.
                            let formatted = format_raw_line(trimmed, self.cols);
                            self.rows.push_back(JsonlRow {
                                value: Value::String(trimmed.to_string()),
                                formatted: formatted.clone(),
                            });
                            let idx = self.events.len();
                            self.display.push(DisplayRow {
                                formatted,
                                event_index: idx,
                                is_expandable: false,
                            });
                            self.events.push(StructuredEvent {
                                kind: EventKind::SystemMessage,
                                timestamp: String::new(),
                                content: trimmed.to_string(),
                                tool_name: None,
                                tool_use_id: None,
                                is_error: false,
                            });
                        }
                    }

                    new_rows += 1;

                    // Enforce ring buffer cap on legacy rows.
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

        // Auto-follow: scroll to bottom when new content arrives.
        if new_rows > 0 && self.following {
            self.scroll_to_bottom();
        }

        // Enforce event cap (tn-kyr3): evict oldest events to prevent
        // unbounded memory growth on long-running tails.
        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(0..excess);
            // Rebase expanded_events indices to match the shifted vec.
            self.expanded_events = self
                .expanded_events
                .iter()
                .filter_map(|&idx| idx.checked_sub(excess))
                .collect();
            // Rebuild display rows with correct indices.
            self.reformat_all();
        }

        if new_rows > 0 {
            debug!(
                path = %self.path.display(),
                new_rows,
                total_events = self.events.len(),
                total_display = self.display.len(),
                offset = self.file_offset,
                "jsonl_tail: appended rows"
            );
        }
    }

    /// Re-render all display rows (e.g. after a column width change or
    /// expansion toggle).
    pub fn reformat_all(&mut self) {
        self.display.clear();
        for (i, event) in self.events.iter().enumerate() {
            let expanded = self.all_expanded || self.expanded_events.contains(&i);
            let new_rows = render_event(event, self.cols, expanded, i);
            self.display.extend(new_rows);
        }
        // Also reformat legacy rows.
        for row in &mut self.rows {
            row.formatted = format_json_row(&row.value, self.cols);
        }
    }

    /// Return the formatted content for display, joining visible rows
    /// with newlines. Shows the structured view with scroll position.
    pub fn formatted_content(&self) -> String {
        let mut out = String::new();

        // Header line.
        let event_count = self.events.len();
        let follow_indicator = if self.following { " [follow]" } else { "" };
        out.push_str(&format!(
            "{}{} events{}{}\r\n",
            ansi::DIM,
            event_count,
            follow_indicator,
            ansi::RESET
        ));

        // Visible display rows.
        let total = self.display.len();
        let start = self.scroll.min(total);
        let end = (start + self.visible_rows.saturating_sub(2)).min(total);
        for row in &self.display[start..end] {
            out.push_str(&row.formatted);
            out.push_str("\r\n");
        }

        // Footer with keybinding hints (compact at narrow widths).
        let footer = if self.cols < 50 {
            format!(
                "{}\u{2191}\u{2193}{} {}Ent{} {}e{} {}f{} {}G{}{}",
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::RESET
            )
        } else {
            format!(
                "{}\u{2191}\u{2193}{}:scroll  {}Enter{}:expand  {}e{}:all  {}f{}:follow  {}G{}:bottom{}",
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::CYAN,
                ansi::DIM,
                ansi::RESET
            )
        };
        out.push_str(&footer);
        out.push_str("\r\n");

        out
    }

    /// Total number of display rows.
    pub fn display_line_count(&self) -> usize {
        self.display.len()
    }

    /// Scroll to the bottom.
    pub fn scroll_to_bottom(&mut self) {
        let total = self.display_line_count();
        let viewport = self.visible_rows.saturating_sub(2); // header + footer
        if total > viewport {
            self.scroll = total - viewport;
        } else {
            self.scroll = 0;
        }
    }

    /// Handle a single input byte sequence (keyboard event).
    /// Returns `true` if the input was consumed.
    pub fn handle_input(&mut self, data: &[u8]) -> bool {
        // Parse escape sequences and single-byte keys.
        match data {
            // Arrow Up or 'k'
            b"\x1b[A" | b"k" => {
                self.scroll = self.scroll.saturating_sub(1);
                self.following = false;
                true
            }
            // Arrow Down or 'j'
            b"\x1b[B" | b"j" => {
                let max_scroll = self.max_scroll();
                self.scroll = (self.scroll + 1).min(max_scroll);
                if self.scroll >= max_scroll {
                    self.following = true;
                }
                true
            }
            // Page Up
            b"\x1b[5~" => {
                let page = self.visible_rows.saturating_sub(2);
                self.scroll = self.scroll.saturating_sub(page);
                self.following = false;
                true
            }
            // Page Down
            b"\x1b[6~" => {
                let page = self.visible_rows.saturating_sub(2);
                let max_scroll = self.max_scroll();
                self.scroll = (self.scroll + page).min(max_scroll);
                if self.scroll >= max_scroll {
                    self.following = true;
                }
                true
            }
            // Home or 'g'
            b"\x1b[H" | b"g" => {
                self.scroll = 0;
                self.following = false;
                true
            }
            // End or 'G'
            b"\x1b[F" | b"G" => {
                self.scroll_to_bottom();
                self.following = true;
                true
            }
            // Enter or Space — toggle expand/collapse
            b"\r" | b" " => {
                self.toggle_nearest_expandable();
                true
            }
            // 'e' — toggle expand/collapse all
            b"e" => {
                self.toggle_all_expansion();
                true
            }
            // 'f' — toggle follow mode
            b"f" => {
                self.following = !self.following;
                if self.following {
                    self.scroll_to_bottom();
                }
                true
            }
            // 't' — jump to next tool call
            b"t" => {
                self.jump_next_tool();
                true
            }
            // 'T' — jump to previous tool call
            b"T" => {
                self.jump_prev_tool();
                true
            }
            _ => false,
        }
    }

    fn max_scroll(&self) -> usize {
        let viewport = self.visible_rows.saturating_sub(2);
        self.display_line_count().saturating_sub(viewport)
    }

    /// Toggle expand/collapse on the nearest expandable row visible in
    /// the viewport (starting from scroll position).
    fn toggle_nearest_expandable(&mut self) {
        let viewport = self.visible_rows.saturating_sub(2);
        let start = self.scroll;
        let end = (start + viewport).min(self.display.len());
        for i in start..end {
            if self.display[i].is_expandable {
                let event_idx = self.display[i].event_index;
                if self.expanded_events.contains(&event_idx) {
                    self.expanded_events.remove(&event_idx);
                } else {
                    self.expanded_events.insert(event_idx);
                }
                let old_scroll = self.scroll;
                self.reformat_all();
                // Adjust scroll if lines were removed above viewport.
                let max = self.max_scroll();
                self.scroll = old_scroll.min(max);
                return;
            }
        }
    }

    /// Toggle expand/collapse all events.
    fn toggle_all_expansion(&mut self) {
        self.all_expanded = !self.all_expanded;
        if !self.all_expanded {
            self.expanded_events.clear();
        }
        let old_scroll = self.scroll;
        self.reformat_all();
        let max = self.max_scroll();
        self.scroll = old_scroll.min(max);
    }

    /// Jump to the next tool call event after current scroll position.
    fn jump_next_tool(&mut self) {
        let mut line_idx = 0;
        for (i, event) in self.events.iter().enumerate() {
            let expanded = self.all_expanded || self.expanded_events.contains(&i);
            let n = render_event(event, self.cols, expanded, i).len();
            if line_idx > self.scroll
                && matches!(event.kind, EventKind::ToolUse | EventKind::ToolResult)
            {
                self.scroll = line_idx;
                self.following = false;
                return;
            }
            line_idx += n;
        }
        // No more tool calls — scroll to bottom.
        self.scroll_to_bottom();
    }

    /// Jump to the previous tool call event before current scroll position.
    fn jump_prev_tool(&mut self) {
        let mut positions = Vec::new();
        let mut line_idx = 0;
        for (i, event) in self.events.iter().enumerate() {
            let expanded = self.all_expanded || self.expanded_events.contains(&i);
            let n = render_event(event, self.cols, expanded, i).len();
            if matches!(event.kind, EventKind::ToolUse | EventKind::ToolResult) {
                positions.push(line_idx);
            }
            line_idx += n;
        }
        if let Some(&pos) = positions.iter().rev().find(|&&p| p < self.scroll) {
            self.scroll = pos;
            self.following = false;
        }
    }

    /// Write the current `formatted_content()` into a shadow `Term` so the
    /// GPU renderer can draw it. Clears the term grid first (cursor home +
    /// erase display), then writes the ANSI-formatted content line by line.
    pub fn refresh_shadow_term(&self, term: &Arc<FairMutex<Term<PaneListener>>>) {
        let content = self.formatted_content();
        let mut processor = vte_ansi::Processor::<vte_ansi::StdSyncHandler>::new();
        let mut guard = term.lock();

        // Clear the grid: cursor to home + erase entire display.
        let clear = b"\x1b[H\x1b[2J";
        processor.advance(&mut *guard, clear);

        // Write the formatted content.
        processor.advance(&mut *guard, content.as_bytes());
    }
}

// ── CC JSONL parsing (ported from thermal-desktop session_log.rs) ─────

/// Flexible deserialization envelope for CC JSONL lines.
///
/// CC JSONL uses a nested format:
/// - `{"type":"user", "message":{"role":"user","content":"..." or [...]}, ...}`
/// - `{"type":"assistant", "message":{"role":"assistant","content":[...]}, ...}`
/// - `{"type":"system", "content":"...", ...}`
///
/// Assistant content arrays contain items with discriminator types:
/// `"text"`, `"tool_use"`, `"thinking"`. Tool results appear in user
/// messages as `"tool_result"` content items.
#[derive(Deserialize)]
struct RawLine {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    /// Top-level content (used by `system` type and legacy flat format).
    #[serde(default)]
    content: Option<Value>,
    /// Nested message envelope (used by `user` and `assistant` types).
    #[serde(default)]
    message: Option<Value>,
    // Legacy flat fields.
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// Parse a single JSONL line into structured events.
///
/// A single line may produce multiple events because CC nests multiple
/// content items (text, tool_use, thinking) inside a single assistant
/// message.
fn parse_jsonl_event(line: &str) -> Vec<StructuredEvent> {
    let raw: RawLine = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let msg_type = match raw.msg_type.as_deref() {
        Some(t) => t,
        None => return Vec::new(),
    };

    let timestamp = raw.timestamp.clone().unwrap_or_default();

    let make = |kind,
                content: String,
                tool_name: Option<String>,
                tool_use_id: Option<String>,
                is_error: bool| {
        StructuredEvent {
            kind,
            timestamp: timestamp.clone(),
            content,
            tool_name,
            tool_use_id,
            is_error,
        }
    };

    let msg_content = raw.message.as_ref().and_then(|m| m.get("content"));

    match msg_type {
        "user" => {
            match msg_content {
                Some(Value::String(s)) => {
                    vec![make(EventKind::UserMessage, s.clone(), None, None, false)]
                }
                Some(Value::Array(arr)) => {
                    let mut events = Vec::new();
                    for item in arr {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("tool_result") => {
                                let content = item
                                    .get("content")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let tool_use_id = item
                                    .get("tool_use_id")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string());
                                let is_error = item
                                    .get("is_error")
                                    .and_then(|e| e.as_bool())
                                    .unwrap_or(false);
                                events.push(make(
                                    EventKind::ToolResult,
                                    content,
                                    None,
                                    tool_use_id,
                                    is_error,
                                ));
                            }
                            Some("text") => {
                                let text = item
                                    .get("text")
                                    .and_then(|t| t.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if !text.is_empty() {
                                    events.push(make(
                                        EventKind::UserMessage,
                                        text,
                                        None,
                                        None,
                                        false,
                                    ));
                                }
                            }
                            _ => {}
                        }
                    }
                    events
                }
                // Fallback: try top-level content.
                None => {
                    let content = extract_content_string(&raw.content);
                    if content.is_empty() {
                        Vec::new()
                    } else {
                        vec![make(EventKind::UserMessage, content, None, None, false)]
                    }
                }
                _ => Vec::new(),
            }
        }

        "assistant" => match msg_content {
            Some(Value::Array(arr)) => {
                let mut events = Vec::new();
                for item in arr {
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            let text = item
                                .get("text")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !text.is_empty() {
                                events.push(make(
                                    EventKind::AssistantText,
                                    text,
                                    None,
                                    None,
                                    false,
                                ));
                            }
                        }
                        Some("tool_use") => {
                            let name = item
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("?")
                                .to_string();
                            let id = item
                                .get("id")
                                .and_then(|i| i.as_str())
                                .map(|s| s.to_string());
                            let input_json = item
                                .get("input")
                                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                                .unwrap_or_default();
                            events.push(make(
                                EventKind::ToolUse,
                                input_json,
                                Some(name),
                                id,
                                false,
                            ));
                        }
                        Some("thinking") => {
                            let thinking = item
                                .get("thinking")
                                .and_then(|t| t.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !thinking.is_empty() {
                                events.push(make(EventKind::Thinking, thinking, None, None, false));
                            }
                        }
                        _ => {}
                    }
                }
                events
            }
            _ => {
                let content = extract_content_string(&raw.content);
                if content.is_empty() {
                    Vec::new()
                } else {
                    vec![make(EventKind::AssistantText, content, None, None, false)]
                }
            }
        },

        "system" | "permission-mode" | "last-prompt" | "attachment" => {
            let content = extract_content_string(&raw.content);
            if content.is_empty() {
                Vec::new()
            } else {
                vec![make(EventKind::SystemMessage, content, None, None, false)]
            }
        }

        // Legacy flat format fields.
        "tool_use" | "tool_call" => {
            let tool = raw.tool.clone().or(raw.name.clone());
            let content = raw
                .input
                .as_ref()
                .map(|v| serde_json::to_string_pretty(v).unwrap_or_default())
                .unwrap_or_default();
            vec![make(
                EventKind::ToolUse,
                content,
                tool,
                raw.tool_use_id.clone(),
                false,
            )]
        }
        "tool_result" | "toolUseResult" => {
            let tool = raw.tool.clone().or(raw.name.clone());
            let content = raw.output.clone().unwrap_or_default();
            vec![make(
                EventKind::ToolResult,
                content,
                tool,
                raw.tool_use_id.clone(),
                raw.is_error.unwrap_or(false),
            )]
        }
        "thinking" => {
            let content = extract_content_string(&raw.content);
            vec![make(EventKind::Thinking, content, None, None, false)]
        }
        "progress" => {
            let content = raw
                .message
                .as_ref()
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or(raw.status.clone())
                .unwrap_or_default();
            vec![make(
                EventKind::Progress,
                content,
                raw.tool.clone(),
                None,
                false,
            )]
        }

        _ => Vec::new(),
    }
}

/// Extract text from CC's polymorphic `content` field.
fn extract_content_string(value: &Option<Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    parts.push(text);
                }
            }
            parts.join("")
        }
        _ => String::new(),
    }
}

// ── Structured event rendering ────────────────────────────────────────

/// Render a structured event into display rows.
///
/// Layout adapts to available width: at narrow widths (< 50 cols) the
/// renderer uses compact indentation and drops timestamps to maximize
/// content visibility. Content lines are word-wrapped instead of
/// truncated so the viewer remains readable in auto-tiled swarm panes.
fn render_event(
    event: &StructuredEvent,
    cols: usize,
    expanded: bool,
    event_idx: usize,
) -> Vec<DisplayRow> {
    let mut rows = Vec::new();
    let compact = cols < 50;
    // Prefix overhead: "  ┃ " = 4 chars normal, " ┃" = 2 chars compact.
    let prefix_cost = if compact { 2 } else { 4 };
    let content_width = cols.saturating_sub(prefix_cost + 1);

    /// Push wrapped content lines with a colored prefix.
    fn push_wrapped(
        rows: &mut Vec<DisplayRow>,
        text: &str,
        width: usize,
        prefix: &str,
        color: &str,
        event_idx: usize,
    ) {
        for wrapped in wrap_text(text, width) {
            rows.push(DisplayRow {
                formatted: format!("{}{}{}{}", prefix, color, wrapped, ansi::RESET),
                event_index: event_idx,
                is_expandable: false,
            });
        }
    }

    match event.kind {
        EventKind::UserMessage => {
            // Header line: compact drops timestamp.
            if compact {
                rows.push(DisplayRow {
                    formatted: format!(" {}{}USER{}", ansi::BOLD, ansi::GREEN, ansi::RESET),
                    event_index: event_idx,
                    is_expandable: false,
                });
            } else {
                let ts = format_time(&event.timestamp);
                rows.push(DisplayRow {
                    formatted: format!(
                        "  {}\u{2503}{} {}{}USER{}  {}{}{}",
                        ansi::GREEN,
                        ansi::RESET,
                        ansi::BOLD,
                        ansi::GREEN,
                        ansi::RESET,
                        ansi::DIM,
                        ts,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: false,
                });
            }

            let prefix = if compact {
                format!(" {}\u{2503}{} ", ansi::GREEN, ansi::RESET)
            } else {
                format!("  {}\u{2503}{} ", ansi::GREEN, ansi::RESET)
            };
            let max_lines = if expanded { 100 } else { COLLAPSED_USER_LINES };
            let mut line_count = 0;
            for line in event.content.lines() {
                if line_count >= max_lines {
                    break;
                }
                let wrapped = wrap_text(line, content_width);
                for w in &wrapped {
                    if line_count >= max_lines {
                        break;
                    }
                    rows.push(DisplayRow {
                        formatted: format!("{}{}", prefix, w),
                        event_index: event_idx,
                        is_expandable: false,
                    });
                    line_count += 1;
                }
            }
            let total = event.content.lines().count();
            if total > max_lines {
                let remaining = total - max_lines;
                rows.push(DisplayRow {
                    formatted: format!(
                        "{}{}\u{25b8} +{} more{}",
                        prefix,
                        ansi::CYAN,
                        remaining,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: true,
                });
            } else if expanded && total > COLLAPSED_USER_LINES {
                rows.push(DisplayRow {
                    formatted: format!("{}{}\u{25be} collapse{}", prefix, ansi::CYAN, ansi::RESET),
                    event_index: event_idx,
                    is_expandable: true,
                });
            }
            if !compact {
                rows.push(DisplayRow {
                    formatted: String::new(),
                    event_index: event_idx,
                    is_expandable: false,
                });
            }
        }

        EventKind::AssistantText => {
            let indent = if compact { " " } else { "  " };
            let max_lines = if expanded {
                200
            } else {
                COLLAPSED_ASSISTANT_LINES
            };
            let mut line_count = 0;
            for line in event.content.lines() {
                if line_count >= max_lines {
                    break;
                }
                let wrapped = wrap_text(line, content_width);
                for w in &wrapped {
                    if line_count >= max_lines {
                        break;
                    }
                    rows.push(DisplayRow {
                        formatted: format!("{}{}{}{}", indent, ansi::MAGENTA, w, ansi::RESET),
                        event_index: event_idx,
                        is_expandable: false,
                    });
                    line_count += 1;
                }
            }
            let total = event.content.lines().count();
            if total > max_lines {
                let remaining = total - max_lines;
                rows.push(DisplayRow {
                    formatted: format!(
                        "{}{}\u{25b8} +{} more{}",
                        indent,
                        ansi::CYAN,
                        remaining,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: true,
                });
            } else if expanded && total > COLLAPSED_ASSISTANT_LINES {
                rows.push(DisplayRow {
                    formatted: format!("{}{}\u{25be} collapse{}", indent, ansi::CYAN, ansi::RESET),
                    event_index: event_idx,
                    is_expandable: true,
                });
            }
            if !compact {
                rows.push(DisplayRow {
                    formatted: String::new(),
                    event_index: event_idx,
                    is_expandable: false,
                });
            }
        }

        EventKind::Thinking => {
            let indent = if compact { " " } else { "  " };
            let label_cost = if compact { 5 } else { 12 }; // "▸ Th…" vs "▸ Thinking  "
            let preview_width = content_width.saturating_sub(label_cost);
            if compact {
                let preview =
                    truncate_str(event.content.lines().next().unwrap_or(""), preview_width);
                rows.push(DisplayRow {
                    formatted: format!(
                        "{}{}{}\u{25b8}Th{} {}{}{}",
                        indent,
                        ansi::DIM,
                        ansi::BLUE,
                        ansi::RESET,
                        ansi::DIM,
                        preview,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: false,
                });
            } else {
                let preview =
                    truncate_str(event.content.lines().next().unwrap_or(""), preview_width);
                rows.push(DisplayRow {
                    formatted: format!(
                        "{}{}{}\u{25b8} Thinking{}  {}{}{}",
                        indent,
                        ansi::DIM,
                        ansi::BLUE,
                        ansi::RESET,
                        ansi::DIM,
                        preview,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: false,
                });
            }
        }

        EventKind::ToolUse => {
            let indent = if compact { " " } else { "  " };
            let tool = event.tool_name.as_deref().unwrap_or("?");
            let tool_color = tool_ansi_color(tool);
            // "▸ Tool " costs indent + 2 + tool.len() + 1
            let summary_width = content_width.saturating_sub(tool.len() + 3);
            let summary = extract_tool_summary(tool, &event.content, summary_width);

            if compact && summary.is_empty() {
                rows.push(DisplayRow {
                    formatted: format!(
                        "{}{}{}\u{25b8} {}{}",
                        indent,
                        ansi::BOLD,
                        tool_color,
                        tool,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: false,
                });
            } else if compact {
                // Tool on first line, summary wrapped below.
                rows.push(DisplayRow {
                    formatted: format!(
                        "{}{}{}\u{25b8} {}{}",
                        indent,
                        ansi::BOLD,
                        tool_color,
                        tool,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: false,
                });
                push_wrapped(
                    &mut rows,
                    &summary,
                    content_width,
                    &format!("{} ", indent),
                    ansi::DIM,
                    event_idx,
                );
            } else {
                rows.push(DisplayRow {
                    formatted: format!(
                        "{}{}{}\u{25b8} {}{} {}{}{}",
                        indent,
                        ansi::BOLD,
                        tool_color,
                        tool,
                        ansi::RESET,
                        ansi::DIM,
                        summary,
                        ansi::RESET
                    ),
                    event_index: event_idx,
                    is_expandable: false,
                });
            }
        }

        EventKind::ToolResult => {
            let indent = if compact { " " } else { "  " };
            let sub_indent = if compact { "  " } else { "    " };
            let (icon, color) = if event.is_error {
                ("\u{2717}", ansi::RED)
            } else {
                ("\u{2713}", ansi::GREEN)
            };

            let total_lines = event.content.lines().count();
            let preview_width = content_width.saturating_sub(3); // "✓ " = 2 chars + space
            let first_line = event.content.lines().next().unwrap_or("");

            // First line: icon + preview, wrapped.
            let first_wrapped = wrap_text(first_line, preview_width);
            for (i, w) in first_wrapped.iter().enumerate() {
                if i == 0 {
                    let line_info = if total_lines > 1 && !expanded {
                        format!(" {}+{}{}", ansi::DIM, total_lines - 1, ansi::RESET)
                    } else {
                        String::new()
                    };
                    rows.push(DisplayRow {
                        formatted: format!(
                            "{}{}{} {}{}{}{}",
                            indent,
                            color,
                            icon,
                            ansi::RESET,
                            ansi::DIM,
                            w,
                            ansi::RESET
                        ),
                        event_index: event_idx,
                        is_expandable: total_lines > 1,
                    });
                    if !line_info.is_empty() {
                        // Append line count hint to the last formatted row.
                        let last = rows.last_mut().unwrap();
                        last.formatted.push_str(&line_info);
                    }
                } else {
                    rows.push(DisplayRow {
                        formatted: format!("{}  {}{}{}", indent, ansi::DIM, w, ansi::RESET),
                        event_index: event_idx,
                        is_expandable: false,
                    });
                }
            }

            // Expanded content lines.
            if expanded && total_lines > 1 {
                let max_expanded = 50;
                let expanded_width = content_width.saturating_sub(sub_indent.len());
                for (count, line) in event.content.lines().skip(1).enumerate() {
                    if count >= max_expanded {
                        break;
                    }
                    for w in wrap_text(line, expanded_width) {
                        rows.push(DisplayRow {
                            formatted: format!("{}{}{}{}", sub_indent, ansi::DIM, w, ansi::RESET),
                            event_index: event_idx,
                            is_expandable: false,
                        });
                    }
                }
                if total_lines - 1 > max_expanded {
                    rows.push(DisplayRow {
                        formatted: format!(
                            "{}{}... {} more{}",
                            sub_indent,
                            ansi::DIM,
                            total_lines - 1 - max_expanded,
                            ansi::RESET
                        ),
                        event_index: event_idx,
                        is_expandable: false,
                    });
                }
            }

            if !compact {
                rows.push(DisplayRow {
                    formatted: String::new(),
                    event_index: event_idx,
                    is_expandable: false,
                });
            }
        }

        EventKind::Progress => {
            let indent = if compact { " " } else { "  " };
            let tool = event.tool_name.as_deref().unwrap_or("?");
            let msg_width = content_width.saturating_sub(tool.len() + 4);
            let msg = truncate_str(&event.content, msg_width);
            rows.push(DisplayRow {
                formatted: format!(
                    "{}{}\u{22ef} {}: {}{}",
                    indent,
                    ansi::DIM,
                    tool,
                    msg,
                    ansi::RESET
                ),
                event_index: event_idx,
                is_expandable: false,
            });
        }

        EventKind::SystemMessage => {
            let indent = if compact { " " } else { "  " };
            let msg = truncate_str(&event.content, content_width.saturating_sub(2));
            rows.push(DisplayRow {
                formatted: format!("{}{}\u{25c6} {}{}", indent, ansi::DIM, msg, ansi::RESET),
                event_index: event_idx,
                is_expandable: false,
            });
        }
    }

    rows
}

/// Extract a meaningful summary from tool input JSON.
fn extract_tool_summary(tool: &str, input_json: &str, max_width: usize) -> String {
    let value: Value = match serde_json::from_str(input_json) {
        Ok(v) => v,
        Err(_) => return truncate_str(input_json, max_width),
    };

    match tool {
        "Bash" => value
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, max_width))
            .unwrap_or_default(),
        "Read" | "Write" | "Edit" => value
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(shorten_path)
            .unwrap_or_default(),
        "Glob" | "Grep" => value
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, max_width))
            .unwrap_or_default(),
        "Agent" => value
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| truncate_str(s, max_width))
            .unwrap_or_default(),
        _ => {
            if let Some(obj) = value.as_object() {
                for (_k, v) in obj.iter() {
                    if let Some(s) = v.as_str()
                        && !s.is_empty()
                    {
                        return truncate_str(s, max_width);
                    }
                }
            }
            String::new()
        }
    }
}

/// Return ANSI color for a tool name.
fn tool_ansi_color(tool: &str) -> &'static str {
    match tool {
        "Bash" => ansi::YELLOW,
        "Read" | "Write" | "Edit" | "Glob" | "Grep" => ansi::CYAN,
        "Agent" => ansi::BLUE,
        _ => ansi::CYAN,
    }
}

/// Shorten a file path for display (keep last 2-3 components).
fn shorten_path(path: &str) -> String {
    let p = Path::new(path);
    let components: Vec<_> = p.components().collect();
    if components.len() <= 3 {
        path.to_string()
    } else {
        let tail: PathBuf = components[components.len() - 3..].iter().collect();
        format!("\u{2026}/{}", tail.display())
    }
}

/// Extract HH:MM:SS from an ISO 8601 timestamp.
fn format_time(ts: &str) -> String {
    if ts.len() >= 19 {
        ts[11..19].to_string()
    } else {
        ts.to_string()
    }
}

// ── Legacy format helpers ─────────────────────────────────────────────

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

/// Wrap a plain text string into lines of at most `width` visible characters.
/// Tries to break at word boundaries when possible.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let cleaned: String = s.chars().map(|c| if c == '\t' { ' ' } else { c }).collect();
    if cleaned.chars().count() <= width {
        return vec![cleaned];
    }

    let mut lines = Vec::new();
    let mut remaining = cleaned.as_str();
    while !remaining.is_empty() {
        let char_count = remaining.chars().count();
        if char_count <= width {
            lines.push(remaining.to_string());
            break;
        }
        // Find the last space within `width` chars to break at.
        let byte_at_width: usize = remaining
            .char_indices()
            .nth(width)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        let candidate = &remaining[..byte_at_width];
        if let Some(space_pos) = candidate.rfind(' ') {
            // Don't break if the space is too far back (< 60% of width).
            if space_pos > byte_at_width / 3 {
                lines.push(remaining[..space_pos].to_string());
                remaining = remaining[space_pos..].trim_start();
                continue;
            }
        }
        // Hard break at width.
        lines.push(candidate.to_string());
        remaining = &remaining[byte_at_width..];
    }
    lines
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

/// Spawn a `notify` file watcher and return the shared state handle plus
/// a shadow `Term` for GPU rendering (tn-pes1).
///
/// The watcher monitors the parent directory (because the file may not
/// exist yet or may be recreated). On each relevant event, it triggers
/// a poll of the file via the shared state, then refreshes the shadow
/// term so the renderer picks up the new content.
///
/// `wake` is called after new rows are appended so the GUI can
/// request a redraw.
#[allow(clippy::type_complexity)]
pub fn spawn_jsonl_watcher(
    path: PathBuf,
    cols: usize,
    rows: usize,
    wake: Box<dyn Fn() + Send + Sync + 'static>,
) -> Result<
    (
        Arc<Mutex<JsonlTailState>>,
        Arc<FairMutex<Term<PaneListener>>>,
        JsonlTailWatcher,
    ),
    anyhow::Error,
> {
    let mut init_state = JsonlTailState::new(path.clone(), cols);
    init_state.visible_rows = rows.max(3);
    let state = Arc::new(Mutex::new(init_state));

    // tn-pes1: create the shadow Term with zero scrollback — the JSONL
    // viewer manages its own scroll offset and re-paints the grid on every
    // state change.
    let term_config = alacritty_terminal::term::Config {
        scrolling_history: 0,
        ..Default::default()
    };
    let term_size = therminal_terminal::pty_runtime::TermSize {
        columns: cols.max(20),
        screen_lines: rows.max(3),
    };
    let listener = PaneListener::new();
    let term = Arc::new(FairMutex::new(Term::new(term_config, &term_size, listener)));

    // Do an initial poll to pick up any existing content.
    if let Ok(mut s) = state.lock() {
        s.poll_file();
        s.refresh_shadow_term(&term);
    }

    // Wrap wake in Arc so both the notify watcher and the poll thread
    // can call it.
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::from(wake);

    let state_for_watcher = Arc::clone(&state);
    let term_for_watcher = Arc::clone(&term);
    let watch_path = path.clone();
    let wake_for_watcher = Arc::clone(&wake);

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
                                    // tn-pes1: repaint the shadow term with new content.
                                    s.refresh_shadow_term(&term_for_watcher);
                                }
                                wake_for_watcher();
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

    // ── Polling fallback ──────────────────────────────────────────────
    // The `notify` watcher may not deliver modify events for all
    // filesystem types (notably Windows→WSL2 UNC paths). A lightweight
    // poll thread calls `poll_file()` every 500ms, which opens the file
    // handle and reads from the last offset. This bypasses Windows SMB
    // metadata caching that makes path-based `metadata().len()` stale
    // on UNC paths for 10+ seconds.  Only triggers a redraw when
    // `poll_file()` actually consumed new bytes (file_offset advanced).
    let poll_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poll_thread = {
        let shutdown = Arc::clone(&poll_shutdown);
        let state_for_poll = Arc::clone(&state);
        let term_for_poll = Arc::clone(&term);
        let poll_path = path.clone();
        let wake_poll = Arc::clone(&wake);
        std::thread::Builder::new()
            .name(format!("jsonl-poll-{}", poll_path.display()))
            .spawn(move || {
                let _ = poll_path; // held for thread naming only
                while !shutdown.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    if shutdown.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let had_new_content = if let Ok(mut s) = state_for_poll.lock() {
                        let before = s.file_offset;
                        s.poll_file();
                        let after = s.file_offset;
                        if after != before {
                            s.refresh_shadow_term(&term_for_poll);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if had_new_content {
                        wake_poll();
                    }
                }
            })?
    };

    Ok((
        state,
        term,
        JsonlTailWatcher {
            _watcher: watcher,
            poll_shutdown,
            _poll_thread: Some(poll_thread),
        },
    ))
}

/// RAII guard holding the `notify` watcher and poll thread alive.
/// Drop to stop watching and shut down the poll thread.
pub struct JsonlTailWatcher {
    _watcher: RecommendedWatcher,
    /// Signal the poll thread to exit.
    poll_shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Join handle for the poll thread (joined on drop for clean shutdown).
    _poll_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for JsonlTailWatcher {
    fn drop(&mut self) {
        self.poll_shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        // Don't block on join — the thread will exit within one poll interval.
    }
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
        // Should contain header + rows + footer.
        assert!(content.matches('\n').count() >= 2);
    }

    // ── Structured parsing tests ──────────────────────────────────────

    #[test]
    fn parse_nested_user_message() {
        let events = parse_jsonl_event(
            r#"{"type":"user","message":{"role":"user","content":"Hello world"},"timestamp":"2026-04-02T00:13:52.232Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::UserMessage);
        assert_eq!(events[0].content, "Hello world");
    }

    #[test]
    fn parse_nested_assistant_text() {
        let events = parse_jsonl_event(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll help you."}]},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::AssistantText);
        assert_eq!(events[0].content, "I'll help you.");
    }

    #[test]
    fn parse_nested_tool_use_in_assistant() {
        let events = parse_jsonl_event(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","id":"toolu_01ABC","input":{"command":"ls -la"}}]},"timestamp":"2026-04-02T00:14:01.000Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::ToolUse);
        assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
        assert_eq!(events[0].tool_use_id.as_deref(), Some("toolu_01ABC"));
        assert!(events[0].content.contains("ls -la"));
    }

    #[test]
    fn parse_nested_tool_result_in_user() {
        let events = parse_jsonl_event(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01ABC","content":"file.rs\nCargo.toml"}]},"timestamp":"2026-04-02T00:14:02.500Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::ToolResult);
        assert_eq!(events[0].tool_use_id.as_deref(), Some("toolu_01ABC"));
        assert_eq!(events[0].content, "file.rs\nCargo.toml");
    }

    #[test]
    fn parse_nested_thinking_in_assistant() {
        let events = parse_jsonl_event(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Let me analyze..."}]},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Thinking);
        assert_eq!(events[0].content, "Let me analyze...");
    }

    #[test]
    fn parse_nested_mixed_assistant_content() {
        let events = parse_jsonl_event(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Let me check."},{"type":"tool_use","name":"Grep","id":"toolu_02DEF","input":{"pattern":"TODO"}}]},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, EventKind::AssistantText);
        assert_eq!(events[1].kind, EventKind::ToolUse);
        assert_eq!(events[1].tool_name.as_deref(), Some("Grep"));
    }

    #[test]
    fn parse_nested_tool_result_error() {
        let events = parse_jsonl_event(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ERR","content":"command not found","is_error":true}]},"timestamp":"2026-04-02T00:14:05.000Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert!(events[0].is_error);
    }

    #[test]
    fn parse_legacy_flat_tool_use() {
        let events = parse_jsonl_event(
            r#"{"type":"tool_use","tool":"Bash","tool_use_id":"tu_01","input":{"command":"ls"},"timestamp":"2026-04-02T00:14:00.000Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::ToolUse);
        assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn parse_legacy_flat_tool_result() {
        let events = parse_jsonl_event(
            r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_01","output":"ok","timestamp":"2026-04-02T00:14:02.000Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::ToolResult);
        assert_eq!(events[0].content, "ok");
    }

    #[test]
    fn parse_system_message() {
        let events = parse_jsonl_event(
            r#"{"type":"system","content":"Session started","timestamp":"2026-04-02T00:10:00.000Z"}"#,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::SystemMessage);
    }

    #[test]
    fn parse_unknown_type_empty() {
        let events = parse_jsonl_event(r#"{"type":"unknown_future_type","data":"something"}"#);
        assert!(events.is_empty());
    }

    #[test]
    fn parse_permission_mode_empty() {
        let events = parse_jsonl_event(
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"abc"}"#,
        );
        // No content field -> empty.
        assert!(events.is_empty());
    }

    // ── Keyboard interaction tests ────────────────────────────────────

    #[test]
    fn handle_input_scroll_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..30 {
            content.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"line {i}\"}}]}}}}\n"
            ));
        }
        std::fs::write(&path, &content).unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.visible_rows = 10;
        state.following = false;
        state.poll_file();

        let initial_scroll = state.scroll;
        assert!(state.handle_input(b"j")); // scroll down
        assert_eq!(state.scroll, initial_scroll + 1);
    }

    #[test]
    fn handle_input_scroll_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for i in 0..30 {
            content.push_str(&format!(
                "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"line {i}\"}}]}}}}\n"
            ));
        }
        std::fs::write(&path, &content).unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.visible_rows = 10;
        state.following = false;
        state.poll_file();
        state.scroll = 5;

        assert!(state.handle_input(b"k")); // scroll up
        assert_eq!(state.scroll, 4);
    }

    #[test]
    fn handle_input_toggle_follow() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, "{\"type\":\"system\",\"content\":\"test\"}\n").unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.poll_file();
        assert!(state.following);
        assert!(state.handle_input(b"f"));
        assert!(!state.following);
        assert!(state.handle_input(b"f"));
        assert!(state.following);
    }

    #[test]
    fn handle_input_unknown_key_not_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        std::fs::write(&path, "{\"x\":1}\n").unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.poll_file();
        assert!(!state.handle_input(b"z")); // unknown key
    }

    #[test]
    fn expand_collapse_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // Write a tool_result with multi-line content.
        let line = r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_01","output":"line1\nline2\nline3\nline4\nline5","timestamp":"2026-04-02T00:14:02.500Z"}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.visible_rows = 20;
        state.following = false;
        state.poll_file();

        // Should have an expandable row (tool_result with >1 lines).
        let expandable_count = state.display.iter().filter(|r| r.is_expandable).count();
        assert!(expandable_count > 0, "should have expandable rows");

        let lines_before = state.display.len();

        // Toggle expand.
        state.handle_input(b"\r");
        let lines_after = state.display.len();

        // After expanding, there should be more display lines.
        assert!(
            lines_after != lines_before,
            "expand should change line count: before={lines_before}, after={lines_after}"
        );
    }

    #[test]
    fn toggle_all_expansion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut content = String::new();
        for _ in 0..3 {
            content.push_str(
                r#"{"type":"tool_result","tool":"Bash","tool_use_id":"tu_01","output":"line1\nline2\nline3","timestamp":"2026-04-02T00:14:02.000Z"}"#,
            );
            content.push('\n');
        }
        std::fs::write(&path, &content).unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.visible_rows = 40;
        state.poll_file();

        assert!(!state.all_expanded);
        state.handle_input(b"e");
        assert!(state.all_expanded);
        state.handle_input(b"e");
        assert!(!state.all_expanded);
    }

    #[test]
    fn render_event_tool_use_summary() {
        let event = StructuredEvent {
            kind: EventKind::ToolUse,
            timestamp: "2026-04-02T00:14:01.000Z".to_string(),
            content: r#"{"command":"ls -la"}"#.to_string(),
            tool_name: Some("Bash".to_string()),
            tool_use_id: Some("tu_01".to_string()),
            is_error: false,
        };
        let rows = render_event(&event, 80, false, 0);
        assert!(!rows.is_empty());
        let combined: String = rows.iter().map(|r| r.formatted.clone()).collect();
        assert!(combined.contains("Bash"), "should show tool name");
        assert!(combined.contains("ls -la"), "should show command summary");
    }

    #[test]
    fn render_event_tool_result_ok() {
        let event = StructuredEvent {
            kind: EventKind::ToolResult,
            timestamp: String::new(),
            content: "file.rs\nCargo.toml".to_string(),
            tool_name: Some("Bash".to_string()),
            tool_use_id: None,
            is_error: false,
        };
        let rows = render_event(&event, 80, false, 0);
        let combined: String = rows.iter().map(|r| r.formatted.clone()).collect();
        assert!(combined.contains("\u{2713}"), "should show check mark");
    }

    #[test]
    fn render_event_tool_result_error() {
        let event = StructuredEvent {
            kind: EventKind::ToolResult,
            timestamp: String::new(),
            content: "command not found".to_string(),
            tool_name: Some("Bash".to_string()),
            tool_use_id: None,
            is_error: true,
        };
        let rows = render_event(&event, 80, false, 0);
        let combined: String = rows.iter().map(|r| r.formatted.clone()).collect();
        assert!(combined.contains("\u{2717}"), "should show cross mark");
    }

    #[test]
    fn extract_tool_summary_bash() {
        let summary = extract_tool_summary("Bash", r#"{"command":"cargo build"}"#, 40);
        assert_eq!(summary, "cargo build");
    }

    #[test]
    fn extract_tool_summary_read() {
        let summary = extract_tool_summary(
            "Read",
            r#"{"file_path":"/home/user/project/src/main.rs"}"#,
            60,
        );
        assert!(summary.contains("main.rs"));
    }

    #[test]
    fn extract_tool_summary_grep() {
        let summary = extract_tool_summary("Grep", r#"{"pattern":"fn main"}"#, 40);
        assert_eq!(summary, "fn main");
    }

    #[test]
    fn shorten_path_long() {
        let short = shorten_path("/home/user/projects/therminal/crates/app/src/main.rs");
        assert!(short.starts_with('\u{2026}'));
        assert!(short.contains("main.rs"));
    }

    #[test]
    fn shorten_path_short() {
        assert_eq!(shorten_path("src/main.rs"), "src/main.rs");
    }

    // ── wrap_text tests ──────────────────────────────────────────────

    #[test]
    fn wrap_text_short_line() {
        assert_eq!(wrap_text("hello", 20), vec!["hello"]);
    }

    #[test]
    fn wrap_text_exact_width() {
        assert_eq!(wrap_text("12345", 5), vec!["12345"]);
    }

    #[test]
    fn wrap_text_word_break() {
        let lines = wrap_text("hello world foo bar", 12);
        assert!(lines.len() >= 2);
        // First line should break at a word boundary.
        assert!(lines[0].len() <= 12);
    }

    #[test]
    fn wrap_text_hard_break() {
        let lines = wrap_text("abcdefghijklmnopqrstuvwxyz", 10);
        assert!(lines.len() >= 3);
        assert_eq!(lines[0].len(), 10);
    }

    #[test]
    fn wrap_text_zero_width() {
        assert_eq!(wrap_text("hello", 0), vec![""]);
    }

    // ── Event cap eviction test (tn-kyr3) ────────────────────────────

    #[test]
    fn event_cap_evicts_old_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        // Write more events than MAX_EVENTS.
        let count = MAX_EVENTS + 500;
        let mut content = String::new();
        for i in 0..count {
            content.push_str(&format!(
                "{{\"type\":\"system\",\"content\":\"event {i}\"}}\n"
            ));
        }
        std::fs::write(&path, &content).unwrap();

        let mut state = JsonlTailState::new(path, 80);
        state.poll_file();

        // events should be capped at MAX_EVENTS.
        assert!(
            state.events.len() <= MAX_EVENTS,
            "events.len()={} should be <= {}",
            state.events.len(),
            MAX_EVENTS,
        );
        // display should also be bounded (1 display row per SystemMessage event).
        assert!(state.display.len() <= MAX_EVENTS);
    }

    // ── Compact rendering test ───────────────────────────────────────

    #[test]
    fn render_event_compact_at_narrow_width() {
        let event = StructuredEvent {
            kind: EventKind::UserMessage,
            timestamp: "2026-04-14T12:00:00Z".to_string(),
            content: "Hello from the user".to_string(),
            tool_name: None,
            tool_use_id: None,
            is_error: false,
        };
        // Narrow (compact mode).
        let rows_narrow = render_event(&event, 40, false, 0);
        // Wide (normal mode).
        let rows_wide = render_event(&event, 100, false, 0);

        // Compact mode should NOT have a blank separator row.
        let last_narrow = &rows_narrow.last().unwrap().formatted;
        assert!(
            !last_narrow.is_empty(),
            "compact should not end with blank separator"
        );

        // Normal mode should have a blank separator row.
        let last_wide = &rows_wide.last().unwrap().formatted;
        assert!(
            last_wide.is_empty(),
            "normal should end with blank separator"
        );

        // Compact header should NOT contain timestamp.
        let header_narrow = &rows_narrow[0].formatted;
        assert!(
            !header_narrow.contains("12:00:00"),
            "compact should omit timestamp"
        );

        // Normal header should contain timestamp.
        let header_wide = &rows_wide[0].formatted;
        assert!(
            header_wide.contains("12:00:00"),
            "normal should show timestamp"
        );
    }
}
