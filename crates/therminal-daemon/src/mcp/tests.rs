use std::sync::Arc;

use therminal_core::config::{AgentTrust, TrustConfig, TrustTier};
use tokio::sync::broadcast;

use crate::session::SessionManager;
use crate::trust::{AgentIdentity, RateLimiter};

use super::TherminalMcpServer;
use super::tools::tool_definitions;

// ── Fixture helpers ─────────────────────────────────────────────────

/// Build a `TherminalMcpServer` with a real (empty) `SessionManager` and
/// the given `TrustConfig`. No PTY is spawned — the session manager is
/// empty so any tool that looks up panes/sessions will return "not found"
/// rather than doing any real work.
pub(crate) fn make_server(trust_config: TrustConfig) -> TherminalMcpServer {
    let (event_tx, _) = broadcast::channel(16);
    let session_mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new(event_tx)));
    let trust_config = Arc::new(trust_config);
    let rate_limiter = Arc::new(RateLimiter::new(100));
    TherminalMcpServer::new(session_mgr, trust_config, rate_limiter, None, None, None)
}

/// Build an `AgentIdentity` with the given name. The `connection_id` is
/// fixed to `0` for tests that don't exercise the per-connection grant key
/// (tn-yuu4); tests that DO care construct the identity directly.
fn agent(name: &str) -> AgentIdentity {
    AgentIdentity {
        name: name.to_string(),
        connection_id: 0,
    }
}

/// `TrustConfig` where the default tier is `Sandboxed` (most restrictive).
fn sandboxed_config() -> TrustConfig {
    TrustConfig {
        default_tier: TrustTier::Sandboxed,
        ..TrustConfig::default()
    }
}

/// `TrustConfig` where the default tier is `Supervised` (can call Writer tools).
fn supervised_config() -> TrustConfig {
    TrustConfig {
        default_tier: TrustTier::Supervised,
        ..TrustConfig::default()
    }
}

/// `TrustConfig` where the default tier is `Trusted` (full Admin access).
fn trusted_config() -> TrustConfig {
    TrustConfig {
        default_tier: TrustTier::Trusted,
        ..TrustConfig::default()
    }
}

// ── string-or-int param coercion (tn-ad0g) ─────────────────────────
//
// Regression guard: at least one MCP client (Claude Code on Windows)
// serializes numeric tool arguments as JSON strings, e.g.
//   {"pane_id":"1"}  /  {"limit":"5"}  /  {"threshold_percent":"50"}
//
// The original strict `#[derive(Deserialize)]` rejected these with
// `-32602 invalid type: string "1", expected u64`. The
// `deser_compat::*_flexible` helpers teach every tool param struct to
// accept either a JSON number *or* a stringified number, restoring
// round-trip for the affected calls.
//
// These tests feed raw JSON objects through `parse_args` — the same
// entry point used by `call_tool` — so they exercise the real
// dispatch path, not a unit test of the deserializer helpers.

fn parse<T: serde::de::DeserializeOwned>(raw: &str) -> T {
    let value: serde_json::Value = serde_json::from_str(raw).expect("json");
    let map = value
        .as_object()
        .cloned()
        .expect("arguments must be a JSON object");
    super::parse_args::<T>(map).expect("parse_args should accept stringified numeric args")
}

