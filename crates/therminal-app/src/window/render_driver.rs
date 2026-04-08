//! Per-frame render driver and semantic-region scrollback jump.
//!
//! Split out from `mod.rs` to keep the coordinator focused on event dispatch.

use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

use super::{App, chrome, help_overlay, render};

/// Direction for [`App::jump_to_region`].
#[derive(Debug, Clone, Copy)]
pub(super) enum JumpDirection {
    Prev,
    Next,
}

impl App {
    /// Render a frame: render all panes and separators.
    pub(super) fn render(&mut self) {
        let mut new_status_bar_hit_areas = chrome::StatusBarHitAreas::default();
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
        render::render_panes_recursive(
            layout,
            focused,
            show_focus,
            pane_count,
            show_pane_headers,
            &mut pane_counter,
            renderer,
            &gpu.device,
            &gpu.queue,
            &view,
            gpu.config.width,
            gpu.config.height,
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

            let (cwd, last_exit_code, agent_name, dimensions) = if let Some(pane) = focused_pane {
                let status = pane.status.lock().unwrap_or_else(|e| e.into_inner());
                let dims = if let Some(term) = pane.backend.term() {
                    let term_guard = term.lock();
                    let cols = alacritty_terminal::grid::Dimensions::columns(&*term_guard);
                    let rows = alacritty_terminal::grid::Dimensions::screen_lines(&*term_guard);
                    (cols, rows)
                } else {
                    (80, 24)
                };
                (
                    status.cwd.clone(),
                    status.last_exit_code,
                    status.agent_name.clone(),
                    dims,
                )
            } else {
                (None, None, None, (80, 24))
            };

            let (workspace_ids, active_workspace) = if let Some(wm) = self.workspaces.as_ref() {
                (wm.workspace_ids(), wm.active_id())
            } else {
                (vec![1], 1)
            };

            // Compute the 1-indexed display number of the focused pane within
            // the layout's left-to-right traversal order, so the footer can
            // surface "pane N" even when per-pane headers are hidden.
            let focused_pane_id = self
                .workspaces
                .as_ref()
                .and_then(|wm| wm.focused_pane())
                .and_then(|fid| {
                    layout
                        .pane_ids()
                        .iter()
                        .position(|pid| *pid == fid)
                        .map(|i| i + 1)
                });

            let status_info = chrome::StatusBarInfo {
                agent_name,
                cwd,
                dimensions,
                last_exit_code,
                show_agent_indicator: self.config.trust.show_agent_indicator,
                workspace_ids,
                active_workspace,
                is_zoomed: self.zoomed_layout.is_some(),
                focused_pane_id,
                template_status: self.config.template_status.clone(),
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
        if self.config.general.show_tab_bar || use_csd {
            let (workspace_ids, active_workspace) = if let Some(wm) = self.workspaces.as_ref() {
                (wm.workspace_ids(), wm.active_id())
            } else {
                (vec![1], 1)
            };

            let tab_labels = super::build_tab_labels(
                &workspace_ids,
                self.workspaces.as_ref(),
                self.rename_state.as_ref(),
            );

            let tab_info = chrome::TabBarInfo {
                workspace_ids,
                active_workspace,
                tab_labels,
            };

            let bar_h = crate::pane::effective_tab_bar_height_csd(
                self.config.general.show_tab_bar,
                use_csd,
            );

            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tab_bar_encoder"),
                });
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
                self.config.general.show_tab_bar,
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

        // ── Help overlay (on top of everything) ─────────────────────────
        if self.show_help_overlay {
            let mut encoder = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("help_overlay_encoder"),
                });
            help_overlay::draw_help_overlay(
                &self.config.keybindings,
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
