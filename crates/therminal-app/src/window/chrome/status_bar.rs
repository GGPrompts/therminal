//! Bottom status bar rendering and hit-testing.

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::GridRenderer;
use therminal_core::config::ConfigTemplateStatus;
use therminal_core::palette::Color as PaletteColor;

use super::colors::STATUS_BAR_BG_COLOR;
use super::render_pass::with_chrome_render_pass;
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
    let bar_h = crate::pane::STATUS_BAR_HEIGHT;
    let sw = surface_width as f32;
    let sh = surface_height as f32;
    let bar_y = sh - bar_h;

    // ── 1. Background rect ──
    draw_status_bar_bg(renderer, device, encoder, view, sw, sh, bar_y, bar_h);

    // ── 2. Compose strings + colors ──
    let strings = StatusBarStrings::compute(info);
    let colors = StatusBarColors::compute(info.last_exit_code);
    let needs_prefix_measure =
        info.is_zoomed && info.show_agent_indicator && info.agent_name.is_some();

    let font_size = renderer.chrome_font_size((bar_h * 0.55).max(10.0));
    let metrics = Metrics::new(font_size, bar_h);

    // ── 3. Phase 1: shape every text slot ──
    shape_status_bar_text(
        &strings,
        &colors,
        info.last_exit_code,
        needs_prefix_measure,
        metrics,
        sw,
        bar_h,
        renderer,
    );

    // ── 4. Phase 2: position TextAreas, prepare + draw ──
    position_and_draw_status_bar_text(
        info,
        &strings,
        &colors,
        font_size,
        sw,
        bar_y,
        bar_h,
        needs_prefix_measure,
        renderer,
        device,
        queue,
        encoder,
        view,
        surface_width,
        surface_height,
    )
}

/// Draw the status bar background fill in a single render pass.
#[allow(clippy::too_many_arguments)]
fn draw_status_bar_bg(
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    sw: f32,
    sh: f32,
    bar_y: f32,
    bar_h: f32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let bg_verts = pixel_rect_to_ndc(0.0, bar_y, sw, bar_h, sw, sh, STATUS_BAR_BG_COLOR);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("statusbar_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    with_chrome_render_pass(encoder, view, "statusbar_bg_pass", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..6, 0..1);
    });
}

/// Pre-formatted strings shown in the status bar. Empty strings denote
/// sections that should be skipped entirely.
struct StatusBarStrings {
    workspace_text: String,
    left_text: String,
    center_text: String,
    right_text: String,
    template_hint: String,
    delegate_text: String,
}

impl StatusBarStrings {
    fn compute(info: &StatusBarInfo) -> Self {
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
            parts
        };

        let center_text = compose_center_text(
            info.claude_title.as_deref(),
            info.cwd.as_deref(),
            info.git_branch.as_deref(),
            info.focused_pane_id,
        );

        let (cols, rows) = info.dimensions;
        let right_text = match info.last_exit_code {
            Some(code) => format!("{cols}x{rows}  [{code}] "),
            None => format!("{cols}x{rows} "),
        };

        let template_hint = compose_template_hint(&info.template_status);
        let delegate_text = info.delegate_summary.clone().unwrap_or_default();

        Self {
            workspace_text,
            left_text,
            center_text,
            right_text,
            template_hint,
            delegate_text,
        }
    }
}

/// Glyph colors used by the status bar. Computed once from the exit code
/// and reused for shaping + TextArea defaults.
struct StatusBarColors {
    workspace_active: GlyphColor,
    agent: GlyphColor,
    muted: GlyphColor,
    center: GlyphColor,
    exit: GlyphColor,
}

