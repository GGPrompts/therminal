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
//! The server listens on a dedicated Unix socket (separate from the IPC socket)
//! and uses the `rmcp` crate for protocol handling.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rmcp::handler::server::tool::schema_for_type;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ListToolsResult, PaginatedRequestParams,
    ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use tracing::{debug, error, info, warn};

use therminal_core::config::TrustConfig;

use crate::session::SessionManager;
use crate::trust::{AgentIdentity, RateLimiter, TrustCheckResult, check_tool_access};

// ── Tool parameter types ────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionIdParam {
    /// The numeric session ID.
    session_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSessionParam {
    /// Optional human-readable name for the session.
    name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteToPaneParam {
    /// The numeric pane ID to write to.
    pane_id: u64,
    /// The text (or bytes as UTF-8) to send to the pane's PTY.
    input: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PaneIdParam {
    /// The numeric pane ID.
    pane_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListPanesParam {
    /// Optional session ID to filter panes by. If omitted, returns panes from all sessions.
    session_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QuerySemanticHistoryParam {
    /// The numeric pane ID to query.
    pane_id: u64,
    /// Optional region type filter. One of: Prompt, Command, Output, Error, ToolCall, Thinking, Annotation.
    region_type: Option<String>,
    /// Optional regex pattern to match against region content preview.
    pattern: Option<String>,
    /// Maximum number of regions to return (default 20).
    limit: Option<usize>,
    /// Only return regions starting at or after this line number.
    since_line: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetPaneGeometryParam {
    /// The numeric pane ID.
    pane_id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitForOutputParam {
    /// The numeric pane ID to watch.
    pane_id: u64,
    /// String or regex pattern to match against output lines.
    pattern: String,
    /// Timeout in milliseconds (default 30000, max 120000).
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// Only match output on or after this line number. If omitted, matches any new output.
    since_line: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetHotspotsParam {
    /// The numeric pane ID to scan for hotspots.
    pane_id: u64,
    /// Optional hotspot type filter. One of: file, url, git_ref, issue.
    hotspot_type: Option<String>,
    /// Maximum number of hotspots to return (default 50).
    limit: Option<usize>,
}

fn default_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SpawnPaneParam {
    /// Session ID to create the pane in. If omitted, uses the default (first) session.
    session_id: Option<u64>,
    /// Shell command to run. If omitted, spawns the user's default shell.
    command: Option<String>,
    /// Working directory for the new pane. If omitted, inherits the current directory.
    cwd: Option<String>,
    /// Split direction: "horizontal" or "vertical". Defaults to "vertical" when splitting.
    split_direction: Option<String>,
    /// Pane ID to split from. If specified, the new pane is created as a sibling of this pane.
    split_from: Option<u64>,
}

// ── Tool result types ───────────────────────────────────────────────────

#[derive(Debug, Serialize, JsonSchema)]
struct SessionListResult {
    session_ids: Vec<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SessionInfoResult {
    session_id: u64,
    name: Option<String>,
    created_at_secs: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SessionCreatedResult {
    session_id: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SessionDestroyedResult {
    session_id: u64,
    destroyed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct WriteToPaneResult {
    pane_id: u64,
    success: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PaneContentResult {
    pane_id: u64,
    lines: Vec<String>,
    cursor_col: usize,
    cursor_line: usize,
    cols: usize,
    rows: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
struct PaneInfo {
    pane_id: u64,
    session_id: u64,
    cols: u16,
    rows: u16,
    title: String,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ListPanesResult {
    panes: Vec<PaneInfo>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct SpawnPaneResult {
    pane_id: u64,
    session_id: u64,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Serialize, JsonSchema)]
struct WaitForOutputResult {
    /// Whether the pattern was matched before timeout.
    matched: bool,
    /// The line number where the match was found (0 if not matched).
    line_number: usize,
    /// The content of the matched line (empty if not matched).
    line_content: String,
    /// Elapsed time in milliseconds.
    elapsed_ms: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetPaneGeometryResult {
    pane_id: u64,
    /// Number of columns in the pane.
    cols: u16,
    /// Number of rows in the pane.
    rows: u16,
    /// Whether the pane can be split horizontally (top/bottom) based on minimum dimensions.
    can_split_h: bool,
    /// Whether the pane can be split vertically (left/right) based on minimum dimensions.
    can_split_v: bool,
}

/// Minimum columns required per pane (derived from MIN_PANE_WIDTH / typical cell width).
const MIN_PANE_COLS: u16 = 10;

/// Minimum rows required per pane (derived from MIN_PANE_HEIGHT / typical cell height).
const MIN_PANE_ROWS: u16 = 4;

#[derive(Debug, Serialize, JsonSchema)]
struct HotspotInfo {
    /// The hotspot type: "file", "url", "git_ref", or "issue".
    #[serde(rename = "type")]
    hotspot_type: String,
    /// The matched text.
    text: String,
    /// Row number in the visible grid (0-based).
    line: usize,
    /// First column of the match (0-based, inclusive).
    col_start: usize,
    /// One-past-last column of the match (exclusive).
    col_end: usize,
    /// Type-specific metadata. For files: resolved absolute path. For URLs: the URL.
    /// For git refs: the ref text. For issues: the issue reference.
    metadata: std::collections::HashMap<String, String>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct GetHotspotsResult {
    pane_id: u64,
    hotspots: Vec<HotspotInfo>,
}

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
struct SemanticRegionInfo {
    /// The region type (Prompt, Command, Output, Error, ToolCall, Thinking, Annotation).
    region_type: String,
    /// The terminal line where this region starts.
    start_line: usize,
    /// The terminal line where this region ends, or null if still open.
    end_line: Option<usize>,
    /// First 200 characters of the region's metadata/content preview.
    content_preview: String,
    /// Metadata (exit_code, cwd, command, timestamp, etc.).
    metadata: std::collections::HashMap<String, String>,
}

// Reserved for terminal.semantic.query_history (Phase 4).
#[allow(dead_code)]
#[derive(Debug, Serialize, JsonSchema)]
struct QuerySemanticHistoryResult {
    pane_id: u64,
    regions: Vec<SemanticRegionInfo>,
}

// ── Helper: serialize to JSON content ───────────────────────────────────

fn json_content<T: Serialize>(value: &T) -> Result<Content, ErrorData> {
    Content::json(value)
        .map_err(|e| ErrorData::internal_error(format!("serialization error: {e}"), None))
}

fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Map<String, serde_json::Value>,
) -> Result<T, ErrorData> {
    serde_json::from_value(serde_json::Value::Object(args))
        .map_err(|e| ErrorData::invalid_params(format!("invalid parameters: {e}"), None))
}

// ── Agent identity extraction ──────────────────────────────────────────

/// Extract the agent identity from the MCP connection context.
///
/// Uses the client's `Implementation.name` from the MCP `initialize`
/// handshake. Falls back to `"unknown"` if the peer info is not available
/// (e.g. before initialization completes).
fn extract_agent_identity(context: &RequestContext<RoleServer>) -> AgentIdentity {
    let name = context
        .peer
        .peer_info()
        .map(|info| info.client_info.name.clone())
        .unwrap_or_else(|| "unknown".to_string());
    AgentIdentity { name }
}

// ── MCP Server ──────────────────────────────────────────────────────────

/// Therminal MCP server handler.
///
/// Wraps a shared `SessionManager` and exposes session/pane operations as
/// MCP tools. Each tool call is gated by trust tier enforcement and
/// audit-logged.
pub struct TherminalMcpServer {
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
}

impl TherminalMcpServer {
    /// Create a new MCP server backed by the given session manager and trust config.
    pub fn new(
        session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
        trust_config: Arc<TrustConfig>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            session_mgr,
            trust_config,
            rate_limiter,
        }
    }

    /// Enforce trust tier and rate limiting for the given tool call.
    ///
    /// Returns `Ok(())` if allowed, or an `Err(CallToolResult)` with
    /// a permission-denied error to return to the client.
    fn enforce_trust(&self, tool_name: &str, agent: &AgentIdentity) -> Result<(), CallToolResult> {
        match check_tool_access(tool_name, agent, &self.trust_config, &self.rate_limiter) {
            TrustCheckResult::Allowed => Ok(()),
            TrustCheckResult::Denied(reason) => {
                Err(CallToolResult::error(vec![Content::text(reason)]))
            }
        }
    }

    async fn handle_list_sessions(&self) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let result = SessionListResult {
            session_ids: mgr.list_sessions(),
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_get_session(
        &self,
        params: SessionIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        match mgr.get_session_info(params.session_id) {
            Some((id, name, created_at_secs)) => {
                let result = SessionInfoResult {
                    session_id: id,
                    name,
                    created_at_secs,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "session not found: {}",
                params.session_id
            ))])),
        }
    }

    async fn handle_create_session(
        &self,
        params: CreateSessionParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        match mgr.create_session(params.name) {
            Ok(session_id) => {
                let result = SessionCreatedResult { session_id };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "failed to create session: {e}"
            ))])),
        }
    }

    async fn handle_spawn_pane(&self, params: SpawnPaneParam) -> Result<CallToolResult, ErrorData> {
        use therminal_terminal::pty::SpawnOptions;

        // Build spawn options from params.
        let spawn_options = SpawnOptions {
            shell: params.command.unwrap_or_default(),
            cwd: params.cwd.unwrap_or_default(),
            ..Default::default()
        };

        // Determine horizontal flag from split_direction (default: vertical).
        let horizontal = match params.split_direction.as_deref() {
            Some("horizontal") => true,
            Some("vertical") | None => false,
            Some(other) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "invalid split_direction: {other}. Valid values: \"horizontal\", \"vertical\""
                ))]));
            }
        };

        let mut mgr = self.session_mgr.lock().await;

        if let Some(split_from_id) = params.split_from {
            // Split from an existing pane.
            match mgr.split_pane_with_options(split_from_id, horizontal, &spawn_options) {
                Ok(new_pane_id) => {
                    // Find the session and pane to get dimensions.
                    let (session_id, cols, rows) =
                        find_pane_info(&mgr, new_pane_id).unwrap_or((0, 80, 24));
                    let result = SpawnPaneResult {
                        pane_id: new_pane_id,
                        session_id,
                        cols,
                        rows,
                    };
                    Ok(CallToolResult::success(vec![json_content(&result)?]))
                }
                Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                    "split_pane failed: {e}"
                ))])),
            }
        } else {
            // Create in an existing session or a new one.
            let session_id = if let Some(sid) = params.session_id {
                if mgr.get_session_info(sid).is_none() {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "session not found: {sid}"
                    ))]));
                }
                sid
            } else if let Some(sid) = mgr.default_session_id() {
                sid
            } else {
                // No sessions exist -- create one.
                match mgr.create_session_with_options(None, &spawn_options) {
                    Ok(sid) => {
                        // The session was just created with a default pane;
                        // find that pane and return it.
                        let (pane_id, cols, rows) =
                            find_first_pane_in_session(&mgr, sid).unwrap_or((0, 80, 24));
                        let result = SpawnPaneResult {
                            pane_id,
                            session_id: sid,
                            cols,
                            rows,
                        };
                        return Ok(CallToolResult::success(vec![json_content(&result)?]));
                    }
                    Err(e) => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "failed to create session: {e}"
                        ))]));
                    }
                }
            };

            // Session exists -- split from its first pane.
            if let Some((first_pane_id, _, _)) = find_first_pane_in_session(&mgr, session_id) {
                match mgr.split_pane_with_options(first_pane_id, horizontal, &spawn_options) {
                    Ok(new_pane_id) => {
                        let (_, cols, rows) =
                            find_pane_info(&mgr, new_pane_id).unwrap_or((session_id, 80, 24));
                        let result = SpawnPaneResult {
                            pane_id: new_pane_id,
                            session_id,
                            cols,
                            rows,
                        };
                        Ok(CallToolResult::success(vec![json_content(&result)?]))
                    }
                    Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "split_pane failed: {e}"
                    ))])),
                }
            } else {
                Ok(CallToolResult::error(vec![Content::text(format!(
                    "session {session_id} has no panes to split from"
                ))]))
            }
        }
    }

    async fn handle_destroy_session(
        &self,
        params: SessionIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        let destroyed = mgr.destroy_session(params.session_id);
        let result = SessionDestroyedResult {
            session_id: params.session_id,
            destroyed,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_write_to_pane(
        &self,
        params: WriteToPaneParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        match mgr.send_keys_to_pane(params.pane_id, params.input.as_bytes()) {
            Ok(()) => {
                let result = WriteToPaneResult {
                    pane_id: params.pane_id,
                    success: true,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "write_to_pane failed: {e}"
            ))])),
        }
    }

    async fn handle_list_panes(&self, params: ListPanesParam) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let mut panes = Vec::new();
        for (session_id, session) in mgr.iter_sessions() {
            if let Some(filter_id) = params.session_id
                && *session_id != filter_id
            {
                continue;
            }
            for window in &session.windows {
                for pane in &window.panes {
                    panes.push(PaneInfo {
                        pane_id: pane.id,
                        session_id: *session_id,
                        cols: pane.cols(),
                        rows: pane.rows(),
                        title: String::new(),
                    });
                }
            }
        }
        let result = ListPanesResult { panes };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_get_pane_geometry(
        &self,
        params: GetPaneGeometryParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        for session in mgr.iter_sessions().map(|(_, s)| s) {
            for window in &session.windows {
                if let Some(pane) = window.pane(params.pane_id) {
                    let cols = pane.cols();
                    let rows = pane.rows();
                    // A horizontal split divides rows; each half must meet MIN_PANE_ROWS.
                    let can_split_h = rows >= MIN_PANE_ROWS * 2;
                    // A vertical split divides cols; each half must meet MIN_PANE_COLS.
                    let can_split_v = cols >= MIN_PANE_COLS * 2;
                    let result = GetPaneGeometryResult {
                        pane_id: params.pane_id,
                        cols,
                        rows,
                        can_split_h,
                        can_split_v,
                    };
                    return Ok(CallToolResult::success(vec![json_content(&result)?]));
                }
            }
        }
        Ok(CallToolResult::error(vec![Content::text(format!(
            "pane not found: {}",
            params.pane_id
        ))]))
    }

    async fn handle_read_pane_content(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        match mgr.capture_pane(params.pane_id) {
            Ok(snap) => {
                let lines: Vec<String> = snap
                    .grid
                    .iter()
                    .map(|row| row.iter().map(|(ch, _)| ch).collect())
                    .collect();
                let result = PaneContentResult {
                    pane_id: snap.pane_id,
                    lines,
                    cursor_col: snap.cursor_col,
                    cursor_line: snap.cursor_line,
                    cols: snap.cols,
                    rows: snap.rows,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "read_pane_content failed: {e}"
            ))])),
        }
    }
    async fn handle_query_semantic_history(
        &self,
        params: QuerySemanticHistoryParam,
    ) -> Result<CallToolResult, ErrorData> {
        use therminal_terminal::region_index::RegionKind;

        // Parse optional region_type filter.
        let kind_filter: Option<RegionKind> = match params.region_type.as_deref() {
            None => None,
            Some(s) => {
                let kind = match s {
                    "Prompt" => RegionKind::Prompt,
                    "Command" => RegionKind::Command,
                    "Output" => RegionKind::Output,
                    "Error" => RegionKind::Error,
                    "ToolCall" => RegionKind::ToolCall,
                    "Thinking" => RegionKind::Thinking,
                    "Annotation" => RegionKind::Annotation,
                    other => {
                        return Ok(CallToolResult::error(vec![Content::text(format!(
                            "unknown region_type: {other}. Valid values: Prompt, Command, Output, Error, ToolCall, Thinking, Annotation"
                        ))]));
                    }
                };
                Some(kind)
            }
        };

        // Compile optional regex pattern.
        let regex = match &params.pattern {
            None => None,
            Some(pat) => match regex::Regex::new(pat) {
                Ok(r) => Some(r),
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "invalid regex pattern: {e}"
                    ))]));
                }
            },
        };

        let limit = params.limit.unwrap_or(20);
        let since_line = params.since_line.unwrap_or(0);

        // Get the region index for this pane.
        let region_index = {
            let mgr = self.session_mgr.lock().await;
            match mgr.pane_region_index(params.pane_id) {
                Ok(idx) => idx,
                Err(e) => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "query_semantic_history failed: {e}"
                    ))]));
                }
            }
        };

        let idx = region_index
            .lock()
            .map_err(|e| ErrorData::internal_error(format!("lock poisoned: {e}"), None))?;

        let kind_name = |k: RegionKind| -> &'static str {
            match k {
                RegionKind::Prompt => "Prompt",
                RegionKind::Command => "Command",
                RegionKind::Output => "Output",
                RegionKind::Error => "Error",
                RegionKind::ToolCall => "ToolCall",
                RegionKind::Thinking => "Thinking",
                RegionKind::Annotation => "Annotation",
            }
        };

        let mut regions = Vec::new();
        for region in idx.regions() {
            // Apply since_line filter.
            if region.start_line < since_line {
                continue;
            }

            // Apply kind filter.
            if let Some(filter_kind) = kind_filter
                && region.kind != filter_kind
            {
                continue;
            }

            // Build content preview from metadata (first 200 chars).
            let content_preview = build_content_preview(region);

            // Apply regex filter on content preview.
            if let Some(ref re) = regex
                && !re.is_match(&content_preview)
            {
                continue;
            }

            regions.push(SemanticRegionInfo {
                region_type: kind_name(region.kind).to_string(),
                start_line: region.start_line,
                end_line: region.end_line,
                content_preview,
                metadata: region.metadata.clone(),
            });

            if regions.len() >= limit {
                break;
            }
        }

        let result = QuerySemanticHistoryResult {
            pane_id: params.pane_id,
            regions,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_get_hotspots(
        &self,
        params: GetHotspotsParam,
    ) -> Result<CallToolResult, ErrorData> {
        use therminal_terminal::hotspot_detection::{HotspotKind, detect_hotspots_from_text};

        // Validate hotspot_type filter if provided.
        let kind_filter: Option<&str> = match params.hotspot_type.as_deref() {
            None => None,
            Some(s @ ("file" | "url" | "git_ref" | "issue")) => Some(s),
            Some(other) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "unknown hotspot_type: {other}. Valid values: file, url, git_ref, issue"
                ))]));
            }
        };

        let limit = params.limit.unwrap_or(50);

        // Capture visible pane content.
        let mgr = self.session_mgr.lock().await;
        let snap = match mgr.capture_pane(params.pane_id) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "get_hotspots failed: {e}"
                ))]));
            }
        };
        drop(mgr);

        // Convert grid rows to strings.
        let rows: Vec<String> = snap
            .grid
            .iter()
            .map(|row| row.iter().map(|(ch, _)| ch).collect())
            .collect();

        let detected = detect_hotspots_from_text(&rows);

        let mut hotspots = Vec::new();
        for h in detected {
            let type_str = h.kind.as_str();

            // Apply type filter.
            if let Some(filter) = kind_filter
                && type_str != filter
            {
                continue;
            }

            // Build type-specific metadata.
            let mut metadata = std::collections::HashMap::new();
            match &h.kind {
                HotspotKind::FilePath | HotspotKind::ErrorLocation => {
                    let (path_part, line_suffix) = split_file_path_parts(&h.text);
                    metadata.insert("path".to_string(), path_part.to_string());
                    if !line_suffix.is_empty() {
                        metadata.insert("location".to_string(), line_suffix.to_string());
                    }
                }
                HotspotKind::Url => {
                    metadata.insert("url".to_string(), h.text.clone());
                }
                HotspotKind::GitRef => {
                    metadata.insert("ref".to_string(), h.text.clone());
                }
                HotspotKind::IssueRef => {
                    metadata.insert("ref".to_string(), h.text.clone());
                }
            }

            hotspots.push(HotspotInfo {
                hotspot_type: type_str.to_string(),
                text: h.text,
                line: h.line,
                col_start: h.col_start,
                col_end: h.col_end,
                metadata,
            });

            if hotspots.len() >= limit {
                break;
            }
        }

        let result = GetHotspotsResult {
            pane_id: params.pane_id,
            hotspots,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    async fn handle_wait_for_output(
        &self,
        params: WaitForOutputParam,
    ) -> Result<CallToolResult, ErrorData> {
        use therminal_protocol::daemon::DaemonEvent;

        // Clamp timeout to max 120 seconds.
        let timeout_ms = params.timeout_ms.min(120_000);
        let timeout_dur = std::time::Duration::from_millis(timeout_ms);

        // Compile the pattern as a regex.
        let regex = match regex::Regex::new(&params.pattern) {
            Ok(r) => r,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "invalid pattern: {e}"
                ))]));
            }
        };

        // Verify the pane exists and subscribe to events while holding the lock briefly.
        let mut event_rx = {
            let mgr = self.session_mgr.lock().await;
            // Check pane exists.
            let pane_found = mgr
                .iter_sessions()
                .flat_map(|(_, s)| s.windows.iter())
                .any(|w| w.pane(params.pane_id).is_some());
            if !pane_found {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "pane not found: {}",
                    params.pane_id
                ))]));
            }
            mgr.subscribe_events()
        };
        // Lock is released here -- the handler is now async/long-running.

        // Also check existing content for an immediate match (if since_line is set,
        // scan current buffer first so we don't miss lines already present).
        if let Some(since_line) = params.since_line {
            let mgr = self.session_mgr.lock().await;
            if let Ok(snap) = mgr.capture_pane(params.pane_id) {
                for (i, row) in snap.grid.iter().enumerate() {
                    let line_num = i;
                    if line_num < since_line {
                        continue;
                    }
                    let line_content: String = row.iter().map(|(ch, _)| ch).collect();
                    let trimmed = line_content.trim_end();
                    if regex.is_match(trimmed) {
                        let result = WaitForOutputResult {
                            matched: true,
                            line_number: line_num,
                            line_content: trimmed.to_string(),
                            elapsed_ms: 0,
                        };
                        return Ok(CallToolResult::success(vec![json_content(&result)?]));
                    }
                }
            }
        }

        let start = std::time::Instant::now();

        // Track cumulative line count from streamed output for since_line filtering.
        // We use a simple line counter: each PaneOutput chunk may contain partial
        // or multiple lines; we scan the decoded text line by line.
        let mut accumulated_lines: usize = params.since_line.unwrap_or(0);

        let result = tokio::time::timeout(timeout_dur, async {
            loop {
                match event_rx.recv().await {
                    Ok(DaemonEvent::PaneOutput { pane_id, data, .. })
                        if pane_id == params.pane_id =>
                    {
                        // Decode output chunk and scan for pattern matches.
                        let text = String::from_utf8_lossy(&data);
                        for line in text.lines() {
                            let trimmed = line.trim_end();
                            if regex.is_match(trimmed) {
                                return WaitForOutputResult {
                                    matched: true,
                                    line_number: accumulated_lines,
                                    line_content: trimmed.to_string(),
                                    elapsed_ms: start.elapsed().as_millis() as u64,
                                };
                            }
                            accumulated_lines += 1;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // Missed some events due to slow consumption; continue.
                        debug!("wait_for_output: lagged by {n} events, continuing");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Channel closed -- pane likely destroyed.
                        break;
                    }
                    _ => {
                        // Other event types -- ignore.
                    }
                }
            }
            WaitForOutputResult {
                matched: false,
                line_number: 0,
                line_content: String::new(),
                elapsed_ms: start.elapsed().as_millis() as u64,
            }
        })
        .await;

        let result = match result {
            Ok(r) => r,
            Err(_timeout) => WaitForOutputResult {
                matched: false,
                line_number: 0,
                line_content: String::new(),
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
        };

        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }
}

