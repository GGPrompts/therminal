//! ClaudeStatePoller — monitors `/tmp/claude-code-state/`, `/tmp/codex-state/`,
//! and `/tmp/copilot-state/` for agent session state files using the `notify`
//! crate.
//!
//! Ported from thermal-desktop (`thermal-core::claude_state`). Claude Code and
//! related agent hooks write a JSON state file per session to one of the
//! directories above at runtime; this module watches those directories and
//! exposes a cached snapshot of current sessions.
//!
//! # Integration
//!
//! TODO: wire `ClaudeStatePoller` updates into the daemon's event bus so MCP
//! clients can subscribe to agent session state changes. For now, poller
//! updates are exposed via [`ClaudeStatePoller::updates`] as an
//! [`std::sync::mpsc`] channel; a future task will hook this into
//! `DaemonEvent`.

use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Result as NotifyResult, Watcher,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{debug, trace, warn};

/// The directory where Claude Code state JSON files are written.
const CLAUDE_STATE_DIR: &str = "/tmp/claude-code-state";

/// The directory where Codex state JSON files are written (via adapter script).
const CODEX_STATE_DIR: &str = "/tmp/codex-state";

/// The directory where Copilot state JSON files are written (via hook script).
const COPILOT_STATE_DIR: &str = "/tmp/copilot-state";

/// Sessions older than this without a live PID are considered dead.
const SESSION_MAX_AGE: time::Duration = time::Duration::hours(2);

/// Grace period before a session with a dead PID is considered dead.
const RECENT_UPDATE_GRACE: time::Duration = time::Duration::seconds(120);

/// How often to run the PID liveness + staleness sweep (avoid syscall spam).
const PRUNE_INTERVAL: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Types (ported from thermal-desktop `ggl_types`)
// ---------------------------------------------------------------------------

/// Status of an agent session.
///
/// Variants must stay in sync with `InferredStatus` in
/// `therminal-terminal::state_inference::types` — that engine is the writer
/// for `daemon-pane-*.json` state files. If a new status is added there it
/// must be added here too, otherwise the poller rejects its own output with
/// an "unknown variant" serde error. See tn-hcq9.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeStatus {
    #[default]
    Idle,
    Processing,
    Streaming,
    Thinking,
    ToolUse,
    AwaitingInput,
}

/// Structured arguments captured for a currently-executing tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Structured details about the current tool invocation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<ToolArgs>,
}

/// State of a single agent session, deserialized from a JSON state file.
///
/// Ported from `thermal-protocol::SessionStateV1`. All optional fields use
/// `#[serde(default)]` so older/newer writers missing fields don't fail.
///
/// # `session_id` vs `claude_session_id`
///
/// The widely-used community `state-tracker.sh` hook writes *both*
/// `session_id` (a 12-char md5(pwd) stable-per-workdir key) and
/// `claude_session_id` (the real Claude Code session UUID that matches the
/// JSONL file stem under `~/.claude/projects/`). Therminal needs the UUID
/// to locate JSONLs and discover subagents, so the custom deserializer
/// below prefers `claude_session_id` when present and falls back to
/// `session_id` for older writers that only populate the one field.
/// See tn-r2a3.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ClaudeSessionStateRepr")]
pub struct ClaudeSessionState {
    #[serde(default)]
    pub session_id: String,
    /// Subagent parent linkage — Claude Code subagents carry the spawning
    /// session's id here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub status: ClaudeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    #[serde(
        default = "default_subagent_count",
        skip_serializing_if = "Option::is_none"
    )]
    pub subagent_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ToolDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_command_duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<u32>,
}

fn default_subagent_count() -> Option<u32> {
    Some(0)
}

/// Private staging struct that mirrors [`ClaudeSessionState`] but also
/// accepts a `claude_session_id` field. The [`From`] impl below picks
/// `claude_session_id` over `session_id` when both are present, so writers
/// like the community `state-tracker.sh` (which stash the UUID in
/// `claude_session_id` and an md5(pwd) stable key in `session_id`) work
/// out of the box. See tn-r2a3.
#[derive(Deserialize)]
struct ClaudeSessionStateRepr {
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    claude_session_id: Option<String>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    status: ClaudeStatus,
    #[serde(default)]
    current_tool: Option<String>,
    #[serde(default = "default_subagent_count")]
    subagent_count: Option<u32>,
    #[serde(default)]
    context_percent: Option<f64>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    last_updated: Option<String>,
    #[serde(default)]
    details: Option<ToolDetails>,
    #[serde(default)]
    hook_type: Option<String>,
    #[serde(default)]
    tmux_pane: Option<String>,
    #[serde(default)]
    pid: Option<i64>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    last_command: Option<String>,
    #[serde(default)]
    last_exit_code: Option<i32>,
    #[serde(default)]
    last_command_started_at: Option<String>,
    #[serde(default)]
    last_command_duration_ms: Option<u64>,
    #[serde(default)]
    consecutive_failures: Option<u32>,
}

