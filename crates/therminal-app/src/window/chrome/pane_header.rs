//! Pane focus borders, split separators, and per-pane header strips.

use std::collections::HashMap;

use glyphon::{Attrs, Color as GlyphColor, Family, Metrics, Resolution, TextArea, TextBounds};
use wgpu::util::DeviceExt;

use crate::grid_renderer::{ColorVertex, GridRenderer};
use crate::pane::{LayoutNode, PaneId, PaneState, SplitDirection};
use therminal_core::geometry::Rect;
use therminal_core::palette::{ChromePalette, Color as PaletteColor};

use super::colors::{HEADER_BUTTON_MARGIN, HEADER_BUTTON_WIDTH};
use super::render_pass::with_chrome_render_pass;
use super::text_cache::{cached_buf, ensure_shaped};

/// Draw a subtle border around the focused pane.
pub(crate) fn draw_pane_focus_border(
    pane: &PaneState,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let vp = pane.viewport;
    // Slightly stronger border (3px) so focus is unambiguous when per-pane
    // headers are hidden (single-pane layouts, or F11 focus mode).
    let t = 3.0_f32;
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let mut verts: Vec<ColorVertex> = Vec::new();

    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        vp.width(),
        t,
        sw,
        sh,
        renderer.chrome_palette.focus_border,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.bottom() - t,
        vp.width(),
        t,
        sw,
        sh,
        renderer.chrome_palette.focus_border,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.x(),
        vp.y(),
        t,
        vp.height(),
        sw,
        sh,
        renderer.chrome_palette.focus_border,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        vp.right() - t,
        vp.y(),
        t,
        vp.height(),
        sw,
        sh,
        renderer.chrome_palette.focus_border,
    ));

    if verts.is_empty() {
        return;
    }

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("focus_border_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let vertex_count = verts.len() as u32;
    with_chrome_render_pass(encoder, view, "focus_border_pass", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..vertex_count, 0..1);
    });
}

/// Draw a 1px separator line in the gap between two split children.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_split_separator(
    direction: SplitDirection,
    first: &LayoutNode,
    second: &LayoutNode,
    focused: Option<PaneId>,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let first_rects = first.leaf_rects_pub();
    let second_rects = second.leaf_rects_pub();

    let (Some(f), Some(s)) = (first_rects.last(), second_rects.first()) else {
        return;
    };

    let sw = surface_width as f32;
    let sh = surface_height as f32;

    let first_ids = first.pane_ids();
    let second_ids = second.pane_ids();
    let is_focused_adjacent = focused
        .map(|fid| first_ids.contains(&fid) || second_ids.contains(&fid))
        .unwrap_or(false);
    let color = if is_focused_adjacent {
        renderer.chrome_palette.separator_focus
    } else {
        renderer.chrome_palette.separator
    };

    let (px, py, pw, ph) = match direction {
        SplitDirection::Horizontal => {
            let sep_x = f.right();
            let sep_y = f.y().min(s.y());
            let sep_h = f.bottom().max(s.bottom()) - sep_y;
            (sep_x, sep_y, 1.0_f32, sep_h)
        }
        SplitDirection::Vertical => {
            let sep_x = f.x().min(s.x());
            let sep_y = f.bottom();
            let sep_w = f.right().max(s.right()) - sep_x;
            (sep_x, sep_y, sep_w, 1.0_f32)
        }
    };

    let verts = pixel_rect_to_ndc(px, py, pw, ph, sw, sh, color);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("separator_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    with_chrome_render_pass(encoder, view, "separator_pass", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..6, 0..1);
    });
}

