//! Auto-tiling: automatic pane creation/removal in response to agent spawn/exit events.
//!
//! Subscribes to `AgentRegistry` events via an `mpsc::Receiver<AgentEvent>` and
//! debounces rapid spawn/exit cycles to avoid layout thrashing.  Debounced events
//! are forwarded to the winit event loop as `UserEvent` variants so that the `App`
//! can split or close panes on the main thread.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use therminal_protocol::PaneId;
use therminal_terminal::agent_registry::AgentEvent;
use therminal_terminal::state_inference::AgentType;

/// An auto-tile action ready to be applied by the event loop.
#[derive(Debug, Clone)]
#[allow(dead_code)] // agent_type reserved for geometry-aware layout (tn-f72)
pub enum AutoTileAction {
    /// Split the parent pane to create space for a new agent.
    Split {
        parent_pane_id: PaneId,
        agent_name: String,
        agent_type: AgentType,
    },
    /// Reclaim the pane that an agent occupied after it exited.
    Reclaim { pane_id: PaneId },
}

/// Pending event waiting for debounce expiry.
struct PendingEvent {
    action: AutoTileAction,
    queued_at: Instant,
}

/// Tracks pending agent events and debounces rapid spawn/exit cycles.
///
/// Call `poll()` periodically (e.g. on every redraw or timer tick) to drain
/// the `AgentEvent` receiver, apply debouncing, and yield ready actions.
pub struct AutoTileDebouncer {
    /// Receiver for agent registry events.
    event_rx: mpsc::Receiver<AgentEvent>,
    /// Debounce duration.
    debounce: Duration,
    /// Pending events keyed by pane ID, awaiting debounce expiry.
    pending: HashMap<PaneId, PendingEvent>,
    /// Panes created by auto-tile (so we know which to reclaim).
    auto_tiled_panes: HashMap<PaneId, PaneId>,
}

impl AutoTileDebouncer {
    pub fn new(event_rx: mpsc::Receiver<AgentEvent>, debounce_ms: u64) -> Self {
        Self {
            event_rx,
            debounce: Duration::from_millis(debounce_ms),
            pending: HashMap::new(),
            auto_tiled_panes: HashMap::new(),
        }
    }

    /// Record that pane `child_pane_id` was auto-created from `parent_pane_id`.
    pub fn register_auto_tiled(&mut self, agent_pane_id: PaneId, child_pane_id: PaneId) {
        self.auto_tiled_panes.insert(agent_pane_id, child_pane_id);
    }

    /// Drain the event receiver and return actions whose debounce has expired.
    pub fn poll(&mut self) -> Vec<AutoTileAction> {
        let now = Instant::now();

        // Drain all available events.
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                AgentEvent::Registered { pane_id, .. } => {
                    // If there's already a pending reclaim for this pane, cancel it
                    // (agent respawned quickly).
                    if self.pending.contains_key(&pane_id) {
                        self.pending.remove(&pane_id);
                    }

                    // NOTE: We no longer auto-split on process-detected agent
                    // registration.  Subagent splits are handled by the
                    // hook-driven SwarmDebouncer (tn-s8w3), which correctly
                    // distinguishes subagents from top-level sessions.  The old
                    // process-detection path cannot tell them apart and would
                    // incorrectly split for `claude` typed at a prompt.
                }
                AgentEvent::Unregistered { pane_id, .. } => {
                    // If there's a pending split for this pane, cancel it (spawned and
                    // exited within the debounce window).
                    if let Some(pending) = self.pending.get(&pane_id)
                        && matches!(pending.action, AutoTileAction::Split { .. })
                    {
                        self.pending.remove(&pane_id);
                        continue;
                    }

                    // If we auto-tiled a pane for this agent, queue a reclaim.
                    if let Some(&child_pane_id) = self.auto_tiled_panes.get(&pane_id) {
                        self.pending.insert(
                            pane_id,
                            PendingEvent {
                                action: AutoTileAction::Reclaim {
                                    pane_id: child_pane_id,
                                },
                                queued_at: now,
                            },
                        );
                    }
                }
                AgentEvent::StatusChanged { .. } => {
                    // Status changes don't affect layout.
                }
            }
        }

        // Collect actions whose debounce has expired.
        let mut ready = Vec::new();
        let expired: Vec<PaneId> = self
            .pending
            .iter()
            .filter(|(_, pe)| now.duration_since(pe.queued_at) >= self.debounce)
            .map(|(&id, _)| id)
            .collect();

        for id in expired {
            if let Some(pe) = self.pending.remove(&id) {
                // Clean up tracking for reclaims.
                if matches!(pe.action, AutoTileAction::Reclaim { .. }) {
                    self.auto_tiled_panes.remove(&id);
                }
                ready.push(pe.action);
            }
        }

        ready
    }

    /// Returns true if there are pending events that haven't expired yet.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

