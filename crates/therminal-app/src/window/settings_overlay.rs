//! Settings overlay model + renderer.
//!
//! This module provides:
//! - A reusable section/control registration model (`SettingsOverlayState`).
//! - Deterministic keyboard navigation state transitions.
//! - Control bindings (`SettingsCommand`) that the app applies to runtime config.
//! - A simple two-pane overlay renderer (left nav, right controls).

use crate::color_mapping::pixel_rect_to_ndc;
use crate::grid_renderer::{ColorVertex, GridRenderer};
use therminal_core::config::ColorsConfig;
use therminal_core::palette::Color as PaletteColor;
use wgpu::util::DeviceExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsFocus {
    Navigation,
    Controls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemePreset {
    OriginalTherminal,
    Paper,
    TokyoNightLight,
    TomorrowNightBright,
    HemisuDark,
}

impl ThemePreset {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::OriginalTherminal => "Original Therminal (default)",
            Self::Paper => "Paper (light)",
            Self::TokyoNightLight => "Tokyo Night Light (light)",
            Self::TomorrowNightBright => "Tomorrow Night Bright (dark)",
            Self::HemisuDark => "Hemisu Dark (dark)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlBinding {
    TogglePaneHeaders,
    ToggleStatusBar,
    ToggleTabBar,
    ApplyThemePreset(ThemePreset),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsCommand {
    TogglePaneHeaders,
    ToggleStatusBar,
    ToggleTabBar,
    ApplyThemePreset(ThemePreset),
}

impl ControlBinding {
    fn command(self) -> SettingsCommand {
        match self {
            Self::TogglePaneHeaders => SettingsCommand::TogglePaneHeaders,
            Self::ToggleStatusBar => SettingsCommand::ToggleStatusBar,
            Self::ToggleTabBar => SettingsCommand::ToggleTabBar,
            Self::ApplyThemePreset(preset) => SettingsCommand::ApplyThemePreset(preset),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsControl {
    pub label: &'static str,
    pub binding: ControlBinding,
}

impl SettingsControl {
    pub(crate) fn new(label: &'static str, binding: ControlBinding) -> Self {
        Self { label, binding }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsSection {
    pub id: &'static str,
    pub title: &'static str,
    pub controls: Vec<SettingsControl>,
}

impl SettingsSection {
    pub(crate) fn new(
        id: &'static str,
        title: &'static str,
        controls: Vec<SettingsControl>,
    ) -> Self {
        Self {
            id,
            title,
            controls,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsOverlayState {
    sections: Vec<SettingsSection>,
    selected_section: usize,
    selected_control_by_section: Vec<usize>,
    focus: SettingsFocus,
}

impl SettingsOverlayState {
    pub(crate) fn new() -> Self {
        let mut s = Self {
            sections: Vec::new(),
            selected_section: 0,
            selected_control_by_section: Vec::new(),
            focus: SettingsFocus::Navigation,
        };
        s.seed_defaults();
        s
    }

    pub(crate) fn reset_navigation(&mut self) {
        self.selected_section = 0;
        self.focus = SettingsFocus::Navigation;
        for idx in &mut self.selected_control_by_section {
            *idx = 0;
        }
    }

    pub(crate) fn register_section(&mut self, section: SettingsSection) {
        self.sections.push(section);
        self.selected_control_by_section.push(0);
        self.clamp_selection();
    }

    pub(crate) fn sections(&self) -> &[SettingsSection] {
        &self.sections
    }

    pub(crate) fn focus(&self) -> SettingsFocus {
        self.focus
    }

    pub(crate) fn active_section_index(&self) -> usize {
        self.selected_section
    }

    pub(crate) fn active_section(&self) -> Option<&SettingsSection> {
        self.sections.get(self.selected_section)
    }

    pub(crate) fn active_control_index(&self) -> usize {
        self.selected_control_by_section
            .get(self.selected_section)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn tab(&mut self, reverse: bool) {
        let order = [SettingsFocus::Navigation, SettingsFocus::Controls];
        let current = match self.focus {
            SettingsFocus::Navigation => 0usize,
            SettingsFocus::Controls => 1usize,
        };
        let next = if reverse {
            (current + order.len() - 1) % order.len()
        } else {
            (current + 1) % order.len()
        };
        self.focus = order[next];
    }

    pub(crate) fn arrow_up(&mut self) {
        match self.focus {
            SettingsFocus::Navigation => self.move_section(-1),
            SettingsFocus::Controls => self.move_control(-1),
        }
    }

    pub(crate) fn arrow_down(&mut self) {
        match self.focus {
            SettingsFocus::Navigation => self.move_section(1),
            SettingsFocus::Controls => self.move_control(1),
        }
    }

    pub(crate) fn arrow_left(&mut self) {
        self.focus = SettingsFocus::Navigation;
    }

    pub(crate) fn arrow_right(&mut self) {
        self.focus = SettingsFocus::Controls;
    }

    pub(crate) fn enter(&mut self) -> Option<SettingsCommand> {
        match self.focus {
            SettingsFocus::Navigation => {
                self.focus = SettingsFocus::Controls;
                None
            }
            SettingsFocus::Controls => self
                .active_section()
                .and_then(|s| s.controls.get(self.active_control_index()))
                .map(|c| c.binding.command()),
        }
    }

    fn move_section(&mut self, delta: i32) {
        if self.sections.is_empty() {
            return;
        }
        let len = self.sections.len() as i32;
        let next = (self.selected_section as i32 + delta).rem_euclid(len);
        self.selected_section = next as usize;
        self.clamp_selection();
    }

    fn move_control(&mut self, delta: i32) {
        let Some(section) = self.sections.get(self.selected_section) else {
            return;
        };
        if section.controls.is_empty() {
            return;
        }
        let len = section.controls.len() as i32;
        let curr = self.active_control_index() as i32;
        let next = (curr + delta).rem_euclid(len) as usize;
        if let Some(idx) = self
            .selected_control_by_section
            .get_mut(self.selected_section)
        {
            *idx = next;
        }
    }

    fn clamp_selection(&mut self) {
        if self.sections.is_empty() {
            self.selected_section = 0;
            return;
        }
        self.selected_section = self.selected_section.min(self.sections.len() - 1);
        if self.selected_control_by_section.len() != self.sections.len() {
            self.selected_control_by_section
                .resize(self.sections.len(), 0);
        }
        for (i, section) in self.sections.iter().enumerate() {
            if section.controls.is_empty() {
                self.selected_control_by_section[i] = 0;
            } else {
                let max_idx = section.controls.len() - 1;
                self.selected_control_by_section[i] =
                    self.selected_control_by_section[i].min(max_idx);
            }
        }
    }

    fn seed_defaults(&mut self) {
        self.register_section(SettingsSection::new(
            "layout",
            "Layout",
            vec![
                SettingsControl::new("Show pane headers", ControlBinding::TogglePaneHeaders),
                SettingsControl::new("Show status bar", ControlBinding::ToggleStatusBar),
                SettingsControl::new("Show tab bar", ControlBinding::ToggleTabBar),
            ],
        ));

        self.register_section(SettingsSection::new(
            "themes",
            "Theme Presets",
            vec![
                SettingsControl::new(
                    "Apply Original Therminal",
                    ControlBinding::ApplyThemePreset(ThemePreset::OriginalTherminal),
                ),
                SettingsControl::new(
                    "Apply Paper",
                    ControlBinding::ApplyThemePreset(ThemePreset::Paper),
                ),
                SettingsControl::new(
                    "Apply Tokyo Night Light",
                    ControlBinding::ApplyThemePreset(ThemePreset::TokyoNightLight),
                ),
                SettingsControl::new(
                    "Apply Tomorrow Night Bright",
                    ControlBinding::ApplyThemePreset(ThemePreset::TomorrowNightBright),
                ),
                SettingsControl::new(
                    "Apply Hemisu Dark",
                    ControlBinding::ApplyThemePreset(ThemePreset::HemisuDark),
                ),
            ],
        ));
    }
}

impl Default for SettingsOverlayState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SettingsRenderValues {
    pub show_pane_headers: bool,
    pub show_status_bar: bool,
    pub show_tab_bar: bool,
}

fn bool_text(value: bool) -> &'static str {
    if value { "ON" } else { "OFF" }
}

fn truncate_for_width(text: &str, width_px: f32) -> String {
    // Heuristic tuned for 18px overlay text: keep rows single-line and avoid overlap.
    let max_chars = (width_px / 9.0).floor().max(4.0) as usize;
    let len = text.chars().count();
    if len <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

pub(crate) fn apply_theme_preset(colors: &mut ColorsConfig, preset: ThemePreset) {
    let (background, foreground, cursor, ansi): (&str, &str, &str, [&str; 16]) = match preset {
        ThemePreset::OriginalTherminal => (
            "#060a12",
            "#e7f0ff",
            "#fef3c7",
            [
                "#060a12", "#ff5f78", "#ffb24f", "#eab308", "#56a7ff", "#56a7ff", "#39ffb6",
                "#e7f0ff", "#7b8fa9", "#ff7d8f", "#59ffc7", "#f97316", "#56a7ff", "#e7f0ff",
                "#4ce5cc", "#fef3c7",
            ],
        ),
        ThemePreset::Paper => (
            "#f2eede",
            "#000000",
            "#000000",
            [
                "#000000", "#cc3e28", "#216609", "#b58900", "#1e6fcc", "#5c21a5", "#158c86",
                "#aaaaaa", "#555555", "#cc3e28", "#216609", "#b58900", "#1e6fcc", "#5c21a5",
                "#158c86", "#aaaaaa",
            ],
        ),
        ThemePreset::TokyoNightLight => (
            "#D5D6DB",
            "#565A6E",
            "#565A6E",
            [
                "#0F0F14", "#8C4351", "#485E30", "#8F5E15", "#34548A", "#5A4A78", "#0F4B6E",
                "#343B58", "#9699A3", "#8C4351", "#485E30", "#8F5E15", "#34548A", "#5A4A78",
                "#0F4B6E", "#343B58",
            ],
        ),
        ThemePreset::TomorrowNightBright => (
            "#000000",
            "#EAEAEA",
            "#EAEAEA",
            [
                "#2A2A2A", "#D54E53", "#B9CA4A", "#E7C547", "#7AA6DA", "#C397D8", "#70C0B1",
                "#EAEAEA", "#969896", "#D54E53", "#B9CA4A", "#E7C547", "#7AA6DA", "#C397D8",
                "#70C0B1", "#FFFFFF",
            ],
        ),
        ThemePreset::HemisuDark => (
            "#000000",
            "#FFFFFF",
            "#BAFFAA",
            [
                "#444444", "#FF0054", "#B1D630", "#9D895E", "#67BEE3", "#B576BC", "#569A9F",
                "#EDEDED", "#777777", "#D65E75", "#BAFFAA", "#ECE1C8", "#9FD3E5", "#DEB3DF",
                "#B6E0E5", "#FFFFFF",
            ],
        ),
    };

    colors.background = Some(background.to_string());
    colors.foreground = Some(foreground.to_string());
    colors.cursor = Some(cursor.to_string());
    colors.ansi = Some(ansi.iter().map(|c| (*c).to_string()).collect());
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_settings_overlay(
    state: &SettingsOverlayState,
    values: SettingsRenderValues,
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

    let panel_w = (sw * 0.78).clamp(760.0, 1200.0).min(sw - 24.0);
    let panel_h = (sh * 0.74).clamp(420.0, 760.0).min(sh - 24.0);
    let panel_x = (sw - panel_w) * 0.5;
    let panel_y = (sh - panel_h) * 0.5;

    let nav_w = (panel_w * 0.30).clamp(180.0, 320.0);
    let content_x = panel_x + nav_w;

    let scrim_color = [0.0, 0.0, 0.0, 0.64];
    let panel_bg = [
        PaletteColor::PLATE.r as f32 / 255.0,
        PaletteColor::PLATE.g as f32 / 255.0,
        PaletteColor::PLATE.b as f32 / 255.0,
        0.97,
    ];
    let nav_bg = [
        PaletteColor::BG_SURFACE.r as f32 / 255.0,
        PaletteColor::BG_SURFACE.g as f32 / 255.0,
        PaletteColor::BG_SURFACE.b as f32 / 255.0,
        0.94,
    ];
    let nav_focus = [0.12, 0.40, 0.86, 0.24];
    let item_focus = [0.12, 0.40, 0.86, 0.28];
    let divider = [1.0, 1.0, 1.0, 0.08];

    let mut verts: Vec<ColorVertex> = Vec::new();
    verts.extend_from_slice(&pixel_rect_to_ndc(0.0, 0.0, sw, sh, sw, sh, scrim_color));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        panel_x, panel_y, panel_w, panel_h, sw, sh, panel_bg,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        panel_x, panel_y, nav_w, panel_h, sw, sh, nav_bg,
    ));
    verts.extend_from_slice(&pixel_rect_to_ndc(
        content_x,
        panel_y + 54.0,
        1.0,
        panel_h - 54.0,
        sw,
        sh,
        divider,
    ));

    let nav_row_h = 34.0_f32;
    let nav_start_y = panel_y + 72.0;
    for (idx, _section) in state.sections().iter().enumerate() {
        if idx == state.active_section_index() {
            let y = nav_start_y + idx as f32 * nav_row_h;
            verts.extend_from_slice(&pixel_rect_to_ndc(
                panel_x + 10.0,
                y,
                nav_w - 20.0,
                nav_row_h - 2.0,
                sw,
                sh,
                nav_focus,
            ));
        }
    }

    if state.focus() == SettingsFocus::Controls {
        let ctrl_row_h = 36.0_f32;
        let ctrl_start_y = panel_y + 112.0;
        let y = ctrl_start_y + state.active_control_index() as f32 * ctrl_row_h;
        verts.extend_from_slice(&pixel_rect_to_ndc(
            content_x + 22.0,
            y,
            panel_w - nav_w - 44.0,
            ctrl_row_h - 3.0,
            sw,
            sh,
            item_focus,
        ));
    }

    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("settings_overlay_rects"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });

    let mut rect_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("settings_overlay_rect_encoder"),
    });
    {
        let mut pass = rect_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("settings_overlay_rect_pass"),
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
        pass.draw(0..verts.len() as u32, 0..1);
    }
    queue.submit(std::iter::once(rect_encoder.finish()));

    let metrics = Metrics::new(18.0, 24.0);
    let title_metrics = Metrics::new(22.0, 28.0);
    renderer.viewport.update(
        queue,
        Resolution {
            width: surface_width,
            height: surface_height,
        },
    );

    let ink = GlyphColor::rgba(
        PaletteColor::INK.r,
        PaletteColor::INK.g,
        PaletteColor::INK.b,
        242,
    );
    let muted = GlyphColor::rgba(
        PaletteColor::INK_DIM.r,
        PaletteColor::INK_DIM.g,
        PaletteColor::INK_DIM.b,
        220,
    );
    let accent = GlyphColor::rgba(87, 161, 255, 255);

    let mut buffers: Vec<Buffer> = Vec::new();
    let mut placements: Vec<(usize, f32, f32, GlyphColor, TextBounds)> = Vec::new();

    let mut add_text = |text: String,
                        left: f32,
                        top: f32,
                        width: f32,
                        height: f32,
                        m: Metrics,
                        color: GlyphColor,
                        weight: Weight| {
        let mut buf = Buffer::new(&mut renderer.font_system, m);
        buf.set_size(
            &mut renderer.font_system,
            Some(width.max(1.0)),
            Some(height.max(1.0)),
        );
        buf.set_text(
            &mut renderer.font_system,
            &text,
            &Attrs::new()
                .family(Family::Name(&renderer.font_config.family))
                .weight(weight)
                .color(color),
            Shaping::Basic,
            None,
        );
        buf.shape_until_scroll(&mut renderer.font_system, false);
        let idx = buffers.len();
        buffers.push(buf);
        placements.push((
            idx,
            left,
            top,
            color,
            TextBounds {
                left: left as i32,
                top: top as i32,
                right: (left + width) as i32,
                bottom: (top + height) as i32,
            },
        ));
    };

    add_text(
        "Settings".to_string(),
        panel_x + 18.0,
        panel_y + 18.0,
        panel_w - 36.0,
        36.0,
        title_metrics,
        ink,
        Weight::SEMIBOLD,
    );
    add_text(
        "Tab/Shift+Tab focus, Arrows move, Enter activate, Esc close".to_string(),
        content_x + 24.0,
        panel_y + 24.0,
        panel_w - nav_w - 48.0,
        28.0,
        metrics,
        muted,
        Weight::NORMAL,
    );

    for (idx, section) in state.sections().iter().enumerate() {
        let _section_id = section.id;
        let marker = if idx == state.active_section_index() {
            "->"
        } else {
            "  "
        };
        let color =
            if idx == state.active_section_index() && state.focus() == SettingsFocus::Navigation {
                accent
            } else {
                ink
            };
        add_text(
            format!("{marker} {}", section.title),
            panel_x + 20.0,
            nav_start_y + idx as f32 * nav_row_h + 6.0,
            nav_w - 34.0,
            nav_row_h - 6.0,
            metrics,
            color,
            Weight::MEDIUM,
        );
    }

    if let Some(section) = state.active_section() {
        add_text(
            section.title.to_string(),
            content_x + 24.0,
            panel_y + 78.0,
            panel_w - nav_w - 48.0,
            34.0,
            title_metrics,
            ink,
            Weight::SEMIBOLD,
        );

        for (i, control) in section.controls.iter().enumerate() {
            let selected = i == state.active_control_index();
            let marker = if selected { "->" } else { "  " };
            let value_text = match control.binding {
                ControlBinding::TogglePaneHeaders => bool_text(values.show_pane_headers),
                ControlBinding::ToggleStatusBar => bool_text(values.show_status_bar),
                ControlBinding::ToggleTabBar => bool_text(values.show_tab_bar),
                ControlBinding::ApplyThemePreset(preset) => preset.label(),
            };
            let row_color = if selected && state.focus() == SettingsFocus::Controls {
                accent
            } else {
                ink
            };
            let row_width = panel_w - nav_w - 56.0;
            let row_text = truncate_for_width(
                &format!("{marker} {}: {value_text}", control.label),
                row_width,
            );
            add_text(
                row_text,
                content_x + 28.0,
                panel_y + 118.0 + i as f32 * 36.0,
                row_width,
                32.0,
                metrics,
                row_color,
                Weight::NORMAL,
            );
        }
    }

    let text_areas: Vec<TextArea<'_>> = placements
        .iter()
        .map(|(idx, left, top, color, clip_bounds)| TextArea {
            buffer: &buffers[*idx],
            left: *left,
            top: *top,
            scale: 1.0,
            bounds: *clip_bounds,
            default_color: *color,
            custom_glyphs: &[],
        })
        .collect();

    let mut text_encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("settings_overlay_text_encoder"),
    });

    if let Err(e) = renderer.overlay_text_renderer.prepare(
        device,
        queue,
        &mut renderer.font_system,
        &mut renderer.overlay_atlas,
        &renderer.viewport,
        text_areas,
        &mut renderer.swash_cache,
    ) {
        tracing::warn!("settings overlay text prepare failed: {}", e);
    }

    {
        let mut pass = text_encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("settings_overlay_text_pass"),
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
            tracing::warn!("settings overlay text render failed: {}", e);
        }
    }

    queue.submit(std::iter::once(text_encoder.finish()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_to_linear(v: f64) -> f64 {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    fn relative_luminance(hex: &str) -> f64 {
        let color = ColorsConfig::parse_hex(hex).expect("valid hex color");
        let r = srgb_to_linear(color.r as f64 / 255.0);
        let g = srgb_to_linear(color.g as f64 / 255.0);
        let b = srgb_to_linear(color.b as f64 / 255.0);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn contrast_ratio(a: &str, b: &str) -> f64 {
        let l1 = relative_luminance(a);
        let l2 = relative_luminance(b);
        let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn tab_switches_focus_between_nav_and_controls() {
        let mut state = SettingsOverlayState::new();
        assert_eq!(state.focus(), SettingsFocus::Navigation);
        state.tab(false);
        assert_eq!(state.focus(), SettingsFocus::Controls);
        state.tab(true);
        assert_eq!(state.focus(), SettingsFocus::Navigation);
    }

    #[test]
    fn arrows_navigate_sections_and_controls() {
        let mut state = SettingsOverlayState::new();
        assert_eq!(state.active_section_index(), 0);
        state.arrow_down();
        assert_eq!(state.active_section_index(), 1);
        state.arrow_right();
        assert_eq!(state.focus(), SettingsFocus::Controls);
        state.arrow_down();
        assert_eq!(state.active_control_index(), 1);
        state.arrow_up();
        assert_eq!(state.active_control_index(), 0);
    }

    #[test]
    fn enter_from_nav_then_enter_returns_command() {
        let mut state = SettingsOverlayState::new();
        assert_eq!(state.enter(), None);
        assert_eq!(state.focus(), SettingsFocus::Controls);
        assert_eq!(state.enter(), Some(SettingsCommand::TogglePaneHeaders));
    }

    #[test]
    fn register_section_extends_navigation_model() {
        let mut state = SettingsOverlayState::new();
        let base = state.sections().len();
        state.register_section(SettingsSection::new(
            "test",
            "Test",
            vec![SettingsControl::new(
                "Toggle",
                ControlBinding::ToggleStatusBar,
            )],
        ));
        assert_eq!(state.sections().len(), base + 1);
        state.arrow_down();
        state.arrow_down();
        assert_eq!(state.active_section().map(|s| s.id), Some("test"));
    }

    #[test]
    fn theme_preset_writes_expected_palette_fields() {
        let mut colors = ColorsConfig::default();
        apply_theme_preset(&mut colors, ThemePreset::OriginalTherminal);
        assert_eq!(colors.background.as_deref(), Some("#060a12"));
        assert_eq!(colors.foreground.as_deref(), Some("#e7f0ff"));
        assert_eq!(colors.cursor.as_deref(), Some("#fef3c7"));
        assert_eq!(
            colors
                .ansi
                .as_ref()
                .and_then(|v| v.first())
                .map(String::as_str),
            Some("#060a12")
        );
        assert_eq!(
            colors
                .ansi
                .as_ref()
                .and_then(|v| v.get(15))
                .map(String::as_str),
            Some("#fef3c7")
        );

        apply_theme_preset(&mut colors, ThemePreset::Paper);
        assert_eq!(colors.background.as_deref(), Some("#f2eede"));
        assert_eq!(colors.foreground.as_deref(), Some("#000000"));
        assert_eq!(colors.cursor.as_deref(), Some("#000000"));
        assert_eq!(colors.ansi.as_ref().map(|v| v.len()), Some(16));
    }

    #[test]
    fn theme_presets_keep_readable_fg_bg_contrast() {
        // WCAG AA threshold for normal text.
        const MIN_CONTRAST: f64 = 4.5;
        let mut colors = ColorsConfig::default();

        apply_theme_preset(&mut colors, ThemePreset::OriginalTherminal);
        let ratio = contrast_ratio(
            colors.background.as_deref().unwrap_or("#000000"),
            colors.foreground.as_deref().unwrap_or("#ffffff"),
        );
        assert!(
            ratio >= MIN_CONTRAST,
            "OriginalTherminal contrast too low: {ratio:.2}"
        );

        apply_theme_preset(&mut colors, ThemePreset::Paper);
        let ratio = contrast_ratio(
            colors.background.as_deref().unwrap_or("#000000"),
            colors.foreground.as_deref().unwrap_or("#ffffff"),
        );
        assert!(ratio >= MIN_CONTRAST, "Paper contrast too low: {ratio:.2}");

        apply_theme_preset(&mut colors, ThemePreset::TokyoNightLight);
        let ratio = contrast_ratio(
            colors.background.as_deref().unwrap_or("#000000"),
            colors.foreground.as_deref().unwrap_or("#ffffff"),
        );
        assert!(
            ratio >= MIN_CONTRAST,
            "TokyoNightLight contrast too low: {ratio:.2}"
        );

        apply_theme_preset(&mut colors, ThemePreset::TomorrowNightBright);
        let ratio = contrast_ratio(
            colors.background.as_deref().unwrap_or("#000000"),
            colors.foreground.as_deref().unwrap_or("#ffffff"),
        );
        assert!(
            ratio >= MIN_CONTRAST,
            "TomorrowNightBright contrast too low: {ratio:.2}"
        );

        apply_theme_preset(&mut colors, ThemePreset::HemisuDark);
        let ratio = contrast_ratio(
            colors.background.as_deref().unwrap_or("#000000"),
            colors.foreground.as_deref().unwrap_or("#ffffff"),
        );
        assert!(
            ratio >= MIN_CONTRAST,
            "HemisuDark contrast too low: {ratio:.2}"
        );
    }
}
