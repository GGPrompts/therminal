//! Per-frame render driver and semantic-region scrollback jump.
//!
//! Split out from `mod.rs` to keep the coordinator focused on event dispatch.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use super::settings_overlay;
use super::trust_escalation_overlay;
use super::{App, OverlayMode, chrome, help_overlay, render};
use crate::pane::PaneId;
use therminal_harness_claude::state::ClaudeStatus;

/// Direction for [`App::jump_to_region`].
#[derive(Debug, Clone, Copy)]
pub(super) enum JumpDirection {
    Prev,
    Next,
}

/// Collapse the richer [`ClaudeStatus`] enum into the coarser
/// [`chrome::DelegateState`] buckets used by the delegate summary. The
/// mapping is intentionally narrow — the chrome-level summary only
/// distinguishes `idle` (waiting / done-ish), `thinking` (reasoning), and
/// `streaming` (actively producing text). `Done` is reserved for the
/// explicit `delegate_status=done` tag set by the orchestrator and is
/// never derived from live Claude state.
fn delegate_state_from_claude(status: ClaudeStatus) -> chrome::DelegateState {
    match status {
        ClaudeStatus::Streaming => chrome::DelegateState::Streaming,
        ClaudeStatus::Processing | ClaudeStatus::Thinking | ClaudeStatus::ToolUse => {
            chrome::DelegateState::Thinking
        }
        ClaudeStatus::Idle | ClaudeStatus::AwaitingInput => chrome::DelegateState::Idle,
    }
}

