//! Daemon lifecycle wire types and IPC protocol.
//!
//! These types are used for health checks, version handoff, session
//! management, and event subscriptions over the daemon IPC channel.
//! They are MessagePack-serialized over Unix sockets with 4-byte
//! big-endian length-prefixed framing.

use serde::{Deserialize, Serialize};

use crate::{PaneId, SessionId, WorkspaceId};

/// Build hash embedded at compile time (git short hash + timestamp).
/// Informational only — not used for handoff decisions.
pub type BuildHash = String;

/// Protocol version for daemon handoff decisions.
///
/// Bump this constant when the IPC wire format or daemon behaviour changes
/// in a way that requires restarting the daemon. Normal rebuilds (UI, renderer,
/// app-side code) do **not** need a bump — the running daemon will be reused.
pub const PROTOCOL_VERSION: u32 = 1;

// ── Daemon state machine ──────────────────────────────────────────────────

/// States in the daemon lifecycle state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonState {
    /// Daemon process launched, initializing subsystems.
    Starting,
    /// Binding the control socket.
    Binding,
    /// Socket bound, ready to accept connections.
    Ready,
    /// Actively serving sessions.
    Running,
    /// Graceful shutdown in progress — draining sessions.
    Draining,
    /// Daemon has stopped.
    Stopped,
}

impl std::fmt::Display for DaemonState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Binding => write!(f, "binding"),
            Self::Ready => write!(f, "ready"),
            Self::Running => write!(f, "running"),
            Self::Draining => write!(f, "draining"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

// ── IPC envelope (multiplexed request/response/event) ─────────────────────

/// Maximum allowed frame payload size (1 MiB).
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;

/// An IPC message envelope that supports request/response multiplexing
/// and server-pushed events over a single connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum IpcMessage {
    /// A request from client to daemon. The `request_id` is echoed back
    /// in the corresponding `Response` for multiplexing.
    Request {
        request_id: u64,
        payload: IpcRequest,
    },
    /// A response from daemon to client, correlated by `request_id`.
    Response {
        request_id: u64,
        payload: IpcResponse,
    },
    /// A server-pushed event to subscribed clients.
    Event { payload: DaemonEvent },
}

/// Typed IPC requests. Extends the original `DaemonRequest` (Ping/Shutdown)
/// with session management and event subscription commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum IpcRequest {
    /// Health check — daemon replies with `Pong`.
    Ping,
    /// Request graceful shutdown.
    GracefulShutdown,
    /// Subscribe to daemon events. The server will push `IpcMessage::Event`
    /// messages for the requested event kinds.
    Subscribe {
        /// Which event kinds to subscribe to. Empty = all events.
        filter: Vec<EventKind>,
    },
    /// Unsubscribe from all events on this connection.
    Unsubscribe,
    /// List active sessions.
    ListSessions,
    /// Get details about a specific session.
    GetSession { session_id: SessionId },
    /// Create a new session.
    CreateSession { name: Option<String> },
    /// Destroy a session.
    DestroySession { session_id: SessionId },
    /// Query daemon state.
    GetState,
    /// Send keys (input) to a specific pane.
    SendKeys { pane_id: PaneId, keys: Vec<u8> },
    /// Capture pane content (terminal grid snapshot).
    CapturePane { pane_id: PaneId },
    /// Split a pane (creates a new pane in the same session).
    SplitPane { pane_id: PaneId, horizontal: bool },
    /// Kill (destroy) a specific pane.
    KillPane { pane_id: PaneId },
    /// Select (focus) a specific pane.
    SelectPane { pane_id: PaneId },
    /// Request handoff with FD passing (Unix only).
    ///
    /// The daemon responds with `HandoffReady` containing a temporary socket
    /// path where the old daemon will send PTY FDs via SCM_RIGHTS, then
    /// initiates graceful shutdown.
    RequestHandoffFds,
    /// Set the full workspace topology for a session.
    ///
    /// The app sends this on workspace create, switch, rename, pane move,
    /// and pane close. The daemon replaces its stored workspace state for
    /// the given session with the provided list.
    SetWorkspaceState {
        session_id: SessionId,
        workspaces: Vec<WorkspaceInfo>,
        active_workspace: WorkspaceId,
    },
    /// Get the daemon's stored workspace topology for a session.
    ///
    /// Used by the app on attach to restore workspace layout, and by MCP
    /// tools to query workspace state without the app being connected.
    GetWorkspaces { session_id: SessionId },
}

