//! Build the glyphon text buffers + placement metadata for the settings
//! overlay (pass 2): title, hint, nav labels, per-control labels.

use glyphon::{Attrs, Buffer, Color as GlyphColor, Family, Metrics, Shaping, TextBounds, Weight};

use crate::grid_renderer::GridRenderer;
use therminal_core::palette::ChromePalette;

use super::super::state::SettingsOverlayState;
use super::super::types::{ControlBinding, ControlType, SettingsFocus};
use super::layout::PanelLayout;

/// One placed glyph buffer with the per-area metadata `glyphon::TextArea`
/// needs at submit time. Detached from `TextArea` itself so the
/// `Vec<Buffer>` and the placement list can be returned together — the
/// `TextArea` borrows back into the buffers vec at submission time.
pub(super) struct TextPlacement {
    pub buffer_idx: usize,
    pub left: f32,
    pub top: f32,
    pub color: GlyphColor,
    pub bounds: TextBounds,
}

fn truncate_for_width(text: &str, width_px: f32) -> String {
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

#[allow(clippy::too_many_arguments)]
fn add_text(
    buffers: &mut Vec<Buffer>,
    placements: &mut Vec<TextPlacement>,
    renderer: &mut GridRenderer,
    text: String,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    m: Metrics,
    color: GlyphColor,
    weight: Weight,
) {
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
    placements.push(TextPlacement {
        buffer_idx: idx,
        left,
        top,
        color,
        bounds: TextBounds {
            left: left as i32,
            top: top as i32,
            right: (left + width) as i32,
            bottom: (top + height) as i32,
        },
    });
}

pub(super) fn build_text_buffers(
    state: &SettingsOverlayState,
    layout: &PanelLayout,
    renderer: &mut GridRenderer,
    chrome_palette: &ChromePalette,
) -> (Vec<Buffer>, Vec<TextPlacement>) {
    let PanelLayout {
        panel_x,
        panel_y,
        panel_w,
        nav_w,
        content_x,
        nav_row_h,
        nav_start_y,
        ..
    } = *layout;

    let metrics = Metrics::new(18.0, 24.0);
    let title_metrics = Metrics::new(22.0, 28.0);

    let ink = GlyphColor::rgba(
        chrome_palette.chrome_fg.r,
        chrome_palette.chrome_fg.g,
        chrome_palette.chrome_fg.b,
        242,
    );
    let muted = GlyphColor::rgba(
        chrome_palette.chrome_fg_muted.r,
        chrome_palette.chrome_fg_muted.g,
        chrome_palette.chrome_fg_muted.b,
        220,
    );
    let accent = GlyphColor::rgba(
        (chrome_palette.focus_border[0] * 255.0) as u8,
        (chrome_palette.focus_border[1] * 255.0) as u8,
        (chrome_palette.focus_border[2] * 255.0) as u8,
        255,
    );
    let signal = GlyphColor::rgba(
        chrome_palette.chrome_fg_focus.r,
        chrome_palette.chrome_fg_focus.g,
        chrome_palette.chrome_fg_focus.b,
        255,
    );

    let mut buffers: Vec<Buffer> = Vec::new();
    let mut placements: Vec<TextPlacement> = Vec::new();

    add_text(
        &mut buffers,
        &mut placements,
        renderer,
        "Settings".to_string(),
        panel_x + 18.0,
        panel_y + 18.0,
        panel_w - 36.0,
        36.0,
        title_metrics,
        ink,
        Weight::SEMIBOLD,
    );
    let hint = if state.is_text_editing() {
        "Type to edit, Enter confirm, Esc cancel, Del remove"
    } else if state.is_select_expanded() {
        "Arrows change value, Enter/Space confirm, Esc cancel"
    } else {
        "Tab/Shift+Tab focus, Arrows move, Enter edit, Del remove, Esc close"
    };
    add_text(
        &mut buffers,
        &mut placements,
        renderer,
        hint.to_string(),
        content_x + 24.0,
        panel_y + 24.0,
        panel_w - nav_w - 48.0,
        28.0,
        metrics,
        muted,
        Weight::NORMAL,
    );

    for (idx, section) in state.sections().iter().enumerate() {
        let marker = if idx == state.active_section_index() {
            ">"
        } else {
            " "
        };
        let color =
            if idx == state.active_section_index() && state.focus() == SettingsFocus::Navigation {
                accent
            } else {
                ink
            };
        add_text(
            &mut buffers,
            &mut placements,
            renderer,
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
            &mut buffers,
            &mut placements,
            renderer,
            section.title.to_string(),
            content_x + 24.0,
            panel_y + 78.0,
            panel_w - nav_w - 48.0,
            34.0,
            title_metrics,
            ink,
            Weight::SEMIBOLD,
        );
        let row_width = panel_w - nav_w - 56.0;
        let label_width = row_width * 0.52;
        let value_col_x = content_x + 28.0 + row_width * 0.55;
        let value_width = row_width * 0.42;
        for (i, control) in section.controls.iter().enumerate() {
            let selected = i == state.active_control_index();
            let marker = if selected { ">" } else { " " };
            let row_color = if selected && state.focus() == SettingsFocus::Controls {
                accent
            } else {
                ink
            };
            let row_y = panel_y + 118.0 + i as f32 * 36.0;
            match &control.control_type {
                ControlType::Toggle { value } => {
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(&format!("{marker} {}", control.label), label_width),
                        content_x + 28.0,
                        row_y,
                        label_width,
                        32.0,
                        metrics,
                        row_color,
                        Weight::NORMAL,
                    );
                    let pill_text = if *value { " ON " } else { " OFF" };
                    let pill_color = if *value { signal } else { muted };
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        pill_text.to_string(),
                        value_col_x + 4.0,
                        row_y + 2.0,
                        48.0,
                        24.0,
                        metrics,
                        pill_color,
                        Weight::BOLD,
                    );
                }
                ControlType::Select {
                    options,
                    selected: sel_idx,
                    expanded,
                } => {
                    let em = if *expanded { "*" } else { "" };
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(&format!("{marker} {}{em}", control.label), label_width),
                        content_x + 28.0,
                        row_y,
                        label_width,
                        32.0,
                        metrics,
                        row_color,
                        Weight::NORMAL,
                    );
                    let opt_label = options
                        .get(*sel_idx)
                        .map(|s| s.as_str())
                        .unwrap_or("(none)");
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(&format!("< {opt_label} >"), value_width),
                        value_col_x + 4.0,
                        row_y + 2.0,
                        value_width,
                        24.0,
                        metrics,
                        row_color,
                        Weight::NORMAL,
                    );
                }
                ControlType::TextInput { value, editing, .. } => {
                    let em = if *editing { "*" } else { "" };
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(&format!("{marker} {}{em}", control.label), label_width),
                        content_x + 28.0,
                        row_y,
                        label_width,
                        32.0,
                        metrics,
                        row_color,
                        Weight::NORMAL,
                    );
                    let display = if value.is_empty() && !*editing {
                        "(empty)".to_string()
                    } else {
                        value.clone()
                    };
                    let text_color = if value.is_empty() && !*editing {
                        muted
                    } else {
                        ink
                    };
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(&display, value_width - 8.0),
                        value_col_x + 4.0,
                        row_y + 2.0,
                        value_width - 8.0,
                        24.0,
                        metrics,
                        text_color,
                        Weight::NORMAL,
                    );
                }
                ControlType::ListRow {
                    display_value,
                    editing,
                    ..
                } => {
                    let em = if *editing { "*" } else { "" };
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(&format!("{marker} {}{em}", control.label), label_width),
                        content_x + 28.0,
                        row_y,
                        label_width,
                        32.0,
                        metrics,
                        row_color,
                        Weight::NORMAL,
                    );
                    let display = if display_value.is_empty() && !*editing {
                        "(empty)".to_string()
                    } else {
                        display_value.clone()
                    };
                    let text_color = if display_value.is_empty() && !*editing {
                        muted
                    } else if *editing {
                        ink
                    } else {
                        muted
                    };
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(&display, value_width - 8.0),
                        value_col_x + 4.0,
                        row_y + 2.0,
                        value_width - 8.0,
                        24.0,
                        metrics,
                        text_color,
                        Weight::NORMAL,
                    );
                }
                ControlType::Action => {
                    let is_active_theme = matches!(
                        &control.binding,
                        ControlBinding::ApplyThemePreset(p) if state.active_theme() == Some(*p)
                    );
                    let suffix = if is_active_theme { "  (active)" } else { "" };
                    let color = if is_active_theme { signal } else { row_color };
                    add_text(
                        &mut buffers,
                        &mut placements,
                        renderer,
                        truncate_for_width(
                            &format!("{marker} {}{suffix}", control.label),
                            row_width,
                        ),
                        content_x + 28.0,
                        row_y,
                        row_width,
                        32.0,
                        metrics,
                        color,
                        Weight::NORMAL,
                    );
                }
            }
        }
    }

    (buffers, placements)
}