impl App {
    /// Render a frame: render all panes and separators.
    pub(super) fn render(&mut self) {
        let mut new_status_bar_hit_areas = chrome::StatusBarHitAreas::default();

        // tn-ztv3.4: Compute the delegate sibling summary before borrowing
        // `grid_renderer` mutably — the scan needs mutable access to
        // `self.delegate_summary`, the agent registry, and per-pane status
        // locks, none of which overlap with the renderer but still conflict
        // with the outer `&mut self` borrow used by the rest of this
        // method. Running it here keeps the state machine advancing
        // regardless of whether the status bar is actually shown.
        let delegate_summary_text = self.scan_and_update_delegate_summary();

        let gpu = match self.gpu.as_ref() {
            Some(g) => g,
            None => return,
        };
        let renderer = match self.grid_renderer.as_mut() {
            Some(r) => r,
            None => return,
        };
        let layout = match self.workspaces.as_ref().map(|wm| wm.layout()) {
            Some(l) => l,
            None => return,
        };

        let output = match gpu.surface.get_current_texture() {
            Ok(tex) => tex,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return;
            }
            Err(wgpu::SurfaceError::OutOfMemory) => {
                warn!("wgpu: out of memory");
                return;
            }
            Err(e) => {
                warn!("wgpu surface error: {e}");
                return;
            }
        };

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("therminal_render"),
            });

        // Clear to background color (respects config overrides).
        let resolved_bg = renderer.resolved_bg();
        let clear_color = wgpu::Color {
            r: resolved_bg[0] as f64,
            g: resolved_bg[1] as f64,
            b: resolved_bg[2] as f64,
            a: resolved_bg[3] as f64,
        };
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }

        // Render each pane.
        let focused = self.workspaces.as_ref().and_then(|wm| wm.focused_pane());
        let pane_count = layout.pane_count();
        let show_focus = pane_count > 1;

        // Submit the clear pass immediately so pane renders can use fresh encoders.
        gpu.queue.submit(std::iter::once(encoder.finish()));

        // Clear hotspot/hyperlink maps once per frame so all panes can
        // contribute entries that persist until the next frame.
        renderer.clear_frame_maps();

        let mut pane_counter = 0;
        let show_pane_headers = self.config.general.show_pane_headers;
        let is_zoomed = self.zoomed_layout.is_some();
        render::render_panes_recursive(
            layout,
            focused,
            show_focus,
            pane_count,
            show_pane_headers,
            is_zoomed,
            &mut pane_counter,
            renderer,
            &gpu.device,
            &gpu.queue,
            &view,
            gpu.config.width,
            gpu.config.height,
            &self.agent_registry,
            &self.claude_cwd,
            self.pattern_engine.as_ref(),
        );

        // ── Overlay pass: chrome backgrounds ────────────────────────────
        // Collect chrome overlay quads (status bar bg, visual bell) into a
        // shared OverlayLayer and render them in a single batched pass.
        let mut chrome_overlay = crate::overlay::OverlayLayer::new();

        // ── Status bar ──────────────────────────────────────────────────
        if self.config.general.show_status_bar {
            // Gather status info from the focused pane.
            let focused_pane = self
                .workspaces
                .as_ref()
                .and_then(|wm| wm.focused_pane())
                .and_then(|fid| layout.find_pane(fid));

            #[allow(clippy::type_complexity)]
            let (
                cwd,
                claude_title,
                claude_status_text,
                last_exit_code,
                agent_name,
                git_branch,
                dimensions,
            ): (
                Option<String>,
                Option<String>,
                Option<String>,
                Option<i32>,
                Option<String>,
                Option<String>,
                (usize, usize),
            ) = if let Some(pane) = focused_pane {
                let status = pane.status.lock().unwrap_or_else(|e| e.into_inner());
                let agent_pid = {
                    let reg = self
                        .agent_registry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    reg.get(pane.id).and_then(|entry| entry.pid)
                };
                let claude_meta =
                    agent_pid.and_then(|pid| self.claude_cwd.chrome_meta_for_pid(pid));
                let dims = if let Some(term) = pane.backend.term() {
                    let term_guard = term.lock();
                    let cols = alacritty_terminal::grid::Dimensions::columns(&*term_guard);
                    let rows = alacritty_terminal::grid::Dimensions::screen_lines(&*term_guard);
                    (cols, rows)
                } else {
                    (80, 24)
                };
                // tn-5fgz: Enriched Claude state text for the status bar.
                let status_text = claude_meta.as_ref().map(|meta| meta.status_bar_text());
                (
                    claude_meta
                        .as_ref()
                        .and_then(|meta| meta.cwd.as_ref())
                        .map(|cwd| cwd.to_string_lossy().into_owned())
                        .or_else(|| status.cwd.clone()),
                    claude_meta.and_then(|meta| meta.session_title),
                    status_text,
                    status.last_exit_code,
                    status.agent_name.clone(),
                    status
                        .git_state
                        .as_ref()
                        .map(crate::git_state::format_for_status_bar),
                    dims,
                )
            } else {
                (None, None, None, None, None, None, (80, 24))
            };

            let (workspace_ids, active_workspace) = if let Some(wm) = self.workspaces.as_ref() {
                (wm.workspace_ids(), wm.active_id())
            } else {
                (vec![1], 1)
            };

            // Use the real daemon PaneId (not a workspace-local ordinal) so
            // the footer matches the header and "Copy pane ID" (tn-5wrx).
            let focused_pane_id = self.workspaces.as_ref().and_then(|wm| wm.focused_pane());

            let status_info = chrome::StatusBarInfo {
                agent_name,
                claude_title,
                claude_status_text,
                cwd,
                dimensions,
                last_exit_code,
                show_agent_indicator: self.config.trust.show_agent_indicator,
                workspace_ids,
                active_workspace,
                is_zoomed: self.zoomed_layout.is_some(),
                focused_pane_id,
                git_branch,
                template_status: self.config.template_status.clone(),
                delegate_summary: delegate_summary_text.clone(),
            };

            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("status_bar_encoder"),
                });
            new_status_bar_hit_areas = chrome::draw_status_bar(
                &status_info,
                renderer,
                &gpu.device,
                &gpu.queue,
                &mut encoder,
                &view,
                gpu.config.width,
                gpu.config.height,
            );

            gpu.queue.submit(std::iter::once(encoder.finish()));
        }

        // ── Tab bar / CSD title bar ────────────────────────────────────
        let use_csd = self.config.general.use_csd;
        let workspace_count = self.workspaces.as_ref().map(|wm| wm.len()).unwrap_or(1);
        let tab_bar_visible = crate::pane::should_show_tab_bar(workspace_count);
        if tab_bar_visible || use_csd {
            let (workspace_ids, active_workspace) = if let Some(wm) = self.workspaces.as_ref() {
                (wm.workspace_ids(), wm.active_id())
            } else {
                (vec![1], 1)
            };

            // tn-5fgz: Build a map of workspace_id -> Claude session title
            // for each workspace whose focused pane has a Claude session.
            let claude_tab_titles: std::collections::HashMap<usize, String> = workspace_ids
                .iter()
                .filter_map(|&ws_id| {
                    let wm = self.workspaces.as_ref()?;
                    let focused_id = wm.focused_pane_for(ws_id)?;
                    let ws_layout = wm.layout_for(ws_id)?;
                    let pane = ws_layout.find_pane(focused_id)?;
                    let agent_pid = {
                        let reg = self
                            .agent_registry
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        reg.get(pane.id).and_then(|entry| entry.pid)
                    };
                    let meta =
                        agent_pid.and_then(|pid| self.claude_cwd.chrome_meta_for_pid(pid))?;
                    let title = meta.session_title?;
                    Some((ws_id, title))
                })
                .collect();
            let claude_titles_ref = if claude_tab_titles.is_empty() {
                None
            } else {
                Some(&claude_tab_titles)
            };

            let tab_labels = super::build_tab_labels(
                &workspace_ids,
                self.workspaces.as_ref(),
                self.rename_state.as_ref(),
                claude_titles_ref,
            );

            let tab_info = chrome::TabBarInfo {
                workspace_ids,
                active_workspace,
                tab_labels,
            };

            let bar_h = crate::pane::effective_tab_bar_height_csd(workspace_count, use_csd);

            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tab_bar_encoder"),
                });
            let csd_reserved = if use_csd {
                crate::pane::CSD_BUTTONS_TOTAL_WIDTH
            } else {
                0.0
            };
            // tn-t2yd.4: when the CSD strip is reserved (use_csd = true) we
            // always render the tab labels even with a single workspace so
            // the tab is clickable and has a right-click menu. Without CSD
            // we keep the tn-t2yd.3 behavior of auto-hiding tabs for a lone
            // workspace.
            let draw_tabs = tab_bar_visible || use_csd;
            chrome::draw_tab_bar(
                &tab_info,
                renderer,
                &gpu.device,
                &gpu.queue,
                &mut encoder,
                &view,
                gpu.config.width,
                gpu.config.height,
                bar_h,
                draw_tabs,
                csd_reserved,
            );

            // Submit tab bar before CSD buttons — both use the shared
            // overlay_text_renderer, and a second prepare() overwrites the
            // vertex buffer that the first render pass references.
            gpu.queue.submit(std::iter::once(encoder.finish()));

            // Draw CSD window control buttons on top of the tab bar.
            if use_csd {
                let mut encoder =
                    gpu.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("csd_buttons_encoder"),
                        });
                let hover_x = self
                    .cursor_position
                    .filter(|(_, py)| (*py as f32) < bar_h)
                    .map(|(px, _)| px as f32);
                chrome::draw_csd_buttons(
                    renderer,
                    &gpu.device,
                    &gpu.queue,
                    &mut encoder,
                    &view,
                    gpu.config.width,
                    gpu.config.height,
                    bar_h,
                    hover_x,
                );
                gpu.queue.submit(std::iter::once(encoder.finish()));
            }
        }

        // ── Modal overlays (on top of everything) ───────────────────────
        match self.overlay_mode {
            Some(OverlayMode::Help) => {
                let mut encoder =
                    gpu.device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("help_overlay_encoder"),
                        });
                help_overlay::draw_help_overlay(
                    &self.config.keybindings,
                    self.help_overlay_scroll_rows,
                    renderer,
                    &gpu.device,
                    &gpu.queue,
                    &mut encoder,
                    &view,
                    gpu.config.width,
                    gpu.config.height,
                );
                gpu.queue.submit(std::iter::once(encoder.finish()));
            }
            Some(OverlayMode::Settings) => {
                settings_overlay::draw_settings_overlay(
                    &mut self.settings_overlay,
                    renderer,
                    &gpu.device,
                    &gpu.queue,
                    &view,
                    gpu.config.width,
                    gpu.config.height,
                );
            }
            Some(OverlayMode::TrustEscalation) => {
                if let Some(ref state) = self.trust_escalation {
                    trust_escalation_overlay::draw_trust_escalation_overlay(
                        state,
                        renderer,
                        &gpu.device,
                        &gpu.queue,
                        &view,
                        gpu.config.width,
                        gpu.config.height,
                    );
                }
            }
            None => {}
        }

        // ── Context menu overlay (on top of everything) ────────────────
        if let Some(ref menu) = self.active_menu {
            crate::menu::render_context_menu(
                menu,
                renderer,
                &gpu.device,
                &gpu.queue,
                &view,
                gpu.config.width,
                gpu.config.height,
            );
        }

        // ── Visual bell overlay ──────────────────────────────────────────
        let bell_intensity = {
            let duration_ms = self.config.bell.visual_bell_duration_ms;
            match self.visual_bell_start {
                Some(start) => {
                    let elapsed_ms = start.elapsed().as_millis() as u64;
                    if elapsed_ms >= duration_ms {
                        0.0
                    } else {
                        1.0 - (elapsed_ms as f32 / duration_ms as f32)
                    }
                }
                None => 0.0,
            }
        };
        if bell_intensity > 0.0 {
            // Push visual bell quad to overlay layer (Modal tier, on top of everything).
            chrome::push_visual_bell_overlay(
                bell_intensity,
                gpu.config.width,
                gpu.config.height,
                &mut chrome_overlay,
            );
        }

        // ── Toast (lower-right transient notification) ──────────────────
        // Drop expired toast first so we don't draw stale text.
        let mut toast_active = false;
        if let Some(t) = self.toast.as_ref() {
            if t.is_expired(Instant::now()) {
                self.toast = None;
            } else {
                toast_active = true;
            }
        }
        if toast_active {
            // Clone the toast so we can pass it to draw_toast without
            // holding a borrow on `self` while also borrowing `renderer`.
            let toast_clone = self.toast.clone().expect("toast_active implies Some");
            super::toast::draw_toast(
                &toast_clone,
                renderer,
                &gpu.device,
                &gpu.queue,
                &view,
                gpu.config.width,
                gpu.config.height,
                &mut chrome_overlay,
            );
        }

        // ── Overlay pass: render all batched chrome/modal quads ─────────
        // This is the second GPU pass — composites semi-transparent overlay
        // geometry on top of the grid content in a single batched draw call.
        if !chrome_overlay.is_empty() {
            chrome_overlay.render(
                renderer,
                &gpu.device,
                &gpu.queue,
                &view,
                gpu.config.width,
                gpu.config.height,
            );
        }

        // ── Widget pass (tn-npd) ─────────────────────────────────────────
        // Pre-rasterized overlay widgets composite on top of the overlay
        // pass as textured quads. Today this is only the agent status
        // badge PoC (top-right of the window). Re-rasterization is gated
        // on a data hash so frames where nothing changed pay only for
        // the draw call, not the rasterization.
        self.draw_widget_overlays(&view);

        output.present();

        self.status_bar_hit_areas = new_status_bar_hit_areas;

        // If visual bell is still active, schedule another redraw for animation.
        if bell_intensity > 0.0 {
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        } else {
            self.visual_bell_start = None;
        }

        // Keep the event loop ticking while a toast is visible so it
        // animates out cleanly when it expires.
        if toast_active && let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }

        // tn-ztv3.4: Keep the event loop ticking while any delegate is
        // tracked (active or fading). The elapsed-time display updates on
        // each redraw, and the fade window relies on repeated ticks to
        // expire cleanly. `is_visible()` returns true for both active and
        // fade-window entries.
        if self.delegate_summary.is_visible()
            && let Some(w) = self.window.as_ref()
        {
            w.request_redraw();
        }
    }

    /// Walk every workspace's pane tree once per frame, drop observations
    /// into the delegate summary state machine, and return the rendered
    /// footer text (or `None` if nothing is tracked).
    ///
    /// Detection convention (tn-ztv3.3 `/gg-delegate` skill):
    ///
    /// - `delegate_profile=<name>` — pane is a delegate sibling.
    /// - `delegate_status=done` — the orchestrator marked the sibling as
    ///   finished (maps to [`chrome::DelegateState::Done`]).
    ///
    /// Live state is inferred from the pane's `ClaudeChromeMeta.status`
    /// (populated by the app-side `ClaudeCwdTracker` poller). When no
    /// Claude metadata is available for the pane we default to `Idle` so
    /// the delegate still appears in the summary.
    fn scan_and_update_delegate_summary(&mut self) -> Option<String> {
        let now = Instant::now();
        let mut present: HashSet<PaneId> = HashSet::new();

        // Snapshot workspace ids first to avoid holding a borrow on
        // `self.workspaces` while we walk panes and update the summary.
        let workspace_ids: Vec<usize> = match self.workspaces.as_ref() {
            Some(wm) => wm.workspace_ids(),
            None => Vec::new(),
        };

        // Collect (pane_id, profile, state) tuples first so we can release
        // the pane status / agent registry locks before touching the
        // mutable summary state.
        #[derive(Clone)]
        struct DelegateObservation {
            pane_id: PaneId,
            profile: String,
            state: chrome::DelegateState,
        }
        let mut observations: Vec<DelegateObservation> = Vec::new();

        for ws_id in workspace_ids {
            let layout = match self.workspaces.as_ref().and_then(|wm| wm.layout_for(ws_id)) {
                Some(l) => l,
                None => continue,
            };
            for pane_id in layout.pane_ids() {
                let pane = match layout.find_pane(pane_id) {
                    Some(p) => p,
                    None => continue,
                };
                let (profile, tagged_status) = {
                    let status = pane.status.lock().unwrap_or_else(|e| e.into_inner());
                    let profile = status.tags.get("delegate_profile").cloned();
                    let tagged_status = status.tags.get("delegate_status").cloned();
                    (profile, tagged_status)
                };
                let Some(profile) = profile else {
                    continue;
                };
                present.insert(pane_id);

                // An explicit `delegate_status=done` tag wins over the
                // live Claude state because the orchestrator sets it after
                // capturing the result, and Claude itself may still report
                // "idle" during the interval before the process exits.
                let state = if tagged_status.as_deref() == Some("done") {
                    chrome::DelegateState::Done
                } else {
                    let agent_pid = {
                        let reg = self
                            .agent_registry
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        reg.get(pane_id).and_then(|entry| entry.pid)
                    };
                    agent_pid
                        .and_then(|pid| self.claude_cwd.chrome_meta_for_pid(pid))
                        .map(|meta| delegate_state_from_claude(meta.status))
                        .unwrap_or(chrome::DelegateState::Idle)
                };

                observations.push(DelegateObservation {
                    pane_id,
                    profile,
                    state,
                });
            }
        }

        for obs in &observations {
            self.delegate_summary
                .update(obs.pane_id, &obs.profile, obs.state, now);
        }

        self.delegate_summary.tick(&present, now);
        self.delegate_summary.render_text(now)
    }

    /// Composite pre-rasterized overlay widgets (tn-npd).
    ///
    /// Runs once per frame as the final pass of `App::render`, after the
    /// grid + chrome overlay pass. Draws:
    ///
    /// 1. **Agent status badge** (top-right) — shows the focused pane's
    ///    detected agent name + inferred status. Only drawn when an agent
    ///    is present. Also feeds the timeline with tool-change events.
    /// 2. **Agent timeline bar** (tn-x85k) — horizontal colored bar of
    ///    recent tool activity. Drawn when visible (keybinding toggle or
    ///    `widgets.agent_timeline.enabled` config). Position and size
    ///    controlled by `[widgets.agent_timeline]` config.
    fn draw_widget_overlays(&mut self, view: &wgpu::TextureView) {
        use crate::widgets::{AgentBadgeSource, BADGE_WIDGET_ID, TIMELINE_WIDGET_ID};
        use glyphon::{Attrs, Buffer, Family, Metrics, Shaping, TextArea, TextBounds};
        use therminal_core::config::TimelinePosition;

        let Some(gpu) = self.gpu.as_ref() else {
            return;
        };
        let Some(widget_renderer) = self.widget_renderer.as_ref() else {
            return;
        };
        let Some(renderer) = self.grid_renderer.as_mut() else {
            return;
        };

        // ── Focused pane's agent name (from PaneStatus) ─────────────────
        let agent_name: Option<String> = self.workspaces.as_ref().and_then(|wm| {
            let layout = wm.layout();
            let fid = wm.focused_pane()?;
            let pane = layout.find_pane(fid)?;
            pane.status.lock().ok().and_then(|s| s.agent_name.clone())
        });

        let workspace_count = self.workspaces.as_ref().map(|wm| wm.len()).unwrap_or(1);

        // ── Live AgentStatus from the shared registry ───────────────────
        let status_owned = agent_name.as_ref().and_then(|_| {
            let fid = self.workspaces.as_ref().and_then(|wm| wm.focused_pane())?;
            self.agent_registry
                .lock()
                .ok()
                .and_then(|reg| reg.get(fid).map(|e| e.status.clone()))
        });

        // Feed the timeline with the current tool status, if an agent is
        // present. This drives the ring buffer from the same per-frame
        // status snapshot the badge already reads, so no new IPC is needed.
        if self.agent_timeline.visible
            && let Some(ref status) = status_owned
        {
            use therminal_terminal::agent_registry::AgentStatus;
            let (tool_name, source) = match status {
                AgentStatus::ToolUse { tool_name } => (
                    tool_name.clone(),
                    crate::widgets::agent_timeline::EventSource::TopLevel,
                ),
                AgentStatus::Thinking => (
                    "Thinking".to_string(),
                    crate::widgets::agent_timeline::EventSource::TopLevel,
                ),
                AgentStatus::Idle => (
                    "Idle".to_string(),
                    crate::widgets::agent_timeline::EventSource::TopLevel,
                ),
                AgentStatus::Processing | AgentStatus::Streaming => (
                    "Thinking".to_string(),
                    crate::widgets::agent_timeline::EventSource::TopLevel,
                ),
                AgentStatus::AwaitingInput => (
                    "Idle".to_string(),
                    crate::widgets::agent_timeline::EventSource::TopLevel,
                ),
                AgentStatus::Active => (
                    "Thinking".to_string(),
                    crate::widgets::agent_timeline::EventSource::TopLevel,
                ),
            };
            self.agent_timeline.record_tool_change(&tool_name, source);
        }

        // ── Agent status badge (PoC) ────────────────────────────────────
        if let Some(ref agent_name) = agent_name
            && let Some(snapshot) =
                AgentBadgeSource::snapshot(Some(agent_name), status_owned.as_ref())
        {
            let (spec, width, height) = AgentBadgeSource::spec_for(&snapshot);

            let right_inset = 12.0_f32;
            let tab_bar_h = crate::pane::effective_tab_bar_height_csd(
                workspace_count,
                self.config.general.use_csd,
            );
            let top_inset = tab_bar_h + 8.0;
            let x = gpu.config.width as f32 - width as f32 - right_inset;
            let y = top_inset;

            if x >= 0.0 && y + height as f32 <= gpu.config.height as f32 {
                let widget_ref = self.widget_manager.upsert(
                    widget_renderer,
                    &gpu.device,
                    &gpu.queue,
                    BADGE_WIDGET_ID,
                    &spec,
                    x,
                    y,
                );
                if let Some(widget) = widget_ref {
                    let widget_x = widget.x;
                    let widget_y = widget.y;
                    let widget_w = widget.width;
                    let widget_h = widget.height;

                    widget_renderer.draw(
                        &gpu.device,
                        &gpu.queue,
                        view,
                        gpu.config.width,
                        gpu.config.height,
                        widget,
                    );

                    // ── Text label on top of the pill ───────────────
                    let label = snapshot.label();
                    let font_size = (widget_h as f32) * 0.52;
                    let line_height = font_size * 1.1;
                    let metrics = Metrics::new(font_size, line_height);
                    let family = renderer.font_config.family.clone();
                    let mut buf = Buffer::new(&mut renderer.font_system, metrics);
                    buf.set_size(
                        &mut renderer.font_system,
                        Some(widget_w as f32),
                        Some(widget_h as f32),
                    );
                    let attrs = Attrs::new().family(Family::Name(&family));
                    buf.set_text(
                        &mut renderer.font_system,
                        &label,
                        &attrs,
                        Shaping::Advanced,
                        None,
                    );
                    buf.shape_until_scroll(&mut renderer.font_system, false);

                    let (tx_local, ty_local) = match spec.kind {
                        crate::widgets::rasterizer::WidgetKind::Pill(ref p) => p.text_origin_px(),
                        _ => (0.0, 0.0),
                    };
                    let text_area = TextArea {
                        buffer: &buf,
                        left: widget_x + tx_local,
                        top: widget_y + ty_local,
                        scale: 1.0,
                        bounds: TextBounds {
                            left: widget_x as i32,
                            top: widget_y as i32,
                            right: (widget_x + widget_w as f32) as i32,
                            bottom: (widget_y + widget_h as f32) as i32,
                        },
                        default_color: glyphon::Color::rgba(235, 240, 250, 255),
                        custom_glyphs: &[],
                    };

                    if let Err(e) = renderer.overlay_text_renderer.prepare(
                        &gpu.device,
                        &gpu.queue,
                        &mut renderer.font_system,
                        &mut renderer.overlay_atlas,
                        &renderer.viewport,
                        [text_area],
                        &mut renderer.swash_cache,
                    ) {
                        tracing::debug!(error = %e, "widget label prepare failed");
                    } else {
                        let mut encoder =
                            gpu.device
                                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                    label: Some("widget_label_encoder"),
                                });
                        {
                            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: Some("widget_label_pass"),
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: wgpu::LoadOp::Load,
                                        store: wgpu::StoreOp::Store,
                                    },
                                    depth_slice: None,
                                })],
                                depth_stencil_attachment: None,
                                timestamp_writes: None,
                                occlusion_query_set: None,
                                multiview_mask: None,
                            });
                            let _ = renderer.overlay_text_renderer.render(
                                &renderer.overlay_atlas,
                                &renderer.viewport,
                                &mut pass,
                            );
                        }
                        gpu.queue.submit(std::iter::once(encoder.finish()));
                    }
                }
            }
        }

        // ── Agent timeline bar (tn-x85k) ────────────────────────────────
        if self.agent_timeline.visible && !self.agent_timeline.is_empty() {
            let sw = gpu.config.width as f32;
            let sh = gpu.config.height as f32;
            let th = self.agent_timeline.height_px() as f32;
            let inset = 12.0_f32;

            // Compute bar width: 40% of window width, clamped.
            let bar_width = ((sw * 0.4).round() as u32).clamp(100, 800);

            // Compute position based on config.
            let status_bar_h = if self.config.general.show_status_bar {
                24.0
            } else {
                0.0
            };
            let tab_bar_h = crate::pane::effective_tab_bar_height_csd(
                workspace_count,
                self.config.general.use_csd,
            );

            let (tl_x, tl_y) = match self.agent_timeline.position() {
                TimelinePosition::TopRight => (sw - bar_width as f32 - inset, tab_bar_h + inset),
                TimelinePosition::BottomRight => (
                    sw - bar_width as f32 - inset,
                    sh - th - status_bar_h - inset,
                ),
                TimelinePosition::BottomCenter => (
                    (sw - bar_width as f32) / 2.0,
                    sh - th - status_bar_h - inset,
                ),
            };

            // Bail if the window is too small to fit the bar.
            if tl_x >= 0.0 && tl_y >= 0.0 && tl_y + th <= sh {
                let spec = self.agent_timeline.spec(bar_width);
                let widget_ref = self.widget_manager.upsert(
                    widget_renderer,
                    &gpu.device,
                    &gpu.queue,
                    TIMELINE_WIDGET_ID,
                    &spec,
                    tl_x,
                    tl_y,
                );
                if let Some(widget) = widget_ref {
                    widget_renderer.draw(
                        &gpu.device,
                        &gpu.queue,
                        view,
                        gpu.config.width,
                        gpu.config.height,
                        widget,
                    );
                }
            }
        }

        // -- Pattern-engine widget placements (tn-068b) -------------------
        // Drain widget matches collected during the render pass and route
        // each through the WidgetManager for rasterization + draw.
        let pattern_matches: Vec<_> = renderer.pattern_widget_sink.drain(..).collect();
        if !pattern_matches.is_empty() {
            use crate::widgets::pattern_widget::{
                compute_placement, spec_from_resolved, widget_id_for,
            };

            let cell_width = renderer.cell_width;
            let cell_height = renderer.cell_height;
            let sw = gpu.config.width;
            let sh = gpu.config.height;

            for m in &pattern_matches {
                let (spec, widget_w, widget_h) = spec_from_resolved(&m.widget);
                let (x, y) = compute_placement(m, cell_width, cell_height, widget_w, sw);

                // Bounds check: skip if the widget would be off-screen.
                if x < 0.0 || y < 0.0 || y + widget_h as f32 > sh as f32 {
                    continue;
                }

                let id = widget_id_for(m.pane_id, m.row, m.start_col);
                let widget_ref = self.widget_manager.upsert(
                    widget_renderer,
                    &gpu.device,
                    &gpu.queue,
                    id,
                    &spec,
                    x,
                    y,
                );
                if let Some(widget) = widget_ref {
                    widget_renderer.draw(
                        &gpu.device,
                        &gpu.queue,
                        view,
                        gpu.config.width,
                        gpu.config.height,
                        widget,
                    );
                }
            }
            tracing::trace!(
                count = pattern_matches.len(),
                "pattern-widget: placed {} widget(s)",
                pattern_matches.len(),
            );
        }
    }

    /// Jump the focused pane's scrollback to the previous/next semantic
    /// region. If `errors_only` is true, only `Error` regions are considered.
    pub(super) fn jump_to_region(&mut self, dir: JumpDirection, errors_only: bool) {
        use alacritty_terminal::grid::{Dimensions, Scroll};
        use therminal_terminal::region_index::RegionKind;

        let focused = match self.focused_pane() {
            Some(id) => id,
            None => return,
        };

        // Snapshot needed pane data without holding the layout borrow over
        // the rest of the method.
        let (term, region_index) = {
            let layout = match self.get_layout() {
                Some(l) => l,
                None => return,
            };
            let pane = match layout.find_pane(focused) {
                Some(p) => p,
                None => return,
            };
            let term = match pane.backend.term() {
                Some(t) => Arc::clone(t),
                None => return,
            };
            (term, Arc::clone(&pane.region_index))
        };

        // Compute the current absolute "viewport top" line so we can find
        // the nearest region in the requested direction.
        let (current_top_line, screen_lines, history_size, current_offset) = {
            let term_guard = term.lock();
            let grid = term_guard.grid();
            let history = grid.history_size();
            let offset = grid.display_offset();
            let screen = grid.screen_lines();
            // The visible viewport's top absolute line is `history - offset`.
            let top = history.saturating_sub(offset);
            (top, screen, history, offset)
        };

        let kinds: &[RegionKind] = if errors_only {
            &[RegionKind::Error]
        } else {
            &[
                RegionKind::Prompt,
                RegionKind::Command,
                RegionKind::Output,
                RegionKind::Error,
                RegionKind::ToolCall,
                RegionKind::Thinking,
            ]
        };

        let target = {
            let idx = match region_index.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let region = match dir {
                JumpDirection::Prev => idx.region_before(current_top_line, kinds),
                JumpDirection::Next => idx.region_after(current_top_line, kinds),
            };
            region.map(|r| {
                let label = format!(
                    "{:?}{}",
                    r.kind,
                    r.metadata
                        .get("command")
                        .map(|c| format!(": {}", c))
                        .unwrap_or_default()
                );
                (r.start_line, label)
            })
        };

        let (start_line, label) = match target {
            Some(t) => t,
            None => {
                self.show_toast(format!(
                    "no {} region",
                    if errors_only { "error" } else { "more" }
                ));
                return;
            }
        };

        // Center the target region in the viewport. The desired display
        // offset puts `start_line` roughly mid-viewport.
        let half = screen_lines / 2;
        let desired_top = start_line.saturating_sub(half);
        let new_offset = history_size.saturating_sub(desired_top).min(history_size);
        let delta = new_offset as i32 - current_offset as i32;

        if delta != 0 {
            // Scroll::Delta is interpreted with positive values scrolling up.
            let mut term_guard = term.lock();
            term_guard.scroll_display(Scroll::Delta(delta));
        }

        info!(target: "therminal::region_jump", "{}", label);
        self.show_toast(label);
    }
}