/// Build a content preview string from a region's metadata (first 200 chars).
fn build_content_preview(region: &therminal_terminal::region_index::Region) -> String {
    // Prefer "command" metadata, then "cwd", then concatenate all metadata values.
    let raw = if let Some(cmd) = region.metadata.get("command") {
        cmd.clone()
    } else if let Some(cwd) = region.metadata.get("cwd") {
        cwd.clone()
    } else if region.metadata.is_empty() {
        String::new()
    } else {
        region
            .metadata
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if raw.len() > 200 {
        format!("{}...", &raw[..197])
    } else {
        raw
    }
}

/// Split a file path like `src/main.rs:42:5` into (`src/main.rs`, `:42:5`).
fn split_file_path_parts(text: &str) -> (&str, &str) {
    if let Some(idx) = text.find(':')
        && text[idx + 1..].starts_with(|c: char| c.is_ascii_digit())
    {
        return (&text[..idx], &text[idx..]);
    }
    (text, "")
}

// ── Helpers for pane lookup ─────────────────────────────────────────────

/// Find (session_id, cols, rows) for a pane by ID across all sessions.
fn find_pane_info(mgr: &SessionManager, pane_id: u64) -> Option<(u64, u16, u16)> {
    for (session_id, session) in mgr.iter_sessions() {
        for window in &session.windows {
            if let Some(pane) = window.pane(pane_id) {
                return Some((*session_id, pane.cols(), pane.rows()));
            }
        }
    }
    None
}

/// Find (pane_id, cols, rows) of the first pane in a session.
fn find_first_pane_in_session(mgr: &SessionManager, session_id: u64) -> Option<(u64, u16, u16)> {
    for (sid, session) in mgr.iter_sessions() {
        if *sid == session_id
            && let Some(window) = session.windows.first()
            && let Some(pane) = window.panes.first()
        {
            return Some((pane.id, pane.cols(), pane.rows()));
        }
    }
    None
}

// ── Tool definitions ────────────────────────────────────────────────────

fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            "terminal.sessions.list",
            "List all active terminal session IDs",
            schema_for_type::<()>(),
        ),
        Tool::new(
            "terminal.sessions.get",
            "Get details about a specific terminal session (name, creation time)",
            schema_for_type::<SessionIdParam>(),
        ),
        Tool::new(
            "terminal.sessions.create",
            "Create a new terminal session with a shell and PTY. Returns the new session ID.",
            schema_for_type::<CreateSessionParam>(),
        ),
        Tool::new(
            "terminal.sessions.destroy",
            "Destroy a terminal session and all its panes",
            schema_for_type::<SessionIdParam>(),
        ),
        Tool::new(
            "terminal.panes.create",
            "Create a new terminal pane with a PTY. Can split from an existing pane or add to a session. Supports custom shell command and working directory.",
            schema_for_type::<SpawnPaneParam>(),
        ),
        Tool::new(
            "terminal.panes.list",
            "List all panes with their dimensions, session membership, and title. Optionally filter by session ID.",
            schema_for_type::<ListPanesParam>(),
        ),
        Tool::new(
            "terminal.panes.write",
            "Write text input to a pane's PTY (send keystrokes or commands to the terminal)",
            schema_for_type::<WriteToPaneParam>(),
        ),
        Tool::new(
            "terminal.panes.get_geometry",
            "Get a pane's grid dimensions (cols, rows) and whether it can be split horizontally or vertically based on minimum pane size constraints",
            schema_for_type::<GetPaneGeometryParam>(),
        ),
        Tool::new(
            "terminal.panes.get_content",
            "Read the current visible content of a terminal pane (grid snapshot with cursor position)",
            schema_for_type::<PaneIdParam>(),
        ),
        Tool::new(
            "terminal.semantic.query_history",
            "Query the semantic region index for a pane. Returns typed regions (Prompt, Command, Output, Error, etc.) with metadata. Supports filtering by region type, regex pattern, and scroll position.",
            schema_for_type::<QuerySemanticHistoryParam>(),
        ),
        Tool::new(
            "terminal.panes.wait_for_output",
            "Wait for output matching a pattern (string or regex) to appear in a pane. Subscribes to live PTY output and returns on first match or timeout. Useful for waiting for command completion, prompts, or specific output.",
            schema_for_type::<WaitForOutputParam>(),
        ),
        Tool::new(
            "terminal.semantic.get_hotspots",
            "Scan visible pane content for actionable hotspots: file paths, URLs, git refs (hashes and branches), and issue references. Returns matches with type, position, text, and type-specific metadata.",
            schema_for_type::<GetHotspotsParam>(),
        ),
    ]
}

