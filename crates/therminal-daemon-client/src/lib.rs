//! Lightweight IPC client for the Therminal daemon.
//!
//! Extracted from `therminal-daemon` so consumers (the GUI, headless tools)
//! can talk to the daemon without pulling in the server, MCP, persistence,
//! or file-watcher dependency stack.
//!
//! See `therminal-daemon` for the matching server implementation.

pub mod client;
pub mod framing;
pub mod ipc_transport;

pub use client::{
    DaemonClient, GUI_REQUEST_TIMEOUT, ping, request_shutdown, send_request,
    send_request_with_timeout,
};
