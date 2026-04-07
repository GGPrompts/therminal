//! MCP resource handling: list, read, subscribe, unsubscribe.
//!
//! Resources follow the `terminal://pane/{pane_id}/{content|output}` URI
//! scheme for per-pane grid snapshots and live PTY output streams, plus a
//! global `therminal://claude/events` stream for Claude agent events.
//!
//! Subscriptions spawn background tasks that forward `DaemonEvent::PaneOutput`
//! or broadcast `TaggedAgentEvent`s as MCP `notifications/resources/updated`
//! messages to the client.

use std::sync::Arc;

use rmcp::ErrorData;
use rmcp::model::{
    Annotated, ErrorCode, ListResourceTemplatesResult, PaginatedRequestParams, RawResource,
    RawResourceTemplate, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
    ResourceUpdatedNotificationParam, SubscribeRequestParams, UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, RoleServer};
use tracing::{debug, info};

use crate::claude_jsonl_tailer::TaggedAgentEvent;

use super::{
    CLAUDE_EVENT_BUFFER_CAP, CLAUDE_EVENTS_URI, TherminalMcpServer, extract_agent_identity,
};

impl TherminalMcpServer {
    /// Build the list of concrete resources from current pane state.
    pub(super) async fn build_resource_list(&self) -> Vec<rmcp::model::Resource> {
        let mgr = self.session_mgr.lock().await;
        let mut resources = Vec::new();
        for (session_id, session) in mgr.iter_sessions() {
            for window in &session.windows {
                for pane in &window.panes {
                    let content_uri = format!("terminal://pane/{}/content", pane.id);
                    resources.push(Annotated::new(
                        RawResource::new(&content_uri, format!("Pane {} content", pane.id))
                            .with_description(format!(
                                "Current visible grid content of pane {} (session {})",
                                pane.id, session_id
                            ))
                            .with_mime_type("text/plain"),
                        None,
                    ));

                    let scrollback_uri = format!("terminal://pane/{}/scrollback", pane.id);
                    resources.push(Annotated::new(
                        RawResource::new(
                            &scrollback_uri,
                            format!("Pane {} scrollback", pane.id),
                        )
                        .with_description(format!(
                            "Historical scrollback above the visible grid for pane {} (session {}). Capped at 10,000 lines, oldest first.",
                            pane.id, session_id
                        ))
                        .with_mime_type("text/plain"),
                        None,
                    ));

                    let output_uri = format!("terminal://pane/{}/output", pane.id);
                    resources.push(Annotated::new(
                        RawResource::new(&output_uri, format!("Pane {} output stream", pane.id))
                            .with_description(format!(
                                "Live PTY output stream for pane {} (session {}). Subscribe for real-time updates.",
                                pane.id, session_id
                            ))
                            .with_mime_type("text/plain"),
                        None,
                    ));
                }
            }
        }

        // Global Claude agent-event stream (always advertised; subscriptions
        // become no-ops if the pipeline failed to start).
        resources.push(Annotated::new(
            RawResource::new(CLAUDE_EVENTS_URI, "Claude agent events".to_string())
                .with_description(
                    "Live structured event stream from all tracked Claude Code (and Codex / Copilot) sessions. \
                     Subscribe for real-time TaggedAgentEvent notifications. read_resource returns an empty snapshot \
                     — events are only delivered via subscription.".to_string(),
                )
                .with_mime_type("application/json"),
            None,
        ));

        resources
    }

    pub(super) async fn list_resource_templates_impl(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let templates = vec![
            Annotated::new(
                RawResourceTemplate::new(
                    "terminal://pane/{pane_id}/content",
                    "Pane content",
                )
                .with_description(
                    "Current visible grid content of a terminal pane. Returns plain text lines with cursor position.",
                )
                .with_mime_type("text/plain"),
                None,
            ),
            Annotated::new(
                RawResourceTemplate::new(
                    "terminal://pane/{pane_id}/output",
                    "Pane output stream",
                )
                .with_description(
                    "Live PTY output stream for a terminal pane. Subscribe for real-time update notifications.",
                )
                .with_mime_type("text/plain"),
                None,
            ),
            Annotated::new(
                RawResourceTemplate::new(
                    "terminal://pane/{pane_id}/scrollback",
                    "Pane scrollback history",
                )
                .with_description(
                    "Historical scrollback above the visible grid for a terminal pane. Static snapshot (no subscriptions), capped at 10,000 lines, oldest first.",
                )
                .with_mime_type("text/plain"),
                None,
            ),
        ];
        Ok(ListResourceTemplatesResult::with_all_items(templates))
    }