impl From<ClaudeSessionStateRepr> for ClaudeSessionState {
    fn from(r: ClaudeSessionStateRepr) -> Self {
        // Prefer `claude_session_id` (real Claude Code UUID) over `session_id`
        // (which some writers use for a stable-per-workdir key instead).
        let session_id = r
            .claude_session_id
            .filter(|s| !s.is_empty())
            .unwrap_or(r.session_id);
        Self {
            session_id,
            parent_session_id: r.parent_session_id,
            agent_id: r.agent_id,
            agent_type: r.agent_type,
            model: r.model,
            status: r.status,
            current_tool: r.current_tool,
            subagent_count: r.subagent_count,
            context_percent: r.context_percent,
            working_dir: r.working_dir,
            last_updated: r.last_updated,
            details: r.details,
            hook_type: r.hook_type,
            tmux_pane: r.tmux_pane,
            pid: r.pid,
            workspace: r.workspace,
            source: r.source,
            last_command: r.last_command,
            last_exit_code: r.last_exit_code,
            last_command_started_at: r.last_command_started_at,
            last_command_duration_ms: r.last_command_duration_ms,
            consecutive_failures: r.consecutive_failures,
        }
    }
}

impl Default for ClaudeSessionState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            parent_session_id: None,
            agent_id: None,
            agent_type: None,
            model: None,
            status: ClaudeStatus::Idle,
            current_tool: None,
            subagent_count: Some(0),
            context_percent: None,
            working_dir: None,
            last_updated: None,
            details: None,
            hook_type: None,
            tmux_pane: None,
            pid: None,
            workspace: None,
            source: None,
            last_command: None,
            last_exit_code: None,
            last_command_started_at: None,
            last_command_duration_ms: None,
            consecutive_failures: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Update events
// ---------------------------------------------------------------------------

/// A single change emitted by [`ClaudeStatePoller`].
#[derive(Debug, Clone)]
pub enum ClaudeStateUpdate {
    /// A new state file appeared, or an existing file was modified. Boxed
    /// to keep the enum compact (`ClaudeSessionState` is ~560 bytes).
    Upserted(Box<ClaudeSessionState>),
    /// A state file was removed (or its session pruned as dead).
    Removed { path: PathBuf },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Infer the `agent_type` string from a state file's parent directory.
fn agent_type_for_path(path: &Path) -> Option<String> {
    let parent = path.parent()?.to_str()?;
    if parent.contains("copilot-state") {
        Some("copilot".to_string())
    } else if parent.contains("codex-state") {
        Some("codex".to_string())
    } else {
        Some("claude".to_string())
    }
}

fn status_priority(status: &ClaudeStatus) -> u8 {
    match status {
        ClaudeStatus::ToolUse => 3,
        ClaudeStatus::Processing | ClaudeStatus::Streaming | ClaudeStatus::Thinking => 2,
        ClaudeStatus::AwaitingInput => 1,
        ClaudeStatus::Idle => 0,
    }
}

fn is_hook_sourced(state: &ClaudeSessionState) -> bool {
    state.source.as_deref() == Some("hook")
}

fn state_supersedes(candidate: &ClaudeSessionState, current: &ClaudeSessionState) -> bool {
    let candidate_hook = is_hook_sourced(candidate);
    let current_hook = is_hook_sourced(current);
    if candidate_hook != current_hook {
        return candidate_hook;
    }

    let candidate_updated = candidate.last_updated.as_deref().unwrap_or("");
    let current_updated = current.last_updated.as_deref().unwrap_or("");

    if candidate_updated != current_updated {
        return candidate_updated > current_updated;
    }

    let candidate_priority = status_priority(&candidate.status);
    let current_priority = status_priority(&current.status);
    if candidate_priority != current_priority {
        return candidate_priority > current_priority;
    }

    let candidate_detail_score = [
        candidate.current_tool.is_some(),
        candidate.details.is_some(),
        candidate.working_dir.is_some(),
        candidate.pid.is_some(),
    ]
    .into_iter()
    .filter(|p| *p)
    .count();
    let current_detail_score = [
        current.current_tool.is_some(),
        current.details.is_some(),
        current.working_dir.is_some(),
        current.pid.is_some(),
    ]
    .into_iter()
    .filter(|p| *p)
    .count();

    candidate_detail_score > current_detail_score
}

fn collapse_sessions_by_id(
    states: impl IntoIterator<Item = ClaudeSessionState>,
) -> Vec<ClaudeSessionState> {
    let mut by_id: HashMap<String, ClaudeSessionState> = HashMap::new();
    let mut anonymous = Vec::new();

    for state in states {
        if state.session_id.is_empty() {
            anonymous.push(state);
            continue;
        }

        match by_id.entry(state.session_id.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(state);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if state_supersedes(&state, entry.get()) {
                    entry.insert(state);
                }
            }
        }
    }

    let mut collapsed: Vec<_> = by_id.into_values().collect();
    collapsed.extend(anonymous);
    collapsed.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    collapsed
}