/// Draw the pane header strip (background + text).
///
/// `center_title` is an optional string rendered in the center of the
/// header. When `Some`, it replaces the default "pane N" center label
/// (used for Claude session titles / working-dir basenames). The
/// far-left always shows the real daemon `PaneId`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_pane_header(
    pane: &PaneState,
    is_focused: bool,
    is_zoomed: bool,
    center_title: Option<&str>,
    claude_badge: Option<&str>,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    let vp = pane.viewport;
    let header_h = crate::pane::PANE_HEADER_HEIGHT;
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // ── Snapshot all pane status fields under one lock ───────────────────
    let snapshot = HeaderStatusSnapshot::from_pane(pane);

    // ── 1. Header background + context gauge (single render pass) ───────
    draw_pane_header_bg(
        vp, header_h, is_focused, &snapshot, renderer, device, encoder, view, sw, sh,
    );

    // ── 2. Exit-code stripe on the left edge ─────────────────────────────
    if let Some(code) = snapshot.last_exit_code {
        draw_pane_header_exit_stripe(vp, header_h, code, renderer, device, encoder, view, sw, sh);
    }

    // ── 3. Header text ───────────────────────────────────────────────────
    let style = HeaderTextStyle::compute(
        is_focused,
        snapshot.git_state.as_ref(),
        &renderer.chrome_palette,
    );
    let strings = HeaderTextStrings::compute(
        pane.id,
        center_title,
        claude_badge,
        &snapshot.tags,
        snapshot.git_state.as_ref(),
    );
    let layout = HeaderButtonLayout::compute(vp, is_zoomed);
    let slots = HeaderTextSlots::new(pane.id, &strings, vp, is_focused, is_zoomed);

    let font_size = renderer.chrome_font_size((header_h * 0.6).max(9.0));
    let metrics = Metrics::new(font_size, header_h);

    shape_pane_header_text(&strings, &slots, &style, metrics, vp, header_h, renderer);
    draw_pane_header_text(
        vp,
        is_zoomed,
        &strings,
        &slots,
        &style,
        &layout,
        renderer,
        device,
        queue,
        encoder,
        view,
        surface_width,
        surface_height,
    );
}

/// Snapshot of `PaneState::status` fields needed by header rendering.
///
/// Acquires the pane status lock once, copies out everything we need, and
/// releases the lock so the rest of the draw can run without holding it.
struct HeaderStatusSnapshot {
    agent_tokens: Option<u64>,
    agent_model: Option<String>,
    last_exit_code: Option<i32>,
    tags: HashMap<String, String>,
    git_state: Option<crate::git_state::GitState>,
}

impl HeaderStatusSnapshot {
    fn from_pane(pane: &PaneState) -> Self {
        let s = pane.status.lock().unwrap_or_else(|e| e.into_inner());
        Self {
            agent_tokens: s.agent_tokens,
            agent_model: s.agent_model.clone(),
            last_exit_code: s.last_exit_code,
            tags: s.tags.clone(),
            git_state: s.git_state.clone(),
        }
    }
}

/// Draw the header background rect plus the optional tn-nhbv context-window
/// gauge in a single render pass — both fill geometry shares one vertex
/// buffer so the gauge adds zero render passes per pane.
#[allow(clippy::too_many_arguments)]
fn draw_pane_header_bg(
    vp: Rect,
    header_h: f32,
    is_focused: bool,
    snapshot: &HeaderStatusSnapshot,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    sw: f32,
    sh: f32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let bg_color = if is_focused {
        renderer.chrome_palette.header_bg
    } else {
        renderer.chrome_palette.header_bg_dim
    };
    let mut bg_verts: Vec<ColorVertex> =
        pixel_rect_to_ndc(vp.x(), vp.y(), vp.width(), header_h, sw, sh, bg_color).to_vec();

    if let Some(gauge) = compute_context_gauge(snapshot, vp, header_h, is_focused) {
        bg_verts.extend_from_slice(&pixel_rect_to_ndc(
            gauge.x,
            gauge.y,
            gauge.w,
            gauge.h,
            sw,
            sh,
            gauge.color,
        ));
    }

    let bg_vert_count = bg_verts.len() as u32;
    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("header_bg_vbuf"),
        contents: bytemuck::cast_slice(&bg_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    with_chrome_render_pass(encoder, view, "header_bg_pass", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, vertex_buf.slice(..));
        pass.draw(0..bg_vert_count, 0..1);
    });
}

/// Computed geometry + color for the tn-nhbv context-window fill gauge.
struct ContextGauge {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [f32; 4],
}

