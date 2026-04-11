//! MCP (Model Context Protocol) server for Therminal.
//!
//! Exposes terminal session management tools via the standard MCP protocol,
//! allowing external tools (Claude Code, TUIs, dashboards) to interact with
//! daemon sessions.
//!
//! Trust enforcement is applied to every tool call: the agent's identity is
//! extracted from the MCP `initialize` handshake, looked up against the
//! `[trust]` config section, and checked against the tool's required tier.
//! Destructive tools are additionally rate-limited.
//!
//! The server listens on a platform-appropriate IPC endpoint (Unix domain
//! socket on Linux/macOS, named pipe on Windows) and uses the `rmcp` crate
//! for protocol handling.
//!
//! This module is split across several files:
//! - `mod.rs` — `TherminalMcpServer` struct definition + construction
//! - `dispatch.rs` — `ServerHandler` trait impl + tool dispatch routing
//! - `types.rs` — param / result structs (MCP tool arguments and return values)
//! - `helpers.rs` — shared free functions (serialization, pane lookup, grid rendering)
//! - `tools.rs` — tool handler implementations + `tool_definitions()`
//! - `resources.rs` — MCP resource list/read/subscribe/unsubscribe logic
//! - `transport.rs` — Unix socket / Windows named pipe server lifecycle
//! - `tests.rs` — unit and integration tests

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::{CallToolResult, Content, ErrorCode};

use therminal_core::config::TrustConfig;

use therminal_harness_claude::jsonl_tailer::TaggedAgentEvent;

/// Map from escalation_id -> oneshot sender for trust escalation responses.
/// Inserted when a trust escalation is raised; removed on resolution via
/// IpcRequest::ResolveTrustEscalation.
pub type EscalationResponseMap =
    std::sync::Mutex<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<bool>>>;

use crate::session::SessionManager;
use crate::trust::{
    AgentIdentity, RateLimiter, SessionGrants, TrustCheckResult, check_resource_access,
    check_tool_access_with_grants,
};
use therminal_terminal::agent_registry::TaggedAgentEvent as TaggedAgentLifecycleEvent;
use therminal_terminal::semantic_patterns::PatternEngine;

pub(super) mod deser_compat;
mod dispatch;
pub(super) mod helpers;
pub mod resources;
pub mod tools;
pub mod transport;
pub(super) mod types;

pub use transport::start_mcp_server;

// Re-export all pub(crate) items from types and helpers so that sibling
// modules (tools.rs, resources.rs) can continue importing via `super::`.
pub(crate) use helpers::*;
pub(crate) use types::*;

// ── MCP Server ──────────────────────────────────────────────────────────

/// Therminal MCP server handler.
///
/// Wraps a shared `SessionManager` and exposes session/pane operations as
/// MCP tools and terminal resources. Each tool/resource call is gated by
/// trust tier enforcement and audit-logged.
///
/// Resources follow the `terminal://pane/{pane_id}/content` and
/// `terminal://pane/{pane_id}/output` URI scheme. Subscriptions to output
/// resources spawn a background task that forwards `DaemonEvent::PaneOutput`
/// events as MCP resource-updated notifications.
///
/// Each accepted MCP connection constructs its own `TherminalMcpServer`
/// instance with a freshly-minted `connection_id` (drawn from a process-wide
/// monotonic counter). The `connection_id` is the trust key used by
/// [`SessionGrants`] so that grants approved on one connection cannot be
/// inherited by another, even if the new connection claims the same
/// self-reported `Implementation.name` (tn-yuu4).
pub struct TherminalMcpServer {
    /// Daemon-assigned per-connection identifier (tn-yuu4). Generated at
    /// `new_with_bus` time from a process-wide monotonic counter and
    /// threaded into every `AgentIdentity` constructed for this connection.
    pub(super) connection_id: u64,
    pub(super) session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    pub(super) trust_config: Arc<TrustConfig>,
    pub(super) rate_limiter: Arc<RateLimiter>,
    /// Optional broadcast sender for the Claude agent-event pipeline. Cloned
    /// per `subscribe(therminal://claude/events)` call so each MCP client gets
    /// its own broadcast::Receiver. `None` if the pipeline failed to start.
    pub(super) claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
    /// Active resource subscriptions: maps URI -> JoinHandle for the background
    /// forwarding task. Protected by a std Mutex since we only hold it briefly.
    pub(super) subscriptions: std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Per-connection ring buffer of recent Claude agent events. Filled by the
    /// background subscription task; drained by `read_resource` calls against
    /// `therminal://claude/events`. Cap is small — clients should re-read on
    /// every `notifications/resources/updated`.
    pub(super) claude_event_buffer:
        Arc<std::sync::Mutex<std::collections::VecDeque<TaggedAgentEvent>>>,
    /// Optional broadcast sender for agent lifecycle events from the
    /// `AgentRegistry`. Cloned per `subscribe(therminal://agents/events)` so
    /// each MCP client gets its own broadcast::Receiver.
    pub(super) agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
    /// Per-connection ring buffer of recent agent lifecycle events. Filled by
    /// the subscription forwarder; drained by `read_resource` against
    /// `therminal://agents/events`.
    pub(super) agent_event_buffer:
        Arc<std::sync::Mutex<std::collections::VecDeque<TaggedAgentLifecycleEvent>>>,
    /// Shared pattern engine (tn-yrjd). `None` when the daemon is built
    /// without pattern support (tests, headless smoke).
    pub(super) pattern_engine: Option<Arc<PatternEngine>>,
    /// Unified event bus (tn-xula). `None` only in legacy unit tests that
    /// construct the server through the historical 6-arg `new`.
    pub(super) event_bus: Option<Arc<crate::event_bus::EventBus>>,
    /// Session-scoped grants for trust escalation approvals (tn-b99).
    pub(super) session_grants: Arc<SessionGrants>,
    /// Pending escalation response channels (tn-b99).
    pub(super) escalation_responses: Arc<EscalationResponseMap>,
    /// Broadcast sender for TrustEscalation events to the GUI (tn-b99).
    pub(super) daemon_event_tx:
        Option<tokio::sync::broadcast::Sender<therminal_protocol::DaemonEvent>>,
}