/// Typed IPC responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "resp")]
pub enum IpcResponse {
    /// Health check response.
    Pong {
        protocol_version: u32,
        build_hash: BuildHash,
        uptime_secs: u64,
        sessions: u32,
        version: String,
    },
    /// Shutdown acknowledged.
    ShutdownAck,
    /// Subscription confirmed.
    Subscribed {
        /// The event kinds now active on this connection.
        filter: Vec<EventKind>,
    },
    /// Unsubscription confirmed.
    Unsubscribed,
    /// List of active session IDs.
    Sessions { session_ids: Vec<SessionId> },
    /// Session details.
    SessionInfo {
        session_id: SessionId,
        name: Option<String>,
        created_at_secs: u64,
    },
    /// Session created.
    SessionCreated { session_id: SessionId },
    /// Session destroyed.
    SessionDestroyed { session_id: SessionId },
    /// Current daemon state.
    State { state: DaemonState },
    /// Keys sent successfully.
    KeysSent { pane_id: PaneId },
    /// Pane content captured.
    PaneCaptured {
        pane_id: PaneId,
        lines: Vec<String>,
        cursor_col: usize,
        cursor_line: usize,
        cols: usize,
        rows: usize,
    },
    /// Pane split — new pane created.
    PaneSplit { new_pane_id: PaneId },
    /// Pane killed.
    PaneKilled { pane_id: PaneId },
    /// Pane selected (focused).
    PaneSelected { pane_id: PaneId },
    /// Handoff ready: the old daemon has prepared FDs for transfer.
    ///
    /// The new daemon should connect to `handoff_socket` to receive the
    /// PTY master FDs and session metadata via SCM_RIGHTS.
    HandoffReady {
        /// Path to the temporary Unix socket for FD transfer.
        handoff_socket: String,
        /// Number of panes (FDs) that will be sent.
        pane_count: usize,
    },
    /// Workspace state updated.
    WorkspaceStateSet { session_id: SessionId },
    /// Workspace topology for a session.
    Workspaces {
        session_id: SessionId,
        workspaces: Vec<WorkspaceInfo>,
        active_workspace: WorkspaceId,
    },
    /// Generic error response.
    Error { message: String },
}

/// Daemon events pushed to subscribed clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum DaemonEvent {
    /// Daemon state changed.
    StateChanged { old: DaemonState, new: DaemonState },
    /// A new session was created.
    SessionCreated { session_id: SessionId },
    /// A session was destroyed.
    SessionDestroyed { session_id: SessionId },
    /// Pane output data (for subscribed watchers).
    PaneOutput {
        session_id: SessionId,
        pane_id: PaneId,
        data: Vec<u8>,
    },
    /// Workspace topology changed for a session.
    WorkspaceChanged {
        session_id: SessionId,
        active_workspace: WorkspaceId,
    },
}

/// Event kind discriminant for subscription filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    StateChanged,
    SessionCreated,
    SessionDestroyed,
    PaneOutput,
    WorkspaceChanged,
}

impl DaemonEvent {
    /// Return the `EventKind` of this event.
    pub fn kind(&self) -> EventKind {
        match self {
            DaemonEvent::StateChanged { .. } => EventKind::StateChanged,
            DaemonEvent::SessionCreated { .. } => EventKind::SessionCreated,
            DaemonEvent::SessionDestroyed { .. } => EventKind::SessionDestroyed,
            DaemonEvent::PaneOutput { .. } => EventKind::PaneOutput,
            DaemonEvent::WorkspaceChanged { .. } => EventKind::WorkspaceChanged,
        }
    }
}

// ── Handoff FD-passing metadata ──────────────────────────────────────────