    pub(super) async fn read_resource_impl(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        let uri = &request.uri;
        let agent = extract_agent_identity(&context);
        self.enforce_resource_trust(uri, &agent)?;

        // Global Claude event stream: drain the buffered events that the
        // subscription background task has accumulated since the last read.
        // Returns a JSON array of TaggedAgentEvent. Empty array if there is
        // no active subscription on this connection.
        if uri == CLAUDE_EVENTS_URI {
            let drained: Vec<TaggedAgentEvent> = {
                let mut buf = self
                    .claude_event_buffer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                buf.drain(..).collect()
            };
            let json = serde_json::to_string(&drained).map_err(|e| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    format!("failed to serialize claude events: {e}"),
                    None,
                )
            })?;
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(json, uri.to_string()).with_mime_type("application/json"),
            ]));
        }

        let (pane_id, kind) = Self::parse_pane_uri(uri).ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("invalid resource URI: {uri}"),
                None,
            )
        })?;

        match kind {
            "content" => {
                let mgr = self.session_mgr.lock().await;
                let snap = mgr.capture_pane(pane_id).map_err(|e| {
                    ErrorData::new(
                        ErrorCode::INTERNAL_ERROR,
                        format!("capture_pane failed: {e}"),
                        None,
                    )
                })?;
                let lines: Vec<String> = snap
                    .grid
                    .iter()
                    .map(|row| row.iter().map(|(ch, _)| ch).collect())
                    .collect();
                let text = lines.join("\n");
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(text, uri.to_string()).with_mime_type("text/plain"),
                ]))
            }
            "output" => {
                // For output, return the current grid content as a snapshot.
                // Live streaming happens via subscribe/notifications.
                let mgr = self.session_mgr.lock().await;
                let snap = mgr.capture_pane(pane_id).map_err(|e| {
                    ErrorData::new(
                        ErrorCode::INTERNAL_ERROR,
                        format!("capture_pane failed: {e}"),
                        None,
                    )
                })?;
                let lines: Vec<String> = snap
                    .grid
                    .iter()
                    .map(|row| row.iter().map(|(ch, _)| ch).collect())
                    .collect();
                let text = lines.join("\n");
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(text, uri.to_string()).with_mime_type("text/plain"),
                ]))
            }
            "scrollback" => {
                let mgr = self.session_mgr.lock().await;
                let snap = mgr.capture_pane(pane_id).map_err(|e| {
                    ErrorData::new(
                        ErrorCode::INTERNAL_ERROR,
                        format!("capture_pane failed: {e}"),
                        None,
                    )
                })?;
                // `PaneSnapshot.scrollback` is oldest-first, capped at 10k lines.
                let lines: Vec<String> = snap
                    .scrollback
                    .iter()
                    .map(|row| row.iter().map(|(ch, _)| ch).collect())
                    .collect();
                let text = lines.join("\n");
                Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(text, uri.to_string()).with_mime_type("text/plain"),
                ]))
            }
            _ => Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("unknown resource kind in URI: {uri}"),
                None,
            )),
        }
    }

    pub(super) async fn subscribe_impl(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let uri = &request.uri;
        let agent = extract_agent_identity(&context);
        self.enforce_resource_trust(uri, &agent)?;

        // Global Claude agent-event stream subscription.
        if uri == CLAUDE_EVENTS_URI {
            let Some(tx) = self.claude_events.as_ref() else {
                return Err(ErrorData::new(
                    ErrorCode(-32002),
                    "claude agent-event pipeline is not running on this daemon".to_string(),
                    None,
                ));
            };
            let mut event_rx = tx.subscribe();
            let peer = context.peer.clone();
            let uri_owned = uri.to_string();
            let buffer = Arc::clone(&self.claude_event_buffer);
            let handle = tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            {
                                let mut buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
                                if buf.len() == CLAUDE_EVENT_BUFFER_CAP {
                                    buf.pop_front();
                                }
                                buf.push_back(event);
                            }
                            let params = ResourceUpdatedNotificationParam::new(&uri_owned);
                            if let Err(e) = peer.notify_resource_updated(params).await {
                                debug!(
                                    error = %e,
                                    uri = %uri_owned,
                                    "failed to send claude resource-updated notification, stopping subscription"
                                );
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            debug!(uri = %uri_owned, "claude event channel closed");
                            break;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            debug!(uri = %uri_owned, lagged = n, "claude subscription lagged");
                        }
                    }
                }
            });
            let mut subs = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(old) = subs.insert(uri.to_string(), handle) {
                old.abort();
            }
            info!(uri = %uri, "claude agent-event subscription active");
            return Ok(());
        }

        let (pane_id, kind) = Self::parse_pane_uri(uri).ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("invalid resource URI for subscription: {uri}"),
                None,
            )
        })?;

        // Only output resources support subscriptions.
        if kind != "output" {
            let hint = if kind == "scrollback" {
                format!(
                    "scrollback is a static snapshot and does not support subscriptions; use read_resource on {uri}"
                )
            } else {
                format!(
                    "resource {uri} does not support subscriptions; use terminal://pane/{pane_id}/output"
                )
            };
            return Err(ErrorData::new(ErrorCode::INVALID_PARAMS, hint, None));
        }

        // Get a broadcast receiver for daemon events.
        let mut event_rx = {
            let mgr = self.session_mgr.lock().await;
            mgr.subscribe_events()
        };

        let peer = context.peer.clone();
        let uri_owned = uri.to_string();

        // Spawn a background task that forwards PaneOutput events as resource-updated notifications.
        let handle = tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(therminal_protocol::daemon::DaemonEvent::PaneOutput {
                        pane_id: event_pane_id,
                        ..
                    }) if event_pane_id == pane_id => {
                        let params = ResourceUpdatedNotificationParam::new(&uri_owned);
                        if let Err(e) = peer.notify_resource_updated(params).await {
                            debug!(
                                error = %e,
                                uri = %uri_owned,
                                "failed to send resource-updated notification, stopping subscription"
                            );
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        debug!(uri = %uri_owned, "event channel closed, ending subscription");
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        debug!(uri = %uri_owned, lagged = n, "subscription lagged, continuing");
                    }
                    _ => {
                        // Other event types -- ignore.
                    }
                }
            }
        });

        // Store the subscription handle (cancel previous if re-subscribing).
        let mut subs = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old_handle) = subs.insert(uri.to_string(), handle) {
            old_handle.abort();
        }

        info!(uri = %uri, "resource subscription active");
        Ok(())
    }

    pub(super) async fn unsubscribe_impl(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        let uri = &request.uri;
        let mut subs = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(handle) = subs.remove(uri) {
            handle.abort();
            info!(uri = %uri, "resource subscription cancelled");
        }
        Ok(())
    }
}