pub(super) const CLAUDE_EVENT_BUFFER_CAP: usize = 256;
pub(super) const AGENT_EVENT_BUFFER_CAP: usize = 256;

/// Monotonic counter for escalation IDs (tn-b99).
static ESCALATION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Monotonic counter for per-connection MCP identifiers (tn-yuu4). The trust
/// layer keys session-scoped grants on this id rather than the client-supplied
/// agent name, so a fresh connection always starts with an empty grant set
/// even if the new client spoofs a previously-approved `Implementation.name`.
static CONNECTION_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Allocate the next per-connection MCP identifier (tn-yuu4).
pub(super) fn next_connection_id() -> u64 {
    CONNECTION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// URI for the global Claude agent-event stream.
pub(super) const CLAUDE_EVENTS_URI: &str = "therminal://claude/events";
/// URI for the global agent lifecycle event stream backed by `AgentRegistry`.
pub(super) const AGENT_EVENTS_URI: &str = "therminal://agents/events";
/// URI prefix for the unified event bus (tn-xula). Filter parameters live in
/// the optional query string per `docs/event-bus-spec.md` §4.
pub(super) const EVENTS_URI: &str = "terminal://events";

impl TherminalMcpServer {
    /// Create a new MCP server backed by the given session manager and trust config.
    pub fn new(
        session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
        trust_config: Arc<TrustConfig>,
        rate_limiter: Arc<RateLimiter>,
        claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
        agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
        pattern_engine: Option<Arc<PatternEngine>>,
    ) -> Self {
        Self::new_with_bus(
            session_mgr,
            trust_config,
            rate_limiter,
            claude_events,
            agent_events,
            pattern_engine,
            None,
        )
    }

    /// Construct a server wired to the unified event bus (tn-xula).
    ///
    /// Allocates a fresh `connection_id` from the process-wide
    /// `CONNECTION_COUNTER` (tn-yuu4) — every accepted MCP connection gets
    /// its own server instance and therefore its own connection id.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_bus(
        session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
        trust_config: Arc<TrustConfig>,
        rate_limiter: Arc<RateLimiter>,
        claude_events: Option<tokio::sync::broadcast::Sender<TaggedAgentEvent>>,
        agent_events: Option<tokio::sync::broadcast::Sender<TaggedAgentLifecycleEvent>>,
        pattern_engine: Option<Arc<PatternEngine>>,
        event_bus: Option<Arc<crate::event_bus::EventBus>>,
    ) -> Self {
        Self {
            connection_id: next_connection_id(),
            session_mgr,
            trust_config,
            rate_limiter,
            claude_events,
            subscriptions: std::sync::Mutex::new(HashMap::new()),
            claude_event_buffer: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(CLAUDE_EVENT_BUFFER_CAP),
            )),
            agent_events,
            agent_event_buffer: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(AGENT_EVENT_BUFFER_CAP),
            )),
            pattern_engine,
            event_bus,
            session_grants: Arc::new(SessionGrants::new()),
            escalation_responses: Arc::new(std::sync::Mutex::new(HashMap::new())),
            daemon_event_tx: None,
        }
    }

    /// Enforce trust tier and rate limiting for the given tool call.
    ///
    /// Returns `Ok(())` if allowed, or an `Err(CallToolResult)` with
    /// a permission-denied error to return to the client.
    pub(super) async fn enforce_trust(
        &self,
        tool_name: &str,
        agent: &AgentIdentity,
    ) -> Result<(), CallToolResult> {
        match check_tool_access_with_grants(
            tool_name,
            agent,
            &self.trust_config,
            &self.rate_limiter,
            Some(&self.session_grants),
        ) {
            TrustCheckResult::Allowed => Ok(()),
            TrustCheckResult::Denied(reason) => {
                Err(CallToolResult::error(vec![Content::text(reason)]))
            }
            TrustCheckResult::Escalation {
                agent_name,
                tool_name: tn,
                current_tier,
                required_tier,
            } => {
                if let Some(ref tx) = self.daemon_event_tx {
                    let esc_id =
                        ESCALATION_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                    {
                        let mut map = self
                            .escalation_responses
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        map.insert(esc_id, resp_tx);
                    }
                    let event = therminal_protocol::DaemonEvent::TrustEscalation {
                        escalation_id: esc_id,
                        agent_name: agent_name.clone(),
                        tool_name: tn.clone(),
                        current_tier: current_tier.to_string(),
                        required_tier: required_tier.to_string(),
                    };
                    let _ = tx.send(event);
                    match tokio::time::timeout(std::time::Duration::from_secs(60), resp_rx).await {
                        Ok(Ok(true)) => {
                            // tn-yuu4: grant is keyed by this connection's
                            // daemon-assigned id, not the client-supplied
                            // agent name. Spoofing the name on a future
                            // connection cannot inherit this approval.
                            self.session_grants
                                .grant(self.connection_id, &agent_name, &tn);
                            Ok(())
                        }
                        _ => {
                            let mut map = self
                                .escalation_responses
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            map.remove(&esc_id);
                            Err(CallToolResult::error(vec![Content::text(format!(
                                "trust escalation denied: agent {:?} requested {:?} (requires {})",
                                agent_name, tn, required_tier,
                            ))]))
                        }
                    }
                } else {
                    Err(CallToolResult::error(vec![Content::text(format!(
                        "permission denied: agent {:?} has tier {}, tool {:?} requires {}",
                        agent_name, current_tier, tn, required_tier,
                    ))]))
                }
            }
        }
    }

    /// Synchronous trust check for tests (no escalation support).
    #[cfg(test)]
    pub(crate) fn enforce_trust_sync(
        &self,
        tool_name: &str,
        agent: &AgentIdentity,
    ) -> Result<(), CallToolResult> {
        use crate::trust::check_tool_access;
        match check_tool_access(tool_name, agent, &self.trust_config, &self.rate_limiter) {
            TrustCheckResult::Allowed => Ok(()),
            TrustCheckResult::Denied(reason) => {
                Err(CallToolResult::error(vec![Content::text(reason)]))
            }
            TrustCheckResult::Escalation {
                tool_name: reason, ..
            } => Err(CallToolResult::error(vec![Content::text(reason)])),
        }
    }

    /// Enforce trust tier for a resource read.
    pub(super) fn enforce_resource_trust(
        &self,
        uri: &str,
        agent: &AgentIdentity,
    ) -> Result<(), ErrorData> {
        match check_resource_access(uri, agent, &self.trust_config) {
            TrustCheckResult::Allowed => Ok(()),
            TrustCheckResult::Denied(reason) => {
                Err(ErrorData::new(ErrorCode(-32001), reason, None))
            }
            TrustCheckResult::Escalation { .. } => Err(ErrorData::new(
                ErrorCode(-32001),
                "trust escalation not supported for resource reads".to_string(),
                None,
            )),
        }
    }

    /// Parse a pane resource URI into (pane_id, resource_kind).
    ///
    /// Accepts `terminal://pane/{id}/content`, `terminal://pane/{id}/output`,
    /// and `terminal://pane/{id}/scrollback`.
    pub(super) fn parse_pane_uri(uri: &str) -> Option<(u64, &str)> {
        let rest = uri.strip_prefix("terminal://pane/")?;
        let (id_str, kind) = rest.split_once('/')?;
        let pane_id: u64 = id_str.parse().ok()?;
        match kind {
            "content" | "output" | "scrollback" => Some((pane_id, kind)),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
