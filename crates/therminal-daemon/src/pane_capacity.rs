//! Per-pane agent capacity cache.
//!
//! Stores the most recent `context_percent`, `model`, and `status` reported
//! by `ClaudeStatePoller` for each pane that has a detected agent. The cache
//! is populated from two sources:
//!
//! 1. **OSC 1341 markers** (primary) — `upsert_from_marker()` merges
//!    marker-supplied fields (`status`, `session_id`, `working_dir`,
//!    `current_tool`, and optionally `context_percent` / `model`) and stamps
//!    `marker_seen_at`.
//! 2. **File poller** (fallback) — when markers are fresh, only file-exclusive
//!    fields (`context_percent`, `model`, `session_title`) are merged via
//!    `merge_file_polled_fields()` (tn-b7qq). When markers are stale or absent,
//!    the full `entry_from_state()` upsert applies.
//!
//! Lookups happen via `SessionManager::pane_capacity()`.
//!
//! ## Staleness (tn-hxso)
//!
//! Each entry carries `last_seen_at` (Unix seconds) — refreshed on every
//! upsert. `evict_stale(ttl_secs)` drops entries older than the TTL. The
//! ensure.rs bridge task calls this on every poll tick so that entries whose
//! state file disappeared (agent exited, `/tmp/claude-code-state/` cleaned)
//! are garbage-collected within one TTL window (default 60 s).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use therminal_protocol::PaneId;
use therminal_terminal::agent_registry::AgentRegistry;

use therminal_harness_claude::state::ClaudeSessionState;

/// Default TTL for stale cache entries (seconds).
pub const DEFAULT_STALE_TTL_SECS: u64 = 60;

/// Staleness threshold for marker-sourced data (seconds). When marker data
/// is fresher than this, file-polled updates are suppressed for the pane.
pub const MARKER_FRESH_SECS: u64 = 30;

/// One pane's most recently observed agent capacity snapshot.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PaneCapacityEntry {
    pub context_percent: Option<f32>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub session_id: String,
    pub session_title: Option<String>,
    pub current_tool: Option<String>,
    pub working_dir: Option<String>,
    pub updated_at: u64,
    /// Wall-clock timestamp (Unix secs) of the most recent upsert.
    pub last_seen_at: u64,
    /// Wall-clock timestamp (Unix secs) of the most recent OSC 1341 marker
    /// update. When `> 0` and `now - marker_seen_at < MARKER_FRESH_SECS`,
    /// file-polled updates use field-aware merging instead of full upsert:
    /// marker-sourced fields are preserved, file-exclusive fields
    /// (`context_percent`, `model`, `session_title`) are merged in (tn-b7qq).
    #[serde(skip_serializing_if = "is_zero")]
    pub marker_seen_at: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Thread-safe cache mapping `PaneId -> PaneCapacityEntry`.
#[derive(Debug, Default)]
pub struct PaneCapacityCache {
    inner: Mutex<HashMap<PaneId, PaneCapacityEntry>>,
}

