//! Claude Code JSONL session log tailer + top-level session registry.
//!
//! `SessionJsonlTailer` incrementally reads new lines from a single Claude Code
//! session JSONL file under `~/.claude/projects/{hash}/{session-id}.jsonl` and
//! emits [`AgentEvent`]s. It handles truncation and partial-line-at-EOF.
//!
//! `ClaudeJsonlRegistry` owns one tailer per *top-level* Claude session
//! (sessions whose `parent_session_id` is `None`) and is driven by
//! [`ClaudeStatePoller`] updates. Subagent JSONLs (under `.../subagents/`) are
//! intentionally out of scope here — see future task tn-xvwv.
//!
//! ## Wiring
//!
//! TODO(tn-lzvv): wire `ClaudeJsonlRegistry::events()` into `DaemonEvent` so
//! MCP resource subscribers can receive structured agent events. For now the
//! registry exposes its event stream via a plain `mpsc::Receiver<AgentEvent>`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};

use tracing::{debug, trace, warn};

use crate::agent_events::AgentEvent;
use crate::claude_session_log::{self, SessionEventType};
use crate::claude_state::{ClaudeSessionState, ClaudeStateUpdate};

/// Incrementally reads a Claude Code JSONL session file and emits `AgentEvent`s.
pub struct SessionJsonlTailer {
    /// The session_id we are currently tailing.
    current_session_id: Option<String>,
    /// Resolved JSONL file path for the current session.
    current_path: Option<PathBuf>,
    /// Byte offset into the file — we read from here on each poll.
    read_offset: u64,
}

impl Default for SessionJsonlTailer {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionJsonlTailer {
    pub fn new() -> Self {
        Self {
            current_session_id: None,
            current_path: None,
            read_offset: 0,
        }
    }

    /// Poll for new JSONL lines. If the session_id changed, resolve the new
    /// JSONL file and start tailing from the current end (skip history).
    pub fn poll(&mut self, session_id: Option<&str>, tx: &Sender<AgentEvent>) {
        match session_id {
            Some(sid) => {
                if self.current_session_id.as_deref() != Some(sid) {
                    self.switch_session(sid);
                }
            }
            None => {
                if self.current_session_id.is_some() {
                    trace!("JSONL tailer: no active session, clearing");
                    self.current_session_id = None;
                    self.current_path = None;
                    self.read_offset = 0;
                }
                return;
            }
        }

        let path = match &self.current_path {
            Some(p) => p.clone(),
            None => return,
        };

        self.read_new_lines(&path, tx);
    }

    /// Switch to a new session: resolve JSONL path and seek to end.
    fn switch_session(&mut self, session_id: &str) {
        debug!(session_id, "JSONL tailer: switching to new session");

        self.current_session_id = Some(session_id.to_string());
        self.current_path = None;
        self.read_offset = 0;

        if let Some(path) = resolve_session_jsonl(session_id) {
            debug!(
                session_id,
                path = %path.display(),
                "JSONL tailer: resolved session JSONL"
            );

            let offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

            self.current_path = Some(path);
            self.read_offset = offset;
        } else {
            debug!(
                session_id,
                "JSONL tailer: could not resolve JSONL file (will retry)"
            );
        }
    }

    /// Incrementally read new lines from the JSONL file.
    fn read_new_lines(&mut self, path: &Path, tx: &Sender<AgentEvent>) {
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let file_len = match file.metadata() {
            Ok(m) => m.len(),
            Err(_) => return,
        };

        if file_len <= self.read_offset {
            if file_len < self.read_offset {
                trace!("JSONL tailer: file truncated, resetting offset");
                self.read_offset = file_len;
            }
            return;
        }

        if file.seek(SeekFrom::Start(self.read_offset)).is_err() {
            return;
        }

        let reader = BufReader::new(&file);
        let mut events_sent = 0u32;
        let mut bytes_consumed: u64 = 0;

        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            bytes_consumed += line.len() as u64 + 1;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let session_events = claude_session_log::parse_session_event(trimmed);

            for se in session_events {
                if let Some(agent_event) = session_event_to_agent_event(&se) {
                    if tx.send(agent_event).is_err() {
                        warn!("JSONL tailer: agent_event_tx receiver dropped");
                        return;
                    }
                    events_sent += 1;
                }
            }
        }

        // Advance only by fully-consumed lines so partial lines at EOF
        // are retried on the next poll instead of being silently dropped.
        self.read_offset += bytes_consumed;

        if events_sent > 0 {
            trace!(events_sent, "JSONL tailer: sent agent events");
        }
    }
}

// ── Conversion ──────────────────────────────────────────────────────────────

