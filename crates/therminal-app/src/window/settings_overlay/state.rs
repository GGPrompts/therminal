//! `SettingsOverlayState`: holds sections, selection state, and panel rect.
//!
//! Constructor + simple accessors live here. Keyboard event handling is
//! in [`super::nav`]; section seeding + value sync is in [`super::sections`].

use super::types::{ControlType, SettingsFocus, SettingsSection, ThemePreset};

#[derive(Debug, Clone)]
pub(crate) struct SettingsOverlayState {
    pub(super) sections: Vec<SettingsSection>,
    pub(super) selected_section: usize,
    pub(super) selected_control_by_section: Vec<usize>,
    pub(super) focus: SettingsFocus,
    pub(super) panel_rect: Option<[f32; 4]>,
    pub(super) active_theme: Option<ThemePreset>,
}

impl SettingsOverlayState {
    pub(crate) fn new() -> Self {
        let mut s = Self {
            sections: Vec::new(),
            selected_section: 0,
            selected_control_by_section: Vec::new(),
            focus: SettingsFocus::Navigation,
            panel_rect: None,
            active_theme: None,
        };
        s.seed_defaults();
        s
    }

    pub(crate) fn set_panel_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.panel_rect = Some([x, y, w, h]);
    }

    pub(crate) fn contains_point(&self, px: f32, py: f32) -> bool {
        if let Some([x, y, w, h]) = self.panel_rect {
            px >= x && px <= x + w && py >= y && py <= y + h
        } else {
            false
        }
    }

    pub(crate) fn reset_navigation(&mut self) {
        self.selected_section = 0;
        self.focus = SettingsFocus::Navigation;
        for idx in &mut self.selected_control_by_section {
            *idx = 0;
        }
        for section in &mut self.sections {
            for control in &mut section.controls {
                match &mut control.control_type {
                    ControlType::TextInput { editing, .. } => {
                        *editing = false;
                    }
                    ControlType::Select { expanded, .. } => {
                        *expanded = false;
                    }
                    _ => {}
                }
            }
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

    pub(crate) fn set_active_theme(&mut self, preset: ThemePreset) {
        self.active_theme = Some(preset);
    }

    pub(crate) fn active_theme(&self) -> Option<ThemePreset> {
        self.active_theme
    }
}

impl Default for SettingsOverlayState {
    fn default() -> Self {
        Self::new()
    }
}
