//! Daemon lifecycle wire types and IPC protocol.
//!
//! These types are used for the daemon startup protocol, health checks,
//! version handoff, and the full IPC request/response/event protocol.
//! They are MessagePack-serialized over Unix sockets with 4-byte
//! big-endian length-prefixed framing.

use serde::{Deserialize, Serialize};

/// Build hash embedded at compile time (git short hash + timestamp).
/// Used for version-mismatch detection during daemon handoff.
pub type BuildHash = String;

// ── Daemon requests ───────────────────────────────────────────────────────

/// A request sent from a client to the daemon over the control socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum DaemonRequest {
    /// Health check — daemon should reply with `Pong`.
    Ping,
    /// Request graceful shutdown for version handoff.
    GracefulShutdown,
}

// ── Daemon responses ──────────────────────────────────────────────────────

/// A response sent from the daemon to a client over the control socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "resp")]
pub enum DaemonResponse {
    /// Health check response with daemon metadata.
    Pong {
        /// Build hash of the running daemon.
        build_hash: BuildHash,
        /// Daemon uptime in seconds.
        uptime_secs: u64,
        /// Number of active sessions.
        sessions: u32,
        /// Crate version string.
        version: String,
    },
    /// Acknowledgement that graceful shutdown has been initiated.
    ShutdownAck,
    /// Handoff complete — new daemon has taken over.
    HandoffComplete,
    /// Error response.
    Error { message: String },
}

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
    GetSession { session_id: String },
    /// Create a new session.
    CreateSession { name: Option<String> },
    /// Destroy a session.
    DestroySession { session_id: String },
    /// Query daemon state.
    GetState,
    /// Send keys (input) to a specific pane.
    SendKeys { pane_id: String, keys: Vec<u8> },
    /// Capture pane content (terminal grid snapshot).
    CapturePane { pane_id: String },
    /// Split a pane (creates a new pane in the same session).
    SplitPane { pane_id: String, horizontal: bool },
    /// Kill (destroy) a specific pane.
    KillPane { pane_id: String },
    /// Select (focus) a specific pane.
    SelectPane { pane_id: String },
}

/// Typed IPC responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "resp")]
pub enum IpcResponse {
    /// Health check response.
    Pong {
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
    Sessions { session_ids: Vec<String> },
    /// Session details.
    SessionInfo {
        session_id: String,
        name: Option<String>,
        created_at_secs: u64,
    },
    /// Session created.
    SessionCreated { session_id: String },
    /// Session destroyed.
    SessionDestroyed { session_id: String },
    /// Current daemon state.
    State { state: DaemonState },
    /// Keys sent successfully.
    KeysSent { pane_id: String },
    /// Pane content captured.
    PaneCaptured {
        pane_id: String,
        lines: Vec<String>,
        cursor_col: usize,
        cursor_line: usize,
        cols: usize,
        rows: usize,
    },
    /// Pane split — new pane created.
    PaneSplit { new_pane_id: String },
    /// Pane killed.
    PaneKilled { pane_id: String },
    /// Pane selected (focused).
    PaneSelected { pane_id: String },
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
    SessionCreated { session_id: String },
    /// A session was destroyed.
    SessionDestroyed { session_id: String },
    /// Pane output data (for subscribed watchers).
    PaneOutput {
        session_id: String,
        pane_id: String,
        data: Vec<u8>,
    },
}

/// Event kind discriminant for subscription filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    StateChanged,
    SessionCreated,
    SessionDestroyed,
    PaneOutput,
}