/// Compute the context gauge rect for the header. Returns `None` when
/// tokens or model are unknown, the model isn't in the registry, or the
/// computed width is zero.
fn compute_context_gauge(
    snapshot: &HeaderStatusSnapshot,
    vp: Rect,
    header_h: f32,
    is_focused: bool,
) -> Option<ContextGauge> {
    let tokens = snapshot.agent_tokens?;
    let model = snapshot.agent_model.as_ref()?;
    let window = crate::model_context::context_window_for_model(model)?;
    let ratio = crate::model_context::fill_ratio(tokens, window)?;
    let alpha = if is_focused { 0.95 } else { 0.70 };
    let palette_color = match crate::model_context::gauge_tier(ratio) {
        crate::model_context::GaugeTier::Green => PaletteColor::STATUS_OK,
        crate::model_context::GaugeTier::Yellow => PaletteColor::STATUS_WARN,
        crate::model_context::GaugeTier::Red => PaletteColor::STATUS_ERROR,
    };
    let color = [
        palette_color.r as f32 / 255.0,
        palette_color.g as f32 / 255.0,
        palette_color.b as f32 / 255.0,
        alpha,
    ];

    let gauge_h = 2.0_f32;
    let clamped = ratio.clamp(0.0, 1.0);
    let gauge_w = (vp.width() * clamped).max(0.0);
    if gauge_w <= 0.0 {
        return None;
    }
    Some(ContextGauge {
        x: vp.x(),
        y: vp.y() + header_h - gauge_h,
        w: gauge_w,
        h: gauge_h,
        color,
    })
}