/// Check if a process with the given PID is still alive.
#[cfg(unix)]
fn pid_is_alive(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // kill(pid, 0) checks existence without sending a signal.
    // Returns 0 on success; -1 + errno=ESRCH means no such process.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i64) -> bool {
    // On non-Unix platforms we can't cheaply probe; assume alive and rely on
    // timestamp-based staleness.
    true
}

fn session_is_dead(state: &ClaudeSessionState) -> bool {
    // Grace period: if the state was updated recently, trust it regardless of PID.
    if let Some(last_updated) = state.last_updated.as_deref()
        && let Ok(updated_at) = OffsetDateTime::parse(last_updated, &Rfc3339)
    {
        let now = OffsetDateTime::now_utc();
        if (now - updated_at) < RECENT_UPDATE_GRACE {
            trace!(session_id = %state.session_id, "session in grace period, skipping dead check");
            return false;
        }
    }

    if let Some(pid) = state.pid {
        if pid > 0 && !pid_is_alive(pid) {
            debug!(session_id = %state.session_id, pid, "session dead: PID not alive");
            return true;
        }
        return false;
    }

    let Some(last_updated) = state.last_updated.as_deref() else {
        return false;
    };
    let Ok(updated_at) = OffsetDateTime::parse(last_updated, &Rfc3339) else {
        return false;
    };
    let dead = (OffsetDateTime::now_utc() - updated_at) > SESSION_MAX_AGE;
    if dead {
        debug!(session_id = %state.session_id, last_updated, "session dead: no PID and timestamp is stale");
    }
    dead
}

/// Result of a low-level state file read attempt. Distinguishes "parsed
/// successfully" (the caller still needs to check liveness) from "couldn't
/// even parse" so the warn-once tracking only fires on genuinely broken
/// files. The `Ok` variant is boxed because `ClaudeSessionState` is ~560
/// bytes — same reason `ClaudeStateUpdate::Upserted` boxes its payload.
enum ReadFileOutcome {
    Ok(Box<ClaudeSessionState>),
    Bad,
}

// ---------------------------------------------------------------------------
// Poller
// ---------------------------------------------------------------------------

/// Watches agent state directories for session file changes.
///
/// Uses the `notify` crate's recommended (OS-native) watcher. Call
/// [`ClaudeStatePoller::poll`] regularly to drain events, re-read changed
/// files, and receive the current session snapshot. Incremental updates are
/// also pushed onto a `std::sync::mpsc` channel accessible via
/// [`ClaudeStatePoller::updates`].
pub struct ClaudeStatePoller {
    _watchers: Vec<RecommendedWatcher>,
    rx: mpsc::Receiver<NotifyResult<Event>>,
    state_dirs: Vec<PathBuf>,
    sessions: HashMap<PathBuf, ClaudeSessionState>,
    last_prune: Instant,
    update_tx: mpsc::Sender<ClaudeStateUpdate>,
    update_rx: Option<mpsc::Receiver<ClaudeStateUpdate>>,
    /// Paths to state files we've already failed to read or parse. We log
    /// the failure once and silently skip on subsequent notify-event re-reads
    /// instead of spamming `warn!` every time the file mtime ticks. Cleared
    /// when a file is removed (so a re-created file gets a fresh chance).
    known_bad_files: HashSet<PathBuf>,
}

impl ClaudeStatePoller {
    /// Create a new poller watching Claude, Codex, and Copilot state
    /// directories. Creates the directories if they do not exist.
    pub fn new() -> NotifyResult<Self> {
        let dirs = vec![
            PathBuf::from(CLAUDE_STATE_DIR),
            PathBuf::from(CODEX_STATE_DIR),
            PathBuf::from(COPILOT_STATE_DIR),
        ];
        Self::with_dirs(dirs)
    }

    /// Create a poller watching an explicit list of directories. Used by
    /// tests to point the watcher at a temporary directory.
    pub fn with_dirs(dirs: Vec<PathBuf>) -> NotifyResult<Self> {
        for dir in &dirs {
            if !dir.exists() {
                let _ = std::fs::create_dir_all(dir);
            }
        }

        let (tx, rx) = mpsc::channel();
        let mut watchers = Vec::new();

        for dir in &dirs {
            let tx_clone = tx.clone();
            let mut watcher = notify::recommended_watcher(tx_clone)?;
            watcher.watch(dir, RecursiveMode::NonRecursive)?;
            watchers.push(watcher);
        }

        // Initial read: parse every JSON file in each watched dir, then
        // filter out dead-pid sessions BEFORE handing the snapshot to the
        // poller. Stale dead-pid files (and files that fail to parse) get
        // deleted from disk so notify Modify events can't keep re-firing on
        // yesterday's leftovers. This was previously deferred until the first
        // 30s `prune_dead_sessions` tick, which left a window where every
        // notify event re-read the same junk files and re-warned. (tn-qfi0)
        let mut sessions = HashMap::new();
        let mut known_bad_files: HashSet<PathBuf> = HashSet::new();
        for dir in &dirs {
            let (alive, dead, bad) = Self::scan_dir_at_boot(dir);
            sessions.extend(alive);
            for path in dead {
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!(path = %path.display(), error = %e, "failed to remove dead state file at boot");
                } else {
                    debug!(path = %path.display(), "removed dead state file at boot");
                }
            }
            // We *could* delete unparseable files too, but they may belong to
            // a future schema version or a different consumer. Just remember
            // them so notify event re-reads don't re-warn.
            known_bad_files.extend(bad);
        }

