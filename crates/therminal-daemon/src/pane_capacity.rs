//! Per-pane agent capacity cache.
//!
//! Stores the most recent `context_percent`, `model`, and `status` reported
//! by `ClaudeStatePoller` for each pane that has a detected agent. The cache
//! is populated by a tokio task in `ensure.rs` that drains the poller's
//! `ClaudeStateUpdate` stream, resolves each update to a `PaneId` via the
//! `AgentRegistry`, and writes the entry. Lookups happen via
//! `SessionManager::pane_capacity()`.
//!
//! This is foundational plumbing for tn-eyjx and tn-oz25, which will expose
//! the data over MCP tools.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use therminal_protocol::PaneId;
use therminal_terminal::agent_registry::AgentRegistry;
use therminal_terminal::state_inference::AgentType;

use therminal_harness_claude::state::ClaudeSessionState;

/// One pane's most recently observed agent capacity snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct PaneCapacityEntry {
    pub context_percent: Option<f32>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub session_id: String,
    pub updated_at: u64,
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

    /// Insert or replace the entry for `pane_id`.
    pub fn upsert(&self, pane_id: PaneId, entry: PaneCapacityEntry) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(pane_id, entry);
        }
    }

    /// Return a clone of the entry for `pane_id`, if any.
    pub fn get(&self, pane_id: PaneId) -> Option<PaneCapacityEntry> {
        self.inner.lock().ok()?.get(&pane_id).cloned()
    }

    /// Best-effort removal of any entry tied to `session_id`.
    pub fn remove_by_session_id(&self, session_id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|_, e| e.session_id != session_id);
        }
    }
}

/// Try to figure out which pane the given agent state belongs to by cross-
/// referencing the agent registry.
///
/// Linkage strategy:
/// 1. **Primary**: match `state.pid` against `AgentEntry.pid`. This is the
///    only deterministic link — process tree → registry pid → state file pid.
/// 2. **Fallback**: if no pid match, pick the most-recently-detected agent of
///    the same `AgentType`. This is heuristic and only correct in the common
///    "one Claude pane open" case; multiple concurrent agents of the same
///    type without pid info will collide. Documented here so future tn-eyjx /
///    tn-oz25 work can decide whether to expose ambiguity to MCP callers.
pub fn resolve_pane_id_from_state(
    state: &ClaudeSessionState,
    registry: &AgentRegistry,
) -> Option<PaneId> {
    // 1. PID match.
    if let Some(pid_i64) = state.pid
        && let Ok(pid_u32) = u32::try_from(pid_i64)
    {
        for entry in registry.agents() {
            if entry.pid == Some(pid_u32) {
                return Some(entry.pane_id);
            }
        }
    }

    // 2. Fallback: most-recent agent of the matching type.
    let want_type = state.agent_type.as_deref().and_then(agent_type_from_str)?;
    registry
        .agents()
        .into_iter()
        .filter(|e| e.agent_type == want_type)
        .max_by_key(|e| e.detected_at)
        .map(|e| e.pane_id)
}

fn agent_type_from_str(s: &str) -> Option<AgentType> {
    match s {
        "claude" => Some(AgentType::Claude),
        "codex" => Some(AgentType::Codex),
        "copilot" => Some(AgentType::Copilot),
        "aider" => Some(AgentType::Aider),
        _ => None,
    }
}

/// Build a `PaneCapacityEntry` from a poller state. Drops sub-percent
/// precision to f32 to keep the DTO small.
pub fn entry_from_state(state: &ClaudeSessionState) -> PaneCapacityEntry {
    PaneCapacityEntry {
        context_percent: state.context_percent.map(|p| p as f32),
        model: state.model.clone(),
        status: Some(format!("{:?}", state.status).to_lowercase()),
        session_id: state.session_id.clone(),
        updated_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_and_get_roundtrip() {
        let cache = PaneCapacityCache::new();
        let entry = PaneCapacityEntry {
            context_percent: Some(73.0),
            model: Some("claude-opus-4-6".into()),
            status: Some("processing".into()),
            session_id: "sess-abc".into(),
            updated_at: 1_700_000_000,
        };
        cache.upsert(42, entry.clone());

        let got = cache.get(42).expect("entry should be present");
        assert_eq!(got.context_percent, Some(73.0));
        assert_eq!(got.model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(got.status.as_deref(), Some("processing"));
        assert_eq!(got.session_id, "sess-abc");

        assert!(cache.get(99).is_none());
    }

    #[test]
    fn remove_by_session_id_drops_matching() {
        let cache = PaneCapacityCache::new();
        cache.upsert(
            1,
            PaneCapacityEntry {
                context_percent: None,
                model: None,
                status: None,
                session_id: "keep".into(),
                updated_at: 0,
            },
        );
        cache.upsert(
            2,
            PaneCapacityEntry {
                context_percent: None,
                model: None,
                status: None,
                session_id: "drop".into(),
                updated_at: 0,
            },
        );
        cache.remove_by_session_id("drop");
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
    }
}
