//! MCP tool handler implementations and `tool_definitions()`.
//!
//! Each `handle_*` method implements one of the tools advertised by the
//! Therminal MCP server. Trust enforcement and argument parsing happen in
//! `mod.rs::call_tool`; these methods assume the call is already authorised.
//!
//! ## CLI-preferred vs MCP-required classification
//!
//! Most read operations have a CLI counterpart (`therminal pane list`, etc.).
//! **Prefer the CLI for any tool in the "CLI-preferred" column below** —
//! it returns the same data with dramatically less prompt-cache churn.
//! Reserve MCP for the "MCP-required" column where the structured shape or
//! subscription contract is essential.
//!
//! | Tool | Preferred surface | Reason |
//! |------|------------------|--------|
//! | `terminal.sessions.list` | CLI (`therminal session list`) | Simple read; TSV << JSON-RPC |
//! | `terminal.sessions.get` | CLI (wrap with `session list --json \| jq`) | Rarely needed standalone |
//! | `terminal.sessions.create` | CLI (`therminal session create`) | Fire-and-forget write |
//! | `terminal.sessions.destroy` | MCP (Admin tier; trust enforcement needed) | Destructive |
//! | `terminal.panes.list` | CLI (`therminal pane list`) | Most frequent read; ~50–150 bytes TSV |
//! | `terminal.panes.create` | CLI (`therminal pane create`) | Fire-and-forget write |
//! | `terminal.panes.destroy` | MCP (Admin tier; trust enforcement needed) | Destructive |
//! | `terminal.panes.get_content` | MCP or CLI peek | Use CLI `pane peek` for quick tail; MCP for full grid with optional params |
//! | `terminal.panes.get_summary` | MCP (`get_summary` is MCP-only, no CLI peer) | Cheapest polling primitive; use in conductor tick loops |
//! | `terminal.panes.peek` | CLI (`therminal pane peek`) | Warm-path polling; TSV tail is cache-friendlier |
//! | `terminal.panes.get_geometry` | CLI (`pane list` includes cols×rows) | Rarely needed standalone |
//! | `terminal.panes.write` | CLI (`therminal pane send`) | Keystroke delivery; round-trip bytes dominate |
//! | `terminal.panes.tag` | CLI (`therminal pane tag`) | Metadata write; one-liner |
//! | `terminal.panes.untag` | CLI (`therminal pane untag`) | Metadata write; one-liner |
//! | `terminal.panes.wait_for_output` | MCP | Blocking wait; needs async MCP contract |
//! | `terminal.panes.query_events` | MCP | Structured ring-buffer; shape matters |
//! | `terminal.semantic.query_history` | MCP | Structured region index; shape matters |
//! | `terminal.semantic.query_commands` | CLI (`therminal semantic commands`) | TSV is sufficient for most callers |
//! | `terminal.semantic.get_hotspots` | CLI (`therminal semantic hotspots`) | TSV is sufficient for most callers |
//! | `terminal.workspaces.list` | CLI (`therminal workspace list`) | Simple read; TSV << JSON-RPC |
//! | `terminal.workspaces.get_layout` | MCP | Binary layout tree; structured shape essential |
//! | `terminal.agents.list` | CLI (`therminal agents list`) | Swarm polling; TSV is cache-friendlier |
//! | `terminal.agents.find_with_capacity` | MCP | Structured capacity sort; shape matters |
//! | `terminal.agents.get_status` | MCP | Sibling-agent coordination; typed contract |
//! | `terminal.agents.get_details` | MCP | Rich inference snapshot; typed contract |
//! | `terminal.agents.get_cadence` | MCP | Timing metrics; structured shape essential |
//! | `terminal.patterns.stats` | MCP | Pattern engine diagnostics; rarely called |
//! | `terminal.events.stats` | MCP | Event bus diagnostics; rarely called |
//!
//! **Decision rule for agents**: if you would call the same tool more than
//! once per agent turn (polling, fan-out, swarm inspection), or if you do not
//! need typed fields fed back into the tool-use loop, **use the CLI**.
//! Use MCP when you need subscriptions, `wait_for_output`, or a structured
//! response that drives downstream tool calls.

use rmcp::ErrorData;
use rmcp::handler::server::tool::schema_for_type;
use rmcp::model::{CallToolResult, Content, Tool};
use tracing::debug;