impl StatusBarColors {
    fn compute(last_exit_code: Option<i32>) -> Self {
        let workspace_active = GlyphColor::rgba(
            PaletteColor::FOCUS.r,
            PaletteColor::FOCUS.g,
            PaletteColor::FOCUS.b,
            255,
        );
        let agent = GlyphColor::rgba(
            PaletteColor::FOCUS.r,
            PaletteColor::FOCUS.g,
            PaletteColor::FOCUS.b,
            230,
        );
        let muted = GlyphColor::rgba(
            PaletteColor::INK_MUTED.r,
            PaletteColor::INK_MUTED.g,
            PaletteColor::INK_MUTED.b,
            230,
        );
        let center = GlyphColor::rgba(
            PaletteColor::INK.r,
            PaletteColor::INK.g,
            PaletteColor::INK.b,
            200,
        );
        let exit = match last_exit_code {
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
            None => muted,
        };
        Self {
            workspace_active,
            agent,
            muted,
            center,
            exit,
        }
    }
}

/// Phase 1: shape every status bar text slot. Mutates the chrome text
/// cache so the immutable Phase 2 can pull buffers by slot name.
#[allow(clippy::too_many_arguments)]
fn shape_status_bar_text(
    strings: &StatusBarStrings,
    colors: &StatusBarColors,
    last_exit_code: Option<i32>,
    needs_prefix_measure: bool,
    metrics: Metrics,
    sw: f32,
    bar_h: f32,
    renderer: &mut GridRenderer,
) {
    let family = renderer.font_config.family.clone();
    let attrs =
        |c: GlyphColor| -> Attrs<'_> { Attrs::new().family(Family::Name(&family)).color(c) };

    let ws_key = format!("{}|{:.0}", strings.workspace_text, sw * 0.25);
    ensure_shaped(
        "sb_workspace",
        &ws_key,
        metrics,
        sw * 0.25,
        bar_h,
        &strings.workspace_text,
        attrs(colors.workspace_active),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    let left_key = format!("{}|{:.0}", strings.left_text, sw * 0.35);
    ensure_shaped(
        "sb_left",
        &left_key,
        metrics,
        sw * 0.35,
        bar_h,
        &strings.left_text,
        attrs(colors.agent),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    let center_key = format!("{}|{sw:.0}", strings.center_text);
    ensure_shaped(
        "sb_center",
        &center_key,
        metrics,
        sw,
        bar_h,
        &strings.center_text,
        attrs(colors.center),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    let right_key = format!("{}|{:.0}|{last_exit_code:?}", strings.right_text, sw * 0.35);
    ensure_shaped(
        "sb_right",
        &right_key,
        metrics,
        sw * 0.35,
        bar_h,
        &strings.right_text,
        attrs(colors.exit),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );

    if !strings.template_hint.is_empty() {
        let hint_key = format!("{}|{:.0}", strings.template_hint, sw * 0.5);
        ensure_shaped(
            "sb_template_hint",
            &hint_key,
            metrics,
            sw * 0.5,
            bar_h,
            &strings.template_hint,
            attrs(colors.muted),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }

    // tn-ztv3.4: Delegate sibling summary. Empty string means "no
    // delegates active" and the section is skipped entirely.
    if !strings.delegate_text.is_empty() {
        let delegate_key = format!("{}|{:.0}", strings.delegate_text, sw * 0.5);
        ensure_shaped(
            "sb_delegate_summary",
            &delegate_key,
            metrics,
            sw * 0.5,
            bar_h,
            &strings.delegate_text,
            attrs(colors.workspace_active),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }

    // Pre-shape a " [ZOOM]" prefix so Phase 2 can subtract its width when
    // computing the agent-indicator hit area while zoomed.
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
            attrs(colors.agent),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }
}

/// Phase 2: read shaped buffers, build TextArea positions, register hit
/// areas, then prepare + render. Returns the hit areas so callers can
/// mouse-test the agent indicator.
#[allow(clippy::too_many_arguments)]
fn position_and_draw_status_bar_text(
    info: &StatusBarInfo,
    strings: &StatusBarStrings,
    colors: &StatusBarColors,
    font_size: f32,
    sw: f32,
    bar_y: f32,
    bar_h: f32,
    needs_prefix_measure: bool,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) -> StatusBarHitAreas {
    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let (text_areas, hit_areas) = build_status_bar_text_areas(
        info,
        strings,
        colors,
        font_size,
        sw,
        bar_y,
        bar_h,
        needs_prefix_measure,
        bounds,
        &renderer.overlay_cache,
    );

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

    with_chrome_render_pass(encoder, view, "statusbar_text_pass", |pass| {
        if let Err(e) =
            renderer
                .overlay_text_renderer
                .render(&renderer.overlay_atlas, &renderer.viewport, pass)
        {
            tracing::warn!("status bar text render failed: {}", e);
        }
    });

    hit_areas
}

/// Build the TextArea list and hit areas for the status bar. Pulled out
/// of `position_and_draw_status_bar_text` so the borrow on
/// `renderer.overlay_cache` (immutable) is scoped tightly and the
/// subsequent prepare/render calls can take `&mut renderer`.
#[allow(clippy::too_many_arguments)]
fn build_status_bar_text_areas<'cache>(
    info: &StatusBarInfo,
    strings: &StatusBarStrings,
    colors: &StatusBarColors,
    font_size: f32,
    sw: f32,
    bar_y: f32,
    bar_h: f32,
    needs_prefix_measure: bool,
    bounds: TextBounds,
    cache: &'cache super::text_cache::ChromeTextCache,
) -> (Vec<TextArea<'cache>>, StatusBarHitAreas) {
    let mut text_areas: Vec<TextArea<'cache>> = Vec::new();
    let mut hit_areas = StatusBarHitAreas::default();

    let workspace_buf = cached_buf(cache, "sb_workspace");
    let workspace_text_width = workspace_buf.map(buffer_run_width).unwrap_or(0.0);

    let center_buf = cached_buf(cache, "sb_center");
    let center_text_width = center_buf.map(buffer_run_width).unwrap_or(0.0);
    let center_offset = ((sw - center_text_width) / 2.0).max(0.0);

    let right_buf = cached_buf(cache, "sb_right");
    let right_text_width = right_buf.map(buffer_run_width).unwrap_or(0.0);
    let right_x = (sw - right_text_width).max(0.0);

    if !strings.workspace_text.is_empty()
        && let Some(buf) = workspace_buf
    {
        text_areas.push(TextArea {
            buffer: buf,
            left: 0.0,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: colors.workspace_active,
            custom_glyphs: &[],
        });
    }

    if !strings.left_text.is_empty()
        && let Some(left_buf) = cached_buf(cache, "sb_left")
    {
        let left_total_w = buffer_run_width(left_buf);
        let agent_prefix_width = if needs_prefix_measure {
            cached_buf(cache, "sb_left_zoom_prefix")
                .map(buffer_run_width)
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
            default_color: colors.agent,
            custom_glyphs: &[],
        });
    }

    if !strings.center_text.is_empty()
        && let Some(buf) = center_buf
    {
        text_areas.push(TextArea {
            buffer: buf,
            left: center_offset,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: colors.center,
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
            default_color: colors.exit,
            custom_glyphs: &[],
        });
    }

    // tn-3ge3 + tn-ztv3.4: stack the template hint and delegate summary
    // to the left of the right_text. Each section advances `next_right`
    // so they don't overlap.
    let gap = font_size * 0.5;
    let mut next_right = right_x;

    if !strings.template_hint.is_empty()
        && let Some(buf) = cached_buf(cache, "sb_template_hint")
    {
        let hint_w = buffer_run_width(buf);
        let hint_x = (next_right - hint_w - gap).max(0.0);
        text_areas.push(TextArea {
            buffer: buf,
            left: hint_x,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: colors.muted,
            custom_glyphs: &[],
        });
        next_right = hint_x;
    }

    if !strings.delegate_text.is_empty()
        && let Some(buf) = cached_buf(cache, "sb_delegate_summary")
    {
        let del_w = buffer_run_width(buf);
        let del_x = (next_right - del_w - gap).max(0.0);
        text_areas.push(TextArea {
            buffer: buf,
            left: del_x,
            top: bar_y,
            scale: 1.0,
            bounds,
            default_color: colors.workspace_active,
            custom_glyphs: &[],
        });
    }

    (text_areas, hit_areas)
}

/// Sum the glyph advance widths of the first layout run in `buf`.
fn buffer_run_width(buf: &glyphon::Buffer) -> f32 {
    buf.layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0)
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
