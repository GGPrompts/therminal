//! Trust escalation modal overlay (tn-b99).
//!
//! Renders a centered modal asking the user to approve or deny a trust
//! tier escalation from an MCP agent. Styled like browser permission prompts.
//! Keyboard navigation: Enter to approve, Escape to deny.

use crate::color_mapping::pixel_rect_to_ndc;
use crate::grid_renderer::{ColorVertex, GridRenderer};
use therminal_core::palette::Color as PaletteColor;
use wgpu::util::DeviceExt;

// ── Colors ──────────────────────────────────────────────────────────────

const SCRIM_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.65];
const PANEL_BG: [f32; 4] = {
    let c = PaletteColor::PLATE;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.97,
    ]
};
const APPROVE_BG: [f32; 4] = [0.18, 0.55, 0.34, 1.0]; // green
const DENY_BG: [f32; 4] = [0.65, 0.22, 0.22, 1.0]; // red
const FOCUSED_BORDER: [f32; 4] = [1.0, 1.0, 1.0, 0.9];

// ── State ───────────────────────────────────────────────────────────────

/// Pending trust escalation displayed as a modal.
#[derive(Debug, Clone)]
pub(crate) struct TrustEscalationState {
    /// Opaque escalation ID for correlating the IPC response.
    pub escalation_id: u64,
    /// Agent name requesting the tool.
    pub agent_name: String,
    /// The MCP tool being requested.
    pub tool_name: String,
    /// The agent's current tier (display string).
    pub current_tier: String,
    /// The tier required by the tool (display string).
    pub required_tier: String,
    /// Which button is focused (true = Approve, false = Deny).
    pub approve_focused: bool,
}

impl TrustEscalationState {
    /// Toggle focus between Approve and Deny.
    pub fn toggle_focus(&mut self) {
        self.approve_focused = !self.approve_focused;
    }
}

// ── Renderer ────────────────────────────────────────────────────────────