// ── SwarmDebouncer ────────────────────────────────────────────────────
//
// A sibling of `AutoTileDebouncer` that applies the same spawn/exit
// cancellation pattern to `SwarmWatcherEvent`s. We kept this as a sibling
// (rather than generalizing `AutoTileDebouncer`) because the event types
// diverge meaningfully: auto-tile keys on `PaneId` and carries `AgentType`,
// while swarm events key on a `String` agent_id and carry a `PathBuf`
// payload. Unifying them would force a wrapper enum that obscures both
// paths. The cancellation logic — "spawn queued; a matching reclaim within
// the debounce window cancels both" — is the only shared shape, and it's
// small enough to duplicate without meaningful upkeep cost.

use crate::pane::swarm_watcher::SwarmWatcherEvent;

/// Pending swarm event awaiting debounce expiry.
struct PendingSwarmEvent {
    event: SwarmWatcherEvent,
    queued_at: Instant,
}

/// Debounces `SwarmWatcherEvent`s so a rapid Spawn → Reclaim cycle cancels
/// cleanly instead of briefly flashing a pane onto the screen.
///
/// Call `poll()` on every redraw (same cadence as `AutoTileDebouncer`) to
/// drain the underlying channel, apply cancellation, and yield ready events.
pub struct SwarmDebouncer {
    event_rx: mpsc::Receiver<SwarmWatcherEvent>,
    debounce: Duration,
    pending: HashMap<String, PendingSwarmEvent>,
}

impl SwarmDebouncer {
    pub fn new(event_rx: mpsc::Receiver<SwarmWatcherEvent>, debounce_ms: u64) -> Self {
        Self {
            event_rx,
            debounce: Duration::from_millis(debounce_ms),
            pending: HashMap::new(),
        }
    }

    /// Drain the receiver and return events whose debounce has expired.
    pub fn poll(&mut self) -> Vec<SwarmWatcherEvent> {
        let now = Instant::now();

        while let Ok(event) = self.event_rx.try_recv() {
            match &event {
                SwarmWatcherEvent::SpawnSubagent { agent_id, .. } => {
                    // Queue the spawn. If a pending reclaim somehow exists
                    // for this agent_id (respawn with same id), drop it.
                    self.pending.insert(
                        agent_id.clone(),
                        PendingSwarmEvent {
                            event: event.clone(),
                            queued_at: now,
                        },
                    );
                }
                SwarmWatcherEvent::ReclaimSubagent { agent_id } => {
                    // If a spawn is pending (not yet flushed), cancel both.
                    if let Some(p) = self.pending.get(agent_id)
                        && matches!(p.event, SwarmWatcherEvent::SpawnSubagent { .. })
                    {
                        self.pending.remove(agent_id);
                        continue;
                    }
                    // Otherwise queue the reclaim so it too passes through
                    // the debounce window before being acted on.
                    self.pending.insert(
                        agent_id.clone(),
                        PendingSwarmEvent {
                            event: event.clone(),
                            queued_at: now,
                        },
                    );
                }
            }
        }

        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| now.duration_since(p.queued_at) >= self.debounce)
            .map(|(k, _)| k.clone())
            .collect();