/// Metadata for a single pane being transferred during FD-passing handoff.
///
/// The actual PTY master file descriptor is sent out-of-band via SCM_RIGHTS;
/// this struct carries the session/pane topology and terminal dimensions so
/// the new daemon can reconstruct its `Session`/`Pane` structs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffPaneMeta {
    pub session_id: SessionId,
    pub session_name: Option<String>,
    pub pane_id: PaneId,
    pub cols: u16,
    pub rows: u16,
}

/// The full handoff payload sent from the old daemon to the new daemon.
///
/// `panes` is ordered to match the FD array sent via SCM_RIGHTS: `panes[i]`
/// corresponds to the i-th file descriptor in the ancillary data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffPayload {
    /// Ordered list of pane metadata, one per FD.
    pub panes: Vec<HandoffPaneMeta>,
}

// ── Workspace topology ──────────────────────────────────────────────────

/// Metadata for a single workspace (tab) within a session.
///
/// The daemon stores this so MCP tools can query workspace topology
/// without the app being connected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    /// Workspace slot number (1-9).
    pub id: WorkspaceId,
    /// Human-readable workspace name (e.g. "build", "logs").
    pub name: String,
    /// Display order (lower = leftmost tab). Usually matches `id`.
    pub order: u32,
    /// Pane IDs currently assigned to this workspace.
    pub pane_ids: Vec<PaneId>,
    /// The focused pane within this workspace, if any.
    pub focused_pane: Option<PaneId>,
}

// ── Persisted state (session/layout persistence across daemon restarts) ──

/// Persisted metadata for a single pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedPane {
    /// Last known working directory (empty if unknown).
    pub cwd: String,
    /// Shell command that was used (empty = default shell).
    pub shell: String,
    /// Terminal columns at time of save.
    pub cols: u16,
    /// Terminal rows at time of save.
    pub rows: u16,
}

/// Persisted metadata for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    /// Human-readable session name, if any.
    pub name: Option<String>,
    /// Panes in this session (flat list; layout topology preserved by order).
    pub panes: Vec<PersistedPane>,
    /// Workspace topology at time of save. Empty for legacy data.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceInfo>,
    /// Which workspace was active at time of save (0 = unknown/legacy).
    #[serde(default)]
    pub active_workspace: WorkspaceId,
}

/// Top-level persisted daemon state, serialised to `sessions.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedState {
    /// All sessions that were active when state was last saved.
    pub sessions: Vec<PersistedSession>,
}

// ── Framing helpers ───────────────────────────────────────────────────────

/// Error type for frame encoding operations.
#[derive(Debug)]
pub enum EncodeFrameError {
    /// MessagePack serialization failed.
    Serialize(rmp_serde::encode::Error),
    /// Payload exceeds `MAX_FRAME_SIZE`.
    FrameTooLarge(usize),
}

impl std::fmt::Display for EncodeFrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialize(e) => write!(f, "serialization error: {e}"),
            Self::FrameTooLarge(size) => {
                write!(
                    f,
                    "frame payload too large: {size} bytes (max {MAX_FRAME_SIZE})"
                )
            }
        }
    }
}

impl std::error::Error for EncodeFrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Serialize(e) => Some(e),
            Self::FrameTooLarge(_) => None,
        }
    }
}

impl From<rmp_serde::encode::Error> for EncodeFrameError {
    fn from(e: rmp_serde::encode::Error) -> Self {
        Self::Serialize(e)
    }
}

// ── IPC frame helpers ─────────────────────────────────────────────────────