/// Draw the trust escalation modal centered in the window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_trust_escalation_overlay(
    state: &TrustEscalationState,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
) {
    use glyphon::{
        Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea,
        TextBounds, Weight,
    };

    let sw = surface_width as f32;
    let sh = surface_height as f32;

    // ── Layout constants ────────────────────────────────────────────────
    let row_h = 24.0_f32;
    let padding_h = 28.0_f32;
    let padding_v = 24.0_f32;
    let title_h = 32.0;
    let detail_rows = 4; // agent, tool, current tier, required tier
    let button_h = 36.0_f32;
    let button_gap = 16.0_f32;
    let section_gap = 16.0_f32;

    let content_h = title_h + section_gap + (detail_rows as f32 * row_h) + section_gap + button_h;
    let panel_w = 420.0_f32.min(sw - 40.0);
    let panel_h = content_h + padding_v * 2.0;
    let panel_x = (sw - panel_w) / 2.0;
    let panel_y = (sh - panel_h) / 2.0;

    // ── Background quads ────────────────────────────────────────────────
    let scrim_verts = pixel_rect_to_ndc(0.0, 0.0, sw, sh, sw, sh, SCRIM_COLOR);
    let panel_verts = pixel_rect_to_ndc(panel_x, panel_y, panel_w, panel_h, sw, sh, PANEL_BG);

    // Buttons
    let btn_w = (panel_w - padding_h * 2.0 - button_gap) / 2.0;
    let btn_y =
        panel_y + padding_v + title_h + section_gap + (detail_rows as f32 * row_h) + section_gap;
    let approve_x = panel_x + padding_h;
    let deny_x = approve_x + btn_w + button_gap;

    let approve_verts = pixel_rect_to_ndc(approve_x, btn_y, btn_w, button_h, sw, sh, APPROVE_BG);
    let deny_verts = pixel_rect_to_ndc(deny_x, btn_y, btn_w, button_h, sw, sh, DENY_BG);

    // Focus border
    let border_w = 2.0_f32;
    let (fx, fy) = if state.approve_focused {
        (approve_x, btn_y)
    } else {
        (deny_x, btn_y)
    };
    let border_top = pixel_rect_to_ndc(
        fx - border_w,
        fy - border_w,
        btn_w + border_w * 2.0,
        border_w,
        sw,
        sh,
        FOCUSED_BORDER,
    );
    let border_bottom = pixel_rect_to_ndc(
        fx - border_w,
        fy + button_h,
        btn_w + border_w * 2.0,
        border_w,
        sw,
        sh,
        FOCUSED_BORDER,
    );
    let border_left = pixel_rect_to_ndc(
        fx - border_w,
        fy,
        border_w,
        button_h,
        sw,
        sh,
        FOCUSED_BORDER,
    );
    let border_right =
        pixel_rect_to_ndc(fx + btn_w, fy, border_w, button_h, sw, sh, FOCUSED_BORDER);

    let mut all_verts: Vec<ColorVertex> = Vec::new();
    all_verts.extend_from_slice(&scrim_verts);
    all_verts.extend_from_slice(&panel_verts);
    all_verts.extend_from_slice(&approve_verts);
    all_verts.extend_from_slice(&deny_verts);
    all_verts.extend_from_slice(&border_top);
    all_verts.extend_from_slice(&border_bottom);
    all_verts.extend_from_slice(&border_left);
    all_verts.extend_from_slice(&border_right);

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("trust_escalation_bg_vbuf"),
        contents: bytemuck::cast_slice(&all_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("trust_escalation_encoder"),
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("trust_escalation_bg_pass"),
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
        pass.draw(0..all_verts.len() as u32, 0..1);
    }

    // ── Text content ────────────────────────────────────────────────────
    let font_size = 14.0_f32;
    let metrics = Metrics::new(font_size, row_h);
    let title_metrics = Metrics::new(font_size + 4.0, title_h);

    let text_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        240,
    );
    let muted_color = GlyphColor::rgba(
        PaletteColor::INK_MUTED.r,
        PaletteColor::INK_MUTED.g,
        PaletteColor::INK_MUTED.b,
        220,
    );
    let white = GlyphColor::rgba(255, 255, 255, 255);

    let inner_left = panel_x + padding_h;
    let inner_right = panel_x + panel_w - padding_h;
    let inner_w = inner_right - inner_left;

    let bounds = TextBounds {
        left: inner_left as i32,
        top: panel_y as i32,
        right: inner_right as i32,
        bottom: (panel_y + panel_h) as i32,
    };

    let mut text_areas: Vec<TextArea<'_>> = Vec::new();
    let mut buffers: Vec<Buffer> = Vec::new();
    let mut _current_y = panel_y + padding_v;

    // Title
    let mut title_buf = Buffer::new(&mut renderer.font_system, title_metrics);
    title_buf.set_size(&mut renderer.font_system, Some(inner_w), Some(title_h));
    title_buf.set_text(
        &mut renderer.font_system,
        "Trust Escalation Required",
        &Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .weight(Weight::BOLD)
            .color(text_color),
        Shaping::Basic,
        None,
    );
    title_buf.shape_until_scroll(&mut renderer.font_system, false);
    buffers.push(title_buf);
    _current_y += title_h + section_gap;

    // Detail rows
    let details = [
        ("Agent:", state.agent_name.as_str()),
        ("Tool:", state.tool_name.as_str()),
        ("Current tier:", state.current_tier.as_str()),
        ("Required tier:", state.required_tier.as_str()),
    ];

    for (label, value) in &details {
        let line = format!("{label}  {value}");
        let mut buf = Buffer::new(&mut renderer.font_system, metrics);
        buf.set_size(&mut renderer.font_system, Some(inner_w), Some(row_h));
        buf.set_text(
            &mut renderer.font_system,
            &line,
            &Attrs::new()
                .family(Family::Name(&renderer.font_config.family))
                .color(muted_color),
            Shaping::Basic,
            None,
        );
        buf.shape_until_scroll(&mut renderer.font_system, false);
        buffers.push(buf);
        _current_y += row_h;
    }
    _current_y += section_gap;

    // Button labels
    let btn_metrics = Metrics::new(font_size + 1.0, button_h);
    let btn_label_w = btn_w - 8.0;

    let mut approve_buf = Buffer::new(&mut renderer.font_system, btn_metrics);
    approve_buf.set_size(&mut renderer.font_system, Some(btn_label_w), Some(button_h));
    approve_buf.set_text(
        &mut renderer.font_system,
        "Approve (Enter)",
        &Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .weight(Weight::BOLD)
            .color(white),
        Shaping::Basic,
        None,
    );
    approve_buf.shape_until_scroll(&mut renderer.font_system, false);
    buffers.push(approve_buf);

    let mut deny_buf = Buffer::new(&mut renderer.font_system, btn_metrics);
    deny_buf.set_size(&mut renderer.font_system, Some(btn_label_w), Some(button_h));
    deny_buf.set_text(
        &mut renderer.font_system,
        "Deny (Esc)",
        &Attrs::new()
            .family(Family::Name(&renderer.font_config.family))
            .weight(Weight::BOLD)
            .color(white),
        Shaping::Basic,
        None,
    );
    deny_buf.shape_until_scroll(&mut renderer.font_system, false);
    buffers.push(deny_buf);

    // Build text areas: title, 4 detail rows, 2 button labels
    let mut y = panel_y + padding_v;
    // Title (index 0)
    text_areas.push(TextArea {
        buffer: &buffers[0],
        left: inner_left,
        top: y,
        scale: 1.0,
        bounds,
        default_color: text_color,
        custom_glyphs: &[],
    });
    y += title_h + section_gap;

    // Detail rows (indices 1..5)
    for i in 0..detail_rows {
        text_areas.push(TextArea {
            buffer: &buffers[1 + i],
            left: inner_left,
            top: y,
            scale: 1.0,
            bounds,
            default_color: muted_color,
            custom_glyphs: &[],
        });
        y += row_h;
    }
    y += section_gap;

    // Approve button label (index 5)
    text_areas.push(TextArea {
        buffer: &buffers[5],
        left: approve_x + 4.0,
        top: y,
        scale: 1.0,
        bounds,
        default_color: white,
        custom_glyphs: &[],
    });

    // Deny button label (index 6)
    text_areas.push(TextArea {
        buffer: &buffers[6],
        left: deny_x + 4.0,
        top: y,
        scale: 1.0,
        bounds,
        default_color: white,
        custom_glyphs: &[],
    });

    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
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
        tracing::warn!("trust escalation text prepare failed: {e}");
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("trust_escalation_text_pass"),
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
            tracing::warn!("trust escalation text render failed: {e}");
        }
    }

    queue.submit(std::iter::once(encoder.finish()));
}