impl PaneCapacityCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    pub fn upsert(&self, pane_id: PaneId, entry: PaneCapacityEntry) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(pane_id, entry);
        }
    }

    /// Update the capacity entry from an OSC 1341 marker. Merges non-None
    /// fields from `patch` into the existing entry (or creates one if absent),
    /// and stamps `marker_seen_at` to the current wall-clock time.
    pub fn upsert_from_marker(&self, pane_id: PaneId, patch: MarkerPatch) {
        let now = now_secs();
        if let Ok(mut g) = self.inner.lock() {
            let entry = g.entry(pane_id).or_insert_with(PaneCapacityEntry::default);
            if let Some(s) = patch.session_id {
                entry.session_id = s;
            }
            if let Some(s) = patch.status {
                entry.status = Some(s);
            }
            if let Some(t) = patch.current_tool {
                entry.current_tool = Some(t);
            }
            if let Some(c) = patch.cwd {
                entry.working_dir = Some(c);
            }
            if let Some(cp) = patch.context_percent {
                entry.context_percent = Some(cp);
            }
            if let Some(m) = patch.model {
                entry.model = Some(m);
            }
            entry.marker_seen_at = now;
            entry.last_seen_at = now;
            entry.updated_at = now;
        }
    }

    /// Returns `true` if the pane has marker-sourced data that is still fresh
    /// (i.e., `now - marker_seen_at < MARKER_FRESH_SECS`). When this returns
    /// `true`, full file-polled upserts should be suppressed — use
    /// `merge_file_polled_fields` instead to fill in fields that markers
    /// don't cover (tn-b7qq).
    pub fn is_marker_fresh(&self, pane_id: PaneId) -> bool {
        let now = now_secs();
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.get(&pane_id).cloned())
            .is_some_and(|e| {
                e.marker_seen_at > 0 && now.saturating_sub(e.marker_seen_at) < MARKER_FRESH_SECS
            })
    }

    /// Merge file-polled fields that markers don't reliably cover into an
    /// existing entry, preserving marker-sourced fields. Called when
    /// `is_marker_fresh` is true so that `context_percent`, `model`, and
    /// `session_title` stay current even when the primary signal comes from
    /// OSC 1341 markers (tn-b7qq).
    ///
    /// Fields merged from file-polled state:
    /// - `context_percent` — only source is the state file
    /// - `model` — rarely present in markers
    /// - `session_title` — never present in markers
    ///
    /// Fields preserved (marker-sourced, not overwritten):
    /// - `status`, `session_id`, `working_dir`, `current_tool`
    /// - `marker_seen_at` (untouched — freshness clock is marker-driven)
    pub fn merge_file_polled_fields(&self, pane_id: PaneId, file_entry: &PaneCapacityEntry) {
        let now = now_secs();
        if let Ok(mut g) = self.inner.lock()
            && let Some(entry) = g.get_mut(&pane_id)
        {
            // Merge fields that markers don't reliably cover.
            if file_entry.context_percent.is_some() {
                entry.context_percent = file_entry.context_percent;
            }
            if file_entry.model.is_some() {
                entry.model.clone_from(&file_entry.model);
            }
            if file_entry.session_title.is_some() {
                entry.session_title.clone_from(&file_entry.session_title);
            }
            // Refresh last_seen_at so the entry isn't evicted by the
            // staleness sweep while the file poller is still ticking.
            entry.last_seen_at = now;
        }
    }

    pub fn get(&self, pane_id: PaneId) -> Option<PaneCapacityEntry> {
        self.inner.lock().ok()?.get(&pane_id).cloned()
    }

    /// Find the pane ID associated with a Claude session ID.
    ///
    /// Used by the hook-push path (tn-s8w3) to resolve which pane a
    /// subagent's parent session is running in. Returns `None` if no
    /// entry matches.
    pub fn find_pane_by_session_id(&self, session_id: &str) -> Option<PaneId> {
        self.inner
            .lock()
            .ok()?
            .iter()
            .find(|(_, e)| e.session_id == session_id)
            .map(|(&pid, _)| pid)
    }

    pub fn remove_by_session_id(&self, session_id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|_, e| e.session_id != session_id);
        }
    }

    /// Evict entries whose `last_seen_at` is older than `now - ttl_secs`.
    pub fn evict_stale(&self, ttl_secs: u64) -> usize {
        let now = now_secs();
        let cutoff = now.saturating_sub(ttl_secs);
        if let Ok(mut g) = self.inner.lock() {
            let before = g.len();
            g.retain(|_, e| e.last_seen_at >= cutoff);
            before - g.len()
        } else {
            0
        }
    }
}

/// Resolve pane ID from state — tries PID match first, then session_id
/// lookup in the capacity cache (tn-y2d8). The session_id tier is more
/// reliable than PID on Windows+WSL where the state file PID (written by
/// a WSL hook via PPID) and the agent registry PID (from the daemon-side
/// WSL probe) are from different namespaces and never match.
pub fn resolve_pane_id_from_state(
    state: &ClaudeSessionState,
    registry: &AgentRegistry,
    capacity_cache: Option<&PaneCapacityCache>,
) -> Option<PaneId> {
    // Tier 1: PID match against the agent registry.
    if let Some(pid_i64) = state.pid
        && let Ok(pid_u32) = u32::try_from(pid_i64)
    {
        for entry in registry.agents() {
            if entry.pid == Some(pid_u32) {
                return Some(entry.pane_id);
            }
        }
    }

    // Tier 2: session_id match against the capacity cache. The state file
    // always carries a session_id, and any pane that was previously resolved
    // (by PID or cwd) already has a cache entry keyed on session_id.  This
    // covers the common Windows+WSL case where the *first* resolution
    // succeeded via the cwd fallback and subsequent updates can fast-path
    // through the session_id match without repeating the cwd scan.
    if !state.session_id.is_empty()
        && let Some(cache) = capacity_cache
        && let Some(pane_id) = cache.find_pane_by_session_id(&state.session_id)
    {
        tracing::debug!(
            pane_id,
            session_id = %state.session_id,
            "pane_capacity: PID miss, matched by session_id in cache"
        );
        return Some(pane_id);
    }

    None
}

