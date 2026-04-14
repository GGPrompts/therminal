//! `ServerHandler` trait impl + tool dispatch routing.
//!
//! Extracted from `mod.rs` to keep the top-level module focused on struct
//! definitions and construction. All MCP protocol entry points live here.

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListResourceTemplatesResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult,
    ServerInfo, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};

use super::helpers::extract_agent_identity;
use super::helpers::parse_args;
use super::types::*;
use super::{TherminalMcpServer, tools};

// ── ServerHandler impl ──────────────────────────────────────────────────

impl ServerHandler for TherminalMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_resources_list_changed()
                .enable_resources_subscribe()
                .build(),
        )
        .with_instructions(
            "Therminal MCP server. Provides tools and resources to manage terminal sessions and panes. \
             Resources: terminal://pane/{id}/content (grid snapshot), terminal://pane/{id}/output (live stream).",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let agent = extract_agent_identity(&context, self.connection_id);
        // Resources are Observer-tier; check against a representative URI.
        self.enforce_resource_trust("terminal://pane/0/content", &agent)?;
        let resources = self.build_resource_list().await;
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        self.list_resource_templates_impl(request, context).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        self.read_resource_impl(request, context).await
    }

    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.subscribe_impl(request, context).await
    }

    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.unsubscribe_impl(request, context).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools::tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.as_ref();
        let args = request.arguments.unwrap_or_default();

        // Extract agent identity from the MCP connection context.
        let agent = extract_agent_identity(&context, self.connection_id);

        // Enforce trust tier and rate limiting.
        if let Err(denied_result) = self.enforce_trust(name, &agent).await {
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
            "terminal.panes.destroy" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_destroy_pane(params).await
            }
            "terminal.panes.list" => {
                let params: ListPanesParam = parse_args(args)?;
                self.handle_list_panes(params).await
            }
            "terminal.panes.write" => {
                let params: WriteToPaneParam = parse_args(args)?;
                self.handle_write_to_pane(params).await
            }
            "terminal.panes.tag" => {
                let params: TagPaneParam = parse_args(args)?;
                self.handle_tag_pane(params).await
            }
            "terminal.panes.untag" => {
                let params: UntagPaneParam = parse_args(args)?;
                self.handle_untag_pane(params).await
            }
            "terminal.panes.get_geometry" => {
                let params: GetPaneGeometryParam = parse_args(args)?;
                self.handle_get_pane_geometry(params).await
            }
            "terminal.panes.get_content" => {
                let params: GetContentParam = parse_args(args)?;
                self.handle_read_pane_content(params).await
            }
            "terminal.panes.get_summary" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_get_pane_summary(params).await
            }
            "terminal.panes.peek" => {
                let params: PeekPaneParam = parse_args(args)?;
                self.handle_peek_pane(params).await
            }
            "terminal.panes.capture_result" => {
                let params: CaptureResultParam = parse_args(args)?;
                self.handle_capture_result(params).await
            }
            "terminal.semantic.query_history" => {
                let params: QuerySemanticHistoryParam = parse_args(args)?;
                self.handle_query_semantic_history(params).await
            }
            "terminal.semantic.query_commands" => {
                let params: QueryCommandsParam = parse_args(args)?;
                self.handle_query_commands(params).await
            }
            "terminal.panes.query_events" => {
                let params: QueryEventsParam = parse_args(args)?;
                self.handle_query_events(params).await
            }
            "terminal.panes.wait_for_output" => {
                let params: WaitForOutputParam = parse_args(args)?;
                self.handle_wait_for_output(params).await
            }
            "terminal.semantic.get_hotspots" => {
                let params: GetHotspotsParam = parse_args(args)?;
                self.handle_get_hotspots(params).await
            }
            "terminal.workspaces.list" => {
                let params: ListWorkspacesParam = parse_args(args)?;
                self.handle_list_workspaces(params).await
            }
            "terminal.workspaces.get_layout" => {
                let params: GetWorkspaceLayoutParam = parse_args(args)?;
                self.handle_get_workspace_layout(params).await
            }
            "terminal.agents.list" => {
                let params: ListAgentsParam = parse_args(args)?;
                self.handle_list_agents(params).await
            }
            "terminal.agents.get_details" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_get_agent_details(params).await
            }
            "terminal.agents.get_status" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_get_agent_status(params).await
            }
            "terminal.agents.get_session_detail" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_get_agent_session_detail(params).await
            }
            "terminal.agents.get_cadence" => {
                let params: PaneIdParam = parse_args(args)?;
                self.handle_get_agent_cadence(params).await
            }
            "terminal.agents.find_with_capacity" => {
                let params: FindWithCapacityParam = parse_args(args)?;
                self.handle_find_with_capacity(params).await
            }
            "terminal.panes.create_tail" => {
                let params: CreateTailParam = parse_args(args)?;
                self.handle_create_tail(params).await
            }
            "terminal.patterns.stats" => self.handle_patterns_stats().await,
            "terminal.events.stats" => self.handle_events_stats().await,
            other => Err(ErrorData::invalid_params(
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }
}
