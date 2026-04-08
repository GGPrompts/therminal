//! PTY management, OSC 633 interception, and state inference engine.

pub use therminal_protocol as protocol;

pub mod agent_registry;
pub mod event_log;
pub mod hotspot_detection;
pub mod input;
pub mod interceptor;
pub mod osc633;
pub mod osc_registry;
pub mod process_detector;
pub mod pty;
pub mod pty_runtime;
pub mod region_index;
pub mod semantic_patterns;
pub mod state_inference;
pub mod terminal;

pub use osc_registry::{
    HarnessEvent, HarnessOscHandler, OscHandlerRegistry, OscRegistrationError, RESERVED_CODE_MAX,
    TaggedHarnessEvent,
};