        let (update_tx, update_rx) = mpsc::channel();

        Ok(Self {
            _watchers: watchers,
            rx,
            state_dirs: dirs,
            sessions,
            last_prune: Instant::now(),
            update_tx,
            update_rx: Some(update_rx),
            known_bad_files,
        })
    }

    /// Scan one watched directory at boot. Returns
    /// `(alive_sessions, dead_paths, bad_paths)` where:
    /// - `alive_sessions` are state files that parsed AND have a live PID
    ///   (or are still inside the recent-update grace window),
    /// - `dead_paths` parsed but failed `session_is_dead`, so the caller can
    ///   delete them from disk,
    /// - `bad_paths` failed to read or parse — caller adds them to
    ///   `known_bad_files` so subsequent notify events skip the warn.
    fn scan_dir_at_boot(
        dir: &Path,
    ) -> (
        HashMap<PathBuf, ClaudeSessionState>,
        Vec<PathBuf>,
        Vec<PathBuf>,
    ) {
        let mut alive = HashMap::new();
        let mut dead = Vec::new();
        let mut bad = Vec::new();

        let Ok(entries) = std::fs::read_dir(dir) else {
            return (alive, dead, bad);
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !Self::is_json(&path) {
                continue;
            }
            match Self::read_file_raw(&path) {
                ReadFileOutcome::Ok(state) => {
                    if session_is_dead(&state) {
                        debug!(
                            session_id = %state.session_id,
                            path = %path.display(),
                            "boot scan: session dead, will remove file"
                        );
                        dead.push(path);
                    } else {
                        alive.insert(path, *state);
                    }
                }
                ReadFileOutcome::Bad => {
                    bad.push(path);
                }
            }
        }

        (alive, dead, bad)
    }

    /// Take the receiver half of the update channel. Can only be called once
    /// per poller — returns `None` on subsequent calls.
    pub fn updates(&mut self) -> Option<mpsc::Receiver<ClaudeStateUpdate>> {
        self.update_rx.take()
    }

    /// Drain pending file-change events, re-read changed JSON files, and
    /// return the current list of all sessions (deduped by session_id).
    pub fn poll(&mut self) -> Vec<ClaudeSessionState> {
        let mut dirty_paths: Vec<PathBuf> = Vec::new();
        let mut removed_paths: Vec<PathBuf> = Vec::new();
        let mut event_count: usize = 0;

        while let Ok(result) = self.rx.try_recv() {
            event_count += 1;
            match result {
                Ok(event) => match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) => {
                        for path in &event.paths {
                            if Self::is_json(path) && !dirty_paths.contains(path) {
                                dirty_paths.push(path.clone());
                            }
                        }
                    }
                    EventKind::Remove(_) => {
                        for path in &event.paths {
                            if Self::is_json(path) {
                                removed_paths.push(path.clone());
                            }
                        }
                    }
                    _ => {}
                },
                Err(e) => {
                    warn!(error = %e, "file watcher error");
                }
            }
        }

        if event_count > 0 {
            debug!(
                events = event_count,
                dirty = dirty_paths.len(),
                removed = removed_paths.len(),
                "poll drain batch"
            );
        }

        for path in &removed_paths {
            // Clear any warn-once flag — if the file reappears we want a
            // fresh chance to parse and report.
            self.known_bad_files.remove(path);
            if self.sessions.remove(path).is_some() {
                let _ = self
                    .update_tx
                    .send(ClaudeStateUpdate::Removed { path: path.clone() });
            }
        }

        for path in &dirty_paths {
            if let Some(state) = self.read_file_tracked(path) {
                let _ = self
                    .update_tx
                    .send(ClaudeStateUpdate::Upserted(Box::new(state.clone())));
                self.sessions.insert(path.clone(), state);
            }
        }

        if self.last_prune.elapsed() >= PRUNE_INTERVAL {
            self.prune_dead_sessions();
            self.last_prune = Instant::now();
        }

        collapse_sessions_by_id(self.sessions.values().cloned())
    }

    /// Re-read all files on disk and return a full snapshot.
    pub fn get_all(&self) -> Vec<ClaudeSessionState> {
        let mut all = HashMap::new();
        for dir in &self.state_dirs {
            all.extend(Self::read_all_files(dir));
        }
        collapse_sessions_by_id(all.into_values())
    }

    fn read_all_files(dir: &Path) -> HashMap<PathBuf, ClaudeSessionState> {
        let mut map = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if Self::is_json(&path)
                    && let Some(state) = Self::read_file(&path)
                {
                    map.insert(path, state);
                }
            }
        }
        map
    }

    fn read_file(path: &Path) -> Option<ClaudeSessionState> {
        match Self::read_file_raw(path) {
            ReadFileOutcome::Ok(state) => {
                if session_is_dead(&state) {
                    None
                } else {
                    trace!(
                        session_id = %state.session_id,
                        status = ?state.status,
                        path = %path.display(),
                        "read state file"
                    );
                    Some(*state)
                }
            }
            ReadFileOutcome::Bad => None,
        }
    }

    /// Lower-level read that distinguishes "parsed but dead" from
    /// "unparseable". Used by boot-time scan and the warn-once notify path.
    /// Logs `warn!` on read/parse failure exactly once per call — callers
    /// MUST gate against `known_bad_files` to avoid spam.
    fn read_file_raw(path: &Path) -> ReadFileOutcome {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to read state file");
                return ReadFileOutcome::Bad;
            }
        };
        let mut state: ClaudeSessionState = match serde_json::from_str(&data) {
            Ok(s) => s,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to parse state file JSON");
                return ReadFileOutcome::Bad;
            }
        };
        if state.agent_type.is_none() {
            state.agent_type = agent_type_for_path(path);
        }
        ReadFileOutcome::Ok(Box::new(state))
    }

    /// Notify-path read that consults `known_bad_files` to suppress repeat
    /// warnings on the same broken/unparseable file. Returns:
    /// - `Some(state)` if the file parsed and the session is alive,
    /// - `None` if the file is dead, broken, or already known-bad.
    fn read_file_tracked(&mut self, path: &Path) -> Option<ClaudeSessionState> {
        if self.known_bad_files.contains(path) {
            trace!(path = %path.display(), "skipping known-bad state file (warn-once)");
            return None;
        }
        match Self::read_file_raw(path) {
            ReadFileOutcome::Ok(state) => {
                if session_is_dead(&state) {
                    None
                } else {
                    trace!(
                        session_id = %state.session_id,
                        status = ?state.status,
                        path = %path.display(),
                        "read state file"
                    );
                    Some(*state)
                }
            }
            ReadFileOutcome::Bad => {
                // First failure for this path was already warned by
                // read_file_raw. Remember it so future notify events stay
                // quiet until the file is removed and re-created.
                self.known_bad_files.insert(path.to_path_buf());
                None
            }
        }
    }

    fn is_json(path: &Path) -> bool {
        path.extension().is_some_and(|ext| ext == "json")
    }

    fn prune_dead_sessions(&mut self) {
        let dead_paths: Vec<PathBuf> = self
            .sessions
            .iter()
            .filter(|(_, state)| session_is_dead(state))
            .map(|(path, _)| path.clone())
            .collect();

        for path in dead_paths {
            let session_id = self
                .sessions
                .get(&path)
                .map(|s| s.session_id.as_str())
                .unwrap_or("?")
                .to_string();
            debug!(session_id = %session_id, path = %path.display(), "pruning dead session");
            self.sessions.remove(&path);
            self.known_bad_files.remove(&path);
            let _ = self
                .update_tx
                .send(ClaudeStateUpdate::Removed { path: path.clone() });
            if let Err(e) = std::fs::remove_file(&path) {
                warn!(path = %path.display(), error = %e, "failed to remove dead state file");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> ClaudeSessionState {
        serde_json::from_str(json).expect("JSON should parse")
    }

    #[test]
    fn status_idle_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"idle\"").unwrap();
        assert_eq!(s, ClaudeStatus::Idle);
    }

    #[test]
    fn status_processing_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"processing\"").unwrap();
        assert_eq!(s, ClaudeStatus::Processing);
    }

    #[test]
    fn status_tool_use_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"tool_use\"").unwrap();
        assert_eq!(s, ClaudeStatus::ToolUse);
    }

    #[test]
    fn status_awaiting_input_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"awaiting_input\"").unwrap();
        assert_eq!(s, ClaudeStatus::AwaitingInput);
    }

    // Regression for tn-hcq9: the terminal-side state inference engine
    // emits "streaming" and "thinking" statuses when a pane is identified as
    // an agent session. Before this fix the daemon poller rejected those
    // files with `unknown variant \`streaming\`` and the warnings polluted
    // every read. These two tests lock the variants in; removing them would
    // resurrect the regression.
    #[test]
    fn status_streaming_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"streaming\"").unwrap();
        assert_eq!(s, ClaudeStatus::Streaming);
    }

    #[test]
    fn status_thinking_deserializes() {
        let s: ClaudeStatus = serde_json::from_str("\"thinking\"").unwrap();
        assert_eq!(s, ClaudeStatus::Thinking);
    }

    #[test]
    fn status_unknown_string_fails() {
        let result: Result<ClaudeStatus, _> = serde_json::from_str("\"unknown_variant\"");
        assert!(result.is_err());
    }

    #[test]
    fn status_default_is_idle() {
        assert_eq!(ClaudeStatus::default(), ClaudeStatus::Idle);
    }

    #[test]
    fn session_full_deserializes() {
        let json = r#"{
            "session_id": "abc-123",
            "status": "processing",
            "current_tool": "Bash",
            "subagent_count": 2,
            "context_percent": 42.5,
            "working_dir": "/home/user/project",
            "last_updated": "2026-03-16T12:00:00Z",
            "hook_type": "pre_tool",
            "tmux_pane": "%1",
            "pid": 9876
        }"#;
        let s = parse(json);
        assert_eq!(s.session_id, "abc-123");
        assert_eq!(s.status, ClaudeStatus::Processing);
        assert_eq!(s.current_tool.as_deref(), Some("Bash"));
        assert_eq!(s.subagent_count, Some(2));
        assert!((s.context_percent.unwrap() - 42.5).abs() < 1e-5);
        assert_eq!(s.working_dir.as_deref(), Some("/home/user/project"));
        assert_eq!(s.hook_type.as_deref(), Some("pre_tool"));
        assert_eq!(s.tmux_pane.as_deref(), Some("%1"));
        assert_eq!(s.pid, Some(9876));
    }

    #[test]
    fn session_minimal_uses_defaults() {
        let json = r#"{"session_id": "min-session"}"#;
        let s = parse(json);
        assert_eq!(s.session_id, "min-session");
        assert_eq!(s.status, ClaudeStatus::Idle);
        assert!(s.current_tool.is_none());
        assert!(s.context_percent.is_none());
        assert!(s.working_dir.is_none());
    }

    #[test]
    fn session_empty_object_uses_defaults() {
        let s: ClaudeSessionState = serde_json::from_str("{}").unwrap();
        assert_eq!(s.session_id, "");
        assert_eq!(s.status, ClaudeStatus::Idle);
    }

    #[test]
    fn session_default_subagent_count() {
        let s = ClaudeSessionState::default();
        assert_eq!(s.subagent_count, Some(0));
    }

    #[test]
    fn session_parent_session_id_deserializes() {
        // Subagents carry parent_session_id — critical for agent topology.
        let json = r#"{"session_id": "child", "parent_session_id": "parent-1"}"#;
        let s = parse(json);
        assert_eq!(s.parent_session_id.as_deref(), Some("parent-1"));
    }

    #[test]
    fn session_without_parent_session_id_is_none() {
        let json = r#"{"session_id": "orphan"}"#;
        let s = parse(json);
        assert!(s.parent_session_id.is_none());
    }

    #[test]
    fn session_prefers_claude_session_id_over_session_id() {
        // state-tracker.sh schema: `session_id` is the md5(pwd) stable key,
        // `claude_session_id` is the real Claude Code UUID. We want the UUID.
        // See tn-r2a3.
        let json = r#"{
            "session_id": "946a34ea4138",
            "claude_session_id": "6498af57-2c8b-4a0c-8735-abcdef012345"
        }"#;
        let s = parse(json);
        assert_eq!(s.session_id, "6498af57-2c8b-4a0c-8735-abcdef012345");
    }

    #[test]
    fn session_falls_back_to_session_id_when_claude_session_id_absent() {
        // Backward compat: older/simpler writers only emit `session_id`
        // and put the UUID directly in it.
        let json = r#"{"session_id": "6498af57-2c8b-4a0c-8735-abcdef012345"}"#;
        let s = parse(json);
        assert_eq!(s.session_id, "6498af57-2c8b-4a0c-8735-abcdef012345");
    }

    #[test]
    fn session_ignores_empty_claude_session_id() {
        // If `claude_session_id` is an empty string, fall back to session_id
        // rather than clobbering it with "". Defensive against writers that
        // always emit the key even when they don't know the UUID yet.
        let json = r#"{
            "session_id": "fallback-uuid",
            "claude_session_id": ""
        }"#;
        let s = parse(json);
        assert_eq!(s.session_id, "fallback-uuid");
    }

    #[test]
    fn session_with_tool_details_deserializes() {
        let json = r#"{
            "session_id": "td-session",
            "status": "tool_use",
            "details": {
                "event": "tool_start",
                "tool": "Read",
                "args": {
                    "file_path": "/some/file.rs",
                    "command": null,
                    "pattern": null,
                    "description": "reading a file"
                }
            }
        }"#;
        let s = parse(json);
        assert_eq!(s.status, ClaudeStatus::ToolUse);
        let details = s.details.expect("details should be present");
        assert_eq!(details.event.as_deref(), Some("tool_start"));
        assert_eq!(details.tool.as_deref(), Some("Read"));
        let args = details.args.expect("args should be present");
        assert_eq!(args.file_path.as_deref(), Some("/some/file.rs"));
        assert_eq!(args.description.as_deref(), Some("reading a file"));
    }

    #[test]
    fn tool_args_all_none_when_omitted() {
        let json = r#"{"session_id": "x", "details": {"event": "e"}}"#;
        let s = parse(json);
        let details = s.details.unwrap();
        assert!(details.args.is_none());
    }

    #[test]
    fn tool_args_partial_fields() {
        let json = r#"{
            "session_id": "partial",
            "details": {
                "args": {"command": "ls -la"}
            }
        }"#;
        let s = parse(json);
        let args = s.details.unwrap().args.unwrap();
        assert_eq!(args.command.as_deref(), Some("ls -la"));
        assert!(args.file_path.is_none());
    }

    #[test]
    fn malformed_json_returns_error() {
        let result: Result<ClaudeSessionState, _> = serde_json::from_str("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn is_json_detects_json_extension() {
        assert!(ClaudeStatePoller::is_json(Path::new("state.json")));
        assert!(!ClaudeStatePoller::is_json(Path::new("state.toml")));
        assert!(!ClaudeStatePoller::is_json(Path::new("state")));
    }

    #[test]
    fn agent_type_claude_dir() {
        let path = Path::new("/tmp/claude-code-state/session-abc.json");
        assert_eq!(agent_type_for_path(path), Some("claude".to_string()));
    }

    #[test]
    fn agent_type_codex_dir() {
        let path = Path::new("/tmp/codex-state/session-xyz.json");
        assert_eq!(agent_type_for_path(path), Some("codex".to_string()));
    }

    #[test]
    fn agent_type_copilot_dir() {
        let path = Path::new("/tmp/copilot-state/session-abc.json");
        assert_eq!(agent_type_for_path(path), Some("copilot".to_string()));
    }

    #[test]
    fn collapse_sessions_prefers_latest_timestamp_for_same_id() {
        let older = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::Idle,
            last_updated: Some("2026-03-26T21:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        let newer = ClaudeSessionState {
            session_id: "dup".into(),
            status: ClaudeStatus::ToolUse,
            current_tool: Some("Bash".into()),
            last_updated: Some("2026-03-26T21:00:01Z".into()),
            ..ClaudeSessionState::default()
        };

        let collapsed = collapse_sessions_by_id(vec![older, newer]);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].status, ClaudeStatus::ToolUse);
    }

    #[test]
    fn stale_session_without_pid_is_dead() {
        let state = ClaudeSessionState {
            session_id: "old".into(),
            agent_type: Some("codex".into()),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(session_is_dead(&state));
    }

    #[cfg(unix)]
    #[test]
    fn session_with_dead_pid_is_dead() {
        let state = ClaudeSessionState {
            session_id: "dead-pid".into(),
            agent_type: Some("claude".into()),
            pid: Some(999_999_999),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(session_is_dead(&state));
    }

    #[cfg(unix)]
    #[test]
    fn session_with_live_pid_is_not_dead() {
        let state = ClaudeSessionState {
            session_id: "live".into(),
            agent_type: Some("claude".into()),
            pid: Some(std::process::id() as i64),
            last_updated: Some("2024-01-01T00:00:00Z".into()),
            ..ClaudeSessionState::default()
        };
        assert!(!session_is_dead(&state));
    }

    // --- End-to-end watcher test -------------------------------------------

    #[test]
    fn watcher_picks_up_new_state_file() {
        use std::thread::sleep;
        use std::time::Duration;

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("claude-code-state");
        std::fs::create_dir_all(&dir).unwrap();

        let mut poller =
            ClaudeStatePoller::with_dirs(vec![dir.clone()]).expect("poller constructs");

        // Initial snapshot should be empty.
        assert!(poller.poll().is_empty());

        // Write a live-PID state file so it survives dead-session pruning.
        let pid = std::process::id() as i64;
        let state_path = dir.join("session-x.json");
        let json = format!(
            r#"{{"session_id":"sess-x","status":"processing","pid":{},"last_updated":"2099-01-01T00:00:00Z","source":"hook"}}"#,
            pid
        );
        std::fs::write(&state_path, json).unwrap();

        // Give the watcher a beat to observe the write. Poll up to ~2s.
        let mut sessions = Vec::new();
        for _ in 0..20 {
            sleep(Duration::from_millis(100));
            sessions = poller.poll();
            if !sessions.is_empty() {
                break;
            }
        }

        assert_eq!(sessions.len(), 1, "expected one session after write");
        assert_eq!(sessions[0].session_id, "sess-x");
        assert_eq!(sessions[0].status, ClaudeStatus::Processing);
    }

    // --- tn-qfi0: boot-time pid filter + warn-once notify path -------------

    /// Boot scan must drop a state file whose PID is not alive AND remove it
    /// from disk. Previously this was deferred until the first 30s
    /// `prune_dead_sessions` tick, leaving stale files for notify events to
    /// re-trigger on.
    #[cfg(unix)]
    #[test]
    fn with_dirs_filters_dead_pid_at_boot_and_deletes_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("claude-code-state");
        std::fs::create_dir_all(&dir).unwrap();

        // Stale state file from a "previous" daemon session: dead PID,
        // very old timestamp (well outside the recent-update grace window).
        let stale_path = dir.join("session-stale.json");
        let stale_json = r#"{
            "session_id":"sess-stale",
            "status":"idle",
            "pid":999999999,
            "last_updated":"2024-01-01T00:00:00Z",
            "source":"hook"
        }"#;
        std::fs::write(&stale_path, stale_json).unwrap();

        let poller = ClaudeStatePoller::with_dirs(vec![dir.clone()]).expect("poller constructs");

        // The stale session must NOT be present in the boot snapshot…
        let snapshot: Vec<_> = poller.sessions.values().cloned().collect();
        assert!(
            snapshot.iter().all(|s| s.session_id != "sess-stale"),
            "boot snapshot should not include dead-pid session: {snapshot:?}"
        );

        // …and the on-disk file should also be gone, so notify Modify events
        // can't keep re-loading it.
        assert!(
            !stale_path.exists(),
            "boot scan should have removed stale state file at {}",
            stale_path.display()
        );
    }

    /// A genuinely live session at boot must survive the boot scan AND its
    /// state file must remain on disk.
    #[cfg(unix)]
    #[test]
    fn with_dirs_keeps_live_session_at_boot() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("claude-code-state");
        std::fs::create_dir_all(&dir).unwrap();

        let live_pid = std::process::id() as i64;
        let live_path = dir.join("session-live.json");
        let live_json = format!(
            r#"{{"session_id":"sess-live","status":"processing","pid":{live_pid},"last_updated":"2024-01-01T00:00:00Z","source":"hook"}}"#
        );
        std::fs::write(&live_path, live_json).unwrap();

        let poller = ClaudeStatePoller::with_dirs(vec![dir.clone()]).expect("poller constructs");

        let kept = poller
            .sessions
            .values()
            .any(|s| s.session_id == "sess-live");
        assert!(kept, "live-pid session should survive boot scan");
        assert!(live_path.exists(), "live session file must not be deleted");
    }

    /// An unparseable file at boot is added to `known_bad_files` and stays
    /// on disk (we don't delete unknown junk). Subsequent notify-driven
    /// re-reads via `read_file_tracked` must NOT re-warn — the file is
    /// silently skipped instead.
    #[test]
    fn boot_scan_marks_unparseable_file_as_known_bad() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("claude-code-state");
        std::fs::create_dir_all(&dir).unwrap();

        let bad_path = dir.join("garbage.json");
        std::fs::write(&bad_path, b"{ this is not valid json").unwrap();

        let mut poller =
            ClaudeStatePoller::with_dirs(vec![dir.clone()]).expect("poller constructs");

        // Boot scan should have remembered this path as bad.
        assert!(
            poller.known_bad_files.contains(&bad_path),
            "boot scan should remember unparseable files in known_bad_files"
        );

        // Garbage file is left on disk — we don't delete unknown content.
        assert!(bad_path.exists());

        // Simulate a notify Modify event re-reading the same file:
        // `read_file_tracked` must short-circuit because the path is known-bad.
        // (We can't easily assert log silence in a unit test, but we can
        // assert the function returns None and the known_bad_files set is
        // unchanged.)
        let result = poller.read_file_tracked(&bad_path);
        assert!(result.is_none());
        assert!(poller.known_bad_files.contains(&bad_path));
    }

    /// Removing a known-bad file (e.g. via notify Remove or by `prune_dead_sessions`)
    /// must clear the warn-once flag so a re-created file with the same path
    /// gets a fresh warning chance.
    #[test]
    fn read_file_tracked_clears_known_bad_on_recreate() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("claude-code-state");
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("flaky.json");
        std::fs::write(&path, b"<not json>").unwrap();

        let mut poller =
            ClaudeStatePoller::with_dirs(vec![dir.clone()]).expect("poller constructs");
        assert!(poller.known_bad_files.contains(&path));

        // Remove and re-create — simulate poll() handling a Remove event by
        // clearing the entry directly (poll() does this on EventKind::Remove).
        poller.known_bad_files.remove(&path);
        assert!(!poller.known_bad_files.contains(&path));

        // Now write a VALID state file at the same path. read_file_tracked
        // should re-parse and return Some(state).
        let live_pid = std::process::id() as i64;
        let json = format!(
            r#"{{"session_id":"sess-recreated","status":"idle","pid":{live_pid},"last_updated":"2099-01-01T00:00:00Z","source":"hook"}}"#
        );
        std::fs::write(&path, json).unwrap();

        let result = poller.read_file_tracked(&path);
        assert!(
            result.is_some(),
            "re-created good file should parse cleanly"
        );
        assert_eq!(result.unwrap().session_id, "sess-recreated");
        assert!(
            !poller.known_bad_files.contains(&path),
            "good re-read should not mark path as bad"
        );
    }
}
