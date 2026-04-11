//! Delegate sibling summary state machine (tn-ztv3.4).
//!
//! Tracks per-pane state of delegate siblings (spawned via `/gg-delegate`)
//! so the status bar can render a compact summary like
//! `delegates: planner=streaming (87s), reviewer=idle` without forcing the
//! user to open each pane.
//!
//! ## Inputs
//!
//! The render driver walks all workspace panes each frame, filters to those
//! tagged `delegate_profile=<name>` (the convention established by the
//! `/gg-delegate` skill in tn-ztv3.3), and calls [`DelegateSummaryState::update`]
//! with the pane id, profile name, and inferred [`DelegateState`] derived
//! from the pane's `ClaudeChromeMeta` status. Panes that stop being tagged
//! as delegates are removed on the next [`DelegateSummaryState::tick`] via
//! the presence set passed by the render driver.
//!
//! ## Debounce
//!
//! State transitions are debounced by [`DEBOUNCE`] (100 ms). A new state is
//! held in `pending_state` until [`DelegateSummaryState::tick`] observes that
//! the debounce window has expired without a newer transition arriving. This
//! prevents streaming/thinking flicker from causing status bar re-layout on
//! every frame during rapid event arrival.
//!
//! ## Fade
//!
//! When every tracked delegate reaches [`DelegateState::Done`] the state
//! machine sets `fade_until = now + FADE_AFTER_DONE`. While the fade timer
//! is running the summary keeps rendering; once the timer expires the whole
//! entry map is cleared and the summary disappears.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::pane::PaneId;

/// How long a fresh state must settle before it replaces the current one.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(100);

/// After all delegates reach `Done`, keep the summary visible for this long
/// so the user can glance at the final state before it disappears.
pub(crate) const FADE_AFTER_DONE: Duration = Duration::from_secs(5);

/// Coarse delegate sibling lifecycle state. The four variants collapse the
/// richer `ClaudeStatus` enum into the bucket set relevant to a chrome-level
/// summary: "nothing happening", "reasoning", "emitting text", "terminated".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelegateState {
    Idle,
    Thinking,
    Streaming,
    Done,
}

impl DelegateState {
    /// States in which the elapsed time since `entered_at` is informative
    /// and should be rendered in the summary as `(<secs>s)`.
    fn is_active(self) -> bool {
        matches!(self, DelegateState::Thinking | DelegateState::Streaming)
    }

    /// Label used in the rendered summary.
    fn label(self) -> &'static str {
        match self {
            DelegateState::Idle => "idle",
            DelegateState::Thinking => "thinking",
            DelegateState::Streaming => "streaming",
            DelegateState::Done => "done",
        }
    }
}

/// Single delegate pane entry. Tracks the current committed state, when it
/// was entered (used for elapsed-time rendering), and any pending debounced
/// transition that hasn't settled yet.
#[derive(Debug, Clone)]
pub(crate) struct DelegateEntry {
    pub profile: String,
    pub state: DelegateState,
    pub entered_at: Instant,
    /// A new state that observed a transition but hasn't passed the
    /// [`DEBOUNCE`] window yet. When the window elapses (via `tick`) it
    /// becomes the committed state.
    pub pending_state: Option<(DelegateState, Instant)>,
}

/// Whole-summary state tracked at the `App` level and fed into `StatusBarInfo`
/// every frame.
#[derive(Debug, Default)]
pub(crate) struct DelegateSummaryState {
    entries: HashMap<PaneId, DelegateEntry>,
    /// Timestamp after which the summary is removed from the chrome entirely
    /// once every tracked entry has reached [`DelegateState::Done`].
    fade_until: Option<Instant>,
}

impl DelegateSummaryState {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record the observed state for a delegate pane. If the pane is already
    /// tracked and the state differs from the committed one, the change is
    /// staged as a pending transition — it only becomes visible after
    /// [`DEBOUNCE`] has elapsed. New panes land in the committed state
    /// immediately so the very first observation shows up right away.
    pub(crate) fn update(
        &mut self,
        pane_id: PaneId,
        profile: &str,
        new_state: DelegateState,
        now: Instant,
    ) {
        match self.entries.get_mut(&pane_id) {
            Some(entry) => {
                // Profile name can shift if the user re-tags the pane; keep
                // it in sync so the rendered text reflects the latest truth.
                if entry.profile != profile {
                    entry.profile = profile.to_string();
                }
                if entry.state == new_state {
                    // Clear any stale pending transition that agreed with
                    // the committed state by the time it was due to commit.
                    entry.pending_state = None;
                    return;
                }
                match entry.pending_state {
                    Some((pending, _)) if pending == new_state => {
                        // Another observation of the same pending value does
                        // not restart the debounce clock.
                    }
                    _ => {
                        entry.pending_state = Some((new_state, now));
                    }
                }
            }
            None => {
                self.entries.insert(
                    pane_id,
                    DelegateEntry {
                        profile: profile.to_string(),
                        state: new_state,
                        entered_at: now,
                        pending_state: None,
                    },
                );
                // A newly spawned delegate cancels any outstanding fade —
                // we're active again.
                if new_state != DelegateState::Done {
                    self.fade_until = None;
                }
            }
        }
    }