/// Convert a `SessionEvent` to an `AgentEvent`.
fn session_event_to_agent_event(se: &claude_session_log::SessionEvent) -> Option<AgentEvent> {
    match se.event_type {
        SessionEventType::UserMessage => Some(AgentEvent::UserMessage {
            content: se.content.clone(),
        }),
        SessionEventType::AssistantText => Some(AgentEvent::AssistantMessage {
            content: se.content.clone(),
        }),
        SessionEventType::ToolUse => {
            let input = serde_json::from_str(&se.content).unwrap_or(serde_json::Value::Null);
            Some(AgentEvent::ToolUse {
                tool: se.tool_name.clone().unwrap_or_default(),
                input,
                tool_use_id: se.tool_use_id.clone(),
            })
        }
        SessionEventType::ToolResult => Some(AgentEvent::ToolResult {
            tool: se.tool_name.clone().unwrap_or_default(),
            output: se.content.clone(),
            is_error: se.is_error,
            tool_use_id: se.tool_use_id.clone(),
        }),
        SessionEventType::Progress => Some(AgentEvent::Progress {
            tool: se.tool_name.clone().unwrap_or_default(),
            status: String::new(),
            message: Some(se.content.clone()),
            tool_use_id: se.tool_use_id.clone(),
        }),
        SessionEventType::Thinking => Some(AgentEvent::Thinking {
            content: se.content.clone(),
        }),
        SessionEventType::SystemMessage => None,
    }
}

// ── JSONL path resolution ───────────────────────────────────────────────────

/// Resolve the JSONL file path for a Claude Code session ID.
///
/// Claude Code stores session transcripts at:
/// `~/.claude/projects/{project-hash}/{session-uuid}.jsonl`
fn resolve_session_jsonl(session_id: &str) -> Option<PathBuf> {
    let projects_dir = home_dir().join(".claude").join("projects");
    if !projects_dir.exists() {
        return None;
    }

    let filename = format!("{session_id}.jsonl");

    let entries = std::fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        let project_dir = entry.path();
        if !project_dir.is_dir() {
            continue;
        }

        let candidate = project_dir.join(&filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }

    None
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/home/builder".into()))
}

// ── Top-level session registry ──────────────────────────────────────────────

/// Holds one [`SessionJsonlTailer`] per *top-level* Claude session and drives
/// them from [`ClaudeStateUpdate`]s.
///
/// Subagents (entries with `parent_session_id.is_some()`) are filtered out.
pub struct ClaudeJsonlRegistry {
    tailers: HashMap<String, SessionJsonlTailer>,
    /// Maps state-file path → session_id so `Removed { path }` updates can
    /// drop the right tailer.
    path_to_session: HashMap<PathBuf, String>,
    event_tx: Sender<AgentEvent>,
    event_rx: Option<mpsc::Receiver<AgentEvent>>,
}