use super::{
    AgentCadenceResult, AgentCapacityInfo, AgentDetailsResult, AgentInfoResult, AgentSessionDetail,
    AgentStatusResult, CadenceSampleResult, CommandInfo, CreateSessionParam, DestroyPaneResult,
    EmptyParams, EventInfo, FindWithCapacityParam, FindWithCapacityResult, GetContentParam,
    GetHotspotsParam, GetHotspotsResult, GetPaneGeometryParam, GetPaneGeometryResult,
    GetWorkspaceLayoutParam, GetWorkspaceLayoutResult, HotspotInfo, LayoutNodeJson,
    ListAgentsParam, ListAgentsResult, ListPanesParam, ListPanesResult, ListWorkspacesParam,
    ListWorkspacesResult, MIN_PANE_COLS, MIN_PANE_ROWS, PaneContentResult, PaneIdParam, PaneInfo,
    PanePeekResult, PaneSummaryResult, PaneTagsResult, PeekPaneParam, QueryCommandsParam,
    QueryCommandsResult, QueryEventsParam, QueryEventsResult, QuerySemanticHistoryParam,
    QuerySemanticHistoryResult, SemanticRegionInfo, SessionCreatedResult, SessionDestroyedResult,
    SessionIdParam, SessionInfoResult, SessionListResult, SpawnPaneParam, SpawnPaneResult,
    TagPaneParam, TherminalMcpServer, UntagPaneParam, WaitForOutputParam, WaitForOutputResult,
    WorkspaceInfoResult, WriteToPaneParam, WriteToPaneResult, build_content_preview,
    find_first_pane_in_session, find_pane_info, json_content, now_unix_secs, pane_content_hash,
    render_grid_lines,
};

