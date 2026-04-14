//! Per-pane agent capacity cache.
//!
//! Stores the most recent `context_percent`, `model`, and `status` reported
//! by `ClaudeStatePoller` for each pane that has a detected agent. The cache
//! is populated by a tokio task in `ensure.rs` that drains the poller's
//! `ClaudeStateUpdate` stream, resolves each update to a `PaneId` via the
//! `AgentRegistry`, and writes the entry. Lookups happen via
//! `SessionManager::pane_capacity()`.
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

/// Resolve pane ID from state — PID match only, no heuristic fallback (tn-hxso).
pub fn resolve_pane_id_from_state(
    state: &ClaudeSessionState,
    registry: &AgentRegistry,
) -> Option<PaneId> {
    if let Some(pid_i64) = state.pid
        && let Ok(pid_u32) = u32::try_from(pid_i64)
    {
        for entry in registry.agents() {
            if entry.pid == Some(pid_u32) {
                return Some(entry.pane_id);
            }
        }
    }
    None
}

/// Build a `PaneCapacityEntry` from a poller state.
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
    }
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
        assert!(resolve_pane_id_from_state(&state, &registry).is_none());
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
        assert!(resolve_pane_id_from_state(&state, &registry).is_none());
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
        assert_eq!(resolve_pane_id_from_state(&state, &registry), Some(42));
    }
}