#[test]
fn pane_id_param_accepts_stringified_pane_id() {
    // The exact wire shape observed from Claude Code on Windows
    // during tn-ad0g Windows verification.
    let params: super::PaneIdParam = parse(r#"{"pane_id":"1"}"#);
    assert_eq!(params.pane_id, 1);
}

#[test]
fn pane_id_param_accepts_native_integer() {
    let params: super::PaneIdParam = parse(r#"{"pane_id":42}"#);
    assert_eq!(params.pane_id, 42);
}

#[test]
fn session_id_param_accepts_stringified_id() {
    let params: super::SessionIdParam = parse(r#"{"session_id":"3"}"#);
    assert_eq!(params.session_id, 3);
}

#[test]
fn get_pane_geometry_accepts_stringified_pane_id() {
    let params: super::GetPaneGeometryParam = parse(r#"{"pane_id":"7"}"#);
    assert_eq!(params.pane_id, 7);
}

#[test]
fn get_hotspots_accepts_stringified_pane_id_and_limit() {
    // Matches `terminal.semantic.get_hotspots { pane_id: 1 }` from the
    // tn-ad0g bug report, plus a stringified limit for completeness.
    let params: super::GetHotspotsParam = parse(r#"{"pane_id":"1","limit":"5"}"#);
    assert_eq!(params.pane_id, 1);
    assert_eq!(params.limit, Some(5));
}

#[test]
fn query_commands_accepts_stringified_pane_id_and_limit() {
    // Matches `terminal.semantic.query_commands { pane_id: 1, limit: 5 }`
    // from the tn-ad0g bug report.
    let params: super::QueryCommandsParam = parse(r#"{"pane_id":"1","limit":"5"}"#);
    assert_eq!(params.pane_id, 1);
    assert_eq!(params.limit, Some(5));
    assert_eq!(params.since_line, None);
}

#[test]
fn query_commands_accepts_native_ints() {
    // Sanity: the flexible deserializer must still accept well-formed
    // integer input. If this regresses, all MCP clients break.
    let params: super::QueryCommandsParam = parse(r#"{"pane_id":1,"limit":5,"since_line":10}"#);
    assert_eq!(params.pane_id, 1);
    assert_eq!(params.limit, Some(5));
    assert_eq!(params.since_line, Some(10));
}

#[test]
fn query_events_accepts_stringified_timestamp() {
    let params: super::QueryEventsParam =
        parse(r#"{"pane_id":"2","since_timestamp_secs":"1700000000","limit":"20"}"#);
    assert_eq!(params.pane_id, 2);
    assert_eq!(params.since_timestamp_secs, Some(1_700_000_000));
    assert_eq!(params.limit, Some(20));
}

#[test]
fn spawn_pane_accepts_stringified_session_id() {
    // Matches `terminal.panes.create { session_id: 1 }` from the
    // tn-ad0g bug report. `split_from` is absent → None.
    let params: super::SpawnPaneParam = parse(r#"{"session_id":"1"}"#);
    assert_eq!(params.session_id, Some(1));
    assert_eq!(params.split_from, None);
}

#[test]
fn spawn_pane_accepts_stringified_split_from() {
    let params: super::SpawnPaneParam =
        parse(r#"{"split_from":"5","split_direction":"horizontal"}"#);
    assert_eq!(params.split_from, Some(5));
    assert_eq!(params.split_direction.as_deref(), Some("horizontal"));
}

#[test]
fn spawn_pane_accepts_startup_command() {
    let params: super::SpawnPaneParam =
        parse(r#"{"split_from":"5","startup_command":"echo hello"}"#);
    assert_eq!(params.split_from, Some(5));
    assert_eq!(params.startup_command.as_deref(), Some("echo hello"));
}

#[test]
fn list_panes_accepts_stringified_session_id() {
    let params: super::ListPanesParam = parse(r#"{"session_id":"4"}"#);
    assert_eq!(params.session_id, Some(4));
}

#[test]
fn list_panes_missing_session_id_is_none() {
    let params: super::ListPanesParam = parse(r#"{}"#);
    assert_eq!(params.session_id, None);
}

#[test]
fn wait_for_output_accepts_stringified_timeout() {
    let params: super::WaitForOutputParam =
        parse(r#"{"pane_id":"1","pattern":"done","timeout_ms":"5000","since_line":"10"}"#);
    assert_eq!(params.pane_id, 1);
    assert_eq!(params.pattern, "done");
    assert_eq!(params.timeout_ms, 5000);
    assert_eq!(params.since_line, Some(10));
}

#[test]
fn wait_for_output_default_timeout_when_omitted() {
    let params: super::WaitForOutputParam = parse(r#"{"pane_id":1,"pattern":"x"}"#);
    assert_eq!(params.timeout_ms, 30_000);
}

#[test]
fn get_workspace_layout_accepts_stringified_ids() {
    let params: super::GetWorkspaceLayoutParam = parse(r#"{"workspace_id":"2","session_id":"3"}"#);
    assert_eq!(params.workspace_id, 2);
    assert_eq!(params.session_id, Some(3));
}

#[test]
fn find_with_capacity_accepts_stringified_threshold() {
    let params: super::FindWithCapacityParam = parse(r#"{"threshold_percent":"50"}"#);
    assert!((params.threshold_percent - 50.0).abs() < 1e-6);
}

#[test]
fn find_with_capacity_accepts_stringified_float() {
    let params: super::FindWithCapacityParam = parse(r#"{"threshold_percent":"37.5"}"#);
    assert!((params.threshold_percent - 37.5).abs() < 1e-6);
}

#[test]
fn tag_pane_accepts_stringified_pane_id() {
    let params: super::TagPaneParam = parse(r#"{"pane_id":"8","tags":{"issue":"tn-1"}}"#);
    assert_eq!(params.pane_id, 8);
    assert_eq!(params.tags.get("issue").map(String::as_str), Some("tn-1"));
}

#[test]
fn untag_pane_accepts_stringified_pane_id() {
    let params: super::UntagPaneParam = parse(r#"{"pane_id":"9","keys":["issue"]}"#);
    assert_eq!(params.pane_id, 9);
    assert_eq!(params.keys.as_ref().unwrap().len(), 1);
}

#[test]
fn write_to_pane_accepts_stringified_pane_id() {
    let params: super::WriteToPaneParam = parse(r#"{"pane_id":"1","input":"ls\n"}"#);
    assert_eq!(params.pane_id, 1);
    assert_eq!(params.input, "ls\n");
}

/// Guard that the `deserialize_with` shim doesn't alter the JSON Schema
/// exposed to clients. We still advertise `integer`-typed fields; the
/// accepted-string behavior is a server-side permissive decode only.
#[test]
fn tool_schema_still_declares_pane_id_as_integer() {
    use rmcp::handler::server::tool::schema_for_type;
    // `schema_for_type::<T>()` returns `Arc<Map<String, Value>>` — the
    // MCP `inputSchema`. Drill into `properties.pane_id.type` and
    // assert it is `"integer"`, not `"string"`.
    let schema = schema_for_type::<super::PaneIdParam>();
    let json: serde_json::Value = serde_json::to_value(&*schema).expect("schema serialises");
    let ty = json
        .get("properties")
        .and_then(|p| p.get("pane_id"))
        .and_then(|f| f.get("type"))
        .expect("pane_id.type present");
    assert_eq!(ty, &serde_json::Value::String("integer".to_string()));
}

/// End-to-end: go all the way through `handle_get_pane_geometry` with
/// a stringified pane_id. This is the Windows-client failure mode from
/// tn-ad0g reproduced in a unit test. We expect a "pane not found"
/// tool error — NOT a transport-level `invalid parameters` error.
#[tokio::test]
async fn handle_get_pane_geometry_stringified_pane_id_round_trips() {
    let server = make_server(trusted_config());
    let raw: serde_json::Value = serde_json::from_str(r#"{"pane_id":"999999"}"#).unwrap();
    let map = raw.as_object().cloned().unwrap();
    let params: super::GetPaneGeometryParam =
        super::parse_args(map).expect("parse_args must accept stringified pane_id");
    let result = server
        .handle_get_pane_geometry(params)
        .await
        .expect("handler ok at transport level");
    // The pane doesn't exist, so the handler returns a tool error.
    // The important assertion is that we got *past* parse_args —
    // before the fix, `parse_args` itself would fail with `-32602`.
    assert_eq!(result.is_error, Some(true));
}

// ── parse_pane_uri ──────────────────────────────────────────────────

#[test]
fn parse_pane_uri_content() {
    let result = TherminalMcpServer::parse_pane_uri("terminal://pane/42/content");
    assert_eq!(result, Some((42, "content")));
}

#[test]
fn parse_pane_uri_output() {
    let result = TherminalMcpServer::parse_pane_uri("terminal://pane/7/output");
    assert_eq!(result, Some((7, "output")));
}

#[test]
fn parse_pane_uri_scrollback() {
    let result = TherminalMcpServer::parse_pane_uri("terminal://pane/13/scrollback");
    assert_eq!(result, Some((13, "scrollback")));
}

#[test]
fn parse_pane_uri_rejects_unknown_kind() {
    assert!(TherminalMcpServer::parse_pane_uri("terminal://pane/1/hotspot").is_none());
}

#[test]
fn parse_pane_uri_rejects_malformed() {
    assert!(TherminalMcpServer::parse_pane_uri("terminal://pane/notanumber/content").is_none());
    assert!(TherminalMcpServer::parse_pane_uri("http://example.com").is_none());
    assert!(TherminalMcpServer::parse_pane_uri("").is_none());
}

// ── split_file_path_parts ───────────────────────────────────────────

#[test]
fn split_file_path_parts_with_line_col() {
    let (path, suffix) = super::split_file_path_parts("src/main.rs:42:5");
    assert_eq!(path, "src/main.rs");
    assert_eq!(suffix, ":42:5");
}

#[test]
fn split_file_path_parts_no_suffix() {
    let (path, suffix) = super::split_file_path_parts("src/main.rs");
    assert_eq!(path, "src/main.rs");
    assert_eq!(suffix, "");
}

// ── build_content_preview ───────────────────────────────────────────

#[test]
fn build_content_preview_uses_command_field() {
    use std::time::Instant;
    use therminal_terminal::region_index::{Region, RegionKind};
    let mut region = Region {
        kind: RegionKind::Command,
        start_line: 0,
        end_line: None,
        metadata: std::collections::HashMap::new(),
        timestamp: Instant::now(),
    };
    region
        .metadata
        .insert("command".to_string(), "cargo build".to_string());
    region
        .metadata
        .insert("cwd".to_string(), "/home/user".to_string());
    let preview = super::build_content_preview(&region);
    assert_eq!(preview, "cargo build");
}

#[test]
fn build_content_preview_falls_back_to_cwd() {
    use std::time::Instant;
    use therminal_terminal::region_index::{Region, RegionKind};
    let mut region = Region {
        kind: RegionKind::Prompt,
        start_line: 0,
        end_line: None,
        metadata: std::collections::HashMap::new(),
        timestamp: Instant::now(),
    };
    region
        .metadata
        .insert("cwd".to_string(), "/home/user/projects".to_string());
    let preview = super::build_content_preview(&region);
    assert_eq!(preview, "/home/user/projects");
}

#[test]
fn build_content_preview_truncates_long_content() {
    use std::time::Instant;
    use therminal_terminal::region_index::{Region, RegionKind};
    let mut region = Region {
        kind: RegionKind::Output,
        start_line: 0,
        end_line: None,
        metadata: std::collections::HashMap::new(),
        timestamp: Instant::now(),
    };
    let long = "x".repeat(300);
    region.metadata.insert("command".to_string(), long);
    let preview = super::build_content_preview(&region);
    assert_eq!(preview.len(), 200);
    assert!(preview.ends_with("..."));
}

// ── enforce_trust (Observer tools) ─────────────────────────────────

/// All Observer tools must be accessible to a Sandboxed agent.
#[test]
fn sandboxed_agent_can_call_all_observer_tools() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    let observer_tools = [
        "terminal.sessions.list",
        "terminal.sessions.get",
        "terminal.panes.list",
        "terminal.panes.get_geometry",
        "terminal.panes.get_content",
        "terminal.panes.get_summary",
        "terminal.panes.peek",
        "terminal.panes.wait_for_output",
        "terminal.semantic.query_history",
        "terminal.semantic.query_commands",
        "terminal.panes.query_events",
        "terminal.semantic.get_hotspots",
        "terminal.workspaces.list",
        "terminal.workspaces.get_layout",
        "terminal.agents.list",
        "terminal.agents.get_details",
        "terminal.agents.get_status",
        "terminal.agents.get_session_detail",
        "terminal.agents.get_cadence",
        "terminal.agents.find_with_capacity",
        "terminal.patterns.stats",
        "terminal.events.stats",
    ];
    for tool in &observer_tools {
        assert!(
            server.enforce_trust_sync(tool, &agent).is_ok(),
            "expected Sandboxed to be allowed for Observer tool: {tool}"
        );
    }
}

// ── enforce_trust (Writer tools) ────────────────────────────────────

/// A Sandboxed agent must be denied all Writer tools.
#[test]
fn sandboxed_agent_denied_writer_tools() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    let writer_tools = [
        "terminal.sessions.create",
        "terminal.panes.write",
        "terminal.panes.create",
    ];
    for tool in &writer_tools {
        let result = server.enforce_trust_sync(tool, &agent);
        assert!(
            result.is_err(),
            "expected Sandboxed to be denied for Writer tool: {tool}"
        );
    }
}

/// A Supervised agent must be allowed all Writer tools.
#[test]
fn supervised_agent_can_call_writer_tools() {
    let server = make_server(supervised_config());
    let agent = agent("supervised-bot");
    let writer_tools = [
        "terminal.sessions.create",
        "terminal.panes.write",
        "terminal.panes.create",
    ];
    for tool in &writer_tools {
        assert!(
            server.enforce_trust_sync(tool, &agent).is_ok(),
            "expected Supervised to be allowed for Writer tool: {tool}"
        );
    }
}

// ── enforce_trust (Admin tools) ─────────────────────────────────────

/// A Sandboxed agent must be denied all Admin tools.
#[test]
fn sandboxed_agent_denied_admin_tools() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    for tool in &["terminal.sessions.destroy", "terminal.panes.destroy"] {
        let result = server.enforce_trust_sync(tool, &agent);
        assert!(
            result.is_err(),
            "expected Sandboxed to be denied for Admin tool: {tool}"
        );
    }
}

/// A Supervised agent must be denied Admin tools.
#[test]
fn supervised_agent_denied_admin_tools() {
    let server = make_server(supervised_config());
    let agent = agent("supervised-bot");
    for tool in &["terminal.sessions.destroy", "terminal.panes.destroy"] {
        let result = server.enforce_trust_sync(tool, &agent);
        assert!(
            result.is_err(),
            "expected Supervised to be denied for Admin tool: {tool}"
        );
    }
}

/// A Trusted agent must be allowed all Admin tools.
#[test]
fn trusted_agent_can_call_admin_tools() {
    let server = make_server(trusted_config());
    let agent = agent("trusted-bot");
    for tool in &["terminal.sessions.destroy", "terminal.panes.destroy"] {
        assert!(
            server.enforce_trust_sync(tool, &agent).is_ok(),
            "expected Trusted to be allowed for Admin tool: {tool}"
        );
    }
}

// ── enforce_trust (all tools parameterized) ─────────────────────────

/// Exhaustive: every tool has a known category — Trusted can call all.
#[test]
fn trusted_agent_can_call_all_tools() {
    let server = make_server(trusted_config());
    let agent = agent("trusted-bot");
    let all_tools = [
        // Observer
        "terminal.sessions.list",
        "terminal.sessions.get",
        "terminal.panes.list",
        "terminal.panes.get_geometry",
        "terminal.panes.get_content",
        "terminal.panes.get_summary",
        "terminal.panes.peek",
        "terminal.panes.wait_for_output",
        "terminal.semantic.query_history",
        "terminal.semantic.query_commands",
        "terminal.panes.query_events",
        "terminal.semantic.get_hotspots",
        "terminal.workspaces.list",
        "terminal.workspaces.get_layout",
        "terminal.agents.list",
        "terminal.agents.get_details",
        "terminal.agents.get_status",
        "terminal.agents.get_session_detail",
        "terminal.agents.get_cadence",
        "terminal.agents.find_with_capacity",
        "terminal.patterns.stats",
        "terminal.events.stats",
        // Writer
        "terminal.sessions.create",
        "terminal.panes.write",
        "terminal.panes.create",
        // Admin
        "terminal.sessions.destroy",
        "terminal.panes.destroy",
    ];
    assert_eq!(all_tools.len(), 27, "expected exactly 27 tools");
    for tool in &all_tools {
        assert!(
            server.enforce_trust_sync(tool, &agent).is_ok(),
            "expected Trusted to be allowed for: {tool}"
        );
    }
}

// ── enforce_trust (per-agent allowlist) ─────────────────────────────

/// Trust bypass regression: an agent with per-agent `allowed_tools` must
/// not gain access to tools not in the list, even if the tier would
/// otherwise permit them.
#[test]
fn per_agent_allowlist_restricts_trusted_agent() {
    let mut config = trusted_config();
    config.agents.insert(
        "restricted-claude".to_string(),
        AgentTrust {
            tier: TrustTier::Trusted,
            allowed_tools: Some(vec!["terminal.sessions.list".to_string()]),
        },
    );
    let server = make_server(config);
    let agent = agent("restricted-claude");

    // Allowed tool must pass.
    assert!(
        server
            .enforce_trust_sync("terminal.sessions.list", &agent)
            .is_ok()
    );

    // All other tools must be denied regardless of tier.
    for tool in &[
        "terminal.sessions.get",
        "terminal.sessions.create",
        "terminal.sessions.destroy",
        "terminal.panes.list",
        "terminal.panes.write",
        "terminal.panes.destroy",
    ] {
        assert!(
            server.enforce_trust_sync(tool, &agent).is_err(),
            "expected allowlist to deny: {tool}"
        );
    }
}

/// An empty allowlist must deny every tool call, even for a Trusted agent.
#[test]
fn per_agent_empty_allowlist_denies_all_tools() {
    let mut config = trusted_config();
    config.agents.insert(
        "locked-agent".to_string(),
        AgentTrust {
            tier: TrustTier::Trusted,
            allowed_tools: Some(vec![]),
        },
    );
    let server = make_server(config);
    let agent = agent("locked-agent");
    for tool in &[
        "terminal.sessions.list",
        "terminal.sessions.create",
        "terminal.sessions.destroy",
    ] {
        assert!(
            server.enforce_trust_sync(tool, &agent).is_err(),
            "expected empty allowlist to deny: {tool}"
        );
    }
}

// ── enforce_trust (rate limiting) ───────────────────────────────────

/// Trusted agents must be rate-limited on Admin tools when the limiter cap is hit.
#[test]
fn admin_tools_rate_limited_for_trusted_agent() {
    let (event_tx, _) = broadcast::channel(16);
    let session_mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new(event_tx)));
    let trust_config = Arc::new(trusted_config());
    // Allow only 1 destructive call per minute.
    let rate_limiter = Arc::new(RateLimiter::new(1));
    let server = TherminalMcpServer::new(session_mgr, trust_config, rate_limiter, None, None, None);
    let agent = agent("trusted-bot");

    // First call allowed.
    assert!(
        server
            .enforce_trust_sync("terminal.sessions.destroy", &agent)
            .is_ok()
    );
    // Second call denied due to rate limit.
    assert!(
        server
            .enforce_trust_sync("terminal.sessions.destroy", &agent)
            .is_err()
    );
}

// ── enforce_resource_trust ──────────────────────────────────────────

/// Sandboxed agents can read pane resources (Observer tier).
#[test]
fn sandboxed_agent_can_read_pane_resources() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_resource_trust("terminal://pane/1/content", &agent)
            .is_ok()
    );
    assert!(
        server
            .enforce_resource_trust("terminal://pane/1/output", &agent)
            .is_ok()
    );
}

/// Sandboxed agents can read pane scrollback resources (Observer tier).
#[test]
fn sandboxed_agent_can_read_pane_scrollback() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_resource_trust("terminal://pane/1/scrollback", &agent)
            .is_ok()
    );
}

/// Sandboxed agents can read the Claude events resource (Observer tier).
/// This is the trust bypass site — the resource must not silently allow
/// lower-trust agents by failing open.
#[test]
fn sandboxed_agent_can_read_claude_events_resource() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_resource_trust(super::CLAUDE_EVENTS_URI, &agent)
            .is_ok(),
        "Sandboxed is Observer-tier and must be allowed for claude/events"
    );
}

/// An agent with no configured tier below Sandboxed would be denied — but
/// since Sandboxed is the minimum, verify that the boundary is correctly
/// enforced: there is no tier below Sandboxed in the current model.
/// This test documents the current trust floor.
#[test]
fn sandboxed_is_minimum_tier_for_resources() {
    let server = make_server(sandboxed_config());
    let agent = agent("any-agent");
    // Sandboxed agents are always allowed for Observer resources.
    assert!(
        server
            .enforce_resource_trust("terminal://pane/99/content", &agent)
            .is_ok()
    );
    assert!(
        server
            .enforce_resource_trust("therminal://claude/events", &agent)
            .is_ok()
    );
}

// ── tool_definitions() surface lock ─────────────────────────────────

/// Lock in the count: exactly 34 tools must be returned. Bumped from
/// 32 to 34: +2 in tn-n5jk (`terminal.panes.pin`, `terminal.panes.unpin`).
#[test]
fn tool_definitions_returns_34_tools() {
    let tools = tool_definitions();
    assert_eq!(tools.len(), 34, "expected exactly 34 tool definitions");
}

/// Lock in the names so a rename or accidental drop is caught immediately.
#[test]
fn tool_definitions_contains_all_expected_names() {
    use std::collections::HashSet;
    let tools = tool_definitions();
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    let expected = [
        "terminal.sessions.list",
        "terminal.sessions.get",
        "terminal.sessions.create",
        "terminal.sessions.destroy",
        "terminal.panes.create",
        "terminal.panes.destroy",
        "terminal.panes.list",
        "terminal.panes.write",
        "terminal.panes.tag",
        "terminal.panes.untag",
        "terminal.panes.get_geometry",
        "terminal.panes.get_content",
        "terminal.panes.get_summary",
        "terminal.panes.peek",
        "terminal.semantic.query_history",
        "terminal.semantic.query_commands",
        "terminal.panes.query_events",
        "terminal.panes.wait_for_output",
        "terminal.semantic.get_hotspots",
        "terminal.workspaces.list",
        "terminal.workspaces.get_layout",
        "terminal.agents.list",
        "terminal.agents.get_details",
        "terminal.agents.get_status",
        "terminal.agents.get_session_detail",
        "terminal.agents.get_cadence",
        "terminal.agents.find_with_capacity",
        "terminal.panes.create_tail",
        "terminal.panes.pin",
        "terminal.panes.unpin",
        "terminal.patterns.stats",
        "terminal.events.stats",
        "terminal.widgets.timeline.toggle",
    ];
    for name in &expected {
        assert!(names.contains(name), "missing tool definition: {name}");
    }
}

// ── CLI ↔ MCP parity (tn-8ysl) ──────────────────────────────────────
//
// A small "shared surface" allowlist: operations that must exist on
// BOTH the MCP tool surface and the `therminal` CLI. The CLI subcommand
// path is recorded alongside each MCP tool name so drift in either
// direction is visible. The CLI column is a hardcoded snapshot of the
// `clap` subcommand structure in `crates/therminal-app/src/cli/` —
// renaming a CLI subcommand without touching this list, or removing
// an MCP tool without touching this list, both break the test.
//
// NOT every MCP tool needs a CLI counterpart (e.g. pane_summary is
// MCP-only today) and not every CLI subcommand maps to an MCP tool
// (e.g. `therminal events --follow` is a streaming subscription). Add
// entries here only when both surfaces intentionally expose the same
// operation.
const SHARED_SURFACE: &[(&str, &str)] = &[
    ("terminal.sessions.list", "session list"),
    ("terminal.panes.list", "pane list"),
    ("terminal.panes.write", "pane send"),
    ("terminal.panes.peek", "pane peek"),
    ("terminal.panes.tag", "pane tag"),
    ("terminal.panes.untag", "pane untag"),
    ("terminal.agents.list", "agents list"),
    ("terminal.workspaces.list", "workspace list"),
    ("terminal.semantic.query_commands", "semantic commands"),
    ("terminal.semantic.get_hotspots", "semantic hotspots"),
];

/// Every MCP name in the shared-surface allowlist must exist in
/// `tool_definitions()`. Fails loudly if a tool is removed or renamed
/// without updating the allowlist (and implicitly the CLI).
#[test]
fn shared_surface_mcp_tools_exist() {
    use std::collections::HashSet;
    let tools = tool_definitions();
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for (mcp, cli) in SHARED_SURFACE {
        assert!(
            names.contains(mcp),
            "shared-surface MCP tool `{mcp}` missing from tool_definitions() \
             (CLI counterpart: `therminal {cli}`). If you renamed the tool, \
             update SHARED_SURFACE and the matching CLI subcommand."
        );
    }
}

/// Guard against the allowlist itself going stale: entries must be
/// unique on both sides and must reference an IpcRequest primitive
/// the daemon actually handles (sampled via a small static map).
#[test]
fn shared_surface_allowlist_is_consistent() {
    use std::collections::HashSet;
    let mut mcp_seen: HashSet<&str> = HashSet::new();
    let mut cli_seen: HashSet<&str> = HashSet::new();
    for (mcp, cli) in SHARED_SURFACE {
        assert!(mcp_seen.insert(mcp), "duplicate MCP entry: {mcp}");
        assert!(cli_seen.insert(cli), "duplicate CLI entry: {cli}");
    }
}

// ── Resource surface lock ────────────────────────────────────────────

/// `build_resource_list` must always include the Claude events URI even
/// when there are no active panes.
#[tokio::test]
async fn build_resource_list_always_includes_claude_events() {
    let server = make_server(trusted_config());
    let resources = server.build_resource_list().await;
    let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
    assert!(
        uris.contains(&super::CLAUDE_EVENTS_URI),
        "expected claude/events in resource list, got: {uris:?}"
    );
}

/// `build_resource_list` must always include the agent lifecycle events
/// URI even when there are no active panes.
#[tokio::test]
async fn build_resource_list_always_includes_agent_events() {
    let server = make_server(trusted_config());
    let resources = server.build_resource_list().await;
    let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
    assert!(
        uris.contains(&super::AGENT_EVENTS_URI),
        "expected agents/events in resource list, got: {uris:?}"
    );
}

/// Sandboxed agents must be allowed to access the agent lifecycle event
/// resource (Observer tier).
#[test]
fn sandboxed_agent_can_read_agent_events_resource() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_resource_trust(super::AGENT_EVENTS_URI, &agent)
            .is_ok(),
        "sandboxed agent should be allowed to read agents/events"
    );
}

/// The agent-lifecycle subscription forwarder must populate the
/// per-connection ring buffer when events are broadcast, without
/// deadlocking the buffer mutex.
#[tokio::test(flavor = "multi_thread")]
async fn agent_event_forwarder_buffers_events_without_deadlock() {
    use therminal_terminal::agent_registry::{AgentEvent, TaggedAgentEvent};
    use therminal_terminal::state_inference::AgentType;

    let server = make_server(trusted_config());
    // Replace agent_events with a fresh broadcast channel we control.
    let (tx, _) = tokio::sync::broadcast::channel::<TaggedAgentEvent>(16);
    // SAFETY: tests construct a fresh server above; we need to reach into
    // it. We rebuild via `new` instead.
    let session_mgr = server.session_mgr.clone();
    let trust_config = server.trust_config.clone();
    let rate_limiter = server.rate_limiter.clone();
    let server = TherminalMcpServer::new(
        session_mgr,
        trust_config,
        rate_limiter,
        None,
        Some(tx.clone()),
        None,
    );

    // Manually drive the same buffer-fill loop the subscribe handler uses,
    // by spawning a stripped-down forwarder that only updates the buffer.
    let buffer = std::sync::Arc::clone(&server.agent_event_buffer);
    let mut rx = tx.subscribe();
    let task = tokio::spawn(async move {
        for _ in 0..3 {
            if let Ok(evt) = rx.recv().await {
                let mut buf = buffer.lock().unwrap();
                if buf.len() == super::AGENT_EVENT_BUFFER_CAP {
                    buf.pop_front();
                }
                buf.push_back(evt);
            }
        }
    });

    // Send three events.
    for i in 0..3u64 {
        let _ = tx.send(TaggedAgentEvent {
            event: AgentEvent::Registered {
                pane_id: i,
                agent_type: AgentType::Claude,
                name: format!("a{i}"),
            },
            pane_id: i,
            timestamp_secs: 0,
        });
    }

    // Wait for the forwarder to drain.
    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("forwarder did not finish in time")
        .expect("forwarder task panicked");

    let buf = server.agent_event_buffer.lock().unwrap();
    assert_eq!(buf.len(), 3);
}

// ── LayoutNodeJson ──────────────────────────────────────────────────

#[test]
fn layout_node_json_from_empty_pane_ids_is_empty() {
    let tree = super::LayoutNodeJson::from_flat_pane_ids(&[]);
    assert_eq!(tree, super::LayoutNodeJson::Empty);
}

#[test]
fn layout_node_json_single_pane_is_leaf() {
    let tree = super::LayoutNodeJson::from_flat_pane_ids(&[42]);
    assert_eq!(tree, super::LayoutNodeJson::Leaf { pane_id: 42 });
}

#[test]
fn layout_node_json_two_panes_single_split() {
    let tree = super::LayoutNodeJson::from_flat_pane_ids(&[1, 2]);
    match tree {
        super::LayoutNodeJson::Split {
            direction,
            ratio,
            left,
            right,
        } => {
            assert_eq!(direction, "horizontal");
            assert!((ratio - 0.5).abs() < f32::EPSILON);
            assert_eq!(*left, super::LayoutNodeJson::Leaf { pane_id: 1 });
            assert_eq!(*right, super::LayoutNodeJson::Leaf { pane_id: 2 });
        }
        other => panic!("expected Split, got {other:?}"),
    }
}

#[test]
fn layout_node_json_many_panes_cascade() {
    let tree = super::LayoutNodeJson::from_flat_pane_ids(&[1, 2, 3, 4]);
    // Walk the right spine: leaves 1, 2, 3, 4.
    fn collect(node: &super::LayoutNodeJson, out: &mut Vec<u64>) {
        match node {
            super::LayoutNodeJson::Leaf { pane_id } => out.push(*pane_id),
            super::LayoutNodeJson::Split { left, right, .. } => {
                collect(left, out);
                collect(right, out);
            }
            super::LayoutNodeJson::Empty => {}
        }
    }
    let mut ids = Vec::new();
    collect(&tree, &mut ids);
    assert_eq!(ids, vec![1, 2, 3, 4]);
}

#[test]
fn layout_node_json_from_snapshot_preserves_directions_and_ratios() {
    use therminal_protocol::daemon::{LayoutSnapshot, LayoutSplitDirection};
    // Horizontal root: leaf(1) | (vertical leaf(2) / leaf(3)).
    let snap = LayoutSnapshot::Split {
        direction: LayoutSplitDirection::Horizontal,
        ratio: 0.6,
        first: Box::new(LayoutSnapshot::Leaf { pane_id: 1 }),
        second: Box::new(LayoutSnapshot::Split {
            direction: LayoutSplitDirection::Vertical,
            ratio: 0.25,
            first: Box::new(LayoutSnapshot::Leaf { pane_id: 2 }),
            second: Box::new(LayoutSnapshot::Leaf { pane_id: 3 }),
        }),
    };
    let tree = super::LayoutNodeJson::from_snapshot(&snap);
    match &tree {
        super::LayoutNodeJson::Split {
            direction,
            ratio,
            left,
            right,
        } => {
            assert_eq!(direction, "horizontal");
            assert!((ratio - 0.6).abs() < f32::EPSILON);
            assert!(matches!(
                left.as_ref(),
                super::LayoutNodeJson::Leaf { pane_id: 1 }
            ));
            match right.as_ref() {
                super::LayoutNodeJson::Split {
                    direction,
                    ratio,
                    left,
                    right,
                } => {
                    assert_eq!(direction, "vertical");
                    assert!((ratio - 0.25).abs() < f32::EPSILON);
                    assert!(matches!(
                        left.as_ref(),
                        super::LayoutNodeJson::Leaf { pane_id: 2 }
                    ));
                    assert!(matches!(
                        right.as_ref(),
                        super::LayoutNodeJson::Leaf { pane_id: 3 }
                    ));
                }
                _ => panic!("expected nested vertical split"),
            }
        }
        _ => panic!("expected horizontal split"),
    }
    let json = serde_json::to_string(&tree).unwrap();
    let parsed: super::LayoutNodeJson = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, tree);
}

#[test]
fn layout_node_json_serde_round_trip() {
    let tree = super::LayoutNodeJson::from_flat_pane_ids(&[10, 20, 30]);
    let json = serde_json::to_string(&tree).expect("serialize");
    let parsed: super::LayoutNodeJson = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, tree);
    // Spot-check the tagged union shape.
    assert!(json.contains("\"type\":\"split\""));
    assert!(json.contains("\"direction\":\"horizontal\""));
    assert!(json.contains("\"type\":\"leaf\""));
}

// ── PaneInfo serialization ──────────────────────────────────────────

#[test]
fn pane_info_serializes_all_optional_fields() {
    let info = super::PaneInfo {
        pane_id: 1,
        session_id: 2,
        cols: 80,
        rows: 24,
        title: String::new(),
        cwd: Some("/home/user/proj".to_string()),
        last_exit_code: Some(0),
        agent_name: Some("claude-code".to_string()),
        tags: Default::default(),
        session_title: None,
    };
    let v = serde_json::to_value(&info).expect("serialize");
    assert_eq!(v["pane_id"], 1);
    assert_eq!(v["session_id"], 2);
    assert_eq!(v["cols"], 80);
    assert_eq!(v["rows"], 24);
    assert_eq!(v["cwd"], "/home/user/proj");
    assert_eq!(v["last_exit_code"], 0);
    assert_eq!(v["agent_name"], "claude-code");
}

#[test]
fn pane_info_serializes_all_none_fields() {
    let info = super::PaneInfo {
        pane_id: 1,
        session_id: 2,
        cols: 80,
        rows: 24,
        title: String::new(),
        cwd: None,
        last_exit_code: None,
        agent_name: None,
        tags: Default::default(),
        session_title: None,
    };
    let v = serde_json::to_value(&info).expect("serialize");
    // None fields are skipped entirely for backward compatibility.
    assert!(v.get("cwd").is_none(), "cwd should be omitted when None");
    assert!(
        v.get("last_exit_code").is_none(),
        "last_exit_code should be omitted when None"
    );
    assert!(
        v.get("agent_name").is_none(),
        "agent_name should be omitted when None"
    );
    assert_eq!(v["pane_id"], 1);
}

#[test]
fn pane_info_nonzero_exit_code_round_trips() {
    let info = super::PaneInfo {
        pane_id: 7,
        session_id: 3,
        cols: 120,
        rows: 40,
        title: String::new(),
        cwd: Some("/tmp".to_string()),
        last_exit_code: Some(127),
        agent_name: None,
        tags: Default::default(),
        session_title: None,
    };
    let v = serde_json::to_value(&info).expect("serialize");
    assert_eq!(v["last_exit_code"], 127);
    assert!(v.get("agent_name").is_none());
}

// ── terminal.agents.get_details ─────────────────────────────────────

/// Calling `handle_get_agent_details` on a nonexistent pane must return
/// a tool error (not a transport error). This matches the convention
/// used by `handle_get_pane_geometry` and `handle_read_pane_content`.
#[tokio::test]
async fn get_agent_details_nonexistent_pane_returns_tool_error() {
    let server = make_server(trusted_config());
    let params = super::PaneIdParam { pane_id: 999_999 };
    let result = server
        .handle_get_agent_details(params)
        .await
        .expect("handler should not error at transport level");
    assert_eq!(result.is_error, Some(true));
}

/// `AgentDetailsResult` with no agent data must serialise to a shape
/// where only `consecutive_failures` is present (all `Option` fields
/// are skipped when `None`).
#[test]
fn agent_details_all_none_serializes_minimally() {
    let details = super::AgentDetailsResult {
        agent_type: None,
        model: None,
        context_percent: None,
        consecutive_failures: 0,
        last_command: None,
        last_exit_code: None,
        last_command_duration_ms: None,
    };
    let v = serde_json::to_value(&details).expect("serialize");
    assert_eq!(v["consecutive_failures"], 0);
    assert!(v.get("agent_type").is_none());
    assert!(v.get("model").is_none());
    assert!(v.get("context_percent").is_none());
    assert!(v.get("last_command").is_none());
    assert!(v.get("last_exit_code").is_none());
    assert!(v.get("last_command_duration_ms").is_none());
}

/// Observer-tier enforcement for `terminal.agents.get_details`: a
/// Sandboxed agent must be allowed, matching the rest of the
/// `terminal.agents.*` surface.
#[test]
fn sandboxed_agent_can_call_get_agent_details() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_trust_sync("terminal.agents.get_details", &agent)
            .is_ok()
    );
}

// ── terminal.agents.get_status ──────────────────────────────────────

/// `terminal.agents.get_status` must be advertised in `tool_definitions()`.
#[test]
fn get_agent_status_tool_is_registered() {
    let defs = super::tools::tool_definitions();
    let names: Vec<&str> = defs.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"terminal.agents.get_status"),
        "tool_definitions missing terminal.agents.get_status: {names:?}"
    );
}

/// Observer-tier enforcement: a Sandboxed-tier caller is permitted.
#[test]
fn sandboxed_agent_can_call_get_agent_status() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_trust_sync("terminal.agents.get_status", &agent)
            .is_ok()
    );
}

/// Regression-lock the bypass class: the trust category map must
/// classify `get_status` as Observer (not None / unknown), so it
/// remains gated by `enforce_trust` even if the dispatch arm is
/// reordered.
#[test]
fn get_agent_status_is_observer_tier() {
    use crate::trust::{ToolCategory, tool_category};
    assert_eq!(
        tool_category("terminal.agents.get_status"),
        Some(ToolCategory::Observer)
    );
}

/// Calling `handle_get_agent_status` with no agent registered and no
/// capacity entry must return a tool error rather than a transport
/// error or an empty success.
#[tokio::test]
async fn get_agent_status_unknown_pane_returns_tool_error() {
    let server = make_server(trusted_config());
    let params = super::PaneIdParam { pane_id: 999_999 };
    let result = server
        .handle_get_agent_status(params)
        .await
        .expect("handler should not error at transport level");
    assert_eq!(result.is_error, Some(true));
}

// ── terminal.agents.get_cadence ─────────────────────────────────────

/// `terminal.agents.get_cadence` must be advertised in `tool_definitions()`.
#[test]
fn get_agent_cadence_tool_is_registered() {
    let defs = super::tools::tool_definitions();
    let names: Vec<&str> = defs.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"terminal.agents.get_cadence"),
        "tool_definitions missing terminal.agents.get_cadence: {names:?}"
    );
}

