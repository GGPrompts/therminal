//! Section seeding + value sync + per-section rebuild helpers, plus the
//! `SettingsRenderValues` DTO and the UI scale option table.

use super::state::SettingsOverlayState;
use super::types::{ControlBinding, ControlType, SettingsControl, SettingsSection, ThemePreset};

/// Predefined UI text scale options (select control values).
pub(crate) const UI_TEXT_SCALE_OPTIONS: &[f32] = &[0.75, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0];

/// Find the index in [`UI_TEXT_SCALE_OPTIONS`] closest to the given scale.
pub(crate) fn ui_text_scale_index(scale: f32) -> usize {
    UI_TEXT_SCALE_OPTIONS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (scale - **a)
                .abs()
                .partial_cmp(&(scale - **b).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
        .unwrap_or(1) // default to 1.0 (index 1)
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsRenderValues {
    pub editor_chain: Vec<String>,
    pub folder_pane_command: Vec<String>,
    pub folder_opener: Vec<String>,
    // Shell section (tn-avjv.6)
    pub shell: String,
    pub shell_args: String,
    pub new_pane_cwd_index: usize,
    // Accessibility section (tn-avjv.6)
    pub high_contrast: bool,
    pub reduced_motion: bool,
    pub ui_text_scale_index: usize,
}

pub(super) fn editor_chain_label(index: usize) -> &'static str {
    const L: [&str; 16] = [
        "Editor #1",
        "Editor #2",
        "Editor #3",
        "Editor #4",
        "Editor #5",
        "Editor #6",
        "Editor #7",
        "Editor #8",
        "Editor #9",
        "Editor #10",
        "Editor #11",
        "Editor #12",
        "Editor #13",
        "Editor #14",
        "Editor #15",
        "Editor #16",
    ];
    L.get(index).copied().unwrap_or("Editor #?")
}

pub(super) fn folder_opener_label(index: usize) -> &'static str {
    const L: [&str; 16] = [
        "Opener #1",
        "Opener #2",
        "Opener #3",
        "Opener #4",
        "Opener #5",
        "Opener #6",
        "Opener #7",
        "Opener #8",
        "Opener #9",
        "Opener #10",
        "Opener #11",
        "Opener #12",
        "Opener #13",
        "Opener #14",
        "Opener #15",
        "Opener #16",
    ];
    L.get(index).copied().unwrap_or("Opener #?")
}

impl SettingsOverlayState {
    pub(crate) fn sync_toggle_values(&mut self, values: &SettingsRenderValues) {
        for section in &mut self.sections {
            for control in &mut section.controls {
                match (&control.binding, &mut control.control_type) {
                    (ControlBinding::ToggleHighContrast, ControlType::Toggle { value }) => {
                        *value = values.high_contrast;
                    }
                    (ControlBinding::ToggleReducedMotion, ControlType::Toggle { value }) => {
                        *value = values.reduced_motion;
                    }
                    _ => {}
                }
            }
        }
        self.rebuild_hotspots_section(values);
        self.rebuild_shell_section(values);
        self.rebuild_accessibility_section(values);
    }

