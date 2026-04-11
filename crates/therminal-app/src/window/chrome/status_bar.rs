//! Bottom status bar rendering and hit-testing.

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::GridRenderer;
use therminal_core::config::ConfigTemplateStatus;
use therminal_core::palette::Color as PaletteColor;

use super::colors::STATUS_BAR_BG_COLOR;
use super::text_cache::{cached_buf, ensure_shaped};

/// Data collected for the status bar from the focused pane.
pub(crate) struct StatusBarInfo {
    /// Agent name (from ProcessDetector), shown on the left when present.
    pub agent_name: Option<String>,
    /// Claude session title for the focused pane when available from the
    /// Claude state poller. This intentionally comes from the same source as
    /// the pane header, not from shell state.
    pub claude_title: Option<String>,
    /// Enriched Claude state text for the status bar left section (tn-5fgz).
    pub claude_status_text: Option<String>,
    /// Current working directory (from OSC 7).
    pub cwd: Option<String>,
    /// Pane grid dimensions (cols, rows).
    pub dimensions: (usize, usize),
    /// Last command exit code (from OSC 633 D mark).
    pub last_exit_code: Option<i32>,
    /// Whether the config allows showing the agent indicator.
    pub show_agent_indicator: bool,
    /// IDs of all existing workspaces (sorted).
    pub workspace_ids: Vec<usize>,
    /// Currently active workspace number.
    pub active_workspace: usize,
    /// Whether a pane is currently zoomed to fullscreen.
    pub is_zoomed: bool,
    /// Real daemon PaneId of the focused pane, surfaced in the footer center
    /// section so users can identify the active pane even when per-pane
    /// headers are hidden via `show_pane_headers = false`. Matches the value
    /// copied by "Copy pane ID" in the context menu (tn-5wrx).
    pub focused_pane_id: Option<u64>,
    /// Git branch display text for the focused pane (tn-e97n).
    pub git_branch: Option<String>,
    /// Result of the template-version scan performed at config load time
    /// (tn-3ge3). When non-`UpToDate`, the status bar shows a small muted
    /// hint nudging the user to regenerate via `therminal --print-config`.
    pub template_status: ConfigTemplateStatus,
    /// Delegate sibling summary text (tn-ztv3.4). When present, a
    /// `delegates: planner=streaming (87s), …` section is rendered between
    /// the center text and the template hint. Always `None` unless at least
    /// one delegate-tagged pane is being tracked by
    /// [`super::DelegateSummaryState`].
    pub delegate_summary: Option<String>,
}

/// Pixel rect (x, y, width, height) returned by chrome hit-test producers.
pub(crate) type ChromeRect = (f32, f32, f32, f32);

/// Hit-test areas produced by `draw_status_bar` for click handling.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct StatusBarHitAreas {
    /// Bounding box of the `[agent: <name>]` indicator text, if drawn.
    pub agent_indicator: Option<ChromeRect>,
}