/// Observer-tier enforcement: a Sandboxed-tier caller is permitted to
/// call `terminal.agents.get_cadence`, matching the rest of the
/// `terminal.agents.*` read surface.
#[test]
fn sandboxed_agent_can_call_get_agent_cadence() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_trust_sync("terminal.agents.get_cadence", &agent)
            .is_ok()
    );
}

/// Regression-lock the trust class: `get_cadence` must classify as
/// Observer so it remains gated by `enforce_trust` even if the
/// dispatch arm is reordered.
#[test]
fn get_agent_cadence_is_observer_tier() {
    use crate::trust::{ToolCategory, tool_category};
    assert_eq!(
        tool_category("terminal.agents.get_cadence"),
        Some(ToolCategory::Observer)
    );
}

/// Calling `handle_get_agent_cadence` on a nonexistent pane must
/// return a tool error (not a transport error). This matches the
/// convention used by `handle_get_agent_details`.
#[tokio::test]
async fn get_agent_cadence_nonexistent_pane_returns_tool_error() {
    let server = make_server(trusted_config());
    let params = super::PaneIdParam { pane_id: 999_999 };
    let result = server
        .handle_get_agent_cadence(params)
        .await
        .expect("handler should not error at transport level");
    assert_eq!(result.is_error, Some(true));
}