/// Encode an `IpcMessage` into a length-prefixed frame (4-byte BE length + MessagePack payload).
pub fn encode_ipc(msg: &IpcMessage) -> Result<Vec<u8>, EncodeFrameError> {
    let payload = rmp_serde::to_vec(msg)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(EncodeFrameError::FrameTooLarge(payload.len()));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode an `IpcMessage` from a MessagePack payload (without length prefix).
pub fn decode_ipc(data: &[u8]) -> Result<IpcMessage, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_state_display() {
        assert_eq!(DaemonState::Starting.to_string(), "starting");
        assert_eq!(DaemonState::Running.to_string(), "running");
        assert_eq!(DaemonState::Draining.to_string(), "draining");
        assert_eq!(DaemonState::Stopped.to_string(), "stopped");
    }

    #[test]
    fn ipc_request_ping_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 42,
            payload: IpcRequest::Ping,
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_response_pong_round_trip() {
        let msg = IpcMessage::Response {
            request_id: 42,
            payload: IpcResponse::Pong {
                protocol_version: 1,
                build_hash: "abc123".into(),
                uptime_secs: 100,
                sessions: 2,
                version: "0.1.0".into(),
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_event_round_trip() {
        let msg = IpcMessage::Event {
            payload: DaemonEvent::StateChanged {
                old: DaemonState::Ready,
                new: DaemonState::Running,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_subscribe_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 1,
            payload: IpcRequest::Subscribe {
                filter: vec![EventKind::StateChanged, EventKind::SessionCreated],
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn ipc_frame_length_prefix() {
        let msg = IpcMessage::Request {
            request_id: 0,
            payload: IpcRequest::GetState,
        };
        let encoded = encode_ipc(&msg).unwrap();
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(len, encoded.len() - 4);
    }

    #[test]
    fn daemon_event_kind() {
        let e = DaemonEvent::SessionCreated { session_id: 1 };
        assert_eq!(e.kind(), EventKind::SessionCreated);
    }

    #[test]
    fn ipc_pane_output_round_trip() {
        let msg = IpcMessage::Event {
            payload: DaemonEvent::PaneOutput {
                session_id: 1,
                pane_id: 1,
                data: vec![0x1b, b'[', b'H'],
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn set_workspace_state_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 7,
            payload: IpcRequest::SetWorkspaceState {
                session_id: 1,
                workspaces: vec![
                    WorkspaceInfo {
                        id: 1,
                        name: "main".into(),
                        order: 0,
                        pane_ids: vec![10, 11],
                        focused_pane: Some(10),
                    },
                    WorkspaceInfo {
                        id: 3,
                        name: "logs".into(),
                        order: 1,
                        pane_ids: vec![20],
                        focused_pane: Some(20),
                    },
                ],
                active_workspace: 1,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn get_workspaces_round_trip() {
        let msg = IpcMessage::Request {
            request_id: 8,
            payload: IpcRequest::GetWorkspaces { session_id: 1 },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn workspaces_response_round_trip() {
        let msg = IpcMessage::Response {
            request_id: 8,
            payload: IpcResponse::Workspaces {
                session_id: 1,
                workspaces: vec![WorkspaceInfo {
                    id: 1,
                    name: "default".into(),
                    order: 0,
                    pane_ids: vec![5],
                    focused_pane: Some(5),
                }],
                active_workspace: 1,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn workspace_changed_event_round_trip() {
        let msg = IpcMessage::Event {
            payload: DaemonEvent::WorkspaceChanged {
                session_id: 1,
                active_workspace: 3,
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn workspace_changed_event_kind() {
        let e = DaemonEvent::WorkspaceChanged {
            session_id: 1,
            active_workspace: 2,
        };
        assert_eq!(e.kind(), EventKind::WorkspaceChanged);
    }

    #[test]
    fn workspace_info_serde_json_round_trip() {
        let info = WorkspaceInfo {
            id: 2,
            name: "build".into(),
            order: 1,
            pane_ids: vec![100, 200],
            focused_pane: Some(100),
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: WorkspaceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, parsed);
    }

    #[test]
    fn persisted_session_workspace_defaults() {
        // Legacy JSON without workspace fields should deserialize with defaults.
        let json = r#"{"name":"old","panes":[]}"#;
        let session: PersistedSession = serde_json::from_str(json).unwrap();
        assert!(session.workspaces.is_empty());
        assert_eq!(session.active_workspace, 0);
    }

    #[test]
    fn persisted_session_with_workspaces_round_trip() {
        let session = PersistedSession {
            name: Some("test".into()),
            panes: vec![],
            workspaces: vec![WorkspaceInfo {
                id: 1,
                name: "main".into(),
                order: 0,
                pane_ids: vec![1],
                focused_pane: Some(1),
            }],
            active_workspace: 1,
        };
        let json = serde_json::to_string(&session).unwrap();
        let parsed: PersistedSession = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.workspaces.len(), 1);
        assert_eq!(parsed.active_workspace, 1);
        assert_eq!(parsed.workspaces[0].name, "main");
    }
}