/// Draw the window status bar at the bottom of the screen.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_status_bar(
    info: &StatusBarInfo,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) -> StatusBarHitAreas {
    let mut hit_areas = StatusBarHitAreas::default();
    use crate::color_mapping::pixel_rect_to_ndc;

    let bar_h = crate::pane::STATUS_BAR_HEIGHT;
    let sw = surface_width as f32;
    let sh = surface_height as f32;
    let bar_y = sh - bar_h;

    // ── Background rect ──
    let bg_verts = pixel_rect_to_ndc(0.0, bar_y, sw, bar_h, sw, sh, STATUS_BAR_BG_COLOR);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("statusbar_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("statusbar_bg_pass"),
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
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..6, 0..1);
    }

    // ── Status bar text ──
    let font_size = renderer.chrome_font_size((bar_h * 0.55).max(10.0));
    let line_height = bar_h;
    let metrics = Metrics::new(font_size, line_height);

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let mut text_areas: Vec<TextArea<'_>> = Vec::new();

    let workspace_text = if info.workspace_ids.len() > 1 {
        let mut s = String::from(" ");
        for &ws_id in &info.workspace_ids {
            if ws_id == info.active_workspace {
                s.push_str(&format!("[{ws_id}] "));
            } else {
                s.push_str(&format!(" {ws_id}  "));
            }
        }
        s
    } else {
        String::new()
    };

    let workspace_active_color = GlyphColor::rgba(
        PaletteColor::FOCUS.r,
        PaletteColor::FOCUS.g,
        PaletteColor::FOCUS.b,
        255,
    );

    let left_text = {
        let mut parts = String::new();
        if info.is_zoomed {
            parts.push_str(" [ZOOM]");
        }
        // tn-5fgz: prefer enriched Claude state text over generic agent name.
        if info.show_agent_indicator {
            if let Some(ref text) = info.claude_status_text {
                parts.push_str(&format!(" [{text}]"));
            } else if let Some(name) = &info.agent_name {
                parts.push_str(&format!(" [agent: {name}]"));
            }
        }
        if parts.is_empty() { None } else { Some(parts) }
    };
    let left_text_ref = left_text.as_deref().unwrap_or("");

    let agent_color = GlyphColor::rgba(
        PaletteColor::FOCUS.r,
        PaletteColor::FOCUS.g,
        PaletteColor::FOCUS.b,
        230,
    );
    let muted_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        230,
    );

    let center_text = compose_center_text(
        info.claude_title.as_deref(),
        info.cwd.as_deref(),
        info.git_branch.as_deref(),
        info.focused_pane_id,
    );
    let center_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        200,
    );

    let (cols, rows) = info.dimensions;
    let right_text = match info.last_exit_code {
        Some(code) => format!("{cols}x{rows}  [{code}] "),
        None => format!("{cols}x{rows} "),
    };

    // tn-3ge3: optional template-version hint, rendered between the center
    // and right sections in muted color. Empty string means "no hint".
    let template_hint = compose_template_hint(&info.template_status);

    let exit_color = match info.last_exit_code {
        Some(0) => GlyphColor::rgba(
            PaletteColor::STATUS_OK.r,
            PaletteColor::STATUS_OK.g,
            PaletteColor::STATUS_OK.b,
            230,
        ),
        Some(_) => GlyphColor::rgba(
            PaletteColor::STATUS_ERROR.r,
            PaletteColor::STATUS_ERROR.g,
            PaletteColor::STATUS_ERROR.b,
            230,
        ),
        None => muted_color,
    };

    let ws_key = format!("{workspace_text}|{:.0}", sw * 0.25);
    let left_key = format!("{left_text_ref}|{:.0}", sw * 0.35);
    let center_key = format!("{center_text}|{sw:.0}");
    let right_key = format!("{right_text}|{:.0}|{:?}", sw * 0.35, info.last_exit_code);

    let family = renderer.font_config.family.clone();
    ensure_shaped(
        "sb_workspace",
        &ws_key,
        metrics,
        sw * 0.25,
        bar_h,
        &workspace_text,
        Attrs::new()
            .family(Family::Name(&family))
            .color(workspace_active_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        "sb_left",
        &left_key,
        metrics,
        sw * 0.35,
        bar_h,
        left_text_ref,
        Attrs::new()
            .family(Family::Name(&family))
            .color(agent_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        "sb_center",
        &center_key,
        metrics,
        sw,
        bar_h,
        &center_text,
        Attrs::new()
            .family(Family::Name(&family))
            .color(center_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        "sb_right",
        &right_key,
        metrics,
        sw * 0.35,
        bar_h,
        &right_text,
        Attrs::new().family(Family::Name(&family)).color(exit_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    if !template_hint.is_empty() {
        let hint_key = format!("{template_hint}|{:.0}", sw * 0.5);
        ensure_shaped(
            "sb_template_hint",
            &hint_key,
            metrics,
            sw * 0.5,
            bar_h,
            &template_hint,
            Attrs::new()
                .family(Family::Name(&family))
                .color(muted_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }

    // tn-ztv3.4: Delegate sibling summary. Empty string means "no
    // delegates active" and the section is skipped entirely.
    let delegate_text = info.delegate_summary.clone().unwrap_or_default();
    if !delegate_text.is_empty() {
        let delegate_key = format!("{delegate_text}|{:.0}", sw * 0.5);
        ensure_shaped(
            "sb_delegate_summary",
            &delegate_key,
            metrics,
            sw * 0.5,
            bar_h,
            &delegate_text,
            Attrs::new()
                .family(Family::Name(&family))
                .color(workspace_active_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }

    let needs_prefix_measure =
        info.is_zoomed && info.show_agent_indicator && info.agent_name.is_some();
    if needs_prefix_measure {
        let prefix = " [ZOOM]";
        let prefix_key = format!("{prefix}|{:.0}", sw * 0.35);
        ensure_shaped(
            "sb_left_zoom_prefix",
            &prefix_key,
            metrics,
            sw * 0.35,
            bar_h,
            prefix,
            Attrs::new()
                .family(Family::Name(&family))
                .color(agent_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }

    // Phase 2: immutable borrow. Missing slots indicate a shaping failure;
    // skip the affected element instead of panicking.
    let workspace_buf = cached_buf(&renderer.overlay_cache, "sb_workspace");
    let workspace_text_width = workspace_buf
        .and_then(|b| b.layout_runs().next())
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);

    let center_buf = cached_buf(&renderer.overlay_cache, "sb_center");
    let center_text_width = center_buf
        .and_then(|b| b.layout_runs().next())
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);
    let center_offset = ((sw - center_text_width) / 2.0).max(0.0);

    let right_buf = cached_buf(&renderer.overlay_cache, "sb_right");
    let right_text_width = right_buf
        .and_then(|b| b.layout_runs().next())
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0);
    let right_x = (sw - right_text_width).max(0.0);

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    if !workspace_text.is_empty()
        && let Some(buf) = workspace_buf
    {
        text_areas.push(TextArea {
            buffer: buf,
            left: 0.0,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: workspace_active_color,
            custom_glyphs: &[],
        });
    }

    if !left_text_ref.is_empty()
        && let Some(left_buf) = cached_buf(&renderer.overlay_cache, "sb_left")
    {
        let left_total_w = left_buf
            .layout_runs()
            .next()
            .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(0.0);

        let agent_prefix_width = if needs_prefix_measure {
            cached_buf(&renderer.overlay_cache, "sb_left_zoom_prefix")
                .and_then(|b| b.layout_runs().next())
                .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
                .unwrap_or(0.0)
        } else {
            0.0
        };

        if info.show_agent_indicator && info.agent_name.is_some() {
            let agent_x = workspace_text_width + agent_prefix_width;
            let agent_w = (left_total_w - agent_prefix_width).max(0.0);
            hit_areas.agent_indicator = Some((agent_x, bar_y, agent_w, bar_h));
        }

        text_areas.push(TextArea {
            buffer: left_buf,
            left: workspace_text_width,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: agent_color,
            custom_glyphs: &[],
        });
    }

    if !center_text.is_empty()
        && let Some(buf) = center_buf
    {
        text_areas.push(TextArea {
            buffer: buf,
            left: center_offset,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: center_color,
            custom_glyphs: &[],
        });
    }

    if let Some(buf) = right_buf {
        text_areas.push(TextArea {
            buffer: buf,
            left: right_x,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: exit_color,
            custom_glyphs: &[],
        });
    }

    // tn-3ge3: render the template-version hint immediately to the left of
    // the right_text. Muted color, no hit-test area, zero impact when the
    // status is UpToDate.
    // tn-ztv3.4: the delegate summary sits to the left of the template
    // hint (or the right_text when no hint is active), using the focus
    // color so it pops against the muted footer. We compute each section's
    // right edge so neighbouring sections can stack without overlap.
    let gap = font_size * 0.5;
    let mut next_right = right_x;

    if !template_hint.is_empty() {
        let hint_buf = cached_buf(&renderer.overlay_cache, "sb_template_hint");
        if let Some(buf) = hint_buf {
            let hint_w = buf
                .layout_runs()
                .next()
                .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
                .unwrap_or(0.0);
            // Small gap between the hint and the right_text so they don't
            // visually merge into a single token.
            let hint_x = (next_right - hint_w - gap).max(0.0);
            text_areas.push(TextArea {
                buffer: buf,
                left: hint_x,
                top: bar_y,
                scale: 1.0,
                bounds,
                default_color: muted_color,
                custom_glyphs: &[],
            });
            next_right = hint_x;
        }
    }

    if !delegate_text.is_empty()
        && let Some(buf) = cached_buf(&renderer.overlay_cache, "sb_delegate_summary")
    {
        let del_w = buf
            .layout_runs()
            .next()
            .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(0.0);
        let del_x = (next_right - del_w - gap).max(0.0);
        text_areas.push(TextArea {
            buffer: buf,
            left: del_x,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: workspace_active_color,
            custom_glyphs: &[],
        });
    }

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("status bar text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("statusbar_text_pass"),
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

        if let Err(e) = renderer.overlay_text_renderer.render(
            &renderer.overlay_atlas,
            &renderer.viewport,
            &mut pass,
        ) {
            tracing::warn!("status bar text render failed: {}", e);
        }
    }

    hit_areas
}

/// Hit-test the status bar at the given physical pixel coordinates.
pub(crate) fn status_bar_hit_test(
    px: f32,
    py: f32,
    hit_areas: &StatusBarHitAreas,
) -> Option<StatusBarHit> {
    if let Some((x, y, w, h)) = hit_areas.agent_indicator
        && px >= x
        && px < x + w
        && py >= y
        && py < y + h
    {
        return Some(StatusBarHit::AgentIndicator);
    }
    None
}

/// Result of a status bar hit-test.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StatusBarHit {
    AgentIndicator,
}

/// Compose the muted template-version hint shown immediately to the left of
/// the dimensions/exit-code section. Returns the empty string when the
/// config is up to date so the rest of the renderer can short-circuit and
/// pay zero cost.
///
/// tn-3ge3 — strings deliberately point at `therminal --print-config` so
/// users have a single deterministic next step. The detection itself is
/// non-invasive and never modifies the user's file.
pub(super) fn compose_template_hint(status: &ConfigTemplateStatus) -> String {
    match status {
        ConfigTemplateStatus::UpToDate => String::new(),
        ConfigTemplateStatus::Unversioned => {
            "config: outdated — therminal --print-config".to_string()
        }
        ConfigTemplateStatus::Outdated { found, current } => {
            format!("config: v{found} → v{current} — therminal --print-config")
        }
    }
}

/// Compose the center section of the status bar from shared Claude chrome
/// metadata (when available), fallback cwd, and the focused pane id.
pub(super) fn compose_center_text(
    claude_title: Option<&str>,
    cwd: Option<&str>,
    git_branch: Option<&str>,
    focused_pane_id: Option<u64>,
) -> String {
    let cwd_text = cwd.map(abbreviate_path).unwrap_or_default();
    let base = match (claude_title, cwd_text.is_empty()) {
        (Some(title), false) => format!("{title}  ·  {cwd_text}"),
        (Some(title), true) => title.to_string(),
        (None, _) => cwd_text,
    };
    // Append git branch after cwd if available (tn-e97n).
    let base = match (git_branch, base.is_empty()) {
        (Some(branch), false) => format!("{base}  {branch}"),
        (Some(branch), true) => branch.to_string(),
        (None, _) => base,
    };
    match (focused_pane_id, base.is_empty()) {
        (Some(n), false) => format!("{base}  ·  pane {n}"),
        (Some(n), true) => format!("pane {n}"),
        (None, _) => base,
    }
}

/// Abbreviate a path for status bar display: replace the home directory with `~`
/// and extract the path from `file://` URLs.
pub(super) fn abbreviate_path(path: &str) -> String {
    let path = if let Some(rest) = path.strip_prefix("file://") {
        rest.find('/').map(|i| &rest[i..]).unwrap_or(rest)
    } else {
        path
    };

    if let Some(home) = super::super::platform_home_dir()
        && let Some(rest) = path.strip_prefix(home.as_str())
    {
        return format!("~{rest}");
    }

    if let Some(win_home) = wsl2_windows_home()
        && let Some(rest) = path.strip_prefix(win_home.as_str())
    {
        return format!("~win{rest}");
    }

    path.to_string()
}

/// Detect the WSL2 Windows user home directory as a Linux path.
fn wsl2_windows_home() -> Option<String> {
    std::env::var_os("WSL_DISTRO_NAME")?;

    if let Ok(userprofile) = std::env::var("USERPROFILE")
        && let Some(linux_path) = windows_path_to_linux(&userprofile)
    {
        return Some(linux_path);
    }

    if let (Ok(drive), Ok(homepath)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        let combined = format!("{drive}{homepath}");
        if let Some(linux_path) = windows_path_to_linux(&combined) {
            return Some(linux_path);
        }
    }

    None
}

/// Convert a Windows-style absolute path to a WSL2 Linux mount path.
pub(super) fn windows_path_to_linux(windows_path: &str) -> Option<String> {
    if windows_path.len() < 3 {
        return None;
    }
    let (drive, rest) = windows_path.split_at(2);
    if !drive.ends_with(':') {
        return None;
    }
    let drive_letter = drive.chars().next()?.to_ascii_lowercase();
    let rest = rest.trim_start_matches(['\\', '/']);
    let rest = rest.replace('\\', "/");
    Some(format!("/mnt/{drive_letter}/{rest}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_info(
        claude_title: Option<&str>,
        cwd: Option<&str>,
        focused_pane_id: Option<u64>,
    ) -> StatusBarInfo {
        StatusBarInfo {
            agent_name: None,
            claude_title: claude_title.map(String::from),
            claude_status_text: None,
            cwd: cwd.map(String::from),
            dimensions: (80, 24),
            last_exit_code: None,
            show_agent_indicator: false,
            workspace_ids: vec![1],
            active_workspace: 1,
            is_zoomed: false,
            focused_pane_id,
            git_branch: None,
            template_status: ConfigTemplateStatus::UpToDate,
            delegate_summary: None,
        }
    }

    #[test]
    fn template_hint_empty_when_up_to_date() {
        let s = compose_template_hint(&ConfigTemplateStatus::UpToDate);
        assert!(s.is_empty());
    }

    #[test]
    fn template_hint_unversioned_uses_print_config_string() {
        let s = compose_template_hint(&ConfigTemplateStatus::Unversioned);
        assert_eq!(s, "config: outdated — therminal --print-config");
    }

    #[test]
    fn template_hint_outdated_includes_versions() {
        let s = compose_template_hint(&ConfigTemplateStatus::Outdated {
            found: 1,
            current: 3,
        });
        assert_eq!(s, "config: v1 → v3 — therminal --print-config");
    }

    #[test]
    fn status_bar_info_carries_focused_pane_id() {
        let info = make_info(None, Some("/tmp"), Some(3));
        assert_eq!(info.focused_pane_id, Some(3));
    }

    #[test]
    fn center_text_includes_pane_number_when_present() {
        // unset HOME so abbreviate_path leaves the literal path alone.
        let prev = std::env::var("HOME").ok();
        // SAFETY: test runs single-threaded for this var; restored at the end.
        unsafe {
            std::env::remove_var("HOME");
        }
        let s = compose_center_text(None, Some("/tmp/foo"), None, Some(2));
        assert!(s.contains("/tmp/foo"));
        assert!(s.contains("pane 2"));
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("HOME", v);
            }
        }
    }

    #[test]
    fn center_text_falls_back_to_pane_only_without_cwd() {
        let s = compose_center_text(None, None, None, Some(7));
        assert_eq!(s, "pane 7");
    }

    #[test]
    fn center_text_omits_pane_when_focused_id_missing() {
        let prev = std::env::var("HOME").ok();
        unsafe {
            std::env::remove_var("HOME");
        }
        let s = compose_center_text(None, Some("/tmp/foo"), None, None);
        assert_eq!(s, "/tmp/foo");
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("HOME", v);
            }
        }
    }

    #[test]
    fn center_text_prefers_claude_title_when_present() {
        let s = compose_center_text(Some("fix login bug"), Some("/tmp/foo"), None, Some(2));
        assert_eq!(s, "fix login bug  ·  /tmp/foo  ·  pane 2");
    }

    #[test]
    fn center_text_uses_claude_title_without_cwd() {
        let s = compose_center_text(Some("fix login bug"), None, None, Some(2));
        assert_eq!(s, "fix login bug  ·  pane 2");
    }
}

#[cfg(test)]
mod abbreviate_path_tests {
    use super::abbreviate_path;

    #[test]
    fn abbreviate_path_replaces_home_with_tilde() {
        if let Some(home) = super::super::super::platform_home_dir() {
            let path = format!("{home}/projects/therminal/src/main.rs");
            let abbr = abbreviate_path(&path);
            assert_eq!(
                abbr, "~/projects/therminal/src/main.rs",
                "expected home dir to be abbreviated to ~"
            );
        }
    }

    #[test]
    fn abbreviate_path_leaves_non_home_paths_alone() {
        let path = "/tmp/random/path";
        let abbr = abbreviate_path(path);
        assert_eq!(abbr, path, "non-home paths should pass through unchanged");
    }

    #[test]
    fn abbreviate_path_strips_file_url_prefix() {
        let path = "file://localhost/tmp/foo";
        let abbr = abbreviate_path(path);
        assert_eq!(abbr, "/tmp/foo");
    }

    #[test]
    fn abbreviate_path_handles_bare_home() {
        if let Some(home) = super::super::super::platform_home_dir() {
            let abbr = abbreviate_path(&home);
            assert_eq!(abbr, "~", "bare home should abbreviate to ~");
        }
    }
}
