//! Central registry tracking all detected agents across panes.
//!
//! Combines process detector results with state inference to maintain a
//! real-time view of which agents are running, what state they're in, and
//! which panes they occupy. Emits events via an `mpsc` channel when
//! agents appear, disappear, or change status.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use therminal_protocol::PaneId;

use crate::state_inference::{AgentType, InferredStatus};

/// Status of an agent in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentStatus {
    /// Agent process detected but no detailed status yet.
    Active,
    Idle,
    Processing,
    Streaming,
    Thinking,
    ToolUse {
        tool_name: String,
    },
    AwaitingInput,
}

impl AgentStatus {
    /// Convert from an [`InferredStatus`].
    pub fn from_inferred(status: &InferredStatus) -> Self {
        match status {
            InferredStatus::Idle => AgentStatus::Idle,
            InferredStatus::Processing => AgentStatus::Processing,
            InferredStatus::Streaming => AgentStatus::Streaming,
            InferredStatus::Thinking => AgentStatus::Thinking,
            InferredStatus::ToolUse { tool_name } => AgentStatus::ToolUse {
                tool_name: tool_name.clone(),
            },
            InferredStatus::AwaitingInput => AgentStatus::AwaitingInput,
        }
    }

    /// String representation for serialization.
    pub fn as_str(&self) -> &str {
        match self {
            AgentStatus::Active => "active",
            AgentStatus::Idle => "idle",
            AgentStatus::Processing => "processing",
            AgentStatus::Streaming => "streaming",
            AgentStatus::Thinking => "thinking",
            AgentStatus::ToolUse { .. } => "tool_use",
            AgentStatus::AwaitingInput => "awaiting_input",
        }
    }

    /// Tool name if status is `ToolUse`.
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            AgentStatus::ToolUse { tool_name } => Some(tool_name),
            _ => None,
        }
    }
}

/// An agent tracked in the registry.
#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub pane_id: PaneId,
    pub name: String,
    pub agent_type: AgentType,
    pub status: AgentStatus,
    pub detected_at: u64,
    pub pid: Option<u32>,
}

/// Events emitted by the agent registry.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    Registered {
        pane_id: PaneId,
        agent_type: AgentType,
        name: String,
    },
    Unregistered {
        pane_id: PaneId,
        agent_type: AgentType,
    },
    StatusChanged {
        pane_id: PaneId,
        old_status: AgentStatus,
        new_status: AgentStatus,
    },
}

impl AgentEvent {
    /// Pane this event refers to.
    pub fn pane_id(&self) -> PaneId {
        match self {
            AgentEvent::Registered { pane_id, .. }
            | AgentEvent::Unregistered { pane_id, .. }
            | AgentEvent::StatusChanged { pane_id, .. } => *pane_id,
        }
    }
}

/// An [`AgentEvent`] tagged with the originating pane id and a wall-clock
/// timestamp, suitable for broadcasting to MCP subscribers.
#[derive(Debug, Clone, Serialize)]
pub struct TaggedAgentEvent {
    pub event: AgentEvent,
    pub pane_id: PaneId,
    pub timestamp_secs: u64,
}

/// Type-erased broadcaster for [`TaggedAgentEvent`]s. Lets the daemon install
/// a forwarder into its tokio broadcast channel without taking a tokio
/// dependency in this crate.
pub type AgentEventBroadcaster = Arc<dyn Fn(TaggedAgentEvent) + Send + Sync>;

