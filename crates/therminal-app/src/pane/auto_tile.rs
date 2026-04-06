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
                AgentEvent::Registered {
                    pane_id,
                    agent_type,
                    name,
                } => {
                    // If there's already a pending reclaim for this pane, cancel it
                    // (agent respawned quickly).
                    if self.pending.contains_key(&pane_id) {
                        self.pending.remove(&pane_id);
                    }

                    // Only auto-tile if this pane doesn't already have an auto-tiled child.
                    if !self.auto_tiled_panes.contains_key(&pane_id) {
                        self.pending.insert(
                            pane_id,
                            PendingEvent {
                                action: AutoTileAction::Split {
                                    parent_pane_id: pane_id,
                                    agent_name: name,
                                    agent_type,
                                },
                                queued_at: now,
                            },
                        );
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn spawn_debounced() {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = AutoTileDebouncer::new(rx, 50);

        tx.send(AgentEvent::Registered {
            pane_id: 1,
            agent_type: AgentType::Claude,
            name: "claude".into(),
        })
        .unwrap();

        // First poll drains the event and queues it with a timestamp.
        let actions = debouncer.poll();
        assert!(actions.is_empty());
        assert!(debouncer.has_pending());

        // After waiting past the debounce interval, the action is ready.
        std::thread::sleep(Duration::from_millis(60));
        let actions = debouncer.poll();
        assert_eq!(actions.len(), 1);
        assert!(matches!(
            actions[0],
            AutoTileAction::Split {
                parent_pane_id: 1,
                ..
            }
        ));
    }

    #[test]
    fn spawn_then_exit_within_debounce_cancels() {
        let (tx, rx) = mpsc::channel();
        let mut debouncer = AutoTileDebouncer::new(rx, 200);

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

        std::thread::sleep(Duration::from_millis(250));
        let actions = debouncer.poll();
        // Both events should have cancelled each other.
        assert!(actions.is_empty());
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
