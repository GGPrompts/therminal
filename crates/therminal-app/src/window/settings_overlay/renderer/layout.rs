//! `PanelLayout`: pure geometry math for the settings overlay panel.

/// Pixel-space layout of the settings overlay panel computed from the
/// surface dimensions. Holds every coordinate the rect builder and the
/// text builder need so neither has to recompute them.
pub(super) struct PanelLayout {
    pub sw: f32,
    pub sh: f32,
    pub panel_x: f32,
    pub panel_y: f32,
    pub panel_w: f32,
    pub panel_h: f32,
    pub nav_w: f32,
    pub content_x: f32,
    pub nav_row_h: f32,
    pub nav_start_y: f32,
    pub ctrl_row_h: f32,
    pub ctrl_start_y: f32,
}

impl PanelLayout {
    pub(super) fn compute(surface_width: u32, surface_height: u32) -> Self {
        let sw = surface_width as f32;
        let sh = surface_height as f32;
        let panel_w = (sw * 0.78).clamp(760.0, 1200.0).min(sw - 24.0);
        let panel_h = (sh * 0.74).clamp(420.0, 760.0).min(sh - 24.0);
        let panel_x = (sw - panel_w) * 0.5;
        let panel_y = (sh - panel_h) * 0.5;
        let nav_w = (panel_w * 0.30).clamp(180.0, 320.0);
        let content_x = panel_x + nav_w;
        let nav_row_h = 34.0_f32;
        let nav_start_y = panel_y + 72.0;
        let ctrl_row_h = 36.0_f32;
        let ctrl_start_y = panel_y + 112.0;
        Self {
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
        }
    }
}