        let mut ready = Vec::with_capacity(expired.len());
        for k in expired {
            if let Some(p) = self.pending.remove(&k) {
                ready.push(p.event);
            }
        }
        ready
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn registered_does_not_auto_split() {
        // Since tn-s8w3, process-detected agent registration no longer
        // triggers auto-tile splits. Subagent splits are handled by the
        // hook-driven SwarmDebouncer instead.
        let (tx, rx) = mpsc::channel();
        let mut debouncer = AutoTileDebouncer::new(rx, 50);

        tx.send(AgentEvent::Registered {
            pane_id: 1,
            agent_type: AgentType::Claude,
            name: "claude".into(),
        })
        .unwrap();

        let actions = debouncer.poll();
        assert!(actions.is_empty());
        assert!(!debouncer.has_pending());

        std::thread::sleep(Duration::from_millis(60));
        let actions = debouncer.poll();
        assert!(
            actions.is_empty(),
            "Registered should not produce Split actions"
        );
    }

    #[test]
    fn registered_then_unregistered_no_reclaim_without_prior_auto_tile() {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = AutoTileDebouncer::new(rx, 50);

        tx.send(AgentEvent::Registered {
            pane_id: 1,
            agent_type: AgentType::Claude,
            name: "claude".into(),
        })
        .unwrap();
        tx.send(AgentEvent::Unregistered {
            pane_id: 1,
            agent_type: AgentType::Claude,
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(60));
        let actions = debouncer.poll();
        // No split was queued, so no reclaim should fire either.
        assert!(actions.is_empty());
    }

    // ── SwarmDebouncer tests ──────────────────────────────────────────

    fn spawn_ev(id: &str) -> SwarmWatcherEvent {
        SwarmWatcherEvent::SpawnSubagent {
            agent_id: id.to_string(),
            jsonl_path: std::path::PathBuf::from(format!("/tmp/agent-{id}.jsonl")),
        }
    }

    fn reclaim_ev(id: &str) -> SwarmWatcherEvent {
        SwarmWatcherEvent::ReclaimSubagent {
            agent_id: id.to_string(),
        }
    }

    #[test]
    fn swarm_spawn_then_reclaim_within_window_cancels() {
        let (tx, rx) = mpsc::channel();
        let mut d = SwarmDebouncer::new(rx, 200);
        tx.send(spawn_ev("a1")).unwrap();
        tx.send(reclaim_ev("a1")).unwrap();
        std::thread::sleep(Duration::from_millis(250));
        let events = d.poll();
        assert!(events.is_empty(), "spawn+reclaim should cancel");
        assert!(!d.has_pending());
    }

    #[test]
    fn swarm_spawn_alone_yields_spawn() {
        let (tx, rx) = mpsc::channel();
        let mut d = SwarmDebouncer::new(rx, 50);
        tx.send(spawn_ev("a2")).unwrap();
        // Immediate poll: queued, not yet ready.
        assert!(d.poll().is_empty());
        assert!(d.has_pending());
        std::thread::sleep(Duration::from_millis(70));
        let events = d.poll();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            SwarmWatcherEvent::SpawnSubagent { agent_id, .. } if agent_id == "a2"
        ));
    }

    #[test]
    fn swarm_reclaim_alone_yields_reclaim() {
        let (tx, rx) = mpsc::channel();
        let mut d = SwarmDebouncer::new(rx, 50);
        // No prior spawn queued — a standalone reclaim (pane was created on
        // a previous run / before the debouncer was wired) should still be
        // delivered after the debounce window.
        tx.send(reclaim_ev("a3")).unwrap();
        assert!(d.poll().is_empty());
        std::thread::sleep(Duration::from_millis(70));
        let events = d.poll();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            SwarmWatcherEvent::ReclaimSubagent { agent_id } if agent_id == "a3"
        ));
    }

    #[test]
    fn exit_reclaims_auto_tiled_pane() {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = AutoTileDebouncer::new(rx, 10);

        // Simulate that pane 1 had agent, and we auto-tiled pane 2 for it.
        debouncer.register_auto_tiled(1, 2);

        tx.send(AgentEvent::Unregistered {
            pane_id: 1,
            agent_type: AgentType::Claude,
        })
        .unwrap();

        // First poll drains the event and queues it.
        let actions = debouncer.poll();
        assert!(actions.is_empty());

        // After debounce expires, the reclaim action is ready.
        std::thread::sleep(Duration::from_millis(20));
        let actions = debouncer.poll();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], AutoTileAction::Reclaim { pane_id: 2 }));
    }
}
