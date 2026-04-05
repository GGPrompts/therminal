//! PTY management, OSC 633 interception, and state inference engine.

pub use therminal_protocol as protocol;

pub mod event_log;
pub mod input;
pub mod interceptor;
pub mod osc633;
pub mod process_detector;
pub mod pty;
pub mod region_index;
pub mod state_inference;
pub mod terminal;
