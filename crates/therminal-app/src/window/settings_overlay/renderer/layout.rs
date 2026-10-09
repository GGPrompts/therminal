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

impl PanelLayout {
    pub(super) fn visible_controls(&self, count: usize, selected: usize) -> std::ops::Range<usize> {
        let capacity = ((self.panel_y + self.panel_h - 24.0 - self.ctrl_start_y) / self.ctrl_row_h)
            .floor()
            .max(1.0) as usize;
        let start = selected
            .saturating_add(1)
            .saturating_sub(capacity)
            .min(count.saturating_sub(capacity));
        start..(start + capacity).min(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_profile_control_stays_visible_in_small_window() {
        let panel = PanelLayout::compute(800, 500);
        let visible = panel.visible_controls(10, 9);
        assert!(visible.contains(&9));
        assert!(visible.start > 0);
        assert!(
            panel.ctrl_start_y + visible.len() as f32 * panel.ctrl_row_h
                <= panel.panel_y + panel.panel_h - 24.0
        );
    }
}