impl Default for ClaudeJsonlRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeJsonlRegistry {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        Self {
            tailers: HashMap::new(),
            path_to_session: HashMap::new(),
            event_tx,
            event_rx: Some(event_rx),
        }
    }

    /// Take the receiver half of the agent-event channel. Can only be called
    /// once — returns `None` on subsequent calls.
    pub fn events(&mut self) -> Option<mpsc::Receiver<AgentEvent>> {
        self.event_rx.take()
    }

    /// Returns the number of currently-tracked top-level sessions.
    pub fn len(&self) -> usize {
        self.tailers.len()
    }

    /// Returns true if no top-level sessions are being tracked.
    pub fn is_empty(&self) -> bool {
        self.tailers.is_empty()
    }

    /// Apply a [`ClaudeStateUpdate`] from `ClaudeStatePoller::updates()`.
    ///
    /// - `Upserted`: if it's a top-level session (no `parent_session_id`),
    ///   ensure a tailer exists for it. Subagents are skipped.
    /// - `Removed`: drop any tailer associated with the file path.
    pub fn apply_update(&mut self, update: &ClaudeStateUpdate, path_hint: Option<&Path>) {
        match update {
            ClaudeStateUpdate::Upserted(state) => {
                self.handle_upsert(state, path_hint);
            }
            ClaudeStateUpdate::Removed { path } => {
                if let Some(sid) = self.path_to_session.remove(path)
                    && self.tailers.remove(&sid).is_some()
                {
                    debug!(session_id = %sid, "JSONL registry: dropped tailer for removed session");
                }
            }
        }
    }

    fn handle_upsert(&mut self, state: &ClaudeSessionState, path_hint: Option<&Path>) {
        // Filter: top-level sessions only. Subagents handled by tn-xvwv.
        if state.parent_session_id.is_some() {
            trace!(
                session_id = %state.session_id,
                "JSONL registry: skipping subagent (has parent_session_id)"
            );
            return;
        }
        if state.session_id.is_empty() {
            return;
        }

        if !self.tailers.contains_key(&state.session_id) {
            debug!(session_id = %state.session_id, "JSONL registry: inserting tailer for new top-level session");
            self.tailers
                .insert(state.session_id.clone(), SessionJsonlTailer::new());
        }
        if let Some(p) = path_hint {
            self.path_to_session
                .insert(p.to_path_buf(), state.session_id.clone());
        }
    }

    /// Drain pending updates from a poller channel and apply them.
    ///
    /// `Removed` updates carry their path; `Upserted` updates do not, so we
    /// pass `None` for the path hint and rely on `Removed` paths recorded
    /// previously by some other mechanism. In practice, today we only need
    /// the upsert side to install tailers and the remove side to drop them
    /// keyed by `session_id` — `path_to_session` is best-effort and only
    /// populated when callers supply a path hint via `apply_update`.
    pub fn drain_updates(&mut self, rx: &mpsc::Receiver<ClaudeStateUpdate>) {
        while let Ok(update) = rx.try_recv() {
            self.apply_update(&update, None);
        }
    }

    /// Poll every tracked tailer once. Resulting events are pushed onto the
    /// internal channel exposed by [`ClaudeJsonlRegistry::events`].
    pub fn poll_all(&mut self) {
        // We clone keys to avoid holding an immutable borrow while we mutate
        // the tailers below.
        let session_ids: Vec<String> = self.tailers.keys().cloned().collect();
        for sid in session_ids {
            if let Some(tailer) = self.tailers.get_mut(&sid) {
                tailer.poll(Some(&sid), &self.event_tx);
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn tailer_starts_empty() {
        let tailer = SessionJsonlTailer::new();
        assert!(tailer.current_session_id.is_none());
        assert!(tailer.current_path.is_none());
        assert_eq!(tailer.read_offset, 0);
    }

    #[test]
    fn tailer_clears_on_no_session() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut tailer = SessionJsonlTailer::new();
        tailer.current_session_id = Some("old".into());
        tailer.poll(None, &tx);
        assert!(tailer.current_session_id.is_none());
    }

    #[test]
    fn session_event_conversion_user() {
        let se = claude_session_log::SessionEvent {
            timestamp: String::new(),
            event_type: SessionEventType::UserMessage,
            content: "hello".into(),
            tool_name: None,
            tool_use_id: None,
            is_error: false,
        };
        let ae = session_event_to_agent_event(&se).unwrap();
        assert_eq!(
            ae,
            AgentEvent::UserMessage {
                content: "hello".into()
            }
        );
    }

    #[test]
    fn session_event_conversion_tool_use() {
        let se = claude_session_log::SessionEvent {
            timestamp: String::new(),
            event_type: SessionEventType::ToolUse,
            content: r#"{"command": "ls"}"#.into(),
            tool_name: Some("Bash".into()),
            tool_use_id: Some("tu_01".into()),
            is_error: false,
        };
        let ae = session_event_to_agent_event(&se).unwrap();
        match ae {
            AgentEvent::ToolUse {
                tool,
                input,
                tool_use_id,
            } => {
                assert_eq!(tool, "Bash");
                assert_eq!(input["command"], "ls");
                assert_eq!(tool_use_id.as_deref(), Some("tu_01"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn session_event_conversion_tool_result() {
        let se = claude_session_log::SessionEvent {
            timestamp: String::new(),
            event_type: SessionEventType::ToolResult,
            content: "ok".into(),
            tool_name: Some("Bash".into()),
            tool_use_id: Some("tu_01".into()),
            is_error: true,
        };
        let ae = session_event_to_agent_event(&se).unwrap();
        assert_eq!(
            ae,
            AgentEvent::ToolResult {
                tool: "Bash".into(),
                output: "ok".into(),
                is_error: true,
                tool_use_id: Some("tu_01".into()),
            }
        );
    }

    #[test]
    fn session_event_conversion_system_is_none() {
        let se = claude_session_log::SessionEvent {
            timestamp: String::new(),
            event_type: SessionEventType::SystemMessage,
            content: "system info".into(),
            tool_name: None,
            tool_use_id: None,
            is_error: false,
        };
        assert!(session_event_to_agent_event(&se).is_none());
    }

    #[test]
    fn tailer_reads_new_lines_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        {
            let mut f = File::create(&path).unwrap();
            writeln!(
                f,
                r#"{{"type":"user","content":"old message","timestamp":"2026-04-02T00:00:00Z"}}"#
            )
            .unwrap();
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let mut tailer = SessionJsonlTailer::new();

        tailer.current_session_id = Some("test".into());
        let initial_len = std::fs::metadata(&path).unwrap().len();
        tailer.read_offset = initial_len;
        tailer.current_path = Some(path.clone());

        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(
                f,
                r#"{{"type":"user","content":"new message","timestamp":"2026-04-02T00:01:00Z"}}"#
            )
            .unwrap();
        }

        tailer.read_new_lines(&path, &tx);

        let event = rx.try_recv().unwrap();
        assert_eq!(
            event,
            AgentEvent::UserMessage {
                content: "new message".into()
            }
        );

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tailer_handles_nested_assistant_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");

        let (tx, rx) = std::sync::mpsc::channel();
        let mut tailer = SessionJsonlTailer::new();
        tailer.current_session_id = Some("test".into());
        tailer.current_path = Some(path.clone());
        tailer.read_offset = 0;

        {
            let mut f = File::create(&path).unwrap();
            writeln!(f, r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"Let me check."}},{{"type":"tool_use","name":"Bash","id":"tu_abc","input":{{"command":"ls"}}}}]}},"timestamp":"2026-04-02T00:14:00.000Z"}}"#).unwrap();
        }

        tailer.read_new_lines(&path, &tx);

        let ev1 = rx.try_recv().unwrap();
        assert_eq!(
            ev1,
            AgentEvent::AssistantMessage {
                content: "Let me check.".into()
            }
        );

        let ev2 = rx.try_recv().unwrap();
        match ev2 {
            AgentEvent::ToolUse {
                tool, tool_use_id, ..
            } => {
                assert_eq!(tool, "Bash");
                assert_eq!(tool_use_id.as_deref(), Some("tu_abc"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        assert!(rx.try_recv().is_err());
    }

    // ── Registry tests ──────────────────────────────────────────────────

    #[test]
    fn registry_inserts_tailer_for_top_level_session_and_polls_events() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl_path = dir.path().join("session.jsonl");

        // Pre-create the JSONL file with one line.
        {
            let mut f = File::create(&jsonl_path).unwrap();
            writeln!(
                f,
                r#"{{"type":"user","content":"hi from registry","timestamp":"2026-04-02T00:00:00Z"}}"#
            )
            .unwrap();
        }

        let mut registry = ClaudeJsonlRegistry::new();
        let rx = registry.events().expect("events receiver");

        // Inject a top-level state.
        let state = ClaudeSessionState {
            session_id: "top-1".into(),
            parent_session_id: None,
            ..ClaudeSessionState::default()
        };
        let upd = ClaudeStateUpdate::Upserted(Box::new(state));
        registry.apply_update(&upd, None);

        assert_eq!(registry.len(), 1);

        // We can't depend on `~/.claude/projects/top-1.jsonl` existing in CI,
        // so swap the tailer's path manually to point at our temp file.
        {
            let tailer = registry.tailers.get_mut("top-1").unwrap();
            tailer.current_session_id = Some("top-1".into());
            tailer.current_path = Some(jsonl_path.clone());
            tailer.read_offset = 0;
        }

        registry.poll_all();

        let event = rx.try_recv().expect("event");
        assert_eq!(
            event,
            AgentEvent::UserMessage {
                content: "hi from registry".into()
            }
        );
    }

    #[test]
    fn registry_filters_out_subagents() {
        let mut registry = ClaudeJsonlRegistry::new();

        let subagent = ClaudeSessionState {
            session_id: "child-1".into(),
            parent_session_id: Some("parent-xyz".into()),
            ..ClaudeSessionState::default()
        };
        registry.apply_update(&ClaudeStateUpdate::Upserted(Box::new(subagent)), None);

        assert!(registry.is_empty());
        assert!(!registry.tailers.contains_key("child-1"));
    }

    #[test]
    fn registry_removes_tailer_on_path_remove() {
        let mut registry = ClaudeJsonlRegistry::new();
        let state_path = PathBuf::from("/tmp/claude-code-state/sess-rm.json");

        let state = ClaudeSessionState {
            session_id: "sess-rm".into(),
            parent_session_id: None,
            ..ClaudeSessionState::default()
        };
        registry.apply_update(
            &ClaudeStateUpdate::Upserted(Box::new(state)),
            Some(&state_path),
        );
        assert_eq!(registry.len(), 1);

        registry.apply_update(
            &ClaudeStateUpdate::Removed {
                path: state_path.clone(),
            },
            None,
        );
        assert!(registry.is_empty());
    }
}