impl DaemonEvent {
    /// Return the `EventKind` of this event.
    pub fn kind(&self) -> EventKind {
        match self {
            DaemonEvent::StateChanged { .. } => EventKind::StateChanged,
            DaemonEvent::SessionCreated { .. } => EventKind::SessionCreated,
            DaemonEvent::SessionDestroyed { .. } => EventKind::SessionDestroyed,
            DaemonEvent::PaneOutput { .. } => EventKind::PaneOutput,
        }
    }
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

/// Serialize a daemon request to MessagePack bytes with a 4-byte length prefix.
pub fn encode_request(req: &DaemonRequest) -> Result<Vec<u8>, EncodeFrameError> {
    let payload = rmp_serde::to_vec(req)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(EncodeFrameError::FrameTooLarge(payload.len()));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Serialize a daemon response to MessagePack bytes with a 4-byte length prefix.
pub fn encode_response(resp: &DaemonResponse) -> Result<Vec<u8>, EncodeFrameError> {
    let payload = rmp_serde::to_vec(resp)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(EncodeFrameError::FrameTooLarge(payload.len()));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode a daemon request from a MessagePack payload (without length prefix).
pub fn decode_request(data: &[u8]) -> Result<DaemonRequest, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

/// Decode a daemon response from a MessagePack payload (without length prefix).
pub fn decode_response(data: &[u8]) -> Result<DaemonResponse, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
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

/// Bridge: convert a legacy `DaemonRequest` into an `IpcRequest`.
impl From<DaemonRequest> for IpcRequest {
    fn from(req: DaemonRequest) -> Self {
        match req {
            DaemonRequest::Ping => IpcRequest::Ping,
            DaemonRequest::GracefulShutdown => IpcRequest::GracefulShutdown,
        }
    }
}

/// Bridge: convert an `IpcResponse` into a legacy `DaemonResponse` where possible.
impl TryFrom<IpcResponse> for DaemonResponse {
    type Error = ();
    fn try_from(resp: IpcResponse) -> Result<Self, ()> {
        match resp {
            IpcResponse::Pong {
                build_hash,
                uptime_secs,
                sessions,
                version,
            } => Ok(DaemonResponse::Pong {
                build_hash,
                uptime_secs,
                sessions,
                version,
            }),
            IpcResponse::ShutdownAck => Ok(DaemonResponse::ShutdownAck),
            IpcResponse::Error { message } => Ok(DaemonResponse::Error { message }),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ping_round_trip() {
        let req = DaemonRequest::Ping;
        let encoded = encode_request(&req).unwrap();
        // Skip 4-byte length prefix
        let decoded = decode_request(&encoded[4..]).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn request_shutdown_round_trip() {
        let req = DaemonRequest::GracefulShutdown;
        let encoded = encode_request(&req).unwrap();
        let decoded = decode_request(&encoded[4..]).unwrap();
        assert_eq!(req, decoded);
    }

    #[test]
    fn response_pong_round_trip() {
        let resp = DaemonResponse::Pong {
            build_hash: "abc1234-1711500000".into(),
            uptime_secs: 3600,
            sessions: 2,
            version: "0.1.0".into(),
        };
        let encoded = encode_response(&resp).unwrap();
        let decoded = decode_response(&encoded[4..]).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn response_error_round_trip() {
        let resp = DaemonResponse::Error {
            message: "something went wrong".into(),
        };
        let encoded = encode_response(&resp).unwrap();
        let decoded = decode_response(&encoded[4..]).unwrap();
        assert_eq!(resp, decoded);
    }

    #[test]
    fn daemon_state_display() {
        assert_eq!(DaemonState::Starting.to_string(), "starting");
        assert_eq!(DaemonState::Running.to_string(), "running");
        assert_eq!(DaemonState::Draining.to_string(), "draining");
        assert_eq!(DaemonState::Stopped.to_string(), "stopped");
    }

    #[test]
    fn length_prefix_is_correct() {
        let req = DaemonRequest::Ping;
        let encoded = encode_request(&req).unwrap();
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        assert_eq!(len, encoded.len() - 4);
    }

    // ── IPC message tests ──────────────────────────────────────────────────

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
        let e = DaemonEvent::SessionCreated {
            session_id: "s1".into(),
        };
        assert_eq!(e.kind(), EventKind::SessionCreated);
    }

    #[test]
    fn ipc_pane_output_round_trip() {
        let msg = IpcMessage::Event {
            payload: DaemonEvent::PaneOutput {
                session_id: "s1".into(),
                pane_id: "p1".into(),
                data: vec![0x1b, b'[', b'H'],
            },
        };
        let encoded = encode_ipc(&msg).unwrap();
        let decoded = decode_ipc(&encoded[4..]).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn legacy_request_to_ipc() {
        let legacy = DaemonRequest::Ping;
        let ipc: IpcRequest = legacy.into();
        assert_eq!(ipc, IpcRequest::Ping);
    }

    #[test]
    fn ipc_response_to_legacy() {
        let ipc = IpcResponse::ShutdownAck;
        let legacy: DaemonResponse = ipc.try_into().unwrap();
        assert_eq!(legacy, DaemonResponse::ShutdownAck);
    }

    #[test]
    fn ipc_sessions_response_no_legacy() {
        let ipc = IpcResponse::Sessions {
            session_ids: vec!["s1".into()],
        };
        let result: Result<DaemonResponse, ()> = ipc.try_into();
        assert!(result.is_err());
    }
}
