//! Therminal Protocol — lightweight wire protocol types for the therminal suite.
//!
//! This crate contains wire types, message bus types, and configuration
//! schemas. It has no GPU, async, or system dependencies — making it suitable
//! for lightweight consumers like `therminal-terminal`.

pub mod config;
pub mod daemon;
pub mod ggl_types;
pub mod message;
pub mod pane;
pub mod state;

pub use config::{ConductorConfig, Layout};
pub use daemon::{DaemonEvent, DaemonState, EventKind, IpcMessage, IpcRequest, IpcResponse};
pub use ggl_types::{
    AgentId, AgentState, ClaudeStatus, PaneInfo, ParseAgentIdError, SessionState, TaskState,
    ToolArgs, ToolDetails,
};
pub use message::{Message, MessageType};

// ── Canonical ID types ──────────────────────────────────────────────────
// These are the single source of truth for entity IDs across all crates.
// Using u64 keeps them Copy, Eq, Hash, and cheap to pass over IPC.

/// Unique identifier for a session.
pub type SessionId = u64;

/// Unique identifier for a window within a session.
pub type WindowId = u64;

/// Unique identifier for a pane.
pub type PaneId = u64;

/// Semantic region types for scrollback tagging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RegionKind {
    Prompt,
    Command,
    Output,
    Error,
    ToolCall,
    Thinking,
    Annotation,
}
