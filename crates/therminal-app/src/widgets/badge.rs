//! Agent status badge: proof-of-concept widget (tn-npd).
//!
//! This was the first consumer of the widget pre-rasterization substrate.
//! It surfaces the focused pane's detected agent + inferred status as a
//! rounded pill in the top-right of the window.
//!
//! The overlay pill has been retired in favor of the pane-header text badge
//! (which uses agent registry data directly). The module is retained as a
//! reference for the widget pattern and can be re-enabled if needed.
#![allow(dead_code)]

use std::hash::{Hash, Hasher};

use therminal_terminal::agent_registry::AgentStatus;

use super::WidgetId;
use super::rasterizer::{PillSpec, WidgetKind, WidgetSpec};

/// Stable widget id for the single PoC agent badge. Hardcoded because
/// v1 only ships one widget — a placement API is out of scope.
pub const BADGE_WIDGET_ID: WidgetId = 0x4147_454E_5442_4447; // "AGENTBDG"

/// Inputs the badge cares about for freshness tracking.
///
/// Hashing this struct gives a compact `data_hash` that the
/// `WidgetManager` compares against the cached entry. Changing any
/// visible state — the name, the status kind, the tool name inside a
/// `ToolUse` status — will invalidate the cache and trigger a
/// rasterization. Cadence bps / internal metrics are deliberately not
/// hashed, since they'd force re-rasterization on every frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentBadgeSnapshot {
    pub name: String,
    pub status_label: String,
    pub status_color: [u8; 4],
}

impl Hash for AgentBadgeSnapshot {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.name.hash(h);
        self.status_label.hash(h);
        self.status_color.hash(h);
    }
}

impl AgentBadgeSnapshot {
    /// Compact summary ("name · status") used as the visible label.
    pub fn label(&self) -> String {
        format!("{} · {}", self.name, self.status_label)
    }

    /// Compute a deterministic data hash for the freshness cache.
    pub fn data_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

/// Adapter that converts a focused pane's agent state into a
/// `WidgetSpec` + an `AgentBadgeSnapshot`.
///
/// Callers should instantiate this once per frame from whatever data
/// they have (pane agent name, pane agent status), then feed the
/// produced spec into `WidgetManager::upsert`. The manager decides
/// whether the spec's hash matches the cached entry.
pub struct AgentBadgeSource;

impl AgentBadgeSource {
    /// Build a snapshot from the focused pane's detected agent +
    /// inferred status. Returns `None` when no agent is present —
    /// the render path interprets that as "don't draw the badge".
    pub fn snapshot(
        agent_name: Option<&str>,
        status: Option<&AgentStatus>,
    ) -> Option<AgentBadgeSnapshot> {
        let name = agent_name?.to_string();
        let (status_label, status_color) = status
            .map(classify_status)
            .unwrap_or_else(|| ("active".to_string(), STATUS_COLOR_ACTIVE));
        Some(AgentBadgeSnapshot {
            name,
            status_label,
            status_color,
        })
    }