    /// Commit debounced transitions, drop panes that no longer carry the
    /// delegate tag, and start / clear the fade timer when the world's state
    /// changes. Call once per frame after all [`update`](Self::update) calls.
    pub(crate) fn tick(&mut self, present: &HashSet<PaneId>, now: Instant) {
        // Drop entries whose panes are no longer tagged as delegates.
        // Preserving them in a Done/faded state would be misleading because
        // the orchestrator may have intentionally retired the tag to hide
        // them from chrome.
        self.entries.retain(|id, _| present.contains(id));

        // Commit pending transitions whose debounce window has expired.
        for entry in self.entries.values_mut() {
            if let Some((state, first_seen)) = entry.pending_state
                && now.duration_since(first_seen) >= DEBOUNCE
            {
                entry.state = state;
                entry.entered_at = now;
                entry.pending_state = None;
            }
        }

        // Fade window bookkeeping: start the timer when every entry has
        // reached Done; clear it if any entry is active again.
        let all_done = !self.entries.is_empty()
            && self
                .entries
                .values()
                .all(|e| e.state == DelegateState::Done);
        if all_done {
            self.fade_until.get_or_insert(now + FADE_AFTER_DONE);
        } else {
            self.fade_until = None;
        }

        // Once the fade window has elapsed, drop everything so the
        // `delegates:` section disappears cleanly.
        if let Some(until) = self.fade_until
            && now >= until
        {
            self.entries.clear();
            self.fade_until = None;
        }
    }

