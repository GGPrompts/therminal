//! Therminal Protocol — lightweight wire protocol types for the therminal suite.
//!
//! This crate contains wire types, message bus types, and configuration
//! schemas. It has no GPU, async, or system dependencies — making it suitable
//! for lightweight consumers like `therminal-terminal`.

pub mod config;
pub mod ggl_types;
pub mod message;
pub mod pane;
pub mod state;

pub use config::{ConductorConfig, Layout};
pub use ggl_types::{
    AgentId, AgentState, ClaudeStatus, PaneInfo, ParseAgentIdError, SessionState, TaskState,
    ToolArgs, ToolDetails,
};
pub use message::{Message, MessageType};

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