    /// Build a `WidgetSpec` + estimated pixel dimensions from a
    /// snapshot. The estimated width is a function of the label length;
    /// it's only used to size the pill and doesn't have to be exact.
    pub fn spec_for(snapshot: &AgentBadgeSnapshot) -> (WidgetSpec, u32, u32) {
        // Heuristic: roughly 7px per glyph at the overlay font size the
        // status bar already uses, plus padding + the dot area. This is
        // close enough for a top-right pill — the exact text placement
        // happens in the text-draw pass and uses the pill's internal
        // metrics, not these estimated dims.
        let label = snapshot.label();
        let approx_text_w = (label.chars().count() as u32) * 7;
        let height: u32 = 26;
        let width: u32 = approx_text_w + height + 24; // dot + padding
        let bg = pill_background();
        let border = Some(([0.2, 0.55, 0.9, 0.9], 1.5));
        let dot = Some([
            snapshot.status_color[0] as f32 / 255.0,
            snapshot.status_color[1] as f32 / 255.0,
            snapshot.status_color[2] as f32 / 255.0,
            snapshot.status_color[3] as f32 / 255.0,
        ]);
        let spec = WidgetSpec {
            data_hash: snapshot.data_hash(),
            kind: WidgetKind::Pill(PillSpec {
                width,
                height,
                corner_radius: height as f32 / 2.0,
                background: bg,
                border,
                dot,
            }),
        };
        (spec, width, height)
    }
}

/// Translate an `AgentStatus` into a human label + dot color.
fn classify_status(status: &AgentStatus) -> (String, [u8; 4]) {
    match status {
        AgentStatus::Active => ("active".to_string(), STATUS_COLOR_ACTIVE),
        AgentStatus::Idle => ("idle".to_string(), STATUS_COLOR_IDLE),
        AgentStatus::Processing => ("processing".to_string(), STATUS_COLOR_BUSY),
        AgentStatus::Streaming => ("streaming".to_string(), STATUS_COLOR_BUSY),
        AgentStatus::Thinking => ("thinking".to_string(), STATUS_COLOR_BUSY),
        AgentStatus::ToolUse { tool_name } => (format!("tool:{tool_name}"), STATUS_COLOR_BUSY),
        AgentStatus::AwaitingInput => ("awaiting input".to_string(), STATUS_COLOR_ATTENTION),
    }
}

// ── Palette ──────────────────────────────────────────────────────────────
// Kept small and private: tuning these requires a cache invalidation pass.

const STATUS_COLOR_ACTIVE: [u8; 4] = [128, 200, 255, 255]; // pale blue
const STATUS_COLOR_IDLE: [u8; 4] = [130, 180, 140, 255]; // muted green
const STATUS_COLOR_BUSY: [u8; 4] = [240, 200, 100, 255]; // amber
const STATUS_COLOR_ATTENTION: [u8; 4] = [240, 120, 120, 255]; // red

fn pill_background() -> [f32; 4] {
    // Dark translucent, matches the overlay chrome tier feel.
    [0.06, 0.09, 0.15, 0.80]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_none_when_no_agent() {
        let snap = AgentBadgeSource::snapshot(None, None);
        assert!(snap.is_none());
    }

    #[test]
    fn snapshot_some_when_agent_no_status() {
        let snap = AgentBadgeSource::snapshot(Some("claude"), None).expect("snapshot");
        assert_eq!(snap.name, "claude");
        assert_eq!(snap.status_label, "active");
    }

    #[test]
    fn snapshot_hash_stable_for_same_inputs() {
        let a = AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Thinking))
            .expect("snap a");
        let b = AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Thinking))
            .expect("snap b");
        assert_eq!(a.data_hash(), b.data_hash());
    }

    #[test]
    fn snapshot_hash_changes_with_status() {
        let a =
            AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Thinking)).expect("a");
        let b = AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Idle)).expect("b");
        assert_ne!(a.data_hash(), b.data_hash());
    }

    #[test]
    fn snapshot_hash_changes_with_agent_name() {
        let a = AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Idle)).expect("a");
        let b = AgentBadgeSource::snapshot(Some("codex"), Some(&AgentStatus::Idle)).expect("b");
        assert_ne!(a.data_hash(), b.data_hash());
    }

    #[test]
    fn snapshot_hash_changes_with_tool_name() {
        let a = AgentBadgeSource::snapshot(
            Some("claude"),
            Some(&AgentStatus::ToolUse {
                tool_name: "bash".into(),
            }),
        )
        .expect("a");
        let b = AgentBadgeSource::snapshot(
            Some("claude"),
            Some(&AgentStatus::ToolUse {
                tool_name: "edit".into(),
            }),
        )
        .expect("b");
        assert_ne!(a.data_hash(), b.data_hash());
    }

    #[test]
    fn spec_for_produces_nonzero_dims() {
        let snap =
            AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Idle)).expect("snap");
        let (spec, w, h) = AgentBadgeSource::spec_for(&snap);
        assert!(w > 0);
        assert!(h > 0);
        match spec.kind {
            WidgetKind::Pill(ref p) => {
                assert_eq!(p.width, w);
                assert_eq!(p.height, h);
                assert!(p.dot.is_some());
            }
            _ => panic!("expected Pill variant"),
        }
    }

    #[test]
    fn label_uses_name_then_status() {
        let snap =
            AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Thinking)).expect("snap");
        assert_eq!(snap.label(), "claude · thinking");
    }

    #[test]
    fn spec_hash_matches_snapshot_hash() {
        let snap =
            AgentBadgeSource::snapshot(Some("claude"), Some(&AgentStatus::Idle)).expect("snap");
        let (spec, _, _) = AgentBadgeSource::spec_for(&snap);
        assert_eq!(spec.data_hash, snap.data_hash());
    }
}
