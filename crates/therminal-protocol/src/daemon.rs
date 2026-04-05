//! Daemon lifecycle wire types.
//!
//! These types are used for the daemon startup protocol, health checks,
//! and version handoff. They are MessagePack-serialized over Unix sockets.

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

// ── Framing helpers ───────────────────────────────────────────────────────

/// Serialize a daemon request to MessagePack bytes with a 4-byte length prefix.
pub fn encode_request(req: &DaemonRequest) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let payload = rmp_serde::to_vec(req)?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Serialize a daemon response to MessagePack bytes with a 4-byte length prefix.
pub fn encode_response(resp: &DaemonResponse) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let payload = rmp_serde::to_vec(resp)?;
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
}