impl TherminalMcpServer {
    pub(super) async fn handle_list_sessions(&self) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let result = SessionListResult {
            session_ids: mgr.list_sessions(),
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    pub(super) async fn handle_get_session(
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

    pub(super) async fn handle_create_session(
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

    pub(super) async fn handle_spawn_pane(
        &self,
        params: SpawnPaneParam,
    ) -> Result<CallToolResult, ErrorData> {
        use therminal_terminal::pty::SpawnOptions;

        // Build spawn options from params.
        let spawn_options = SpawnOptions {
            shell: params.command.unwrap_or_default(),
            cwd: params.cwd.unwrap_or_default(),
            ..Default::default()
        };
        let startup_command = params.startup_command.as_deref();

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
            match mgr.split_pane_with_options(
                split_from_id,
                horizontal,
                &spawn_options,
                startup_command,
                None,
            ) {
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
                        if let Err(e) = mgr.maybe_send_startup_command(pane_id, startup_command) {
                            return Ok(CallToolResult::error(vec![Content::text(format!(
                                "startup_command failed: {e}"
                            ))]));
                        }
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
                match mgr.split_pane_with_options(
                    first_pane_id,
                    horizontal,
                    &spawn_options,
                    startup_command,
                    None,
                ) {
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

    pub(super) async fn handle_destroy_pane(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        match mgr.kill_pane(params.pane_id) {
            Ok(()) => {
                let result = DestroyPaneResult {
                    success: true,
                    message: format!("pane {} destroyed", params.pane_id),
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => {
                let result = DestroyPaneResult {
                    success: false,
                    message: format!("failed to destroy pane: {e}"),
                };
                Ok(CallToolResult::error(vec![json_content(&result)?]))
            }
        }
    }

    pub(super) async fn handle_destroy_session(
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

    pub(super) async fn handle_write_to_pane(
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

    pub(super) async fn handle_tag_pane(
        &self,
        params: TagPaneParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        match mgr.tag_pane(params.pane_id, params.tags) {
            Ok(tags) => {
                let result = PaneTagsResult {
                    pane_id: params.pane_id,
                    tags,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "tag_pane failed: {e}"
            ))])),
        }
    }

    pub(super) async fn handle_untag_pane(
        &self,
        params: UntagPaneParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mut mgr = self.session_mgr.lock().await;
        match mgr.untag_pane(params.pane_id, params.keys) {
            Ok(tags) => {
                let result = PaneTagsResult {
                    pane_id: params.pane_id,
                    tags,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "untag_pane failed: {e}"
            ))])),
        }
    }

    pub(super) async fn handle_list_panes(
        &self,
        params: ListPanesParam,
    ) -> Result<CallToolResult, ErrorData> {
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
                    let cwd = {
                        let c = pane.cwd();
                        if c.is_empty() { None } else { Some(c) }
                    };
                    let last_exit_code = pane.last_exit_code();
                    let agent_name = mgr.agent_registry().get(pane.id).map(|e| e.name.clone());
                    let tags = pane.tags();
                    let session_title = mgr.pane_capacity(pane.id).and_then(|e| e.session_title);
                    panes.push(PaneInfo {
                        pane_id: pane.id,
                        session_id: *session_id,
                        cols: pane.cols(),
                        rows: pane.rows(),
                        title: String::new(),
                        cwd,
                        last_exit_code,
                        agent_name,
                        tags,
                        session_title,
                    });
                }
            }
        }
        let result = ListPanesResult { panes };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    pub(super) async fn handle_list_workspaces(
        &self,
        params: ListWorkspacesParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let mut workspaces = Vec::new();

        for (session_id, session) in mgr.iter_sessions() {
            if let Some(filter_id) = params.session_id
                && *session_id != filter_id
            {
                continue;
            }
            let active_ws = session.active_workspace;
            for ws in &session.workspace_state {
                workspaces.push(WorkspaceInfoResult {
                    workspace_id: ws.id,
                    name: ws.name.clone(),
                    pane_count: ws.pane_ids.len(),
                    is_active: ws.id == active_ws,
                    pane_ids: ws.pane_ids.clone(),
                });
            }
        }

        let result = ListWorkspacesResult { workspaces };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    pub(super) async fn handle_get_workspace_layout(
        &self,
        params: GetWorkspaceLayoutParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        // Find the first session whose workspace_state contains `workspace_id`.
        // If `session_id` is provided, restrict to that session.
        for (session_id, session) in mgr.iter_sessions() {
            if let Some(filter_id) = params.session_id
                && *session_id != filter_id
            {
                continue;
            }
            if let Some(ws) = session
                .workspace_state
                .iter()
                .find(|w| w.id == params.workspace_id)
            {
                let (layout, degraded) = match &ws.layout {
                    Some(snap) => (LayoutNodeJson::from_snapshot(snap), false),
                    None => (LayoutNodeJson::from_flat_pane_ids(&ws.pane_ids), true),
                };
                let result = GetWorkspaceLayoutResult {
                    workspace_id: ws.id,
                    session_id: *session_id,
                    layout,
                    focused_pane: ws.focused_pane,
                    degraded,
                };
                return Ok(CallToolResult::success(vec![json_content(&result)?]));
            }
        }

        Err(ErrorData::invalid_params(
            format!("workspace {} not found", params.workspace_id),
            None,
        ))
    }

    pub(super) async fn handle_list_agents(
        &self,
        params: ListAgentsParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let entries = match &params.status {
            Some(status) => mgr.list_agents_by_status(status),
            None => mgr.list_agents(),
        };

        let agents: Vec<AgentInfoResult> = entries
            .into_iter()
            .map(|e| AgentInfoResult {
                pane_id: e.pane_id,
                name: e.name,
                agent_type: e.agent_type.as_str().to_string(),
                status: e.status.as_str().to_string(),
                current_tool: e.status.tool_name().map(String::from),
                detected_at: e.detected_at,
                pid: e.pid,
            })
            .collect();

        let result = ListAgentsResult { agents };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return detailed inference data for the agent running in a pane.
    ///
    /// The pane must exist. `agent_type` is sourced exclusively from
    /// `AgentRegistry` (process-tree detection, tn-hxso). No fallback to
    /// the inference engine's own type detection.
    pub(super) async fn handle_get_agent_details(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        // Verify the pane exists — return a tool error if not, matching the
        // convention of get_pane_geometry / read_pane_content.
        let Some(snapshot) = mgr.pane_agent_details(params.pane_id) else {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "pane not found: {}",
                params.pane_id
            ))]));
        };

        let registry_agent_type = mgr
            .agent_registry()
            .get(params.pane_id)
            .map(|e| e.agent_type.as_str().to_string());

