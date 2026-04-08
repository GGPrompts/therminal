//! Bottom status bar rendering and hit-testing.

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::GridRenderer;
use therminal_core::palette::Color as PaletteColor;

use super::colors::STATUS_BAR_BG_COLOR;
use super::text_cache::{cached_buf, ensure_shaped};

/// Data collected for the status bar from the focused pane.
pub(crate) struct StatusBarInfo {
    /// Agent name (from ProcessDetector), shown on the left when present.
    pub agent_name: Option<String>,
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
    /// 1-indexed display number of the focused pane (left-to-right tree order),
    /// surfaced in the footer center section so users can identify the active
    /// pane even when per-pane headers are hidden via `show_pane_headers = false`.
    pub focused_pane_id: Option<usize>,
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
    let font_size = (bar_h * 0.55).max(10.0);
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
        if info.show_agent_indicator
            && let Some(name) = &info.agent_name
        {
            parts.push_str(&format!(" [agent: {name}]"));
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

    let center_text = compose_center_text(info.cwd.as_deref(), info.focused_pane_id);
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

/// Compose the center section of the status bar from cwd and the focused
/// pane's display number. Extracted for unit testing.
pub(super) fn compose_center_text(cwd: Option<&str>, focused_pane_id: Option<usize>) -> String {
    let cwd_text = cwd.map(abbreviate_path).unwrap_or_default();
    match (focused_pane_id, cwd_text.is_empty()) {
        (Some(n), false) => format!("{cwd_text}  ·  pane {n}"),
        (Some(n), true) => format!("pane {n}"),
        (None, _) => cwd_text,
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

    if let Ok(home) = std::env::var("HOME")
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

    fn make_info(cwd: Option<&str>, focused_pane_id: Option<usize>) -> StatusBarInfo {
        StatusBarInfo {
            agent_name: None,
            cwd: cwd.map(String::from),
            dimensions: (80, 24),
            last_exit_code: None,
            show_agent_indicator: false,
            workspace_ids: vec![1],
            active_workspace: 1,
            is_zoomed: false,
            focused_pane_id,
        }
    }

    #[test]
    fn status_bar_info_carries_focused_pane_id() {
        let info = make_info(Some("/tmp"), Some(3));
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
        let s = compose_center_text(Some("/tmp/foo"), Some(2));
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
        let s = compose_center_text(None, Some(7));
        assert_eq!(s, "pane 7");
    }

    #[test]
    fn center_text_omits_pane_when_focused_id_missing() {
        let prev = std::env::var("HOME").ok();
        unsafe {
            std::env::remove_var("HOME");
        }
        let s = compose_center_text(Some("/tmp/foo"), None);
        assert_eq!(s, "/tmp/foo");
        if let Some(v) = prev {
            unsafe {
                std::env::set_var("HOME", v);
            }
        }
    }
}
