//! Local-mode synchronous split path and the public dispatcher methods
//! shared by local and daemon modes:
//! - `split_focused_pane_auto[_with]`
//! - `split_focused_pane[_with]`
//! - `split_pane_by_id`
//! - `spawn_n_panes`
//! - `open_focused_agent_event_log_tail`

use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::pane::{LayoutNode, PaneId, SplitDirection};
use therminal_terminal::interceptor::InterceptorConfig;

use super::super::{make_pane_callbacks, split_spawn_options};
use super::DaemonSplitOnComplete;
use crate::window::App;

impl App {
    /// Split the currently focused pane with auto-detected direction.
    pub(crate) fn split_focused_pane_auto(&mut self) {
        self.split_focused_pane_auto_with(DaemonSplitOnComplete::FocusAndRelayout);
    }

    /// Split the currently focused pane with auto-detected direction and a
    /// custom daemon-mode completion action.
    pub(crate) fn split_focused_pane_auto_with(&mut self, on_complete: DaemonSplitOnComplete) {
        // Restore layout before splitting so the new pane joins the full tree.
        if self.zoomed_layout.is_some() {
            self.zoom_toggle_focused_pane();
        }
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        let layout = match self.get_layout() {
            Some(l) => l,
            None => return,
        };
        let pane = match layout.find_pane(focused) {
            Some(p) => p,
            None => return,
        };
        let fallback = match self.last_split_direction {
            SplitDirection::Horizontal => SplitDirection::Vertical,
            SplitDirection::Vertical => SplitDirection::Horizontal,
        };
        let direction = LayoutNode::auto_split_direction(pane.viewport, fallback);
        self.split_focused_pane_with(direction, on_complete);
    }

    /// Split the currently focused pane.
    pub(crate) fn split_focused_pane(&mut self, direction: SplitDirection) {
        self.split_focused_pane_with(direction, DaemonSplitOnComplete::FocusAndRelayout);
    }