        let result = AgentDetailsResult {
            agent_type: registry_agent_type,
            model: snapshot.model,
            context_percent: snapshot.context_percent,
            consecutive_failures: snapshot.consecutive_failures.max(0) as u32,
            last_command: snapshot.last_command,
            last_exit_code: snapshot.last_exit_code,
            last_command_duration_ms: snapshot
                .last_command_duration_ms
                .and_then(|d| if d < 0 { None } else { Some(d as u64) }),
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return output cadence metrics for the agent running in a pane.
    ///
    /// The pane must exist (returns a tool error otherwise). All metric
    /// fields default to zero / `false` / empty when the pane has no
    /// streaming activity, so callers can use the result unconditionally
    /// instead of branching on `Option`s. `recent_samples` is capped at
    /// 50 entries by the underlying snapshot DTO so the wire payload stays
    /// bounded even if the chunk-stats window grows in future revisions.
    pub(super) async fn handle_get_agent_cadence(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        let Some(snapshot) = mgr.pane_agent_cadence(params.pane_id) else {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "pane not found: {}",
                params.pane_id
            ))]));
        };

        let recent_samples: Vec<CadenceSampleResult> = snapshot
            .recent_samples
            .into_iter()
            .map(|s| CadenceSampleResult {
                timestamp_secs: s.timestamp_secs,
                bytes: s.bytes as u64,
                gap_ms: s.gap_ms,
            })
            .collect();

        let result = AgentCadenceResult {
            chunk_count: snapshot.chunk_count as u64,
            avg_arrival_ms: snapshot.avg_arrival_ms,
            max_gap_ms: snapshot.max_gap_ms,
            is_spinner: snapshot.is_spinner,
            is_streaming: snapshot.is_streaming,
            recent_samples,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return a dynamic mode + capacity snapshot for one pane's agent.
    ///
    /// Strict subset of `handle_get_agent_details` intended for sibling-agent
    /// coordination. Combines the `AgentRegistry` view (`agent_type`,
    /// `status`, `current_tool`) with the `PaneCapacityCache` view
    /// (`context_percent`, `model`). If neither lookup yields anything for
    /// the pane, returns a tool error.
    pub(super) async fn handle_get_agent_status(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        let registry_entry = mgr.agent_registry().get(params.pane_id);
        let capacity_entry = mgr.pane_capacity(params.pane_id);

        if registry_entry.is_none() && capacity_entry.is_none() {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "no agent for pane_id {}",
                params.pane_id
            ))]));
        }

        let (agent_type, status, current_tool) = match registry_entry {
            Some(e) => (
                Some(e.agent_type.as_str().to_string()),
                e.status.as_str().to_string(),
                e.status.tool_name().map(String::from),
            ),
            None => (None, "unknown".to_string(), None),
        };

        let (context_percent, model) = match capacity_entry {
            Some(e) => (e.context_percent, e.model),
            None => (None, None),
        };

        let result = AgentStatusResult {
            pane_id: params.pane_id,
            agent_type,
            status,
            current_tool,
            context_percent,
            model,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return the enriched Claude Code session detail for a pane (tn-ifee).
    ///
    /// Surfaces the five fields agreed in tn-ifee:
    /// `session_title`, `current_tool`, `working_dir`, `context_percent`,
    /// `model`. All fields are sourced from the per-pane `PaneCapacityCache`
    /// — populated by the Claude state poller in `therminal-harness-claude`.
    /// All fields are nullable. Returns a tool error only if the pane does
    /// not exist.
    pub(super) async fn handle_get_agent_session_detail(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        // Verify pane exists — match the convention of get_agent_details.
        if mgr.pane_agent_details(params.pane_id).is_none() {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "pane not found: {}",
                params.pane_id
            ))]));
        }

        let cap = mgr.pane_capacity(params.pane_id);
        let result = AgentSessionDetail {
            pane_id: params.pane_id,
            session_title: cap.as_ref().and_then(|e| e.session_title.clone()),
            current_tool: cap.as_ref().and_then(|e| e.current_tool.clone()),
            working_dir: cap.as_ref().and_then(|e| e.working_dir.clone()),
            context_percent: cap.as_ref().and_then(|e| e.context_percent),
            model: cap.as_ref().and_then(|e| e.model.clone()),
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return all agents whose remaining context-window capacity meets the
    /// threshold.
    ///
    /// `threshold_percent` is interpreted as the minimum *remaining* percent.
    /// For each agent in the registry, look up the matching `PaneCapacityCache`
    /// entry and compute `remaining_percent = 100.0 - context_percent`. If no
    /// capacity entry exists (or `context_percent` is `None`), the agent is
    /// included by design — unknown capacity is treated as "potentially has
    /// capacity" so callers don't accidentally exclude fresh panes. Results
    /// are sorted by `remaining_percent` descending; unknown sorts last.
    pub(super) async fn handle_find_with_capacity(
        &self,
        params: FindWithCapacityParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        let threshold = params.threshold_percent;

        let mut agents: Vec<AgentCapacityInfo> = mgr
            .list_agents()
            .into_iter()
            .filter_map(|entry| {
                let capacity = mgr.pane_capacity(entry.pane_id);
                let (context_percent, model) = match capacity {
                    Some(e) => (e.context_percent, e.model),
                    None => (None, None),
                };
                let remaining_percent = context_percent.map(|c| 100.0 - c);
                let include = match remaining_percent {
                    None => true,
                    Some(r) => r >= threshold,
                };
                if !include {
                    return None;
                }
                Some(AgentCapacityInfo {
                    pane_id: entry.pane_id,
                    agent_type: Some(entry.agent_type.as_str().to_string()),
                    status: entry.status.as_str().to_string(),
                    context_percent,
                    remaining_percent,
                    model,
                })
            })
            .collect();

        // Sort descending by remaining_percent; None last.
        agents.sort_by(|a, b| match (a.remaining_percent, b.remaining_percent) {
            (Some(ap), Some(bp)) => bp.partial_cmp(&ap).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

        let result = FindWithCapacityResult { agents };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return recent shell commands captured via OSC 633 `CommandTracker`.
    ///
    /// Snapshots the per-pane `CommandTracker` (cloned under the lock) and
    /// applies the optional `since_line` filter, then keeps the newest
    /// `limit` entries (default 20). Result `commands` are returned
    /// oldest-first within the truncated window.
    pub(super) async fn handle_query_commands(
        &self,
        params: QueryCommandsParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        let blocks = match mgr.pane_command_blocks(params.pane_id) {
            Some(b) => b,
            None => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "pane not found: {}",
                    params.pane_id
                ))]));
            }
        };

        let limit = params.limit.unwrap_or(20).min(20);
        let since_line = params.since_line.unwrap_or(0);

        // Filter by `since_line`, drop all but the newest `limit` blocks,
        // and return them oldest-first within the truncated window.
        let filtered: Vec<_> = blocks
            .into_iter()
            .filter(|b| b.start_line >= since_line)
            .collect();
        let total = filtered.len();
        let start = total.saturating_sub(limit);
        let commands: Vec<CommandInfo> = filtered[start..]
            .iter()
            .map(|b| CommandInfo {
                command_text: b.command.clone(),
                exit_code: b.exit_code,
                duration_ms: b.duration.map(|d| d.as_millis() as u64),
                start_line: b.start_line,
                end_line: b.end_line,
                timestamp_secs: b.started_at.and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs())
                }),
            })
            .collect();

        let result = QueryCommandsResult {
            pane_id: params.pane_id,
            commands,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return recent structured events from a pane's in-memory `EventLog`.
    ///
    /// Snapshots the per-pane ring buffer (capped at 5000 entries) and
    /// applies the optional `since_timestamp_secs` filter, then keeps the
    /// newest `limit` entries (default 100). Result `events` are returned
    /// oldest-first within the truncated window. The JSONL file is never
    /// read — only the in-memory ring is exposed.
    pub(super) async fn handle_query_events(
        &self,
        params: QueryEventsParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        let limit = params.limit.unwrap_or(100);
        let stored =
            match mgr.pane_event_log_snapshot(params.pane_id, params.since_timestamp_secs, limit) {
                Some(events) => events,
                None => {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "pane not found: {}",
                        params.pane_id
                    ))]));
                }
            };

        let events: Vec<EventInfo> = stored
            .into_iter()
            .map(|s| {
                // Serialize the SessionEvent (uses #[serde(tag = "event")]
                // internally) so we get an object like {"event": "spawn",
                // "command": ..., "cwd": ...}. Split off the tag for the
                // typed `event_type` field, leaving the rest as `details`.
                let mut value = serde_json::to_value(&s.event)
                    .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
                let event_type = value
                    .as_object_mut()
                    .and_then(|m| m.remove("event"))
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                EventInfo {
                    timestamp_secs: s.timestamp_secs,
                    event_type,
                    details: value,
                }
            })
            .collect();

        let result = QueryEventsResult {
            pane_id: params.pane_id,
            events,
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    pub(super) async fn handle_get_pane_geometry(
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

    pub(super) async fn handle_read_pane_content(
        &self,
        params: GetContentParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;
        match mgr.capture_pane(params.pane_id) {
            Ok(snap) => {
                // Compute the content hash from the FULL untrimmed grid so
                // it stays stable regardless of which trim/compact/rows
                // options the caller picks.
                let content_hash = pane_content_hash(&snap);

                let mut lines =
                    render_grid_lines(&snap, params.trim_trailing_whitespace, params.compact);

                // Apply `rows = last_N` AFTER trim/compact filtering so the
                // semantics are "last N visible / non-empty rows", not
                // "last N grid rows including blank padding".
                let truncated_rows = if let Some(last_n) = params.rows {
                    let total = lines.len();
                    let take = last_n.min(total);
                    let drop = total - take;
                    if drop > 0 {
                        lines.drain(0..drop);
                    }
                    drop
                } else {
                    0
                };

                let result = PaneContentResult {
                    pane_id: snap.pane_id,
                    lines,
                    cursor_col: snap.cursor_col,
                    cursor_line: snap.cursor_line,
                    cols: snap.cols,
                    rows: snap.rows,
                    content_hash,
                    truncated_rows,
                };
                Ok(CallToolResult::success(vec![json_content(&result)?]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "read_pane_content failed: {e}"
            ))])),
        }
    }

    /// Return a tiny status snapshot for a pane (~100 bytes) — designed
    /// for conductor polling. See tn-sp3n.
    pub(super) async fn handle_get_pane_summary(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        use therminal_terminal::hotspot_detection::detect_hotspots_from_text;

        let mgr = self.session_mgr.lock().await;

        // Capture the pane so the cursor / hash come from a single
        // consistent grid view.
        let snap = match mgr.capture_pane(params.pane_id) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "get_pane_summary failed: {e}"
                ))]));
            }
        };

        let content_hash = pane_content_hash(&snap);

        // Most-recent finished command (newest entry from the per-pane
        // CommandTracker, if any).
        let (last_command, last_exit_code) = match mgr.pane_command_blocks(params.pane_id) {
            Some(blocks) => match blocks.into_iter().last() {
                Some(b) => (b.command, b.exit_code),
                None => (None, None),
            },
            None => (None, None),
        };

        // Agent registry lookup (cheap — single hashmap probe).
        let registry_entry = mgr.agent_registry().get(params.pane_id);
        let agent_name = registry_entry.as_ref().map(|e| e.name.clone());
        let agent_status = registry_entry
            .as_ref()
            .map(|e| e.status.as_str().to_string());
        let session_title = mgr
            .pane_capacity(params.pane_id)
            .and_then(|e| e.session_title);

        drop(mgr);

        // Hotspot count from the visible grid. Use trimmed lines so
        // padding spaces don't pollute regex matches; we don't return the
        // hotspots themselves here (callers who need them call
        // `terminal.semantic.get_hotspots`).
        let trimmed = render_grid_lines(&snap, true, false);
        let hotspots = detect_hotspots_from_text(&trimmed);
        let hotspot_count = hotspots.len();

        let result = PaneSummaryResult {
            pane_id: params.pane_id,
            cursor_col: snap.cursor_col,
            cursor_line: snap.cursor_line,
            content_hash,
            last_command,
            last_exit_code,
            hotspot_count,
            agent_name,
            agent_status,
            session_title,
            timestamp_secs: now_unix_secs(),
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    /// Return the last N non-empty lines of a pane plus a content hash and
    /// timestamp. Cheaper than `get_content` for "show me what just
    /// happened" polling. See tn-sp3n.
    pub(super) async fn handle_peek_pane(
        &self,
        params: PeekPaneParam,
    ) -> Result<CallToolResult, ErrorData> {
        // Cap requested lines at 50 — anything bigger should use
        // `get_content { rows: N }` instead.
        let requested = params.lines.unwrap_or(10).min(50);

        let mgr = self.session_mgr.lock().await;
        let snap = match mgr.capture_pane(params.pane_id) {
            Ok(s) => s,
            Err(e) => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "peek_pane failed: {e}"
                ))]));
            }
        };
        drop(mgr);

        let content_hash = pane_content_hash(&snap);

        // Trim + compact so blank padding doesn't waste the window.
        let mut lines = render_grid_lines(&snap, true, true);
        let total = lines.len();
        let take = requested.min(total);
        let drop_n = total - take;
        if drop_n > 0 {
            lines.drain(0..drop_n);
        }

        let result = PanePeekResult {
            pane_id: params.pane_id,
            lines,
            content_hash,
            timestamp_secs: now_unix_secs(),
        };
        Ok(CallToolResult::success(vec![json_content(&result)?]))
    }

    pub(super) async fn handle_query_semantic_history(
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

    pub(super) async fn handle_get_hotspots(
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
                    let (path_part, line_suffix) = super::split_file_path_parts(&h.text);
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
                line: h.row,
                col_start: h.start_col,
                col_end: h.end_col,
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

    pub(super) async fn handle_wait_for_output(
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

    /// Handle `terminal.patterns.stats` (tn-yrjd).
    ///
    /// Returns the full `EngineStats` snapshot the pattern engine
    /// maintains: per-pattern match counts / avg-cost / status, per-pack
    /// aggregates, and global cap-reached state. When no engine is wired
    /// (tests or when `patterns.enabled = false` but the engine hasn't
    /// been constructed) we return an empty stats object instead of a
    /// tool-call error so downstream consumers have a stable shape.
    /// Handle `terminal.events.stats` (tn-xula).
    pub(super) async fn handle_events_stats(&self) -> Result<CallToolResult, ErrorData> {
        let stats = match &self.event_bus {
            Some(bus) => bus.stats(),
            None => crate::event_bus::EventBusStats {
                total_events: 0,
                events_harness: 0,
                events_pattern: 0,
                events_core: 0,
                dropped_subscribers: 0,
                buffer_used: 0,
                buffer_capacity: 0,
                buffer_fill_pct: 0.0,
                current_cursor: 0,
            },
        };
        Ok(CallToolResult::success(vec![json_content(&stats)?]))
    }

    pub(super) async fn handle_patterns_stats(&self) -> Result<CallToolResult, ErrorData> {
        let stats = match &self.pattern_engine {
            Some(engine) => engine.stats().with_cap_limit(engine.config().max_patterns),
            None => therminal_terminal::semantic_patterns::EngineStats {
                packs: Vec::new(),
                global: therminal_terminal::semantic_patterns::GlobalStats {
                    total_loaded: 0,
                    total_active: 0,
                    total_disabled: 0,
                    cap_reached: false,
                    cap_limit: 0,
                    pack_load_errors: Vec::new(),
                },
            },
        };
        Ok(CallToolResult::success(vec![json_content(&stats)?]))
    }
}

// ── Tool definitions ────────────────────────────────────────────────────

pub(super) fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            "terminal.sessions.list",
            "List all active terminal session IDs",
            schema_for_type::<EmptyParams>(),
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
            "Create a new terminal pane with a PTY. Can split from an existing pane or add to a session. Supports custom shell command, working directory, and an optional startup command injected after the first prompt.",
            schema_for_type::<SpawnPaneParam>(),
        ),
        Tool::new(
            "terminal.panes.destroy",
            "Destroy a terminal pane and its PTY. If the pane is the last in its window, the window is removed. If the session has no remaining windows, the session is also destroyed.",
            schema_for_type::<PaneIdParam>(),
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
            "terminal.panes.tag",
            "Merge opaque key/value tags into a pane's metadata. Tags are arbitrary strings — therminal does not interpret them — and can be used by external tools (issue trackers, branch-naming conventions, conductor worker IDs) to bind a pane to external concepts. Existing keys are overwritten; keys not present in the request are left untouched. Returns the full tag set after the merge. Tags persist across daemon restarts.",
            schema_for_type::<TagPaneParam>(),
        ),
        Tool::new(
            "terminal.panes.untag",
            "Remove tags from a pane. If `keys` is omitted, all tags on the pane are cleared; otherwise only the named keys are removed (no-op for keys that aren't set). Returns the remaining tag set.",
            schema_for_type::<UntagPaneParam>(),
        ),
        Tool::new(
            "terminal.panes.get_geometry",
            "Get a pane's grid dimensions (cols, rows) and whether it can be split horizontally or vertically based on minimum pane size constraints",
            schema_for_type::<GetPaneGeometryParam>(),
        ),
        Tool::new(
            "terminal.panes.get_content",
            "Read the current visible content of a terminal pane. By default, trailing whitespace is trimmed from every row (so blank rows on a sparse pane become empty strings) and a stable `content_hash` is returned alongside the lines so polling clients can short-circuit on unchanged screens. Optional params: `trim_trailing_whitespace=false` to get the historical fixed-width grid, `compact=true` to drop fully-blank rows, and `rows=N` to return only the last N visible / non-empty rows. Cursor position and grid dimensions are always included.",
            schema_for_type::<GetContentParam>(),
        ),
        Tool::new(
            "terminal.panes.get_summary",
            "Lightweight (~100 bytes) status snapshot for a pane: cursor position, content_hash, last finished command + exit code, hotspot count, agent name + status. Designed for conductor polling — answers 'is this pane idle, working, or done?' without pulling any grid content. Compare the returned `content_hash` against the previous tick to decide whether you need a follow-up `get_content` / `peek` call.",
            schema_for_type::<PaneIdParam>(),
        ),
        Tool::new(
            "terminal.panes.peek",
            "Cheap 'what just happened?' snapshot: returns the last N non-empty trimmed lines from a pane (default 10, capped at 50) plus the same `content_hash` as `get_content` and a Unix timestamp. ~500 bytes for a typical mostly-empty pane. Use this when `get_summary` says the screen changed and you want a quick look without paying for the full grid.",
            schema_for_type::<PeekPaneParam>(),
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
        Tool::new(
            "terminal.workspaces.list",
            "List workspace tabs with their names, pane counts, and active status. Optionally filter by session ID.",
            schema_for_type::<ListWorkspacesParam>(),
        ),
        Tool::new(
            "terminal.workspaces.get_layout",
            "Get the binary layout tree and focused pane for a workspace. Returns a tagged-union tree of splits (direction, ratio, left, right) and leaves (pane_id). Note: the tree is currently a degraded cascade built from flat pane IDs until the real LayoutNode is plumbed into the daemon; check the `degraded` field.",
            schema_for_type::<GetWorkspaceLayoutParam>(),
        ),
        Tool::new(
            "terminal.agents.list",
            "List all detected AI agents across terminal panes with their type, status, and pane location. Optionally filter by status.",
            schema_for_type::<ListAgentsParam>(),
        ),
        Tool::new(
            "terminal.semantic.query_commands",
            "Return recent shell commands with exit codes and durations from the OSC 633 CommandTracker. Supports `since_line` and `limit` (default 20). Stub: currently returns an empty list because the CommandTracker is not yet plumbed from the reader thread into the daemon-side Pane.",
            schema_for_type::<QueryCommandsParam>(),
        ),
        Tool::new(
            "terminal.panes.query_events",
            "Return recent structured lifecycle events for a pane from its in-memory EventLog (spawn, status_change, command_start, command_finish, resize, pty_eof, bell). Supports `since_timestamp_secs` (Unix seconds) and `limit` (default 100). The buffer is capped at 5000 entries; the JSONL file on disk is never read.",
            schema_for_type::<QueryEventsParam>(),
        ),
        Tool::new(
            "terminal.agents.get_details",
            "Get process-tree inference data for the agent running in a pane: agent_type (from AgentRegistry, consistent with agents.list), model, context_percent, consecutive_failures, last_command, last_exit_code, last_command_duration_ms. For state-file-driven fields, use terminal.agents.get_session_detail instead.",
            schema_for_type::<PaneIdParam>(),
        ),
        Tool::new(
            "terminal.agents.get_status",
            "Get a dynamic mode + capacity snapshot for the agent in a pane: agent_type, status, current_tool (from AgentRegistry) plus context_percent and model (from PaneCapacityCache). Strict subset of terminal.agents.get_details intended for sibling-agent coordination. Returns an error if neither registry nor capacity data is known for the pane.",
            schema_for_type::<PaneIdParam>(),
        ),
        Tool::new(
            "terminal.agents.get_session_detail",
            "Get the enriched Claude Code session detail for a pane (tn-ifee): session_title, current_tool, working_dir, context_percent, model. Sourced from the per-pane `PaneCapacityCache` populated by the Claude state poller from `/tmp/claude-code-state/`. Every field is nullable — absence is normal when hooks aren't installed or haven't ticked yet. Returns an error only if the pane does not exist.",
            schema_for_type::<PaneIdParam>(),
        ),
        Tool::new(
            "terminal.agents.get_cadence",
            "Get output cadence metrics for the agent in a pane: chunk_count, avg_arrival_ms, max_gap_ms, is_spinner, is_streaming, plus recent_samples (capped at 50, oldest first). Backed by the per-pane AgentStateInference engine's chunk-stats sliding window. Useful for predicting time-to-completion, animating progress, and distinguishing stalled agents from thinking agents. Returns an error only if the pane does not exist; panes with no streaming activity return zero / false / empty defaults.",
            schema_for_type::<PaneIdParam>(),
        ),
        Tool::new(
            "terminal.agents.find_with_capacity",
            "Return all detected agents whose REMAINING context-window capacity is at least `threshold_percent` (0.0 - 100.0). `remaining_percent = 100.0 - context_percent`. Agents whose capacity is unknown (no PaneCapacityCache entry) are INCLUDED — treated as 'potentially has capacity' so callers don't accidentally exclude fresh panes. Results are sorted by `remaining_percent` descending; agents with unknown capacity sort last.",
            schema_for_type::<FindWithCapacityParam>(),
        ),
        Tool::new(
            "terminal.events.stats",
            "Return aggregate stats for the unified event bus (tn-xula): total events published, per-source-class counts (harness/pattern/core), dropped-subscriber count, current ring buffer fill, and the current monotonic cursor. Read-only (Observer tier).",
            schema_for_type::<EmptyParams>(),
        ),
        Tool::new(
            "terminal.patterns.stats",
            "Return match statistics for every loaded semantic pattern pack (tn-yrjd). Output shape matches `docs/pattern-performance-model.md` §6.3: per-pattern match_count/miss_count/avg_match_ms/slow_count/status, per-pack active/disabled/error counts plus load_errors, and global total_loaded/total_active/total_disabled/cap_reached/cap_limit plus pack_load_errors. Read-only (Observer tier).",
            schema_for_type::<EmptyParams>(),
        ),
    ]
}