    fn rebuild_hotspots_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "hotspots") {
            Some(idx) => idx,
            None => return,
        };
        let mut controls = Vec::new();
        for (i, entry) in values.editor_chain.iter().enumerate() {
            controls.push(SettingsControl::with_type(
                editor_chain_label(i),
                ControlBinding::EditorChainEntry(i),
                ControlType::list_row(entry.clone()),
            ));
        }
        let cmd_text = values.folder_pane_command.join(" ");
        controls.push(SettingsControl::with_type(
            "Folder pane command",
            ControlBinding::FolderPaneCommand,
            ControlType::text_input(cmd_text),
        ));
        for (i, entry) in values.folder_opener.iter().enumerate() {
            controls.push(SettingsControl::with_type(
                folder_opener_label(i),
                ControlBinding::FolderOpenerEntry(i),
                ControlType::list_row(entry.clone()),
            ));
        }
        let prev_sel = self
            .selected_control_by_section
            .get(section_idx)
            .copied()
            .unwrap_or(0);
        self.sections[section_idx].controls = controls;
        let max_idx = self.sections[section_idx].controls.len().saturating_sub(1);
        if let Some(sel) = self.selected_control_by_section.get_mut(section_idx) {
            *sel = prev_sel.min(max_idx);
        }
    }

    fn rebuild_shell_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "shell") {
            Some(idx) => idx,
            None => return,
        };
        let cwd_options = vec![
            "Inherit from focused pane".to_string(),
            "Home directory".to_string(),
        ];
        let cwd_selected = values.new_pane_cwd_index;
        let controls = vec![
            SettingsControl::with_type(
                "Default shell",
                ControlBinding::DefaultShell,
                ControlType::text_input(&values.shell),
            ),
            SettingsControl::with_type(
                "Shell arguments",
                ControlBinding::ShellArgs,
                ControlType::text_input(&values.shell_args),
            ),
            SettingsControl::with_type(
                "New pane cwd",
                ControlBinding::NewPaneCwd,
                ControlType::select(cwd_options, cwd_selected),
            ),
        ];
        let prev_sel = self
            .selected_control_by_section
            .get(section_idx)
            .copied()
            .unwrap_or(0);
        self.sections[section_idx].controls = controls;
        let max_idx = self.sections[section_idx].controls.len().saturating_sub(1);
        if let Some(sel) = self.selected_control_by_section.get_mut(section_idx) {
            *sel = prev_sel.min(max_idx);
        }
    }

    fn rebuild_accessibility_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "accessibility") {
            Some(idx) => idx,
            None => return,
        };
        let scale_options: Vec<String> = UI_TEXT_SCALE_OPTIONS
            .iter()
            .map(|s| format!("{:.0}%", s * 100.0))
            .collect();
        let scale_selected = values.ui_text_scale_index;
        let controls = vec![
            SettingsControl::with_type(
                "High contrast mode",
                ControlBinding::ToggleHighContrast,
                ControlType::toggle(values.high_contrast),
            ),
            SettingsControl::with_type(
                "Reduced motion",
                ControlBinding::ToggleReducedMotion,
                ControlType::toggle(values.reduced_motion),
            ),
            SettingsControl::with_type(
                "UI text scale",
                ControlBinding::UiTextScale,
                ControlType::select(scale_options, scale_selected),
            ),
        ];
        let prev_sel = self
            .selected_control_by_section
            .get(section_idx)
            .copied()
            .unwrap_or(0);
        self.sections[section_idx].controls = controls;
        let max_idx = self.sections[section_idx].controls.len().saturating_sub(1);
        if let Some(sel) = self.selected_control_by_section.get_mut(section_idx) {
            *sel = prev_sel.min(max_idx);
        }
    }

    pub(super) fn seed_defaults(&mut self) {
        // tn-t2yd.2: Layout section (show pane headers / show status bar)
        // was removed — those toggles are replaced by the runtime F11
        // focus mode keybinding (`KeyAction::FocusMode`).
        self.register_section(SettingsSection::new("shell", "Shell", vec![]));
        self.register_section(SettingsSection::new("hotspots", "Hotspots", vec![]));
        self.register_section(SettingsSection::new(
            "themes",
            "Theme Presets",
            vec![
                SettingsControl::new(
                    ThemePreset::OriginalTherminal.menu_label(),
                    ControlBinding::ApplyThemePreset(ThemePreset::OriginalTherminal),
                ),
                SettingsControl::new(
                    ThemePreset::Paper.menu_label(),
                    ControlBinding::ApplyThemePreset(ThemePreset::Paper),
                ),
                SettingsControl::new(
                    ThemePreset::TokyoNightLight.menu_label(),
                    ControlBinding::ApplyThemePreset(ThemePreset::TokyoNightLight),
                ),
                SettingsControl::new(
                    ThemePreset::TomorrowNightBright.menu_label(),
                    ControlBinding::ApplyThemePreset(ThemePreset::TomorrowNightBright),
                ),
                SettingsControl::new(
                    ThemePreset::HemisuDark.menu_label(),
                    ControlBinding::ApplyThemePreset(ThemePreset::HemisuDark),
                ),
            ],
        ));
        self.register_section(SettingsSection::new(
            "accessibility",
            "Accessibility",
            vec![],
        ));
    }
}