/// `AgentCadenceResult` with no streaming activity (zero / false /
/// empty defaults) must serialise to a stable shape so callers can
/// rely on the field set being present.
#[test]
fn agent_cadence_defaults_serialize_to_zero_shape() {
    let result = super::AgentCadenceResult {
        chunk_count: 0,
        avg_arrival_ms: 0.0,
        max_gap_ms: 0.0,
        is_spinner: false,
        is_streaming: false,
        recent_samples: Vec::new(),
    };
    let v = serde_json::to_value(&result).expect("serialize");
    assert_eq!(v["chunk_count"], 0);
    assert_eq!(v["avg_arrival_ms"], 0.0);
    assert_eq!(v["max_gap_ms"], 0.0);
    assert_eq!(v["is_spinner"], false);
    assert_eq!(v["is_streaming"], false);
    assert!(v["recent_samples"].is_array());
    assert_eq!(v["recent_samples"].as_array().unwrap().len(), 0);
}

/// End-to-end: a freshly-created pane has no (or trivially little)
/// streaming activity, so `handle_get_agent_cadence` must succeed and
/// return a structurally valid `AgentCadenceResult`. Validates the
/// full plumbing path:
/// `SessionManager::pane_agent_cadence` -> `Pane::agent_cadence_snapshot`
/// -> `AgentStateInference::cadence_snapshot` -> `AgentCadenceResult`,
/// and locks in the `recent_samples` cap so future window growth can't
/// silently leak unbounded data through MCP.
#[tokio::test]
async fn get_agent_cadence_returns_defaults_for_fresh_pane() {
    let server = make_server(trusted_config());
    let pane_id = {
        let mut mgr = server.session_mgr.lock().await;
        let session_id = mgr
            .create_session(Some("cadence-test".to_string()))
            .expect("create session");
        mgr.iter_sessions()
            .find(|(id, _)| **id == session_id)
            .and_then(|(_, s)| s.windows.first())
            .and_then(|w| w.panes.first())
            .map(|p| p.id)
            .expect("session has at least one pane")
    };

    let result = server
        .handle_get_agent_cadence(super::PaneIdParam { pane_id })
        .await
        .expect("handler should not error at transport level");
    assert_ne!(
        result.is_error,
        Some(true),
        "expected success for live pane"
    );

    let payload = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("text content");
    let v: serde_json::Value = serde_json::from_str(&payload).expect("parse json");

    // chunk_count is u64 in the result; freshly-spawned panes will
    // usually be 0 but a shell prompt might already have arrived, so
    // we assert structural invariants instead of an exact count.
    let chunk_count = v["chunk_count"].as_u64().expect("chunk_count is u64");
    let samples = v["recent_samples"]
        .as_array()
        .expect("recent_samples is array");
    // The cap MUST hold even if a prompt did arrive.
    assert!(samples.len() <= 50, "recent_samples cap (<=50) violated");
    // chunk_count and samples.len() should agree up to the cap.
    assert_eq!(samples.len(), chunk_count.min(50) as usize);
    // Boolean classification fields must always be present.
    assert!(v["is_spinner"].is_boolean());
    assert!(v["is_streaming"].is_boolean());
    // Numeric metrics must always be present.
    assert!(v["avg_arrival_ms"].is_number());
    assert!(v["max_gap_ms"].is_number());
}

