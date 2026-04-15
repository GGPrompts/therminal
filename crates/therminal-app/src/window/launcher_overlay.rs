//! Launcher overlay: a centered panel showing shell profiles as a grid of
//! tiles with Nerd Font icons.
//!
//! Triggered by `KeyAction::ShowLauncher` (Ctrl+Shift+L). Each profile gets
//! a colored tile with a large icon glyph and the profile name below it.
//! A "Default Shell" tile is always prepended. Arrow keys navigate, Enter
//! spawns a pane with the selected profile, Esc dismisses.

use std::collections::HashMap;

use wgpu::util::DeviceExt;

use crate::color_mapping::pixel_rect_to_ndc;
use crate::grid_renderer::{ColorVertex, GridRenderer};
use therminal_core::config::ProfileConfig;
use therminal_core::palette::Color as PaletteColor;

use glyphon::{
    Attrs, Buffer, Color as GlyphColor, Family, Metrics, Resolution, Shaping, TextArea, TextBounds,
    Weight,
};

// ── Constants ────────────────────────────────────────────────────────────

/// Semi-transparent dark background for the overlay scrim.
const SCRIM_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.6];

/// Panel background (PLATE from palette with high alpha).
const PANEL_BG_COLOR: [f32; 4] = {
    let c = PaletteColor::PLATE;
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        0.95,
    ]
};

/// Tile dimensions.
const TILE_W: f32 = 120.0;
const TILE_H: f32 = 100.0;
const TILE_GAP: f32 = 16.0;
const TILE_CORNER_INSET: f32 = 3.0;

/// Bevel edge thickness for the 3D look.
const BEVEL_PX: f32 = 2.0;

/// Default tile color when the profile has no color specified.
const DEFAULT_TILE_COLOR: [f32; 4] = [0.25, 0.30, 0.40, 0.85];

/// Default Nerd Font terminal icon (nf-cod-terminal).
const DEFAULT_ICON: &str = "\u{f489}";

// ── State ────────────────────────────────────────────────────────────────

/// An entry in the launcher tile grid.
#[derive(Debug, Clone)]
pub(crate) struct LauncherEntry {
    /// Display name.
    pub name: String,
    /// Nerd Font glyph string.
    pub icon: String,
    /// Tile background RGBA color.
    pub color: [f32; 4],
    /// Profile name key (None = default shell).
    pub profile_name: Option<String>,
}

/// Launcher overlay state persisted across frames.
#[derive(Debug, Clone, Default)]
pub(crate) struct LauncherState {
    /// Currently selected tile index.
    pub selected: usize,
    /// Cached tile list (rebuilt each time the overlay opens).
    pub entries: Vec<LauncherEntry>,
    /// Number of columns in the current layout (set by renderer).
    pub cols: usize,
}

impl LauncherState {
    /// Build the tile list from profiles config. Prepends a "Default Shell" tile.
    pub fn build_entries(&mut self, profiles: &HashMap<String, ProfileConfig>) {
        self.entries.clear();

        // Default shell tile.
        self.entries.push(LauncherEntry {
            name: "Default Shell".to_string(),
            icon: DEFAULT_ICON.to_string(),
            color: DEFAULT_TILE_COLOR,
            profile_name: None,
        });

        // Sorted profile entries for deterministic ordering.
        let mut profile_names: Vec<&String> = profiles.keys().collect();
        profile_names.sort();

        for name in profile_names {
            let profile = &profiles[name];
            let icon = profile
                .icon
                .clone()
                .unwrap_or_else(|| DEFAULT_ICON.to_string());
            let color = profile
                .color
                .as_deref()
                .and_then(parse_hex_color)
                .unwrap_or(DEFAULT_TILE_COLOR);
            self.entries.push(LauncherEntry {
                name: name.clone(),
                icon,
                color,
                profile_name: Some(name.clone()),
            });
        }

        self.selected = 0;
    }

    /// Move selection by arrow key. `dx`/`dy` are -1, 0, or 1.
    pub fn navigate(&mut self, dx: i32, dy: i32) {
        if self.entries.is_empty() || self.cols == 0 {
            return;
        }
        let count = self.entries.len();
        let row = self.selected / self.cols;
        let col = self.selected % self.cols;

        let new_col = (col as i32 + dx).clamp(0, self.cols as i32 - 1) as usize;
        let new_row = (row as i32 + dy).max(0) as usize;
        let new_idx = new_row * self.cols + new_col;
        if new_idx < count {
            self.selected = new_idx;
        }
    }