// ── ServerHandler impl ──────────────────────────────────────────────────

impl ServerHandler for TherminalMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(
            "Therminal MCP server. Provides tools to manage terminal sessions and panes.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.as_ref();
        let args = request.arguments.unwrap_or_default();

        // Extract agent identity from the MCP connection context.
        let agent = extract_agent_identity(&context);

        // Enforce trust tier and rate limiting.
        if let Err(denied_result) = self.enforce_trust(name, &agent) {
            return Ok(denied_result);
        }

        match name {
            "terminal.sessions.list" => self.handle_list_sessions().await,
            "terminal.sessions.get" => {
                let params: SessionIdParam = parse_args(args)?;
                self.handle_get_session(params).await
            }
            "terminal.sessions.create" => {
                let params: CreateSessionParam = parse_args(args)?;
                self.handle_create_session(params).await
            }
            "terminal.sessions.destroy" => {
                let params: SessionIdParam = parse_args(args)?;
                self.handle_destroy_session(params).await
            }
            "terminal.panes.create" => {
                let params: SpawnPaneParam = parse_args(args)?;
                self.handle_spawn_pane(params).await
            }
            "terminal.panes.list" => {
                let params: ListPanesParam = parse_args(args)?;
                self.handle_list_panes(params).await
            }
            "terminal.panes.write" => {
                let params: WriteToPaneParam = parse_args(args)?;
                self.handle_write_to_pane(params).await
            }
            "terminal.panes.get_geometry" => {
                let params: GetPaneGeometryParam = parse_args(args)?;
                self.handle_get_pane_geometry(params).await
            }
            "terminal.panes.get_content" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_read_pane_content(params).await
            }
            "terminal.semantic.query_history" => {
                let params: QuerySemanticHistoryParam = parse_args(args)?;
                self.handle_query_semantic_history(params).await
            }
            "terminal.panes.wait_for_output" => {
                let params: WaitForOutputParam = parse_args(args)?;
                self.handle_wait_for_output(params).await
            }
            "terminal.semantic.get_hotspots" => {
                let params: GetHotspotsParam = parse_args(args)?;
                self.handle_get_hotspots(params).await
            }
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}