/// Draw the 4px exit-code stripe on the left edge of the header.
#[allow(clippy::too_many_arguments)]
fn draw_pane_header_exit_stripe(
    vp: Rect,
    header_h: f32,
    exit_code: i32,
    renderer: &GridRenderer,
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    sw: f32,
    sh: f32,
) {
    use crate::color_mapping::pixel_rect_to_ndc;

    let stripe_w = 4.0_f32;
    let stripe_color = if exit_code == 0 {
        renderer.chrome_palette.exit_ok
    } else {
        renderer.chrome_palette.exit_error
    };
    let stripe_verts = pixel_rect_to_ndc(vp.x(), vp.y(), stripe_w, header_h, sw, sh, stripe_color);

    let stripe_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("header_exit_stripe_vbuf"),
        contents: bytemuck::cast_slice(&stripe_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    with_chrome_render_pass(encoder, view, "header_exit_stripe_pass", |pass| {
        pass.set_pipeline(&renderer.rect_pipeline);
        pass.set_vertex_buffer(0, stripe_buf.slice(..));
        pass.draw(0..6, 0..1);
    });
}

/// Glyph colors used by header text. Computed once from `is_focused` and
/// the optional git state and reused for shaping + TextArea defaults.
struct HeaderTextStyle {
    index_color: GlyphColor,
    process_color: GlyphColor,
    git_branch_color: GlyphColor,
    claude_badge_color: GlyphColor,
    close_color: GlyphColor,
    button_color: GlyphColor,
}

impl HeaderTextStyle {
    fn compute(
        is_focused: bool,
        git_state: Option<&crate::git_state::GitState>,
        palette: &ChromePalette,
    ) -> Self {
        // Build a glyph color from a palette text role with an explicit
        // alpha so per-state focus modulation stays in one spot.
        let glyph = |c: PaletteColor, alpha: u8| GlyphColor::rgba(c.r, c.g, c.b, alpha);

        let index_color = glyph(palette.chrome_fg_muted, if is_focused { 255 } else { 200 });
        let process_color = if is_focused {
            glyph(palette.chrome_fg, 255)
        } else {
            glyph(palette.chrome_fg_muted, 220)
        };
        let git_branch_color = match git_state {
            Some(gs) if gs.detached => {
                glyph(palette.chrome_fg_warn, if is_focused { 230 } else { 170 })
            }
            Some(gs) if crate::git_state::is_default_branch(&gs.branch) => {
                glyph(palette.chrome_fg_muted, if is_focused { 220 } else { 160 })
            }
            Some(_) => glyph(palette.chrome_fg_focus, if is_focused { 230 } else { 170 }),
            None => index_color,
        };
        let claude_badge_color = glyph(palette.chrome_fg_focus, if is_focused { 230 } else { 170 });
        let close_color = glyph(palette.chrome_fg_alert, if is_focused { 230 } else { 160 });
        let button_color = glyph(palette.chrome_fg_muted, if is_focused { 230 } else { 170 });
        Self {
            index_color,
            process_color,
            git_branch_color,
            claude_badge_color,
            close_color,
            button_color,
        }
    }
}

/// Pre-formatted strings shown in the header. Empty strings mean the
/// section is skipped entirely.
struct HeaderTextStrings {
    index_text: String,
    process_text: String,
    badge_text: String,
    git_branch_text: String,
    claude_badge_text: String,
}

impl HeaderTextStrings {
    fn compute(
        pane_id: PaneId,
        center_title: Option<&str>,
        claude_badge: Option<&str>,
        tags: &HashMap<String, String>,
        git_state: Option<&crate::git_state::GitState>,
    ) -> Self {
        let index_text = format!(" {pane_id}");
        let process_text = match center_title {
            Some(title) => title.to_string(),
            None => format!("pane {pane_id}"),
        };
        let badge_text = format_tag_badges(tags);
        let git_branch_text = git_state
            .map(crate::git_state::format_for_header)
            .unwrap_or_default();
        let claude_badge_text = claude_badge.unwrap_or("").to_string();
        Self {
            index_text,
            process_text,
            badge_text,
            git_branch_text,
            claude_badge_text,
        }
    }
}

/// Slot/key identifiers for the chrome text cache. One slot per text
/// element so the cache can keep its `Buffer` warm across frames.
struct HeaderTextSlots {
    idx_slot: String,
    idx_key: String,
    proc_slot: String,
    proc_key: String,
    badge_slot: String,
    badge_key: String,
    git_slot: String,
    git_key: String,
    claude_slot: String,
    claude_key: String,
    close_slot: String,
    close_key: String,
    zoom_slot: String,
    zoom_key: String,
    zoom_label: &'static str,
    vsplit_slot: String,
    vsplit_key: String,
    hsplit_slot: String,
    hsplit_key: String,
}

impl HeaderTextSlots {
    fn new(
        pane_id: PaneId,
        strings: &HeaderTextStrings,
        vp: Rect,
        is_focused: bool,
        is_zoomed: bool,
    ) -> Self {
        let focus_tag = if is_focused { "f" } else { "u" };
        let zoom_tag = if is_zoomed { "z" } else { "n" };
        // Filled square when zoomed (restore), empty square when normal (maximize).
        let zoom_label: &'static str = if is_zoomed { " \u{25a3}" } else { " \u{25a1}" };
        Self {
            idx_slot: format!("hdr_idx_{pane_id}"),
            idx_key: format!("{}|{:.0}|{focus_tag}", strings.index_text, vp.width()),
            proc_slot: format!("hdr_proc_{pane_id}"),
            proc_key: format!("{}|{:.0}|{focus_tag}", strings.process_text, vp.width()),
            badge_slot: format!("hdr_badge_{pane_id}"),
            badge_key: format!("{}|{:.0}|{focus_tag}", strings.badge_text, vp.width()),
            git_slot: format!("hdr_git_{pane_id}"),
            git_key: format!("{}|{:.0}|{focus_tag}", strings.git_branch_text, vp.width()),
            claude_slot: format!("hdr_claude_{pane_id}"),
            claude_key: format!(
                "{}|{:.0}|{focus_tag}",
                strings.claude_badge_text,
                vp.width()
            ),
            close_slot: format!("hdr_close_{pane_id}"),
            close_key: format!("X|{focus_tag}"),
            zoom_slot: format!("hdr_zoom_{pane_id}"),
            zoom_key: format!("{zoom_label}|{focus_tag}|{zoom_tag}"),
            zoom_label,
            vsplit_slot: format!("hdr_vsplit_{pane_id}"),
            vsplit_key: format!("V|{focus_tag}"),
            hsplit_slot: format!("hdr_hsplit_{pane_id}"),
            hsplit_key: format!("H|{focus_tag}"),
        }
    }
}

/// X positions of the four header buttons (right-to-left).
struct HeaderButtonLayout {
    btn_x_close: f32,
    btn_x_zoom: f32,
    btn_x_vsplit: f32,
    btn_x_hsplit: f32,
    /// Leftmost button x — used to anchor the right edge of the git branch
    /// text. When zoomed, the split buttons are hidden so this matches
    /// `btn_x_zoom`; otherwise it matches `btn_x_hsplit`.
    leftmost_btn_x: f32,
}

impl HeaderButtonLayout {
    fn compute(vp: Rect, is_zoomed: bool) -> Self {
        let btn_x_close = vp.x() + vp.width() - HEADER_BUTTON_MARGIN - HEADER_BUTTON_WIDTH;
        let btn_x_zoom = btn_x_close - HEADER_BUTTON_WIDTH;
        let btn_x_vsplit = btn_x_zoom - HEADER_BUTTON_WIDTH;
        let btn_x_hsplit = btn_x_vsplit - HEADER_BUTTON_WIDTH;
        let leftmost_btn_x = if is_zoomed { btn_x_zoom } else { btn_x_hsplit };
        Self {
            btn_x_close,
            btn_x_zoom,
            btn_x_vsplit,
            btn_x_hsplit,
            leftmost_btn_x,
        }
    }
}

/// Phase 1: shape every header text buffer that may end up rendered.
/// Mutates `renderer.font_system` / `renderer.overlay_cache` so the buffers
/// are warm for the immutable Phase 2.
fn shape_pane_header_text(
    strings: &HeaderTextStrings,
    slots: &HeaderTextSlots,
    style: &HeaderTextStyle,
    metrics: Metrics,
    vp: Rect,
    header_h: f32,
    renderer: &mut GridRenderer,
) {
    let family = renderer.font_config.chrome_font_family().to_string();
    let attrs =
        |c: GlyphColor| -> Attrs<'_> { Attrs::new().family(Family::Name(&family)).color(c) };

    ensure_shaped(
        &slots.idx_slot,
        &slots.idx_key,
        metrics,
        vp.width(),
        header_h,
        &strings.index_text,
        attrs(style.index_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        &slots.proc_slot,
        &slots.proc_key,
        metrics,
        vp.width(),
        header_h,
        &strings.process_text,
        attrs(style.process_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    if !strings.badge_text.is_empty() {
        ensure_shaped(
            &slots.badge_slot,
            &slots.badge_key,
            metrics,
            vp.width(),
            header_h,
            &strings.badge_text,
            attrs(style.index_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }
    if !strings.git_branch_text.is_empty() {
        ensure_shaped(
            &slots.git_slot,
            &slots.git_key,
            metrics,
            vp.width(),
            header_h,
            &strings.git_branch_text,
            attrs(style.git_branch_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }
    if !strings.claude_badge_text.is_empty() {
        ensure_shaped(
            &slots.claude_slot,
            &slots.claude_key,
            metrics,
            vp.width(),
            header_h,
            &strings.claude_badge_text,
            attrs(style.claude_badge_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }
    ensure_shaped(
        &slots.close_slot,
        &slots.close_key,
        metrics,
        HEADER_BUTTON_WIDTH,
        header_h,
        " X",
        attrs(style.close_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    ensure_shaped(
        &slots.zoom_slot,
        &slots.zoom_key,
        metrics,
        HEADER_BUTTON_WIDTH,
        header_h,
        slots.zoom_label,
        attrs(style.button_color),
        &mut renderer.font_system,
        &mut renderer.overlay_cache,
    );
    // Split buttons disappear when the pane is zoomed (no room to split).
    let is_zoomed = slots.zoom_label == " \u{25a3}";
    if !is_zoomed {
        ensure_shaped(
            &slots.vsplit_slot,
            &slots.vsplit_key,
            metrics,
            HEADER_BUTTON_WIDTH,
            header_h,
            " V",
            attrs(style.button_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
        ensure_shaped(
            &slots.hsplit_slot,
            &slots.hsplit_key,
            metrics,
            HEADER_BUTTON_WIDTH,
            header_h,
            " H",
            attrs(style.button_color),
            &mut renderer.font_system,
            &mut renderer.overlay_cache,
        );
    }
}

/// Phase 2: build TextArea references against the cached buffers, prepare
/// glyphon, and emit the header text render pass.
#[allow(clippy::too_many_arguments)]
fn draw_pane_header_text(
    vp: Rect,
    is_zoomed: bool,
    strings: &HeaderTextStrings,
    slots: &HeaderTextSlots,
    style: &HeaderTextStyle,
    layout: &HeaderButtonLayout,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let bounds = TextBounds {
        left: 0,
        top: 0,
        right: surface_width as i32,
        bottom: surface_height as i32,
    };

    let text_areas = match build_header_text_areas(
        vp,
        is_zoomed,
        strings,
        slots,
        style,
        layout,
        bounds,
        &renderer.overlay_cache,
    ) {
        Some(areas) => areas,
        None => return,
    };

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("header text prepare failed: {}", e);
    }

    with_chrome_render_pass(encoder, view, "header_text_pass", |pass| {
        if let Err(e) =
            renderer
                .overlay_text_renderer
                .render(&renderer.overlay_atlas, &renderer.viewport, pass)
        {
            tracing::warn!("header text render failed: {}", e);
        }
    });
}

/// Build the TextArea list for the pane header. Returns `None` when one of
/// the required (index/process) text buffers is missing — the caller
/// should skip the entire text render pass in that case so the warning
/// matches the pre-refactor behavior.
#[allow(clippy::too_many_arguments)]
fn build_header_text_areas<'cache>(
    vp: Rect,
    is_zoomed: bool,
    strings: &HeaderTextStrings,
    slots: &HeaderTextSlots,
    style: &HeaderTextStyle,
    layout: &HeaderButtonLayout,
    bounds: TextBounds,
    cache: &'cache super::text_cache::ChromeTextCache,
) -> Option<Vec<TextArea<'cache>>> {
    let index_buf = cached_buf(cache, &slots.idx_slot).or_else(|| {
        tracing::warn!("pane header: index buffer slot missing; skipping header draw");
        None
    })?;
    let process_buf = cached_buf(cache, &slots.proc_slot).or_else(|| {
        tracing::warn!("pane header: process buffer slot missing; skipping header draw");
        None
    })?;

    let process_text_width = buffer_run_width(process_buf);
    let center_offset = ((vp.width() - process_text_width) / 2.0).max(0.0);

    let mut text_areas: Vec<TextArea<'_>> = Vec::with_capacity(8);
    text_areas.push(TextArea {
        buffer: index_buf,
        left: vp.x(),
        top: vp.y(),
        scale: 1.0,
        bounds,
        default_color: style.index_color,
        custom_glyphs: &[],
    });
    text_areas.push(TextArea {
        buffer: process_buf,
        left: vp.x() + center_offset,
        top: vp.y(),
        scale: 1.0,
        bounds,
        default_color: style.process_color,
        custom_glyphs: &[],
    });

    // Right edge of the badge cluster — the Claude badge chains off it.
    let mut badge_right = vp.x() + center_offset + process_text_width;
    if !strings.badge_text.is_empty()
        && let Some(badge_buf) = cached_buf(cache, &slots.badge_slot)
    {
        let badge_left = badge_right + 8.0;
        let badge_w = buffer_run_width(badge_buf);
        badge_right = badge_left + badge_w;
        text_areas.push(TextArea {
            buffer: badge_buf,
            left: badge_left,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: style.index_color,
            custom_glyphs: &[],
        });
    }
    if !strings.claude_badge_text.is_empty()
        && let Some(claude_buf) = cached_buf(cache, &slots.claude_slot)
    {
        text_areas.push(TextArea {
            buffer: claude_buf,
            left: badge_right + 8.0,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: style.claude_badge_color,
            custom_glyphs: &[],
        });
    }
    if !strings.git_branch_text.is_empty()
        && let Some(git_buf) = cached_buf(cache, &slots.git_slot)
    {
        let git_text_width = buffer_run_width(git_buf);
        let git_left = (layout.leftmost_btn_x - git_text_width - 8.0).max(vp.x());
        text_areas.push(TextArea {
            buffer: git_buf,
            left: git_left,
            top: vp.y(),
            scale: 1.0,
            bounds,
            default_color: style.git_branch_color,
            custom_glyphs: &[],
        });
    }
    if !is_zoomed {
        push_button_area(
            &mut text_areas,
            cache,
            &slots.hsplit_slot,
            layout.btn_x_hsplit,
            vp.y(),
            bounds,
            style.button_color,
        );
        push_button_area(
            &mut text_areas,
            cache,
            &slots.vsplit_slot,
            layout.btn_x_vsplit,
            vp.y(),
            bounds,
            style.button_color,
        );
    }
    push_button_area(
        &mut text_areas,
        cache,
        &slots.zoom_slot,
        layout.btn_x_zoom,
        vp.y(),
        bounds,
        style.button_color,
    );
    push_button_area(
        &mut text_areas,
        cache,
        &slots.close_slot,
        layout.btn_x_close,
        vp.y(),
        bounds,
        style.close_color,
    );

    Some(text_areas)
}

/// Push a TextArea for one of the right-side header buttons, skipping
/// silently if the cached buffer is missing.
fn push_button_area<'cache>(
    text_areas: &mut Vec<TextArea<'cache>>,
    cache: &'cache super::text_cache::ChromeTextCache,
    slot: &str,
    left: f32,
    top: f32,
    bounds: TextBounds,
    color: GlyphColor,
) {
    if let Some(buf) = cached_buf(cache, slot) {
        text_areas.push(TextArea {
            buffer: buf,
            left,
            top,
            scale: 1.0,
            bounds,
            default_color: color,
            custom_glyphs: &[],
        });
    }
}

/// Sum the glyph advance widths of the first layout run in `buf`.
fn buffer_run_width(buf: &glyphon::Buffer) -> f32 {
    buf.layout_runs()
        .next()
        .map(|run| run.glyphs.iter().map(|g| g.w).sum::<f32>())
        .unwrap_or(0.0)
}

// ── Tag badge formatting (tn-166y) ────────────────────────────────────

const VALUE_ONLY_KEYS: &[&str] = &["issue_id", "branch", "worker"];
const MAX_VISIBLE_BADGES: usize = 3;

fn format_tag_badges(tags: &HashMap<String, String>) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut keys: Vec<&String> = tags.keys().collect();
    keys.sort();
    let total = keys.len();
    let mut parts: Vec<String> = Vec::with_capacity(MAX_VISIBLE_BADGES + 1);
    for key in keys.iter().take(MAX_VISIBLE_BADGES) {
        let value = &tags[key.as_str()];
        if VALUE_ONLY_KEYS.contains(&key.as_str()) {
            parts.push(value.clone());
        } else {
            parts.push(format!("{key}={value}"));
        }
    }
    if total > MAX_VISIBLE_BADGES {
        parts.push("\u{2026}".to_string());
    }
    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tag_badges_empty() {
        assert_eq!(format_tag_badges(&HashMap::new()), "");
    }

    #[test]
    fn format_tag_badges_well_known_value_only() {
        let mut t = HashMap::new();
        t.insert("issue_id".into(), "tn-abc".into());
        t.insert("branch".into(), "feat/foo".into());
        let r = format_tag_badges(&t);
        assert!(r.contains("feat/foo") && r.contains("tn-abc"));
        assert!(!r.contains("issue_id=") && !r.contains("branch="));
    }

    #[test]
    fn format_tag_badges_custom_key_value() {
        let mut t = HashMap::new();
        t.insert("env".into(), "prod".into());
        assert_eq!(format_tag_badges(&t), "env=prod");
    }

    #[test]
    fn format_tag_badges_overflow() {
        let mut t = HashMap::new();
        for c in &["a", "b", "c", "d"] {
            t.insert(c.to_string(), "x".into());
        }
        let r = format_tag_badges(&t);
        assert!(r.contains('\u{2026}'));
        assert!(r.contains("a=x") && r.contains("b=x") && r.contains("c=x"));
        assert!(!r.contains("d=x"));
    }

    #[test]
    fn format_tag_badges_deterministic() {
        let mut t = HashMap::new();
        t.insert("worker".into(), "alice".into());
        t.insert("branch".into(), "main".into());
        t.insert("issue_id".into(), "tn-1".into());
        assert_eq!(format_tag_badges(&t), "main  tn-1  alice");
    }
}