    /// Return the selected entry, if any.
    pub fn selected_entry(&self) -> Option<&LauncherEntry> {
        self.entries.get(self.selected)
    }
}

// ── Color parsing ────────────────────────────────────────────────────────

/// Parse a `#RRGGBB` or `#RGB` hex string to an `[f32; 4]` RGBA color.
fn parse_hex_color(hex: &str) -> Option<[f32; 4]> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 0.85])
        }
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            Some([r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 0.85])
        }
        _ => None,
    }
}

// ── Draw the launcher overlay ────────────────────────────────────────────

/// Draw the launcher overlay centered in the window.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_launcher_overlay(
    state: &LauncherState,
    renderer: &mut GridRenderer,
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    encoder: &mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    surface_width: u32,
    surface_height: u32,
    focus_border_color: [f32; 4],
) {
    let sw = surface_width as f32;
    let sh = surface_height as f32;

    if state.entries.is_empty() {
        return;
    }

    // ── Compute grid layout ────────────────────────────────────────────
    let padding_h = 32.0_f32;
    let padding_v = 28.0_f32;
    let title_h = 36.0_f32;
    let title_gap = 12.0_f32;

    let max_panel_w = (sw * 0.8).min(800.0);
    let usable_w = max_panel_w - padding_h * 2.0;
    let cols = ((usable_w + TILE_GAP) / (TILE_W + TILE_GAP))
        .floor()
        .max(1.0) as usize;
    let cols = cols.min(state.entries.len());
    let rows = state.entries.len().div_ceil(cols);

    let grid_w = cols as f32 * TILE_W + (cols as f32 - 1.0).max(0.0) * TILE_GAP;
    let grid_h = rows as f32 * TILE_H + (rows as f32 - 1.0).max(0.0) * TILE_GAP;

    let panel_w = (grid_w + padding_h * 2.0).min(sw - 20.0);
    let panel_h = (title_h + title_gap + grid_h + padding_v * 2.0).min(sh - 20.0);

    let panel_x = (sw - panel_w) / 2.0;
    let panel_y = (sh - panel_h) / 2.0;

    let grid_x = panel_x + (panel_w - grid_w) / 2.0;
    let grid_y = panel_y + padding_v + title_h + title_gap;

    // ── Build vertex buffer ────────────────────────────────────────────
    let mut all_verts: Vec<ColorVertex> = Vec::new();

    // Full-screen scrim.
    all_verts.extend_from_slice(&pixel_rect_to_ndc(0.0, 0.0, sw, sh, sw, sh, SCRIM_COLOR));

    // Panel background.
    all_verts.extend_from_slice(&pixel_rect_to_ndc(
        panel_x,
        panel_y,
        panel_w,
        panel_h,
        sw,
        sh,
        PANEL_BG_COLOR,
    ));

    // ── Tiles ──────────────────────────────────────────────────────────
    for (idx, entry) in state.entries.iter().enumerate() {
        let col = idx % cols;
        let row = idx / cols;
        let tx = grid_x + col as f32 * (TILE_W + TILE_GAP);
        let ty = grid_y + row as f32 * (TILE_H + TILE_GAP);

        let is_selected = idx == state.selected;

        // Focus highlight (drawn first, behind tile so it acts as border).
        if is_selected {
            let border = 3.0_f32;
            all_verts.extend_from_slice(&pixel_rect_to_ndc(
                tx - border,
                ty - border,
                TILE_W + border * 2.0,
                TILE_H + border * 2.0,
                sw,
                sh,
                focus_border_color,
            ));
        }

        // Bevel: lighter top edge.
        let top_edge = lighten(entry.color, 0.15);
        all_verts.extend_from_slice(&pixel_rect_to_ndc(
            tx + TILE_CORNER_INSET,
            ty,
            TILE_W - TILE_CORNER_INSET * 2.0,
            BEVEL_PX,
            sw,
            sh,
            top_edge,
        ));

        // Bevel: darker bottom edge.
        let bottom_edge = darken(entry.color, 0.15);
        all_verts.extend_from_slice(&pixel_rect_to_ndc(
            tx + TILE_CORNER_INSET,
            ty + TILE_H - BEVEL_PX,
            TILE_W - TILE_CORNER_INSET * 2.0,
            BEVEL_PX,
            sw,
            sh,
            bottom_edge,
        ));

        // Main tile body.
        all_verts.extend_from_slice(&pixel_rect_to_ndc(
            tx,
            ty,
            TILE_W,
            TILE_H,
            sw,
            sh,
            entry.color,
        ));
    }

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("launcher_overlay_bg_vbuf"),
        contents: bytemuck::cast_slice(&all_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("launcher_overlay_bg_pass"),
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

    // ── Text content ───────────────────────────────────────────────────
    let text_color = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        240,
    );
    let accent_color = GlyphColor::rgba(
        PaletteColor::FOCUS.r,
        PaletteColor::FOCUS.g,
        PaletteColor::FOCUS.b,
        255,
    );

    let panel_bounds = TextBounds {
        left: panel_x as i32,
        top: panel_y as i32,
        right: (panel_x + panel_w) as i32,
        bottom: (panel_y + panel_h) as i32,
    };

    let mut buffers: Vec<Buffer> = Vec::new();
    struct RowInfo {
        buf_idx: usize,
        x: f32,
        y: f32,
        color: GlyphColor,
    }
    let mut rows_info: Vec<RowInfo> = Vec::new();

    // Title.
    let title_metrics = Metrics::new(18.0, 28.0);
    let mut title_buf = Buffer::new(&mut renderer.font_system, title_metrics);
    title_buf.set_size(
        &mut renderer.font_system,
        Some(panel_w - padding_h * 2.0),
        Some(28.0),
    );
    title_buf.set_text(
        &mut renderer.font_system,
        "Launcher",
        &Attrs::new()
            .family(Family::Name(renderer.font_config.chrome_font_family()))
            .weight(Weight::BOLD)
            .color(text_color),
        Shaping::Basic,
        None,
    );
    title_buf.shape_until_scroll(&mut renderer.font_system, false);
    buffers.push(title_buf);
    rows_info.push(RowInfo {
        buf_idx: 0,
        x: panel_x + padding_h,
        y: panel_y + padding_v,
        color: text_color,
    });

    // Per-tile icon + label text.
    let icon_metrics = Metrics::new(32.0, 36.0);
    let label_metrics = Metrics::new(12.0, 16.0);

    for (idx, entry) in state.entries.iter().enumerate() {
        let col = idx % cols;
        let row = idx / cols;
        let tx = grid_x + col as f32 * (TILE_W + TILE_GAP);
        let ty = grid_y + row as f32 * (TILE_H + TILE_GAP);

        // Icon glyph (centered in tile, upper portion).
        let mut icon_buf = Buffer::new(&mut renderer.font_system, icon_metrics);
        icon_buf.set_size(&mut renderer.font_system, Some(TILE_W), Some(36.0));
        icon_buf.set_text(
            &mut renderer.font_system,
            &entry.icon,
            &Attrs::new()
                .family(Family::Name(renderer.font_config.chrome_font_family()))
                .weight(Weight::NORMAL)
                .color(GlyphColor::rgba(255, 255, 255, 230)),
            Shaping::Advanced,
            None,
        );
        icon_buf.shape_until_scroll(&mut renderer.font_system, false);
        buffers.push(icon_buf);
        rows_info.push(RowInfo {
            buf_idx: buffers.len() - 1,
            x: tx,
            y: ty + 16.0,
            color: GlyphColor::rgba(255, 255, 255, 230),
        });

        // Label text (below icon).
        let is_selected = idx == state.selected;
        let label_color = if is_selected {
            accent_color
        } else {
            GlyphColor::rgba(255, 255, 255, 200)
        };
        let mut label_buf = Buffer::new(&mut renderer.font_system, label_metrics);
        label_buf.set_size(&mut renderer.font_system, Some(TILE_W), Some(16.0));
        label_buf.set_text(
            &mut renderer.font_system,
            &entry.name,
            &Attrs::new()
                .family(Family::Name(renderer.font_config.chrome_font_family()))
                .weight(if is_selected {
                    Weight::BOLD
                } else {
                    Weight::NORMAL
                })
                .color(label_color),
            Shaping::Basic,
            None,
        );
        label_buf.shape_until_scroll(&mut renderer.font_system, false);
        buffers.push(label_buf);
        rows_info.push(RowInfo {
            buf_idx: buffers.len() - 1,
            x: tx,
            y: ty + 60.0,
            color: label_color,
        });
    }

    // Build TextArea references.
    let text_areas: Vec<TextArea<'_>> = rows_info
        .iter()
        .map(|row| TextArea {
            buffer: &buffers[row.buf_idx],
            left: row.x,
            top: row.y,
            scale: 1.0,
            bounds: panel_bounds,
            default_color: row.color,
            custom_glyphs: &[],
        })
        .collect();

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
        tracing::warn!("launcher overlay text prepare failed: {}", e);
    }

    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("launcher_overlay_text_pass"),
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
            tracing::warn!("launcher overlay text render failed: {}", e);
        }
    }
}

