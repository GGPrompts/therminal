//! Claude Code JSONL session log tailer + top-level session registry.
//!
//! `SessionJsonlTailer` incrementally reads new lines from a single Claude Code
//! session JSONL file under `~/.claude/projects/{hash}/{session-id}.jsonl` and
//! emits [`AgentEvent`]s. It handles truncation and partial-line-at-EOF.
//!
//! `ClaudeJsonlRegistry` owns one tailer per *top-level* Claude session
//! (sessions whose `parent_session_id` is `None`) and is driven by
//! [`ClaudeStatePoller`] updates. It also discovers and tails subagent JSONLs
//! that live under `~/.claude/projects/{hash}/{parent-sid}/subagents/agent-*.jsonl`,
//! one [`SessionJsonlTailer`] per subagent. Events from both top-level and
//! subagent tailers are emitted as [`TaggedAgentEvent`]s carrying an
//! [`EventSource`] discriminator so consumers can rebuild the parent/child
//! topology.
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
use crate::session_log::{self, SessionEventType};
use crate::state::{ClaudeSessionState, ClaudeStateUpdate};

/// Identifies which JSONL stream a [`TaggedAgentEvent`] originated from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventSource {
    /// Top-level Claude session (no parent).
    TopLevel { session_id: String },
    /// Subagent (sidechain) spawned via the `Task` tool by a parent session.
    Subagent {
        parent_session_id: String,
        agent_id: String,
    },
}