// ── terminal.agents.find_with_capacity ──────────────────────────────

/// `terminal.agents.find_with_capacity` must be advertised in
/// `tool_definitions()`.
#[test]
fn find_with_capacity_tool_is_registered() {
    let defs = super::tools::tool_definitions();
    let names: Vec<&str> = defs.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"terminal.agents.find_with_capacity"),
        "tool_definitions missing terminal.agents.find_with_capacity: {names:?}"
    );
}

/// Observer-tier enforcement: a Sandboxed-tier caller is permitted.
#[test]
fn sandboxed_agent_can_call_find_with_capacity() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_trust_sync("terminal.agents.find_with_capacity", &agent)
            .is_ok()
    );
}

/// Lock the trust category: must classify as Observer so it remains
/// gated by `enforce_trust` even if dispatch arms are reordered.
#[test]
fn find_with_capacity_is_observer_tier() {
    use crate::trust::{ToolCategory, tool_category};
    assert_eq!(
        tool_category("terminal.agents.find_with_capacity"),
        Some(ToolCategory::Observer)
    );
}

/// Threshold filtering and sort order:
///   - pane 1: 20% used → 80% remaining (included at threshold 50)
///   - pane 2: 80% used → 20% remaining (excluded)
///   - pane 3: no capacity entry → unknown (included)
/// Sort: 80% remaining first, then unknown last.
#[tokio::test]
async fn find_with_capacity_filters_and_sorts() {
    use crate::pane_capacity::PaneCapacityEntry;
    use therminal_terminal::state_inference::AgentType;

    let server = make_server(trusted_config());
    {
        let mut mgr = server.session_mgr.lock().await;
        mgr.register_agent(1, "a1".to_string(), AgentType::Claude, None);
        mgr.register_agent(2, "a2".to_string(), AgentType::Claude, None);
        mgr.register_agent(3, "a3".to_string(), AgentType::Claude, None);
        let cache = mgr.pane_capacity_cache();
        cache.upsert(
            1,
            PaneCapacityEntry {
                context_percent: Some(20.0),
                model: Some("claude-opus".to_string()),
                status: None,
                session_id: "s1".to_string(),
                updated_at: 0,
                ..Default::default()
            },
        );
        cache.upsert(
            2,
            PaneCapacityEntry {
                context_percent: Some(80.0),
                model: Some("claude-opus".to_string()),
                status: None,
                session_id: "s2".to_string(),
                updated_at: 0,
                ..Default::default()
            },
        );
        // pane 3: no capacity entry on purpose.
    }

    let params = super::FindWithCapacityParam {
        threshold_percent: 50.0,
    };
    let result = server
        .handle_find_with_capacity(params)
        .await
        .expect("handler should not error at transport level");
    assert_ne!(result.is_error, Some(true));

    // Extract the agents JSON from the call result.
    let payload = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content");
    let v: serde_json::Value = serde_json::from_str(&payload).expect("json");
    let agents = v["agents"].as_array().expect("agents array");
    let ids: Vec<u64> = agents
        .iter()
        .map(|a| a["pane_id"].as_u64().unwrap())
        .collect();

    // Pane 2 (20% remaining) is excluded; pane 1 (80% remaining) sorts
    // first; pane 3 (unknown) sorts last.
    assert_eq!(ids, vec![1, 3], "got: {ids:?}");

    // Pane 1 carries computed remaining_percent = 80.0
    assert!((agents[0]["remaining_percent"].as_f64().unwrap() - 80.0).abs() < 1e-6);
    assert!((agents[0]["context_percent"].as_f64().unwrap() - 20.0).abs() < 1e-6);
    // Pane 3 has neither (skipped via skip_serializing_if).
    assert!(agents[1].get("remaining_percent").is_none());
    assert!(agents[1].get("context_percent").is_none());
}