    /// Split the currently focused pane with a custom daemon-mode completion action.
    pub(crate) fn split_focused_pane_with(
        &mut self,
        direction: SplitDirection,
        on_complete: DaemonSplitOnComplete,
    ) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };
        // tn-beez Phase B: in daemon mode, route splits through the daemon
        // so the resulting pane id is the canonical daemon id and shows up
        // in MCP `terminal.panes.list` + persists across daemon restart.
        // The RPC is fired asynchronously; completion arrives via
        // `UserEvent::DaemonSplitComplete` → `finish_split_pane_remote`.
        if self.is_daemon_mode() {
            self.split_pane_remote(focused, direction, on_complete);
            return;
        }
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
            osc_7337: self.config.terminal.osc_7337,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let base_spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
            env: self.config.general.env.clone(),
            advertise_kitty_graphics: self.config.terminal.kitty_graphics,
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        // Direct field access needed here: layout_mut + renderer + config must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        // Inherit source pane's cwd (from OSC 7) so the new shell starts in
        // the same directory the user was working in.
        let spawn_options = split_spawn_options(
            &base_spawn_options,
            layout,
            focused,
            self.config.general.new_pane_cwd,
        );

        // tn-ou30: compute the header height that resize_all_panes will apply
        // AFTER the split so the PTY starts at the correct size.
        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

        let new_id = layout.split_pane(
            focused,
            direction,
            |viewport| match crate::pane::spawn_pane(
                viewport,
                renderer,
                scrollback,
                interceptor_cfg.clone(),
                scan_interval_secs,
                &spawn_options,
                registry.clone(),
                |pane_id| make_pane_callbacks(&proxy, pane_id),
                post_split_header_h,
            ) {
                Ok(pane) => Some(pane),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to spawn pane for split");
                    None
                }
            },
        );

        if let Some(new_id) = new_id {
            info!("Split pane {focused} {:?} -> new pane {new_id}", direction);
            self.last_split_direction = direction;
            self.set_focused_pane(Some(new_id));
            self.relayout_and_redraw();
            self.publish_workspace_state();
        } else {
            self.request_redraw();
        }
    }

    /// Open a structured JSONL tail pane for the focused pane's Claude
    /// session transcript.
    ///
    /// Triggered by clicking the `[agent: <name>]` indicator in the status
    /// bar. Resolves the Claude session ID to its JSONL transcript file
    /// under `~/.claude/projects/` and opens a `JsonlTail` pane (same as
    /// subagent panes) with structured, color-coded rendering.
    pub(crate) fn open_focused_agent_event_log_tail(&mut self) {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => {
                debug!("open_focused_agent_event_log_tail: no focused pane");
                return;
            }
        };

        // Look up the Claude session ID from the pane's status. This is
        // populated by the daemon's AgentChanged event forwarder.
        let session_id = self.workspaces.as_ref().and_then(|wm| {
            let pane = wm.layout().find_pane(focused)?;
            pane.status
                .lock()
                .ok()
                .and_then(|s| s.claude_session_id.clone())
        });

        let jsonl_path = session_id
            .as_deref()
            .and_then(therminal_harness_claude::jsonl_tailer::resolve_session_jsonl);

        let jsonl_path = match jsonl_path {
            Some(p) => p,
            None => {
                info!(pane = focused, "no Claude JSONL transcript found for pane");
                self.show_toast("no Claude session transcript found".to_string());
                return;
            }
        };

        info!(
            "Opening JSONL tail pane for pane {} at {}",
            focused,
            jsonl_path.display()
        );

        // Open a JsonlTail pane (structured, color-coded) using the same
        // pattern as spawn_subagent_pane. Works in both local and daemon
        // modes since JsonlTail uses a notify file watcher, not a PTY.
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };

        let direction = self
            .get_layout()
            .and_then(|l| l.find_pane(focused))
            .map(|p| LayoutNode::auto_split_direction(p.viewport, SplitDirection::Horizontal))
            .unwrap_or(SplitDirection::Horizontal);

        let proxy = self.event_proxy.clone();
        let jsonl_path_for_closure = jsonl_path.clone();
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };

        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

        let new_id = layout.split_pane(focused, direction, |viewport| {
            let (cols, rows) = crate::pane::state::grid_size_for_rect_with_header(
                viewport,
                renderer,
                post_split_header_h,
            );
            let cols = cols.max(20);
            let rows = rows.max(3);

            let wake = {
                let proxy = proxy.clone();
                Box::new(move || {
                    let _ = proxy.send_event(crate::window::UserEvent::PtyOutput);
                })
            };

            match crate::pane::jsonl_tail::spawn_jsonl_watcher(
                jsonl_path_for_closure.clone(),
                cols,
                rows,
                wake,
            ) {
                Ok((state, term, watcher)) => {
                    let id = crate::pane::spawn::next_pane_id();
                    Some(crate::pane::state::PaneState {
                        id,
                        viewport,
                        status: std::sync::Arc::new(std::sync::Mutex::new(
                            crate::pane::state::PaneStatus::default(),
                        )),
                        region_index: std::sync::Arc::new(std::sync::Mutex::new(
                            therminal_terminal::region_index::RegionIndex::new(),
                        )),
                        backend: crate::pane::backend::PaneBackendKind::JsonlTail {
                            path: jsonl_path_for_closure,
                            state,
                            term,
                            watcher,
                        },
                        pinned: false,
                        image_store: std::sync::Arc::new(std::sync::Mutex::new(
                            therminal_terminal::graphics::ImageStore::default(),
                        )),
                        placements: std::sync::Arc::new(std::sync::Mutex::new(
                            therminal_terminal::graphics::PlacementSet::new(),
                        )),
                    })
                }
                Err(e) => {
                    warn!(error = %e, "failed to create JSONL tail watcher");
                    None
                }
            }
        });

        if new_id.is_some() {
            self.relayout_and_redraw();
        } else {
            self.show_toast("failed to open JSONL tail pane".to_string());
        }
    }

    /// Split a specific pane by ID.
    pub(crate) fn split_pane_by_id(&mut self, target_id: PaneId, direction: SplitDirection) {
        // tn-beez Phase B: daemon mode routes through the daemon so the
        // new pane carries a daemon id (visible to MCP / persisted).
        if self.is_daemon_mode() {
            self.split_pane_remote(
                target_id,
                direction,
                DaemonSplitOnComplete::FocusAndRelayout,
            );
            return;
        }
        self.split_pane_by_id_local(target_id, direction);
    }

    /// Local-only variant of `split_pane_by_id` that spawns a terminal pane
    /// directly (no daemon RPC). Used both as the local-mode branch of
    /// `split_pane_by_id` and as a graceful fallback from `split_pane_remote`
    /// when the source pane has no daemon mapping (tn-7mvn — e.g. splitting
    /// off a WebView pane, which is GUI-only and never round-trips through
    /// the daemon).
    ///
    /// Trade-off: the new terminal pane stays GUI-local and therefore isn't
    /// visible to MCP `terminal.panes.list` or persisted across daemon
    /// restart. Acceptable for v1 — panes split from WebViews are already
    /// rooted in GUI-only state.
    pub(crate) fn split_pane_by_id_local(&mut self, target_id: PaneId, direction: SplitDirection) {
        let renderer = match self.grid_renderer.as_ref() {
            Some(r) => r,
            None => return,
        };
        let scrollback = self.config.general.scrollback_lines;
        let interceptor_cfg = InterceptorConfig {
            osc_633: self.config.terminal.osc_633,
            osc_133: self.config.terminal.osc_133,
            osc_7: self.config.terminal.osc_7,
            osc_9: self.config.terminal.osc_9,
            osc_1337: self.config.terminal.osc_1337,
            osc_7777: self.config.terminal.osc_7777,
            osc_7337: self.config.terminal.osc_7337,
        };
        let scan_interval_secs = self.config.trust.agent_scan_interval;
        let base_spawn_options = therminal_terminal::pty::SpawnOptions {
            shell: self.config.general.shell.clone(),
            shell_args: self.config.general.shell_args.clone(),
            env: self.config.general.env.clone(),
            advertise_kitty_graphics: self.config.terminal.kitty_graphics,
            ..Default::default()
        };
        let proxy = self.event_proxy.clone();
        let registry = Some(Arc::clone(&self.agent_registry));
        // Direct field access needed here: layout_mut + renderer + config must coexist.
        let layout = match self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
            Some(l) => l,
            None => return,
        };
        // Inherit source pane's cwd (from OSC 7) or use home depending on config.
        let spawn_options = split_spawn_options(
            &base_spawn_options,
            layout,
            target_id,
            self.config.general.new_pane_cwd,
        );

        let post_split_header_h =
            crate::pane::effective_header_height(layout.pane_count() + 1, !self.focus_mode);

        let new_id =
            layout.split_pane(
                target_id,
                direction,
                |viewport| match crate::pane::spawn_pane(
                    viewport,
                    renderer,
                    scrollback,
                    interceptor_cfg.clone(),
                    scan_interval_secs,
                    &spawn_options,
                    registry.clone(),
                    |pane_id| make_pane_callbacks(&proxy, pane_id),
                    post_split_header_h,
                ) {
                    Ok(pane) => Some(pane),
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to spawn pane for split");
                        None
                    }
                },
            );

        if let Some(new_id) = new_id {
            info!(
                "Split pane {target_id} {:?} -> new pane {new_id}",
                direction
            );
            self.last_split_direction = direction;
            self.set_focused_pane(Some(new_id));
            self.relayout_and_redraw();
            self.publish_workspace_state();
        } else {
            self.request_redraw();
        }
    }

    /// Spawn N panes with auto-tiling layout.
    /// Creates panes one at a time using the existing split infrastructure,
    /// with a single relayout at the end.
    #[allow(dead_code)]
    pub(crate) fn spawn_n_panes(&mut self, n: usize) {
        if n == 0 {
            return;
        }

        if self.workspaces.is_none() {
            info!("No layout exists, cannot spawn panes without initial setup");
            return;
        }

        for _ in 0..n {
            self.split_focused_pane_auto();
        }

        info!("Spawned {n} additional panes via auto-split");
    }

    /// Create a WebView pane by splitting the focused pane (tn-s5vj).
    ///
    /// The webview loads the given URL in a platform-native child surface
    /// (WebKitGTK on Linux, WebView2 on Windows, WKWebView on macOS).
    /// The pane participates in the layout tree like any terminal pane
    /// and gets the same header chrome, context menu, and resize behavior.
    ///
    /// On success returns the newly allocated `PaneId`. On failure returns
    /// a human-readable reason so the tn-jnn4 daemon forwarder can surface
    /// it through `IpcRequest::AckWebViewPaneSpawn { error }` to the MCP /
    /// CLI caller.
    pub(crate) fn create_webview_pane(&mut self, url: &str) -> Result<crate::pane::PaneId, String> {
        let focused = match self.focused_pane() {
            Some(id) => id,
            None => {
                info!("create_webview_pane: no focused pane to split from");
                return Err("no focused pane to split from".to_string());
            }
        };

        let direction = self
            .get_layout()
            .and_then(|l| l.find_pane(focused))
            .map(|p| {
                crate::pane::LayoutNode::auto_split_direction(
                    p.viewport,
                    crate::pane::SplitDirection::Horizontal,
                )
            })
            .unwrap_or(crate::pane::SplitDirection::Horizontal);

        // Normalize the URL before anything else touches it (tn-jju2). wry
        // rejects bare hostnames like `ggprompts.com` with a malformed-URL
        // error — on Windows WebView2 it's HRESULT 0x80070057 — so shell
        // users who type domains without `https://` end up with a stranded
        // blank pane. Normalization happens here rather than at the call
        // sites so every entry point (inline prompt, MCP, CLI) benefits.
        let url_owned = crate::pane::webview::normalize_webview_url(url);
        if url_owned.is_empty() {
            return Err("url is empty".to_string());
        }

        // Step 1: Insert the WebView PaneState into the layout tree.
        let layout = self
            .workspaces
            .as_mut()
            .map(|wm| wm.layout_mut())
            .ok_or_else(|| "workspace manager not initialised".to_string())?;

        let new_id = layout
            .split_pane(focused, direction, |viewport| {
                Some(crate::pane::spawn_webview_pane(viewport, &url_owned))
            })
            .ok_or_else(|| {
                self.show_toast("failed to split for webview pane".to_string());
                "failed to split for webview pane".to_string()
            })?;

        // Step 2: Now that layout is no longer mutably borrowed, create
        // the platform-native webview. Look up the new pane's viewport.
        let viewport = self
            .get_layout()
            .and_then(|l| l.find_pane(new_id))
            .map(|p| p.viewport);

        if let Some(viewport) = viewport {
            let window = match self.window.as_ref() {
                Some(w) => Arc::clone(w),
                None => {
                    warn!("create_webview_pane: no window available, removing orphaned pane");
                    if let Some(layout) = self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
                        layout.remove_pane(new_id);
                    }
                    // Without these the half-created pane lingers visually
                    // and the daemon's workspace snapshot diverges from the
                    // GUI layout (tn-jju2).
                    self.relayout_and_redraw();
                    self.publish_workspace_state();
                    return Err("no active window to attach webview to".to_string());
                }
            };
            let pane_count = self.get_layout().map(|l| l.pane_count()).unwrap_or(2);
            let header_h = crate::pane::effective_header_height(pane_count, !self.focus_mode);
            let content_rect = crate::pane::webview::webview_content_rect(viewport, header_h);

            if let Err(e) = self.webview_manager.create(
                new_id,
                &url_owned,
                content_rect,
                &window,
                self.event_proxy.clone(),
            ) {
                warn!(error = %e, "failed to create native webview, removing pane");
                // Remove the pane from layout since the webview failed.
                if let Some(layout) = self.workspaces.as_mut().map(|wm| wm.layout_mut()) {
                    layout.remove_pane(new_id);
                }
                // Re-layout + republish so the GUI repaints without the ghost
                // pane and the daemon's workspace snapshot stays in sync. The
                // prior code left both stale and stranded the user with an
                // unclosable blank pane (tn-jju2).
                self.relayout_and_redraw();
                self.publish_workspace_state();
                self.show_toast(format!("WebView failed: {e}"));
                return Err(format!("failed to create native webview: {e}"));
            }

            // tn-shgq: the wry WebView child HWND takes OS keyboard focus on
            // creation (despite `.with_focused(false)`), so the source pane
            // — which stays focused in our internal state (tn-2wco) — no
            // longer receives key events through winit. Pull OS focus back
            // to the main window so the user can keep typing into the pane
            // that spawned the webview. A single synchronous call isn't
            // enough on Windows: WebView2 re-grabs focus asynchronously
            // when the page finishes loading (often seconds later), so we
            // also kick off a retry-burst that reposts `RestoreMainFocus`
            // events over ~10 s to win the async race.
            crate::window::restore_main_window_focus(&window);
            crate::window::App::schedule_webview_focus_retries(self.event_proxy.clone());
        }

        info!(pane_id = new_id, url = %url_owned, "created webview pane");
        self.last_split_direction = direction;
        // tn-2wco: do NOT auto-focus the new WebView pane. Keystrokes to
        // WebView panes are silently dropped by `handle_key_input`
        // (pty_input.rs), so auto-focusing would strand the user with no
        // input path and no cue. Spawning a webview is an ambient action;
        // focus stays on the source pane so the user can keep working.
        self.relayout_and_redraw();
        Ok(new_id)
    }

    /// Main-thread handler for `UserEvent::DaemonSpawnWebViewPane` (tn-jnn4).
    ///
    /// The daemon-event listener translates an `IpcRequest::SpawnWebViewPane`
    /// from an MCP/CLI caller into a `SpawnWebViewPaneRequested` event and
    /// forwards it into the winit loop. This method runs on the main thread
    /// so it can drive wry on the window handle, then fires
    /// `IpcRequest::AckWebViewPaneSpawn` back to the daemon to complete the
    /// caller's pending future.
    ///
    /// `split_from`, `split_direction`, `ratio`, and `session_id` from the
    /// original request are currently ignored — `create_webview_pane` always
    /// splits the focused pane with an auto-picked direction. Wiring those
    /// fields through is follow-up work; for the v1 round-trip the defaults
    /// match what the GUI menu would do.
    pub(crate) fn handle_daemon_webview_spawn(
        &mut self,
        request_id: u64,
        url: &str,
        _split_from: Option<crate::pane::PaneId>,
        _split_direction: Option<String>,
        _ratio: Option<f32>,
        _session_id: Option<therminal_protocol::SessionId>,
    ) {
        let result = self.create_webview_pane(url);
        let (pane_id, error) = match result {
            Ok(id) => (Some(id), None),
            Err(msg) => {
                warn!(request_id, url, error = %msg, "webview spawn failed on GUI");
                (None, Some(msg))
            }
        };
        let req = therminal_protocol::daemon::IpcRequest::AckWebViewPaneSpawn {
            request_id,
            pane_id,
            error,
        };
        if let Err(e) = self.daemon_rpc_blocking(req) {
            warn!(
                request_id,
                error = %e,
                "failed to ack SpawnWebViewPane back to daemon"
            );
        }
    }
}