    /// Whether any entry is still being tracked. Includes entries in the
    /// fade window — the summary keeps rendering their final state until
    /// [`FADE_AFTER_DONE`] elapses.
    pub(crate) fn is_visible(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Whether at least one tracked delegate is in an active state
    /// (thinking / streaming). Exposed for tests and for diagnostics that
    /// want to distinguish "live" from "fading out" without walking the
    /// entry map themselves.
    #[cfg(test)]
    pub(crate) fn any_active(&self) -> bool {
        self.entries.values().any(|e| e.state.is_active())
    }

    /// Render the compact footer text, or `None` if nothing should show.
    /// The format is `delegates: profile=state (Ns), profile=state`, with
    /// the `(Ns)` elapsed suffix only attached to active states.
    pub(crate) fn render_text(&self, now: Instant) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        // Sort by profile then pane id for stable rendering across frames.
        let mut rows: Vec<(&PaneId, &DelegateEntry)> = self.entries.iter().collect();
        rows.sort_by(|(a_id, a), (b_id, b)| a.profile.cmp(&b.profile).then_with(|| a_id.cmp(b_id)));

        let mut parts: Vec<String> = Vec::with_capacity(rows.len());
        for (_, entry) in rows {
            let suffix = if entry.state.is_active() {
                let secs = now.saturating_duration_since(entry.entered_at).as_secs();
                format!(" ({secs}s)")
            } else {
                String::new()
            };
            parts.push(format!(
                "{}={}{}",
                entry.profile,
                entry.state.label(),
                suffix
            ));
        }
        Some(format!("delegates: {}", parts.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn new_entry_commits_immediately() {
        let mut s = DelegateSummaryState::new();
        let t = now();
        s.update(1, "planner", DelegateState::Streaming, t);
        s.tick(&HashSet::from([1]), t);
        let text = s.render_text(t).expect("has text");
        assert!(text.contains("planner=streaming"));
    }

    #[test]
    fn state_transition_is_debounced() {
        let mut s = DelegateSummaryState::new();
        let t0 = now();
        s.update(1, "planner", DelegateState::Streaming, t0);
        s.tick(&HashSet::from([1]), t0);

        // 50 ms later we see a transition — not enough to commit.
        let t1 = t0 + Duration::from_millis(50);
        s.update(1, "planner", DelegateState::Thinking, t1);
        s.tick(&HashSet::from([1]), t1);
        assert!(s.render_text(t1).unwrap().contains("streaming"));

        // 150 ms after the original observation — past DEBOUNCE.
        let t2 = t0 + Duration::from_millis(150);
        s.tick(&HashSet::from([1]), t2);
        assert!(s.render_text(t2).unwrap().contains("thinking"));
    }

    #[test]
    fn debounce_resets_on_flapping_back() {
        let mut s = DelegateSummaryState::new();
        let t0 = now();
        s.update(1, "planner", DelegateState::Streaming, t0);
        s.tick(&HashSet::from([1]), t0);

        // Observe a new state then flap back to the committed one before
        // the debounce expires. The committed state should stay put.
        let t1 = t0 + Duration::from_millis(30);
        s.update(1, "planner", DelegateState::Thinking, t1);
        let t2 = t0 + Duration::from_millis(60);
        s.update(1, "planner", DelegateState::Streaming, t2);
        let t3 = t0 + Duration::from_millis(200);
        s.tick(&HashSet::from([1]), t3);
        assert!(s.render_text(t3).unwrap().contains("streaming"));
    }

    #[test]
    fn multiple_delegates_render_sorted_by_profile() {
        let mut s = DelegateSummaryState::new();
        let t = now();
        s.update(2, "reviewer", DelegateState::Idle, t);
        s.update(1, "planner", DelegateState::Streaming, t);
        s.tick(&HashSet::from([1, 2]), t);
        let text = s.render_text(t).expect("has text");
        let planner_pos = text.find("planner").unwrap();
        let reviewer_pos = text.find("reviewer").unwrap();
        assert!(
            planner_pos < reviewer_pos,
            "profiles should sort alphabetically: {text}"
        );
    }

    #[test]
    fn elapsed_seconds_rendered_for_active_states() {
        let mut s = DelegateSummaryState::new();
        let t0 = now();
        s.update(1, "planner", DelegateState::Streaming, t0);
        s.tick(&HashSet::from([1]), t0);
        let text = s.render_text(t0 + Duration::from_secs(42)).unwrap();
        assert!(text.contains("planner=streaming (42s)"), "got: {text}");
    }

    #[test]
    fn elapsed_seconds_hidden_for_idle_and_done() {
        let mut s = DelegateSummaryState::new();
        let t0 = now();
        s.update(1, "planner", DelegateState::Idle, t0);
        s.tick(&HashSet::from([1]), t0);
        let text = s.render_text(t0 + Duration::from_secs(10)).unwrap();
        assert!(!text.contains("s)"), "idle should not show elapsed: {text}");
    }

    #[test]
    fn all_done_starts_fade_and_then_clears() {
        let mut s = DelegateSummaryState::new();
        let t0 = now();
        s.update(1, "planner", DelegateState::Done, t0);
        s.update(2, "reviewer", DelegateState::Done, t0);
        s.tick(&HashSet::from([1, 2]), t0);
        assert!(s.is_visible());
        assert!(s.render_text(t0).unwrap().contains("done"));

        // Still visible in the fade window.
        let t_mid = t0 + Duration::from_secs(3);
        s.tick(&HashSet::from([1, 2]), t_mid);
        assert!(s.is_visible());

        // After FADE_AFTER_DONE the whole section is gone.
        let t_end = t0 + FADE_AFTER_DONE + Duration::from_millis(10);
        s.tick(&HashSet::from([1, 2]), t_end);
        assert!(!s.is_visible());
        assert!(s.render_text(t_end).is_none());
    }

    #[test]
    fn new_active_delegate_cancels_fade() {
        let mut s = DelegateSummaryState::new();
        let t0 = now();
        s.update(1, "planner", DelegateState::Done, t0);
        s.tick(&HashSet::from([1]), t0);
        // Fade timer is now running.
        let t1 = t0 + Duration::from_secs(1);
        s.update(2, "reviewer", DelegateState::Streaming, t1);
        s.tick(&HashSet::from([1, 2]), t1);

        // Even well past FADE_AFTER_DONE, the summary is still visible
        // because reviewer is active.
        let t_far = t0 + FADE_AFTER_DONE + Duration::from_secs(10);
        s.tick(&HashSet::from([1, 2]), t_far);
        assert!(s.is_visible());
        assert!(s.any_active());
    }

    #[test]
    fn entry_is_removed_when_pane_stops_being_delegate() {
        let mut s = DelegateSummaryState::new();
        let t0 = now();
        s.update(1, "planner", DelegateState::Streaming, t0);
        s.tick(&HashSet::from([1]), t0);
        assert!(s.is_visible());

        // Pane is no longer tagged as a delegate — render driver
        // passes an empty presence set.
        let t1 = t0 + Duration::from_millis(200);
        s.tick(&HashSet::new(), t1);
        assert!(!s.is_visible());
    }

    #[test]
    fn any_active_reflects_only_active_states() {
        let mut s = DelegateSummaryState::new();
        let t = now();
        s.update(1, "planner", DelegateState::Idle, t);
        s.update(2, "reviewer", DelegateState::Done, t);
        s.tick(&HashSet::from([1, 2]), t);
        assert!(!s.any_active());

        s.update(3, "scout", DelegateState::Thinking, t);
        s.tick(&HashSet::from([1, 2, 3]), t);
        assert!(s.any_active());
    }

    #[test]
    fn render_text_none_when_empty() {
        let s = DelegateSummaryState::new();
        assert!(s.render_text(now()).is_none());
    }

    #[test]
    fn profile_rename_propagates() {
        let mut s = DelegateSummaryState::new();
        let t = now();
        s.update(1, "planner", DelegateState::Streaming, t);
        s.tick(&HashSet::from([1]), t);
        assert!(s.render_text(t).unwrap().contains("planner"));

        s.update(1, "planner-v2", DelegateState::Streaming, t);
        s.tick(&HashSet::from([1]), t);
        assert!(s.render_text(t).unwrap().contains("planner-v2"));
    }
}