// ── tn-ifee: terminal.agents.get_session_detail ─────────────────────

#[test]
fn get_agent_session_detail_tool_is_registered() {
    let defs = super::tools::tool_definitions();
    let names: Vec<&str> = defs.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"terminal.agents.get_session_detail"),
        "tool_definitions missing terminal.agents.get_session_detail: {names:?}"
    );
}

#[test]
fn get_agent_session_detail_is_observer_tier() {
    use crate::trust::{ToolCategory, tool_category};
    assert_eq!(
        tool_category("terminal.agents.get_session_detail"),
        Some(ToolCategory::Observer)
    );
}

#[tokio::test]
async fn get_agent_session_detail_nonexistent_pane_returns_tool_error() {
    let server = make_server(trusted_config());
    let result = server
        .handle_get_agent_session_detail(super::PaneIdParam { pane_id: 999_999 })
        .await
        .expect("handler should not error at transport level");
    assert_eq!(result.is_error, Some(true));
}

/// End-to-end: create a real pane, populate the capacity cache with all
/// five tn-ifee fields, call the handler, assert every field is present
/// in the response.
#[tokio::test]
async fn get_agent_session_detail_returns_all_five_fields() {
    use crate::pane_capacity::PaneCapacityEntry;

    let server = make_server(trusted_config());
    let pane_id = {
        let mut mgr = server.session_mgr.lock().await;
        let session_id = mgr
            .create_session(Some("session-detail-test".to_string()))
            .expect("create session");
        let pane_id = mgr
            .iter_sessions()
            .find(|(id, _)| **id == session_id)
            .and_then(|(_, s)| s.windows.first())
            .and_then(|w| w.panes.first())
            .map(|p| p.id)
            .expect("session has at least one pane");
        mgr.pane_capacity_cache().upsert(
            pane_id,
            PaneCapacityEntry {
                context_percent: Some(9.0),
                model: Some("claude-opus-4-6".into()),
                status: Some("tool_use".into()),
                session_id: "73d64234-6693-4cff-ae42-e175a401cee8".into(),
                session_title: Some("therminal / tn-ifee".into()),
                current_tool: Some("Bash".into()),
                working_dir: Some("/home/marci/projects/therminal".into()),
                updated_at: 0,
                last_seen_at: 0,
                marker_seen_at: 0,
            },
        );
        pane_id
    };

    let result = server
        .handle_get_agent_session_detail(super::PaneIdParam { pane_id })
        .await
        .expect("handler should not error at transport level");
    assert_ne!(result.is_error, Some(true));

    let payload = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("text content");
    let v: serde_json::Value = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(v["pane_id"].as_u64(), Some(pane_id));
    assert_eq!(v["session_title"], "therminal / tn-ifee");
    assert_eq!(v["current_tool"], "Bash");
    assert_eq!(v["working_dir"], "/home/marci/projects/therminal");
    assert_eq!(v["model"], "claude-opus-4-6");
    assert!((v["context_percent"].as_f64().unwrap() - 9.0).abs() < 1e-6);
}

