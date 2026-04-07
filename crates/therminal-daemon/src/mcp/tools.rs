//! MCP tool handler implementations and `tool_definitions()`.
//!
//! Each `handle_*` method implements one of the tools advertised by the
//! Therminal MCP server. Trust enforcement and argument parsing happen in
//! `mod.rs::call_tool`; these methods assume the call is already authorised.

use rmcp::ErrorData;
use rmcp::handler::server::tool::schema_for_type;
use rmcp::model::{CallToolResult, Content, Tool};
use tracing::debug;

use super::{
    AgentDetailsResult, AgentInfoResult, CreateSessionParam, DestroyPaneResult, GetHotspotsParam,
    GetHotspotsResult, GetPaneGeometryParam, GetPaneGeometryResult, GetWorkspaceLayoutParam,
    GetWorkspaceLayoutResult, HotspotInfo, LayoutNodeJson, ListAgentsParam, ListAgentsResult,
    ListPanesParam, ListPanesResult, ListWorkspacesParam, ListWorkspacesResult, MIN_PANE_COLS,
    MIN_PANE_ROWS, PaneContentResult, PaneIdParam, PaneInfo, QuerySemanticHistoryParam,
    QuerySemanticHistoryResult, SemanticRegionInfo, SessionCreatedResult, SessionDestroyedResult,
    SessionIdParam, SessionInfoResult, SessionListResult, SpawnPaneParam, SpawnPaneResult,
    TherminalMcpServer, WaitForOutputParam, WaitForOutputResult, WorkspaceInfoResult,
    WriteToPaneParam, WriteToPaneResult, build_content_preview, find_first_pane_in_session,
    find_pane_info, json_content,
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
                    panes.push(PaneInfo {
                        pane_id: pane.id,
                        session_id: *session_id,
                        cols: pane.cols(),
                        rows: pane.rows(),
                        title: String::new(),
                        cwd,
                        last_exit_code,
                        agent_name,
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
                let layout = LayoutNodeJson::from_flat_pane_ids(&ws.pane_ids);
                let result = GetWorkspaceLayoutResult {
                    workspace_id: ws.id,
                    session_id: *session_id,
                    layout,
                    focused_pane: ws.focused_pane,
                    degraded: true,
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
    /// The pane must exist. `agent_type` is populated from `AgentRegistry`
    /// when an agent is currently registered on the pane; all other
    /// inference fields (model, context_percent, last_command, etc.) are
    /// currently always `None` because `AgentStateInference` is not yet
    /// plumbed into the daemon — tracked as a follow-up issue.
    pub(super) async fn handle_get_agent_details(
        &self,
        params: PaneIdParam,
    ) -> Result<CallToolResult, ErrorData> {
        let mgr = self.session_mgr.lock().await;

        // Verify the pane exists — return a tool error if not, matching the
        // convention of get_pane_geometry / read_pane_content.
        if find_pane_info(&mgr, params.pane_id).is_none() {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "pane not found: {}",
                params.pane_id
            ))]));
        }

        let agent_type = mgr
            .agent_registry()
            .get(params.pane_id)
            .map(|e| e.agent_type.as_str().to_string());

        let result = AgentDetailsResult {
            agent_type,
            model: None,
            context_percent: None,
            consecutive_failures: 0,
            last_command: None,
            last_exit_code: None,
            last_command_duration_ms: None,
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
}

// ── Tool definitions ────────────────────────────────────────────────────

pub(super) fn tool_definitions() -> Vec<Tool> {
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
            "terminal.agents.get_details",
            "Get detailed inference data for the agent running in a pane: agent_type, model, context_percent, consecutive_failures, last_command, last_exit_code, last_command_duration_ms. All inference fields except consecutive_failures are currently None — the underlying state inference engine is not yet plumbed into the daemon (stub returning agent_type from AgentRegistry).",
            schema_for_type::<PaneIdParam>(),
        ),
    ]
}