/// Build a `PaneCapacityEntry` from a poller state (file-polled path).
/// `marker_seen_at` is left at 0 (no marker data from file polling).
pub fn entry_from_state(state: &ClaudeSessionState) -> PaneCapacityEntry {
    let now = now_secs();
    PaneCapacityEntry {
        context_percent: state.context_percent.map(|p| p as f32),
        model: state.model.clone(),
        status: Some(format!("{:?}", state.status).to_lowercase()),
        session_id: state.session_id.clone(),
        session_title: state.session_title.clone(),
        current_tool: state.current_tool.clone(),
        working_dir: state.working_dir.clone(),
        updated_at: now,
        last_seen_at: now,
        marker_seen_at: 0,
    }
}

/// Partial update from an OSC 1341 marker. Only fields present in the
/// marker are `Some`; the rest are `None` and left untouched in the existing
/// capacity entry.
#[derive(Debug, Default)]
pub struct MarkerPatch {
    pub session_id: Option<String>,
    pub status: Option<String>,
    pub current_tool: Option<String>,
    pub cwd: Option<String>,
    pub context_percent: Option<f32>,
    pub model: Option<String>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use therminal_terminal::state_inference::AgentType;

    #[test]
    fn upsert_and_get_roundtrip() {
        let cache = PaneCapacityCache::new();
        let entry = PaneCapacityEntry {
            context_percent: Some(73.0),
            model: Some("claude-opus-4-6".into()),
            status: Some("processing".into()),
            session_id: "sess-abc".into(),
            updated_at: 1_700_000_000,
            last_seen_at: 1_700_000_000,
            ..Default::default()
        };
        cache.upsert(42, entry.clone());
        let got = cache.get(42).expect("entry should be present");
        assert_eq!(got.context_percent, Some(73.0));
        assert_eq!(got.model.as_deref(), Some("claude-opus-4-6"));
        assert!(cache.get(99).is_none());
    }

    #[test]
    fn remove_by_session_id_drops_matching() {
        let cache = PaneCapacityCache::new();
        cache.upsert(
            1,
            PaneCapacityEntry {
                session_id: "keep".into(),
                ..Default::default()
            },
        );
        cache.upsert(
            2,
            PaneCapacityEntry {
                session_id: "drop".into(),
                ..Default::default()
            },
        );
        cache.remove_by_session_id("drop");
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
    }

    #[test]
    fn evict_stale_removes_old_entries() {
        let cache = PaneCapacityCache::new();
        cache.upsert(
            1,
            PaneCapacityEntry {
                session_id: "old".into(),
                last_seen_at: 100,
                ..Default::default()
            },
        );
        let now = now_secs();
        cache.upsert(
            2,
            PaneCapacityEntry {
                session_id: "fresh".into(),
                last_seen_at: now,
                ..Default::default()
            },
        );
        let evicted = cache.evict_stale(DEFAULT_STALE_TTL_SECS);
        assert_eq!(evicted, 1);
        assert!(cache.get(1).is_none());
        assert!(cache.get(2).is_some());
    }

    #[test]
    fn evict_stale_no_entries_is_noop() {
        let cache = PaneCapacityCache::new();
        assert_eq!(cache.evict_stale(60), 0);
    }

    #[test]
    fn resolve_no_pid_returns_none() {
        let registry = AgentRegistry::new();
        let state = ClaudeSessionState {
            pid: None,
            agent_type: Some("codex".into()),
            ..Default::default()
        };
        assert!(resolve_pane_id_from_state(&state, &registry, None).is_none());
    }

    #[test]
    fn resolve_unmatched_pid_returns_none() {
        let mut registry = AgentRegistry::new();
        registry.register(1, "claude".into(), AgentType::Claude, Some(1000));
        let state = ClaudeSessionState {
            pid: Some(9999),
            agent_type: Some("claude".into()),
            ..Default::default()
        };
        assert!(resolve_pane_id_from_state(&state, &registry, None).is_none());
    }