// ── terminal.semantic.query_commands ────────────────────────────────

#[test]
fn sandboxed_agent_can_call_query_commands() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_trust_sync("terminal.semantic.query_commands", &agent)
            .is_ok()
    );
}

#[tokio::test]
async fn query_commands_nonexistent_pane_returns_tool_error() {
    let server = make_server(trusted_config());
    let params = super::QueryCommandsParam {
        pane_id: 999_999,
        since_line: None,
        limit: None,
    };
    let result = server
        .handle_query_commands(params)
        .await
        .expect("handler should not error at transport level");
    assert_eq!(result.is_error, Some(true));
}

#[test]
fn query_commands_result_empty_serializes() {
    let result = super::QueryCommandsResult {
        pane_id: 1,
        commands: Vec::new(),
    };
    let v = serde_json::to_value(&result).expect("serialize");
    assert_eq!(v["pane_id"], 1);
    assert_eq!(v["commands"].as_array().unwrap().len(), 0);
}

/// End-to-end check: create a real session, drive OSC 633 marks through
/// a `TherminalInterceptor` sharing the pane's command tracker `Arc`,
/// then assert `handle_query_commands` returns the expected blocks.
#[tokio::test]
async fn query_commands_returns_tracked_blocks() {
    use alacritty_terminal::vte::SequenceInterceptor;
    use therminal_terminal::interceptor::TherminalInterceptor;

    let server = make_server(trusted_config());

    // Create a real session + pane (spawns a real shell PTY).
    let pane_id = {
        let mut mgr = server.session_mgr.lock().await;
        let session_id = mgr
            .create_session(Some("test-session".to_string()))
            .expect("create session");
        // Find the pane id of the freshly created session.
        mgr.iter_sessions()
            .find(|(id, _)| **id == session_id)
            .and_then(|(_, s)| s.windows.first())
            .and_then(|w| w.panes.first())
            .map(|p| p.id)
            .expect("session has at least one pane")
    };

    // Grab the shared tracker Arc and drive OSC 633 marks through a
    // standalone interceptor that holds the same Arc — this mirrors
    // what the reader thread does in production.
    let tracker_arc = {
        let mgr = server.session_mgr.lock().await;
        mgr.pane_command_tracker_arc(pane_id)
            .expect("pane has tracker")
    };
    let (mut interceptor, _rx) = TherminalInterceptor::with_defaults_and_tracker(tracker_arc);

    // Two complete commands.
    let sequences: &[&[&[u8]]] = &[
        &[b"633", b"A"],
        &[b"633", b"B"],
        &[b"633", b"E", b"echo first"],
        &[b"633", b"C"],
        &[b"633", b"D", b"0"],
        &[b"633", b"A"],
        &[b"633", b"B"],
        &[b"633", b"E", b"false"],
        &[b"633", b"C"],
        &[b"633", b"D", b"1"],
    ];
    for params in sequences {
        interceptor.intercept_osc(params, true);
    }

    // Call the tool handler and assert the result.
    let params = super::QueryCommandsParam {
        pane_id,
        since_line: None,
        limit: None,
    };
    let result = server
        .handle_query_commands(params)
        .await
        .expect("handler ok");
    assert_ne!(result.is_error, Some(true), "expected success");

    let json = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
    let cmds = parsed["commands"].as_array().expect("commands array");
    assert_eq!(cmds.len(), 2, "expected two tracked commands");
    assert_eq!(cmds[0]["command_text"], "echo first");
    assert_eq!(cmds[0]["exit_code"], 0);
    assert_eq!(cmds[1]["command_text"], "false");
    assert_eq!(cmds[1]["exit_code"], 1);

    // limit caps to most recent.
    let params = super::QueryCommandsParam {
        pane_id,
        since_line: None,
        limit: Some(1),
    };
    let result = server
        .handle_query_commands(params)
        .await
        .expect("handler ok");
    let json = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
    let cmds = parsed["commands"].as_array().expect("commands array");
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0]["command_text"], "false");
}

// ── terminal.panes.query_events ─────────────────────────────────────

#[test]
fn sandboxed_agent_can_call_query_events() {
    let server = make_server(sandboxed_config());
    let agent = agent("sandboxed-bot");
    assert!(
        server
            .enforce_trust_sync("terminal.panes.query_events", &agent)
            .is_ok()
    );
}

#[test]
fn supervised_agent_can_call_query_events() {
    let server = make_server(supervised_config());
    let agent = agent("supervised-bot");
    assert!(
        server
            .enforce_trust_sync("terminal.panes.query_events", &agent)
            .is_ok()
    );
}

#[tokio::test]
async fn query_events_nonexistent_pane_returns_tool_error() {
    let server = make_server(trusted_config());
    let params = super::QueryEventsParam {
        pane_id: 999_999,
        since_timestamp_secs: None,
        limit: None,
    };
    let result = server
        .handle_query_events(params)
        .await
        .expect("handler should not error at transport level");
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn query_events_fresh_pane_returns_empty() {
    use therminal_terminal::event_log::SessionEvent;

    let server = make_server(trusted_config());
    let pane_id = {
        let mut mgr = server.session_mgr.lock().await;
        let session_id = mgr
            .create_session(Some("evt-session".to_string()))
            .expect("create session");
        mgr.iter_sessions()
            .find(|(id, _)| **id == session_id)
            .and_then(|(_, s)| s.windows.first())
            .and_then(|w| w.panes.first())
            .map(|p| p.id)
            .expect("session has at least one pane")
    };

    let params = super::QueryEventsParam {
        pane_id,
        since_timestamp_secs: None,
        limit: None,
    };
    let result = server
        .handle_query_events(params)
        .await
        .expect("handler ok");
    assert_ne!(result.is_error, Some(true));
    let json = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(parsed["events"].as_array().unwrap().len(), 0);

    // Inject a few events directly via the shared Arc.
    let log_arc = {
        let mgr = server.session_mgr.lock().await;
        mgr.pane_event_log_arc(pane_id).expect("event log arc")
    };
    {
        let mut log = log_arc.lock().unwrap();
        log.log(&SessionEvent::Spawn {
            command: "bash".into(),
            cwd: "/home/test".into(),
        });
        log.log(&SessionEvent::Bell);
        log.log(&SessionEvent::Bell);
        log.log(&SessionEvent::Resize { cols: 80, rows: 24 });
    }

    // Default limit returns all four oldest-first.
    let params = super::QueryEventsParam {
        pane_id,
        since_timestamp_secs: None,
        limit: None,
    };
    let result = server
        .handle_query_events(params)
        .await
        .expect("handler ok");
    let json = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
    let events = parsed["events"].as_array().expect("events array");
    assert_eq!(events.len(), 4);
    assert_eq!(events[0]["event_type"], "spawn");
    assert_eq!(events[0]["details"]["command"], "bash");
    assert_eq!(events[3]["event_type"], "resize");

    // Limit caps to most recent.
    let params = super::QueryEventsParam {
        pane_id,
        since_timestamp_secs: None,
        limit: Some(1),
    };
    let result = server
        .handle_query_events(params)
        .await
        .expect("handler ok");
    let json = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
    let events = parsed["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event_type"], "resize");

    // since_timestamp_secs in the far future filters everything out.
    let params = super::QueryEventsParam {
        pane_id,
        since_timestamp_secs: Some(u64::MAX),
        limit: None,
    };
    let result = server
        .handle_query_events(params)
        .await
        .expect("handler ok");
    let json = result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("content");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("json");
    assert_eq!(parsed["events"].as_array().unwrap().len(), 0);
}

#[test]
fn query_events_tool_is_registered() {
    use std::collections::HashSet;
    let tools = tool_definitions();
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains("terminal.panes.query_events"));
}