/// Central registry of all detected agents across all panes.
pub struct AgentRegistry {
    agents: HashMap<PaneId, AgentEntry>,
    event_tx: mpsc::Sender<AgentEvent>,
    event_rx: Option<mpsc::Receiver<AgentEvent>>,
    /// Secondary event channel for notification subscribers (e.g. bell/notifications).
    #[allow(dead_code)]
    notification_tx: mpsc::Sender<AgentEvent>,
    notification_rx: Option<mpsc::Receiver<AgentEvent>>,
    /// Optional broadcaster for tagged lifecycle events (used by the MCP
    /// `therminal://agents/events` resource).
    broadcaster: Option<AgentEventBroadcaster>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::channel();
        let (notification_tx, notification_rx) = mpsc::channel();
        Self {
            agents: HashMap::new(),
            event_tx,
            event_rx: Some(event_rx),
            notification_tx,
            notification_rx: Some(notification_rx),
            broadcaster: None,
        }
    }

    /// Install a broadcaster that receives every emitted event tagged with
    /// its pane id and a wall-clock timestamp. Replaces any prior broadcaster.
    pub fn set_broadcaster(&mut self, broadcaster: AgentEventBroadcaster) {
        self.broadcaster = Some(broadcaster);
    }

    pub fn take_event_rx(&mut self) -> Option<mpsc::Receiver<AgentEvent>> {
        self.event_rx.take()
    }

    /// Take the notification event receiver. Used by the notification
    /// subsystem to react to agent status changes.
    pub fn take_notification_rx(&mut self) -> Option<mpsc::Receiver<AgentEvent>> {
        self.notification_rx.take()
    }

    /// Broadcast an event to both the primary and notification channels and
    /// (if installed) the tagged broadcaster.
    fn emit(&self, event: AgentEvent) {
        let _ = self.event_tx.send(event.clone());
        let _ = self.notification_tx.send(event.clone());
        if let Some(b) = &self.broadcaster {
            let pane_id = event.pane_id();
            let timestamp_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            b(TaggedAgentEvent {
                event,
                pane_id,
                timestamp_secs,
            });
        }
    }

    pub fn register(
        &mut self,
        pane_id: PaneId,
        name: String,
        agent_type: AgentType,
        pid: Option<u32>,
    ) {
        if let Some(old) = self.agents.remove(&pane_id) {
            self.emit(AgentEvent::Unregistered {
                pane_id,
                agent_type: old.agent_type,
            });
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let entry = AgentEntry {
            pane_id,
            name: name.clone(),
            agent_type,
            status: AgentStatus::Active,
            detected_at: now,
            pid,
        };
        self.agents.insert(pane_id, entry);
        self.emit(AgentEvent::Registered {
            pane_id,
            agent_type,
            name,
        });
    }

    pub fn unregister(&mut self, pane_id: PaneId) -> bool {
        if let Some(old) = self.agents.remove(&pane_id) {
            self.emit(AgentEvent::Unregistered {
                pane_id,
                agent_type: old.agent_type,
            });
            true
        } else {
            false
        }
    }

    pub fn update_status(&mut self, pane_id: PaneId, new_status: AgentStatus) -> bool {
        if let Some(entry) = self.agents.get_mut(&pane_id) {
            if entry.status != new_status {
                let old_status = entry.status.clone();
                entry.status = new_status.clone();
                self.emit(AgentEvent::StatusChanged {
                    pane_id,
                    old_status,
                    new_status,
                });
            }
            true
        } else {
            false
        }
    }

    pub fn agents(&self) -> Vec<AgentEntry> {
        self.agents.values().cloned().collect()
    }
    pub fn agents_by_status(&self, status: &str) -> Vec<AgentEntry> {
        self.agents
            .values()
            .filter(|e| e.status.as_str() == status)
            .cloned()
            .collect()
    }
    pub fn get(&self, pane_id: PaneId) -> Option<&AgentEntry> {
        self.agents.get(&pane_id)
    }
    pub fn len(&self) -> usize {
        self.agents.len()
    }
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_list() {
        let mut reg = AgentRegistry::new();
        assert!(reg.is_empty());
        reg.register(1, "node".into(), AgentType::Claude, Some(1234));
        assert_eq!(reg.len(), 1);
        let agents = reg.agents();
        assert_eq!(agents[0].agent_type, AgentType::Claude);
        assert_eq!(agents[0].status, AgentStatus::Active);
    }

    #[test]
    fn unregister() {
        let mut reg = AgentRegistry::new();
        reg.register(1, "node".into(), AgentType::Claude, None);
        assert!(reg.unregister(1));
        assert!(reg.is_empty());
        assert!(!reg.unregister(1));
    }

    #[test]
    fn update_status() {
        let mut reg = AgentRegistry::new();
        reg.register(1, "node".into(), AgentType::Claude, None);
        assert!(reg.update_status(1, AgentStatus::Thinking));
        assert_eq!(reg.get(1).unwrap().status, AgentStatus::Thinking);
        assert!(!reg.update_status(999, AgentStatus::Idle));
    }

    #[test]
    fn agents_by_status_filter() {
        let mut reg = AgentRegistry::new();
        reg.register(1, "node".into(), AgentType::Claude, None);
        reg.register(2, "codex".into(), AgentType::Codex, None);
        reg.update_status(1, AgentStatus::Thinking);
        assert_eq!(reg.agents_by_status("thinking").len(), 1);
        assert_eq!(reg.agents_by_status("active").len(), 1);
    }

    #[test]
    fn register_replaces_existing() {
        let mut reg = AgentRegistry::new();
        reg.register(1, "node".into(), AgentType::Claude, None);
        reg.register(1, "codex".into(), AgentType::Codex, None);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get(1).unwrap().agent_type, AgentType::Codex);
    }

    #[test]
    fn events_are_emitted() {
        let mut reg = AgentRegistry::new();
        let rx = reg.take_event_rx().unwrap();
        reg.register(1, "node".into(), AgentType::Claude, None);
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::Registered { pane_id: 1, .. }
        ));
        reg.update_status(1, AgentStatus::Streaming);
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::StatusChanged { pane_id: 1, .. }
        ));
        reg.unregister(1);
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::Unregistered { pane_id: 1, .. }
        ));
    }

    #[test]
    fn status_from_inferred() {
        assert_eq!(
            AgentStatus::from_inferred(&InferredStatus::Idle),
            AgentStatus::Idle
        );
        assert_eq!(
            AgentStatus::from_inferred(&InferredStatus::Thinking),
            AgentStatus::Thinking
        );
    }

    #[test]
    fn broadcaster_receives_tagged_events() {
        use std::sync::Mutex;
        let mut reg = AgentRegistry::new();
        let received: Arc<Mutex<Vec<TaggedAgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received);
        reg.set_broadcaster(Arc::new(move |evt| {
            received_clone.lock().unwrap().push(evt);
        }));
        reg.register(7, "node".into(), AgentType::Claude, None);
        reg.update_status(7, AgentStatus::Thinking);
        reg.unregister(7);
        let evts = received.lock().unwrap();
        assert_eq!(evts.len(), 3);
        assert_eq!(evts[0].pane_id, 7);
        assert!(matches!(evts[0].event, AgentEvent::Registered { .. }));
        assert!(matches!(evts[1].event, AgentEvent::StatusChanged { .. }));
        assert!(matches!(evts[2].event, AgentEvent::Unregistered { .. }));
    }
}
