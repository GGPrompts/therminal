//! PTY management, OSC 633 interception, and state inference engine.

pub use therminal_protocol as protocol;

pub mod event_log;
pub mod input;
pub mod osc633;
pub mod state_inference;
pub mod terminal;