#[test]
fn query_commands_tool_is_registered() {
    use std::collections::HashSet;
    let tools = tool_definitions();
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains("terminal.semantic.query_commands"));
}

/// With an empty session manager, the only resources should be the
/// global event streams (claude/events and agents/events).
#[tokio::test]
async fn build_resource_list_no_panes_only_claude_events() {
    let server = make_server(trusted_config());
    let resources = server.build_resource_list().await;
    assert_eq!(
        resources.len(),
        3,
        "expected only the global event-stream resources with no panes \
         (terminal://events + claude/events + agents/events)"
    );
}

// ── tn-sp3n: cheap polling helpers ──────────────────────────────────

use crate::session::PaneSnapshot;

/// Build a deterministic synthetic `PaneSnapshot` for testing the
/// trim/compact/hash helpers without spawning a real PTY.
fn make_snapshot(grid_chars: &[&str], cols: usize) -> PaneSnapshot {
    let grid: Vec<Vec<(char, bool)>> = grid_chars
        .iter()
        .map(|row| {
            let mut cells: Vec<(char, bool)> = row.chars().map(|c| (c, false)).collect();
            while cells.len() < cols {
                cells.push((' ', false));
            }
            cells.truncate(cols);
            cells
        })
        .collect();
    PaneSnapshot {
        pane_id: 1,
        title: String::new(),
        scrollback: Vec::new(),
        grid,
        cursor_col: 0,
        cursor_line: 0,
        cols,
        rows: grid_chars.len(),
    }
}

#[test]
fn render_grid_lines_trims_trailing_whitespace_by_default() {
    let snap = make_snapshot(&["hello", "", "world"], 80);
    let lines = super::render_grid_lines(&snap, true, false);
    assert_eq!(
        lines,
        vec!["hello".to_string(), "".to_string(), "world".to_string()]
    );
    // Joined size should be tiny — proves the padding is gone.
    assert!(lines.iter().map(|s| s.len()).sum::<usize>() < 20);
}

#[test]
fn render_grid_lines_no_trim_returns_padded_grid() {
    let snap = make_snapshot(&["hi"], 40);
    let lines = super::render_grid_lines(&snap, false, false);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].len(), 40, "row must be padded out to cols");
    assert!(lines[0].starts_with("hi"));
}

#[test]
fn render_grid_lines_compact_drops_blank_rows() {
    let snap = make_snapshot(&["a", "", "b", "   ", "c"], 80);
    let lines = super::render_grid_lines(&snap, true, true);
    assert_eq!(
        lines,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn pane_content_hash_changes_on_edit() {
    let a = make_snapshot(&["foo", "bar"], 80);
    let b = make_snapshot(&["foo", "BAR"], 80);
    let h_a = super::pane_content_hash(&a);
    let h_b = super::pane_content_hash(&b);
    assert_ne!(h_a, h_b, "hash must change when a cell changes");
    assert_eq!(h_a.len(), 16, "hash is hex-encoded u64");
}

#[test]
fn pane_content_hash_stable_across_calls() {
    let snap = make_snapshot(&["abc", "def"], 80);
    let h1 = super::pane_content_hash(&snap);
    let h2 = super::pane_content_hash(&snap);
    assert_eq!(h1, h2);
}

#[test]
fn pane_content_hash_changes_on_cursor_move() {
    let mut snap = make_snapshot(&["abc"], 80);
    let h_origin = super::pane_content_hash(&snap);
    snap.cursor_col = 2;
    let h_moved = super::pane_content_hash(&snap);
    assert_ne!(h_origin, h_moved, "cursor moves are observable to clients");
}

#[test]
fn get_content_param_default_trim_is_true() {
    // No trim_trailing_whitespace key — should default to true.
    let p: super::GetContentParam = parse(r#"{"pane_id":1}"#);
    assert!(p.trim_trailing_whitespace);
    assert!(!p.compact);
    assert_eq!(p.rows, None);
}

#[test]
fn get_content_param_accepts_all_fields() {
    let p: super::GetContentParam =
        parse(r#"{"pane_id":"1","trim_trailing_whitespace":false,"compact":true,"rows":"5"}"#);
    assert_eq!(p.pane_id, 1);
    assert!(!p.trim_trailing_whitespace);
    assert!(p.compact);
    assert_eq!(p.rows, Some(5));
}

#[test]
fn peek_pane_param_default_lines_none() {
    let p: super::PeekPaneParam = parse(r#"{"pane_id":1}"#);
    assert_eq!(p.lines, None);
}

#[test]
fn get_summary_and_peek_tools_registered() {
    use std::collections::HashSet;
    let tools = tool_definitions();
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(names.contains("terminal.panes.get_summary"));
    assert!(names.contains("terminal.panes.peek"));
    // Sanity: get_content is still there.
    assert!(names.contains("terminal.panes.get_content"));
}

/// End-to-end byte-savings smoke test for the trim default. Spawns a
/// real session, asks for the default and the historical (untrimmed)
/// shape, and verifies the trimmed payload is dramatically smaller.
#[tokio::test]
async fn get_content_default_trim_saves_bytes_on_sparse_pane() {
    let server = make_server(trusted_config());
    let pane_id = {
        let mut mgr = server.session_mgr.lock().await;
        let session_id = mgr
            .create_session(Some("trim-test".to_string()))
            .expect("create session");
        mgr.iter_sessions()
            .find(|(id, _)| **id == session_id)
            .and_then(|(_, s)| s.windows.first())
            .and_then(|w| w.panes.first())
            .map(|p| p.id)
            .expect("session has at least one pane")
    };

    // Trimmed (default) — what the wire pays today.
    let trimmed = server
        .handle_read_pane_content(super::GetContentParam {
            pane_id,
            trim_trailing_whitespace: true,
            compact: false,
            rows: None,
        })
        .await
        .expect("trimmed call ok");
    let trimmed_text = trimmed
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("trimmed content");
    let trimmed_bytes = trimmed_text.len();

    // Untrimmed — the historical payload shape.
    let untrimmed = server
        .handle_read_pane_content(super::GetContentParam {
            pane_id,
            trim_trailing_whitespace: false,
            compact: false,
            rows: None,
        })
        .await
        .expect("untrimmed call ok");
    let untrimmed_text = untrimmed
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("untrimmed content");
    let untrimmed_bytes = untrimmed_text.len();

    // The exact ratio depends on shell startup, but on a freshly
    // spawned shell most rows are pure padding. Trim should cut
    // payload size at LEAST in half — usually 70-90%.
    assert!(
        trimmed_bytes < untrimmed_bytes,
        "trimmed ({trimmed_bytes}) must be smaller than untrimmed ({untrimmed_bytes})"
    );
    assert!(
        trimmed_bytes * 2 <= untrimmed_bytes,
        "trim must save at least 50% on a sparse pane: trimmed={trimmed_bytes} untrimmed={untrimmed_bytes}"
    );
    eprintln!(
        "tn-sp3n bytes: trimmed={trimmed_bytes} untrimmed={untrimmed_bytes} savings={:.1}%",
        100.0 - (trimmed_bytes as f64 / untrimmed_bytes as f64) * 100.0
    );

    // Both responses must include a non-empty content_hash.
    let parsed_trim: serde_json::Value = serde_json::from_str(&trimmed_text).expect("trim json");
    assert!(parsed_trim["content_hash"].as_str().is_some());

    // The summary tool should be even smaller.
    let summary = server
        .handle_get_pane_summary(super::PaneIdParam { pane_id })
        .await
        .expect("summary ok");
    let summary_text = summary
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("summary content");
    eprintln!("tn-sp3n summary bytes: {}", summary_text.len());
    // Summary should be cheaper than even the trimmed content payload.
    assert!(
        summary_text.len() < trimmed_bytes,
        "summary ({}) should be smaller than trimmed get_content ({trimmed_bytes})",
        summary_text.len()
    );

    // The peek tool should also be cheap on a fresh pane.
    let peek = server
        .handle_peek_pane(super::PeekPaneParam {
            pane_id,
            lines: Some(10),
        })
        .await
        .expect("peek ok");
    let peek_text = peek
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("peek content");
    eprintln!("tn-sp3n peek bytes: {}", peek_text.len());
    assert!(peek_text.len() < untrimmed_bytes);
}

#[test]
fn get_content_truncated_rows_set_when_rows_param_used() {
    // White-box test of the in-memory pipeline (no real PTY needed):
    // build a fake snapshot, run it through render+truncate logic
    // mirroring the handler.
    let snap = make_snapshot(&["a", "b", "c", "d", "e"], 80);
    let mut lines = super::render_grid_lines(&snap, true, false);
    let total_before = lines.len();
    let last_n = 2usize;
    let take = last_n.min(total_before);
    let drop = total_before - take;
    if drop > 0 {
        lines.drain(0..drop);
    }
    assert_eq!(lines, vec!["d".to_string(), "e".to_string()]);
    assert_eq!(drop, 3);
}
