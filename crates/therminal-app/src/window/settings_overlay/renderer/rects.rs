//! Build the rect-pipeline vertex list for the settings overlay (pass 1):
//! scrim, panel, nav strip, focus ring, and per-control background pills.

use crate::color_mapping::pixel_rect_to_ndc;
use crate::grid_renderer::ColorVertex;
use therminal_core::palette::ChromePalette;

use super::super::state::SettingsOverlayState;
use super::super::types::{ControlType, SettingsFocus};
use super::CaretOffsets;
use super::layout::PanelLayout;

pub(super) fn build_rect_vertices(
    state: &SettingsOverlayState,
    layout: &PanelLayout,
    chrome_palette: &ChromePalette,
    caret_offsets: &CaretOffsets,
) -> Vec<ColorVertex> {
    let PanelLayout {
        sw,
        sh,
        panel_x,
        panel_y,
        panel_w,
        panel_h,
        nav_w,
        content_x,
        nav_row_h,
        nav_start_y,
        ctrl_row_h,
        ctrl_start_y,
    } = *layout;

    let scrim_color = [0.0, 0.0, 0.0, 0.64];
    let panel_bg = [
        chrome_palette.header_bg[0],
        chrome_palette.header_bg[1],
        chrome_palette.header_bg[2],
        0.97,
    ];
    let nav_bg = [
        chrome_palette.status_bar_bg[0],
        chrome_palette.status_bar_bg[1],
        chrome_palette.status_bar_bg[2],
        0.94,
    ];
    let nav_focus = [
        chrome_palette.focus_border[0],
        chrome_palette.focus_border[1],
        chrome_palette.focus_border[2],
        0.24,
    ];
    let item_focus = [
        chrome_palette.focus_border[0],
        chrome_palette.focus_border[1],
        chrome_palette.focus_border[2],
        0.28,
    ];
    let divider = [
        chrome_palette.separator[0],
        chrome_palette.separator[1],
        chrome_palette.separator[2],
        0.08,
    ];

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

    let focus_ring_border = [
        chrome_palette.focus_border[0],
        chrome_palette.focus_border[1],
        chrome_palette.focus_border[2],
        0.6,
    ];
    if state.focus() == SettingsFocus::Controls {
        let y = ctrl_start_y + state.active_control_index() as f32 * ctrl_row_h;
        let row_x = content_x + 22.0;
        let row_w = panel_w - nav_w - 44.0;
        let row_h = ctrl_row_h - 3.0;
        let bw = 2.0_f32;
        verts.extend_from_slice(&pixel_rect_to_ndc(
            row_x, y, row_w, row_h, sw, sh, item_focus,
        ));
        verts.extend_from_slice(&pixel_rect_to_ndc(
            row_x,
            y,
            row_w,
            bw,
            sw,
            sh,
            focus_ring_border,
        ));
        verts.extend_from_slice(&pixel_rect_to_ndc(
            row_x,
            y + row_h - bw,
            row_w,
            bw,
            sw,
            sh,
            focus_ring_border,
        ));
        verts.extend_from_slice(&pixel_rect_to_ndc(
            row_x,
            y + bw,
            bw,
            row_h - 2.0 * bw,
            sw,
            sh,
            focus_ring_border,
        ));
        verts.extend_from_slice(&pixel_rect_to_ndc(
            row_x + row_w - bw,
            y + bw,
            bw,
            row_h - 2.0 * bw,
            sw,
            sh,
            focus_ring_border,
        ));
    }

    if let Some(section) = state.active_section() {
        let toggle_on_bg = [0.22, 0.78, 0.45, 0.85];
        let toggle_off_bg = [0.35, 0.38, 0.44, 0.65];
        let text_field_bg = [0.0, 0.0, 0.0, 0.30];
        let text_field_editing_bg = [0.0, 0.0, 0.0, 0.50];
        let text_cursor_color = [
            chrome_palette.focus_border[0],
            chrome_palette.focus_border[1],
            chrome_palette.focus_border[2],
            0.9,
        ];
        let value_col_x = content_x + 28.0 + (panel_w - nav_w - 56.0) * 0.55;
        let section_idx = state.active_section_index();
        // Caret placement uses the shaped pixel width of `value[..cursor]`
        // precomputed in `mod.rs::build_caret_offsets`. Fallback to 0.0 if
        // the offset is missing (shouldn't happen for editing controls).
        let caret_x_from_offset = |i: usize, field_w: f32| -> f32 {
            let off = caret_offsets.get(&(section_idx, i)).copied().unwrap_or(0.0);
            // 4.0 px left padding matches the text draw offset
            // (`value_col_x + 4.0` in text.rs), and the clamp keeps the
            // caret inside the field when the value overflows.
            value_col_x + 4.0 + off.min(field_w - 8.0).max(0.0)
        };
        for (i, control) in section.controls.iter().enumerate() {
            let row_y = ctrl_start_y + i as f32 * ctrl_row_h;
            match &control.control_type {
                ControlType::Toggle { value } => {
                    let pill_w = 48.0_f32;
                    let pill_h = 22.0_f32;
                    let pill_y = row_y + (ctrl_row_h - pill_h) * 0.5;
                    let bg = if *value { toggle_on_bg } else { toggle_off_bg };
                    verts.extend_from_slice(&pixel_rect_to_ndc(
                        value_col_x,
                        pill_y,
                        pill_w,
                        pill_h,
                        sw,
                        sh,
                        bg,
                    ));
                }
                ControlType::TextInput { editing, .. } => {
                    let field_w = (panel_w - nav_w - 56.0) * 0.42;
                    let field_h = 24.0_f32;
                    let field_y = row_y + (ctrl_row_h - field_h) * 0.5;
                    let bg = if *editing {
                        text_field_editing_bg
                    } else {
                        text_field_bg
                    };
                    verts.extend_from_slice(&pixel_rect_to_ndc(
                        value_col_x,
                        field_y,
                        field_w,
                        field_h,
                        sw,
                        sh,
                        bg,
                    ));
                    if *editing {
                        let cursor_x = caret_x_from_offset(i, field_w);
                        verts.extend_from_slice(&pixel_rect_to_ndc(
                            cursor_x,
                            field_y + 2.0,
                            2.0,
                            field_h - 4.0,
                            sw,
                            sh,
                            text_cursor_color,
                        ));
                    }
                }
                ControlType::Select { .. } => {
                    let field_w = (panel_w - nav_w - 56.0) * 0.42;
                    let field_h = 24.0_f32;
                    let field_y = row_y + (ctrl_row_h - field_h) * 0.5;
                    verts.extend_from_slice(&pixel_rect_to_ndc(
                        value_col_x,
                        field_y,
                        field_w,
                        field_h,
                        sw,
                        sh,
                        text_field_bg,
                    ));
                }
                ControlType::ListRow { editing, .. } => {
                    let field_w = (panel_w - nav_w - 56.0) * 0.42;
                    let field_h = 24.0_f32;
                    let field_y = row_y + (ctrl_row_h - field_h) * 0.5;
                    if *editing {
                        verts.extend_from_slice(&pixel_rect_to_ndc(
                            value_col_x,
                            field_y,
                            field_w,
                            field_h,
                            sw,
                            sh,
                            text_field_editing_bg,
                        ));
                        let cursor_x = caret_x_from_offset(i, field_w);
                        verts.extend_from_slice(&pixel_rect_to_ndc(
                            cursor_x,
                            field_y + 2.0,
                            2.0,
                            field_h - 4.0,
                            sw,
                            sh,
                            text_cursor_color,
                        ));
                    }
                }
                ControlType::Action => {}
            }
        }
    }

    verts
}