/// An [`AgentEvent`] paired with the source stream it came from.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TaggedAgentEvent {
    pub event: AgentEvent,
    pub source: EventSource,
}

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

    /// Construct a tailer pre-bound to an explicit JSONL file path. Used for
    /// subagent JSONLs whose paths are discovered directly rather than
    /// resolved from a session_id. The tailer reads from offset 0 so the
    /// full sidechain transcript is captured.
    pub fn for_path(session_id: String, path: PathBuf) -> Self {
        Self {
            current_session_id: Some(session_id),
            current_path: Some(path),
            read_offset: 0,
        }
    }

    /// Poll for new JSONL lines. If the session_id changed, resolve the new
    /// JSONL file and start tailing from the current end (skip history).
    pub fn poll(
        &mut self,
        session_id: Option<&str>,
        tx: &Sender<TaggedAgentEvent>,
        tag: &EventSource,
    ) {
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

        self.read_new_lines(&path, tx, tag);
    }

    /// Poll an explicit-path tailer (used by subagents). No session-id switch
    /// logic; just reads any new bytes from the configured path.
    pub fn poll_path(&mut self, tx: &Sender<TaggedAgentEvent>, tag: &EventSource) {
        let path = match &self.current_path {
            Some(p) => p.clone(),
            None => return,
        };
        self.read_new_lines(&path, tx, tag);
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
    fn read_new_lines(&mut self, path: &Path, tx: &Sender<TaggedAgentEvent>, tag: &EventSource) {
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

        let mut reader = BufReader::new(&mut file);
        let mut events_sent = 0u32;
        let mut last_complete_offset: u64 = self.read_offset;
        let mut bytes_read_so_far: u64 = 0;
        let mut line_buf = String::new();

        loop {
            line_buf.clear();
            let n = match reader.read_line(&mut line_buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            bytes_read_so_far += n as u64;

            // Only treat a line as consumed if it ended with a newline.
            // A partial tail (no trailing '\n') must be retried next poll.
            let ends_with_newline = line_buf.ends_with('\n');
            if !ends_with_newline {
                break;
            }
            last_complete_offset = self.read_offset + bytes_read_so_far;

            let trimmed = line_buf.trim();
            if trimmed.is_empty() {
                continue;
            }

            let session_events = session_log::parse_session_event(trimmed);

            for se in session_events {
                if let Some(agent_event) = session_event_to_agent_event(&se) {
                    let tagged = TaggedAgentEvent {
                        event: agent_event,
                        source: tag.clone(),
                    };
                    if tx.send(tagged).is_err() {
                        warn!("JSONL tailer: agent_event_tx receiver dropped");
                        return;
                    }
                    events_sent += 1;
                }
            }
        }

        // Advance only by fully-consumed lines so partial lines at EOF
        // are retried on the next poll instead of being silently dropped.
        // Using the actual byte count from `read_line` handles both LF and
        // CRLF line endings correctly (read_line includes the terminator).
        self.read_offset = last_complete_offset;

        if events_sent > 0 {
            trace!(events_sent, "JSONL tailer: sent agent events");
        }
    }
}

// ── Conversion ──────────────────────────────────────────────────────────────

/// Convert a `SessionEvent` to an `AgentEvent`.
fn session_event_to_agent_event(se: &session_log::SessionEvent) -> Option<AgentEvent> {
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
    if !is_valid_session_id(session_id) {
        return None;
    }
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

/// Validate a Claude session id before using it in path construction.
/// Prevents path traversal via untrusted state files.
fn is_valid_session_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Resolve the user's home directory via the `dirs` crate, which handles
/// `$HOME` on Unix and `%USERPROFILE%` on Windows native. Falls back to
/// `/home/builder` only when no platform home can be determined — which
/// is a near-impossible state on a real user machine and indicates either
/// a broken environment or a CI sandbox with neither env var set.
///
/// Prior to tn-ix8c this function read `$HOME` directly, which is typically
/// unset on Windows native, so the subagent JSONL tailer silently watched
/// `/home/builder/.claude/projects/` (nonexistent) and emitted no events.
fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/home/builder"))
}

// ── Top-level session registry ──────────────────────────────────────────────

/// Tracks one subagent JSONL stream owned by a parent session.
struct SubagentTailer {
    tailer: SessionJsonlTailer,
    tag: EventSource,
}

/// Holds one [`SessionJsonlTailer`] per *top-level* Claude session and drives
/// them from [`ClaudeStateUpdate`]s. Also discovers and tails subagent
/// (sidechain) JSONLs nested under each parent session's directory.
///
/// Subagents are tracked separately and emitted with
/// [`EventSource::Subagent`] tags so consumers can rebuild the parent/child
/// topology.
pub struct ClaudeJsonlRegistry {
    tailers: HashMap<String, SessionJsonlTailer>,
    /// Subagents grouped by parent session id, keyed by agent id.
    subagents: HashMap<String, HashMap<String, SubagentTailer>>,
    /// Maps state-file path → session_id so `Removed { path }` updates can
    /// drop the right tailer.
    path_to_session: HashMap<PathBuf, String>,
    /// For tests / overrides: explicit subagents directories per parent
    /// session, used in place of the standard `~/.claude/projects/...` lookup.
    subagent_dir_overrides: HashMap<String, PathBuf>,
    event_tx: Sender<TaggedAgentEvent>,
    event_rx: Option<mpsc::Receiver<TaggedAgentEvent>>,
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
            subagents: HashMap::new(),
            path_to_session: HashMap::new(),
            subagent_dir_overrides: HashMap::new(),
            event_tx,
            event_rx: Some(event_rx),
        }
    }

    /// Take the receiver half of the agent-event channel. Can only be called
    /// once — returns `None` on subsequent calls.
    pub fn events(&mut self) -> Option<mpsc::Receiver<TaggedAgentEvent>> {
        self.event_rx.take()
    }

    /// Override the subagents directory for a given parent session. Used by
    /// tests to point at a tempdir; in production the discovery walks
    /// `~/.claude/projects/{hash}/{parent-sid}/subagents/` instead.
    pub fn set_subagent_dir_override(&mut self, parent_session_id: &str, dir: PathBuf) {
        self.subagent_dir_overrides
            .insert(parent_session_id.to_string(), dir);
    }

    /// Total number of currently-tracked subagent tailers across all parents.
    pub fn subagent_count(&self) -> usize {
        self.subagents.values().map(|m| m.len()).sum()
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
                    // Drop any subagent tailers spawned under this parent.
                    if let Some(children) = self.subagents.remove(&sid) {
                        debug!(
                            session_id = %sid,
                            subagents = children.len(),
                            "JSONL registry: dropped subagent tailers with parent"
                        );
                    }
                    self.subagent_dir_overrides.remove(&sid);
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
        if !is_valid_session_id(&state.session_id) {
            warn!(session_id = %state.session_id, "JSONL registry: rejecting invalid session_id");
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
        for sid in &session_ids {
            if let Some(tailer) = self.tailers.get_mut(sid) {
                let tag = EventSource::TopLevel {
                    session_id: sid.clone(),
                };
                tailer.poll(Some(sid), &self.event_tx, &tag);
            }
        }

        // Discover any new subagent JSONLs for each parent and spawn tailers.
        for sid in &session_ids {
            self.discover_subagents(sid);
        }

        // Drive existing subagent tailers.
        let parent_ids: Vec<String> = self.subagents.keys().cloned().collect();
        for parent in parent_ids {
            if let Some(children) = self.subagents.get_mut(&parent) {
                for sub in children.values_mut() {
                    sub.tailer.poll_path(&self.event_tx, &sub.tag);
                }
            }
        }
    }

    /// Scan the subagents directory for a parent session and spawn tailers
    /// for any newly-appeared `agent-*.jsonl` files. No-op if the directory
    /// doesn't exist (the parent may not have spawned any sidechains).
    fn discover_subagents(&mut self, parent_sid: &str) {
        let dirs = self.subagent_dirs_for_parent(parent_sid);
        if dirs.is_empty() {
            return;
        }

        let known: std::collections::HashSet<String> = self
            .subagents
            .get(parent_sid)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();

        let mut new_entries: Vec<(String, PathBuf)> = Vec::new();
        for dir in &dirs {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.starts_with("agent-") || !name.ends_with(".jsonl") {
                    continue;
                }
                let agent_id = name
                    .trim_start_matches("agent-")
                    .trim_end_matches(".jsonl")
                    .to_string();
                if agent_id.is_empty() || known.contains(&agent_id) {
                    continue;
                }
                new_entries.push((agent_id, path));
            }
        }

        if new_entries.is_empty() {
            return;
        }

        let bucket = self.subagents.entry(parent_sid.to_string()).or_default();
        for (agent_id, path) in new_entries {
            debug!(
                parent = %parent_sid,
                agent_id = %agent_id,
                path = %path.display(),
                "JSONL registry: spawning subagent tailer"
            );
            let tag = EventSource::Subagent {
                parent_session_id: parent_sid.to_string(),
                agent_id: agent_id.clone(),
            };
            let tailer = SessionJsonlTailer::for_path(agent_id.clone(), path);
            bucket.insert(agent_id, SubagentTailer { tailer, tag });
        }
    }

    /// Resolve all candidate `subagents/` directories for a parent session.
    /// In tests an explicit override may be set; otherwise we walk every
    /// project hash dir under `~/.claude/projects/` and probe for
    /// `{hash}/{parent_sid}/subagents`.
    fn subagent_dirs_for_parent(&self, parent_sid: &str) -> Vec<PathBuf> {
        if let Some(dir) = self.subagent_dir_overrides.get(parent_sid) {
            return vec![dir.clone()];
        }
        if !is_valid_session_id(parent_sid) {
            warn!(parent_sid = %parent_sid, "JSONL registry: rejecting invalid parent_sid for subagent dir lookup");
            return Vec::new();
        }

        let projects_dir = home_dir().join(".claude").join("projects");
        let Ok(entries) = std::fs::read_dir(&projects_dir) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for entry in entries.flatten() {
            let project_dir = entry.path();
            if !project_dir.is_dir() {
                continue;
            }
            let candidate = project_dir.join(parent_sid).join("subagents");
            if candidate.is_dir() {
                out.push(candidate);
            }
        }
        out
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
        let tag = EventSource::TopLevel {
            session_id: "old".into(),
        };
        tailer.poll(None, &tx, &tag);
        assert!(tailer.current_session_id.is_none());
    }

    #[test]
    fn session_event_conversion_user() {
        let se = session_log::SessionEvent {
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
        let se = session_log::SessionEvent {
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
        let se = session_log::SessionEvent {
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
        let se = session_log::SessionEvent {
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

        let tag = EventSource::TopLevel {
            session_id: "test".into(),
        };
        tailer.read_new_lines(&path, &tx, &tag);

        let event = rx.try_recv().unwrap();
        assert_eq!(
            event.event,
            AgentEvent::UserMessage {
                content: "new message".into()
            }
        );
        assert_eq!(event.source, tag);

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tailer_handles_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crlf.jsonl");

        {
            let mut f = File::create(&path).unwrap();
            let line1 =
                b"{\"type\":\"user\",\"content\":\"a\",\"timestamp\":\"2026-04-02T00:00:00Z\"}\r\n";
            let line2 =
                b"{\"type\":\"user\",\"content\":\"b\",\"timestamp\":\"2026-04-02T00:00:01Z\"}\r\n";
            f.write_all(line1).unwrap();
            f.write_all(line2).unwrap();
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let mut tailer = SessionJsonlTailer::new();
        tailer.current_session_id = Some("test".into());
        tailer.read_offset = 0;
        tailer.current_path = Some(path.clone());

        let tag = EventSource::TopLevel {
            session_id: "test".into(),
        };
        tailer.read_new_lines(&path, &tx, &tag);

        let _ = rx.try_recv().unwrap();
        let _ = rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err());

        let file_len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            tailer.read_offset, file_len,
            "CRLF offset must equal full file length"
        );

        tailer.read_new_lines(&path, &tx, &tag);
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

        let tag = EventSource::TopLevel {
            session_id: "test".into(),
        };
        tailer.read_new_lines(&path, &tx, &tag);

        let ev1 = rx.try_recv().unwrap();
        assert_eq!(
            ev1.event,
            AgentEvent::AssistantMessage {
                content: "Let me check.".into()
            }
        );

        let ev2 = rx.try_recv().unwrap().event;
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
            event.event,
            AgentEvent::UserMessage {
                content: "hi from registry".into()
            }
        );
        assert_eq!(
            event.source,
            EventSource::TopLevel {
                session_id: "top-1".into()
            }
        );
    }

    #[test]
    fn registry_filters_out_subagents_from_state_poller() {
        // Subagent state-file entries are still skipped by the top-level
        // registry — subagents are discovered via the JSONL filesystem layout
        // (the `subagents/` directory) instead of via state files.
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

    // ── Subagent discovery tests ────────────────────────────────────────

    fn install_parent_with_subagent_dir(
        registry: &mut ClaudeJsonlRegistry,
        parent_sid: &str,
        sub_dir: &Path,
        state_path: &Path,
    ) {
        let state = ClaudeSessionState {
            session_id: parent_sid.into(),
            parent_session_id: None,
            ..ClaudeSessionState::default()
        };
        registry.apply_update(
            &ClaudeStateUpdate::Upserted(Box::new(state)),
            Some(state_path),
        );
        registry.set_subagent_dir_override(parent_sid, sub_dir.to_path_buf());
    }

    #[test]
    fn registry_discovers_subagent_jsonls_and_tags_events() {
        let dir = tempfile::tempdir().unwrap();
        let sub_dir = dir.path().join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();

        let agent_id = "a2834e759e4f02c68";
        let sub_path = sub_dir.join(format!("agent-{agent_id}.jsonl"));
        {
            let mut f = File::create(&sub_path).unwrap();
            writeln!(
                f,
                r#"{{"type":"user","content":"hello from subagent","timestamp":"2026-04-06T00:00:00Z"}}"#
            )
            .unwrap();
        }

        let mut registry = ClaudeJsonlRegistry::new();
        let rx = registry.events().expect("events receiver");

        let state_path = PathBuf::from("/tmp/claude-code-state/parent-xyz.json");
        install_parent_with_subagent_dir(&mut registry, "parent-xyz", &sub_dir, &state_path);

        // First poll discovers + reads the subagent file.
        registry.poll_all();

        // Drain events; we should see at least one Subagent-tagged event.
        let mut found = false;
        while let Ok(ev) = rx.try_recv() {
            if let EventSource::Subagent {
                parent_session_id,
                agent_id: aid,
            } = &ev.source
            {
                assert_eq!(parent_session_id, "parent-xyz");
                assert_eq!(aid, agent_id);
                assert_eq!(
                    ev.event,
                    AgentEvent::UserMessage {
                        content: "hello from subagent".into()
                    }
                );
                found = true;
            }
        }
        assert!(found, "expected a Subagent-tagged event");
        assert_eq!(registry.subagent_count(), 1);
    }

    #[test]
    fn registry_drops_subagent_tailers_when_parent_removed() {
        let dir = tempfile::tempdir().unwrap();
        let sub_dir = dir.path().join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub_path = sub_dir.join("agent-deadbeef.jsonl");
        File::create(&sub_path).unwrap();

        let mut registry = ClaudeJsonlRegistry::new();
        let _rx = registry.events();
        let state_path = PathBuf::from("/tmp/claude-code-state/parent-rm.json");
        install_parent_with_subagent_dir(&mut registry, "parent-rm", &sub_dir, &state_path);

        registry.poll_all();
        assert_eq!(registry.subagent_count(), 1);

        registry.apply_update(
            &ClaudeStateUpdate::Removed {
                path: state_path.clone(),
            },
            None,
        );
        assert!(registry.is_empty());
        assert_eq!(
            registry.subagent_count(),
            0,
            "subagent tailers should be cleaned up with parent"
        );
    }

    #[test]
    fn registry_does_not_double_spawn_subagents() {
        let dir = tempfile::tempdir().unwrap();
        let sub_dir = dir.path().join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let sub_path = sub_dir.join("agent-stable.jsonl");
        File::create(&sub_path).unwrap();

        let mut registry = ClaudeJsonlRegistry::new();
        let _rx = registry.events();
        install_parent_with_subagent_dir(
            &mut registry,
            "parent-stable",
            &sub_dir,
            Path::new("/tmp/claude-code-state/parent-stable.json"),
        );

        registry.poll_all();
        registry.poll_all();
        registry.poll_all();
        assert_eq!(registry.subagent_count(), 1);
    }
}