// ── Server lifecycle ────────────────────────────────────────────────────

/// Start the MCP server, accepting connections on the given Unix socket.
///
/// Each accepted connection is served independently. The server runs until
/// the `shutdown` notify is triggered. Trust enforcement uses the provided
/// `TrustConfig` to gate tool access per agent.
pub async fn start_mcp_server(
    config: therminal_core::config::McpConfig,
    session_mgr: Arc<tokio::sync::Mutex<SessionManager>>,
    trust_config: Arc<TrustConfig>,
    rate_limiter: Arc<RateLimiter>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<()> {
    if !config.enabled {
        info!("MCP server disabled by config");
        return Ok(());
    }

    let socket_path = config.resolved_socket_path();

    // Clean stale socket
    match std::fs::remove_file(&socket_path) {
        Ok(()) => debug!(path = %socket_path.display(), "removed stale MCP socket"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::anyhow!(
                "failed to remove stale MCP socket {}: {e}",
                socket_path.display()
            ));
        }
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind MCP socket: {}", socket_path.display()))?;

    // Set socket permissions on Unix (owner-only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(&socket_path, perms).ok();
    }

    info!(path = %socket_path.display(), "MCP server listening");

    loop {
        tokio::select! {
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _addr)) => {
                        let sm = Arc::clone(&session_mgr);
                        let tc = Arc::clone(&trust_config);
                        let rl = Arc::clone(&rate_limiter);
                        tokio::spawn(async move {
                            let server = TherminalMcpServer::new(sm, tc, rl);
                            let (reader, writer) = stream.into_split();
                            match server.serve((reader, writer)).await {
                                Ok(running) => {
                                    if let Err(e) = running.waiting().await {
                                        debug!(error = %e, "MCP connection task ended");
                                    }
                                }
                                Err(e) => {
                                    debug!(error = %e, "MCP connection init failed");
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!(error = %e, "MCP accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = shutdown.notified() => {
                info!("MCP server shutting down");
                break;
            }
        }
    }

    // Clean up socket
    cleanup_socket(&socket_path);
    Ok(())
}

/// Remove the MCP socket file.
fn cleanup_socket(path: &Path) {
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            warn!(error = %e, path = %path.display(), "failed to remove MCP socket on cleanup");
        } else {
            debug!(path = %path.display(), "MCP socket cleaned up");
        }
    }
}