    #[test]
    fn resolve_matching_pid_links_correctly() {
        let mut registry = AgentRegistry::new();
        registry.register(42, "claude".into(), AgentType::Claude, Some(1234));
        let state = ClaudeSessionState {
            pid: Some(1234),
            agent_type: Some("claude".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_pane_id_from_state(&state, &registry, None),
            Some(42)
        );
    }

    #[test]
    fn resolve_by_session_id_when_pid_misses() {
        let registry = AgentRegistry::new();
        let cache = PaneCapacityCache::new();
        // Simulate a prior successful resolution that populated the cache.
        cache.upsert(
            7,
            PaneCapacityEntry {
                session_id: "sess-wsl-123".into(),
                ..Default::default()
            },
        );
        let state = ClaudeSessionState {
            pid: Some(99999), // PID from WSL hook — won't match any registry entry
            session_id: "sess-wsl-123".into(),
            agent_type: Some("claude".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_pane_id_from_state(&state, &registry, Some(&cache)),
            Some(7)
        );
    }

    #[test]
    fn resolve_pid_takes_precedence_over_session_id() {
        let mut registry = AgentRegistry::new();
        registry.register(10, "claude".into(), AgentType::Claude, Some(5000));
        let cache = PaneCapacityCache::new();
        // Cache points session_id to a different pane.
        cache.upsert(
            20,
            PaneCapacityEntry {
                session_id: "sess-both".into(),
                ..Default::default()
            },
        );
        let state = ClaudeSessionState {
            pid: Some(5000),
            session_id: "sess-both".into(),
            agent_type: Some("claude".into()),
            ..Default::default()
        };
        // PID match should win — returns pane 10, not 20.
        assert_eq!(
            resolve_pane_id_from_state(&state, &registry, Some(&cache)),
            Some(10)
        );
    }

    #[test]
    fn resolve_empty_session_id_skips_cache_tier() {
        let registry = AgentRegistry::new();
        let cache = PaneCapacityCache::new();
        cache.upsert(
            3,
            PaneCapacityEntry {
                session_id: "".into(),
                ..Default::default()
            },
        );
        let state = ClaudeSessionState {
            pid: None,
            session_id: String::new(),
            agent_type: Some("claude".into()),
            ..Default::default()
        };
        // Empty session_id should skip the cache tier entirely.
        assert!(resolve_pane_id_from_state(&state, &registry, Some(&cache)).is_none());
    }

    #[test]
    fn upsert_from_marker_creates_entry() {
        let cache = PaneCapacityCache::new();
        let patch = MarkerPatch {
            session_id: Some("sess-42".into()),
            status: Some("processing".into()),
            cwd: Some("/home/user".into()),
            ..Default::default()
        };
        cache.upsert_from_marker(10, patch);
        let entry = cache.get(10).expect("entry created");
        assert_eq!(entry.session_id, "sess-42");
        assert_eq!(entry.status.as_deref(), Some("processing"));
        assert_eq!(entry.working_dir.as_deref(), Some("/home/user"));
        assert!(entry.marker_seen_at > 0);
    }

    #[test]
    fn upsert_from_marker_merges_into_existing() {
        let cache = PaneCapacityCache::new();
        // Pre-populate with a file-polled entry.
        cache.upsert(
            10,
            PaneCapacityEntry {
                session_id: "sess-42".into(),
                model: Some("old-model".into()),
                session_title: Some("My Session".into()),
                last_seen_at: now_secs(),
                ..Default::default()
            },
        );
        // Marker update patches status and model, but not session_title.
        let patch = MarkerPatch {
            status: Some("idle".into()),
            model: Some("claude-opus-4-6".into()),
            ..Default::default()
        };
        cache.upsert_from_marker(10, patch);
        let entry = cache.get(10).expect("entry exists");
        assert_eq!(entry.status.as_deref(), Some("idle"));
        assert_eq!(entry.model.as_deref(), Some("claude-opus-4-6"));
        // session_title preserved from file-polled entry
        assert_eq!(entry.session_title.as_deref(), Some("My Session"));
        // session_id preserved (patch had None)
        assert_eq!(entry.session_id, "sess-42");
        assert!(entry.marker_seen_at > 0);
    }

    #[test]
    fn is_marker_fresh_true_for_recent() {
        let cache = PaneCapacityCache::new();
        let patch = MarkerPatch {
            status: Some("idle".into()),
            ..Default::default()
        };
        cache.upsert_from_marker(10, patch);
        assert!(cache.is_marker_fresh(10));
    }

    #[test]
    fn is_marker_fresh_false_for_no_marker() {
        let cache = PaneCapacityCache::new();
        // File-polled entry has marker_seen_at = 0
        cache.upsert(
            10,
            PaneCapacityEntry {
                session_id: "x".into(),
                last_seen_at: now_secs(),
                ..Default::default()
            },
        );
        assert!(!cache.is_marker_fresh(10));
    }

    #[test]
    fn is_marker_fresh_false_for_stale() {
        let cache = PaneCapacityCache::new();
        // Simulate an old marker_seen_at
        cache.upsert(
            10,
            PaneCapacityEntry {
                session_id: "x".into(),
                last_seen_at: now_secs(),
                marker_seen_at: now_secs().saturating_sub(MARKER_FRESH_SECS + 5),
                ..Default::default()
            },
        );
        assert!(!cache.is_marker_fresh(10));
    }

    #[test]
    fn is_marker_fresh_false_for_unknown_pane() {
        let cache = PaneCapacityCache::new();
        assert!(!cache.is_marker_fresh(999));
    }

    #[test]
    fn merge_file_polled_fields_fills_gaps_without_overwriting_markers() {
        let cache = PaneCapacityCache::new();
        // Simulate a marker update that set status, session_id, cwd, current_tool.
        let patch = MarkerPatch {
            session_id: Some("sess-marker".into()),
            status: Some("processing".into()),
            current_tool: Some("Edit".into()),
            cwd: Some("/marker/dir".into()),
            ..Default::default()
        };
        cache.upsert_from_marker(10, patch);
        let before = cache.get(10).unwrap();
        assert!(before.context_percent.is_none());
        assert!(before.model.is_none());
        assert!(before.session_title.is_none());

        // File-polled entry has all fields including the ones markers cover.
        let file_entry = PaneCapacityEntry {
            context_percent: Some(42.5),
            model: Some("claude-opus-4-6".into()),
            status: Some("idle".into()),    // differs from marker
            session_id: "sess-file".into(), // differs from marker
            session_title: Some("My Cool Session".into()),
            current_tool: Some("Bash".into()), // differs from marker
            working_dir: Some("/file/dir".into()), // differs from marker
            updated_at: now_secs(),
            last_seen_at: now_secs(),
            marker_seen_at: 0,
        };

        cache.merge_file_polled_fields(10, &file_entry);
        let after = cache.get(10).unwrap();

        // File-exclusive fields should be merged in.
        assert_eq!(after.context_percent, Some(42.5));
        assert_eq!(after.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(after.session_title.as_deref(), Some("My Cool Session"));

        // Marker-sourced fields should be preserved (not overwritten).
        assert_eq!(after.status.as_deref(), Some("processing"));
        assert_eq!(after.session_id, "sess-marker");
        assert_eq!(after.current_tool.as_deref(), Some("Edit"));
        assert_eq!(after.working_dir.as_deref(), Some("/marker/dir"));

        // marker_seen_at should be untouched.
        assert_eq!(after.marker_seen_at, before.marker_seen_at);
    }

    #[test]
    fn merge_file_polled_fields_noop_for_unknown_pane() {
        let cache = PaneCapacityCache::new();
        let file_entry = PaneCapacityEntry {
            context_percent: Some(50.0),
            model: Some("test-model".into()),
            ..Default::default()
        };
        // Should not panic or create an entry.
        cache.merge_file_polled_fields(999, &file_entry);
        assert!(cache.get(999).is_none());
    }

    #[test]
    fn merge_file_polled_fields_skips_none_values() {
        let cache = PaneCapacityCache::new();
        // Pre-populate with marker data that includes context_percent.
        let patch = MarkerPatch {
            session_id: Some("sess-1".into()),
            context_percent: Some(30.0),
            ..Default::default()
        };
        cache.upsert_from_marker(10, patch);

        // File-polled entry has None for context_percent but Some for model.
        let file_entry = PaneCapacityEntry {
            context_percent: None,
            model: Some("new-model".into()),
            session_title: None,
            ..Default::default()
        };

        cache.merge_file_polled_fields(10, &file_entry);
        let after = cache.get(10).unwrap();

        // context_percent should be preserved from marker (file had None).
        assert_eq!(after.context_percent, Some(30.0));
        // model should be merged from file.
        assert_eq!(after.model.as_deref(), Some("new-model"));
    }
}