/// Compute the number of grid columns for a given panel width (exported for
/// the state's `navigate` method to stay in sync with the renderer).
pub(crate) fn compute_cols(entry_count: usize, surface_width: f32) -> usize {
    let max_panel_w = (surface_width * 0.8).min(800.0);
    let padding_h = 32.0_f32;
    let usable_w = max_panel_w - padding_h * 2.0;
    let cols = ((usable_w + TILE_GAP) / (TILE_W + TILE_GAP))
        .floor()
        .max(1.0) as usize;
    cols.min(entry_count)
}

// ── Color helpers ────────────────────────────────────────────────────────

/// Lighten an RGBA color by `amount` (0..1).
fn lighten(c: [f32; 4], amount: f32) -> [f32; 4] {
    [
        (c[0] + amount).min(1.0),
        (c[1] + amount).min(1.0),
        (c[2] + amount).min(1.0),
        c[3],
    ]
}

/// Darken an RGBA color by `amount` (0..1).
fn darken(c: [f32; 4], amount: f32) -> [f32; 4] {
    [
        (c[0] - amount).max(0.0),
        (c[1] - amount).max(0.0),
        (c[2] - amount).max(0.0),
        c[3],
    ]
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_6_digit() {
        let c = parse_hex_color("#2563eb").unwrap();
        assert!((c[0] - 0.145).abs() < 0.01);
        assert!((c[1] - 0.388).abs() < 0.01);
        assert!((c[2] - 0.922).abs() < 0.01);
        assert!((c[3] - 0.85).abs() < 0.01);
    }

    #[test]
    fn parse_hex_3_digit() {
        let c = parse_hex_color("#f00").unwrap();
        assert!((c[0] - 1.0).abs() < 0.01);
        assert!((c[1] - 0.0).abs() < 0.01);
    }

    #[test]
    fn parse_hex_invalid() {
        assert!(parse_hex_color("nope").is_none());
        assert!(parse_hex_color("#gg00ff").is_none());
    }

    #[test]
    fn build_entries_default_shell_first() {
        let mut state = LauncherState::default();
        let profiles = HashMap::new();
        state.build_entries(&profiles);
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries[0].name, "Default Shell");
        assert!(state.entries[0].profile_name.is_none());
    }

    #[test]
    fn build_entries_sorted() {
        let mut state = LauncherState::default();
        let mut profiles = HashMap::new();
        profiles.insert(
            "zsh".to_string(),
            ProfileConfig {
                shell: Some("/bin/zsh".to_string()),
                ..Default::default()
            },
        );
        profiles.insert(
            "bash".to_string(),
            ProfileConfig {
                shell: Some("/bin/bash".to_string()),
                ..Default::default()
            },
        );
        state.build_entries(&profiles);
        assert_eq!(state.entries.len(), 3);
        assert_eq!(state.entries[0].name, "Default Shell");
        assert_eq!(state.entries[1].name, "bash");
        assert_eq!(state.entries[2].name, "zsh");
    }

    #[test]
    fn navigate_wraps_correctly() {
        let mut state = LauncherState::default();
        let mut profiles = HashMap::new();
        for i in 0..6 {
            profiles.insert(
                format!("p{i}"),
                ProfileConfig {
                    shell: Some(format!("/bin/sh{i}")),
                    ..Default::default()
                },
            );
        }
        state.build_entries(&profiles);
        state.cols = 3;

        // Start at 0, move right.
        state.navigate(1, 0);
        assert_eq!(state.selected, 1);

        // Move down.
        state.navigate(0, 1);
        assert_eq!(state.selected, 4);

        // Don't go past the end.
        state.navigate(0, 1);
        assert_eq!(state.selected, 4); // 7 entries, row 2 col 1 = idx 7 > 6

        // Move left to boundary.
        state.navigate(-1, 0);
        assert_eq!(state.selected, 3);
        state.navigate(-1, 0);
        assert_eq!(state.selected, 3); // clamped at 0
    }

    #[test]
    fn compute_cols_small_window() {
        // Small window should still give at least 1 col.
        let cols = compute_cols(5, 200.0);
        assert!(cols >= 1);
    }

    #[test]
    fn lighten_darken() {
        let c = [0.5, 0.5, 0.5, 0.8];
        let l = lighten(c, 0.2);
        assert!((l[0] - 0.7).abs() < 0.001);
        let d = darken(c, 0.2);
        assert!((d[0] - 0.3).abs() < 0.001);
    }
}
