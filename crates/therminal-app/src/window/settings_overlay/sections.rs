//! Section seeding + value sync + per-section rebuild helpers, plus the
//! `SettingsRenderValues` DTO and the UI scale option table.

use super::state::SettingsOverlayState;
use super::types::{ControlBinding, ControlType, SettingsControl, SettingsSection, ThemePreset};

/// Snapshot of in-progress editing state for a single control.
enum ControlSnapshot {
    SelectEditing {
        selected: usize,
        expanded: bool,
    },
    TextEditing {
        value: String,
        cursor: usize,
    },
    ListRowEditing {
        value: String,
        cursor: usize,
        original: String,
    },
    None,
}

/// Capture editing state for every control in a section so we can restore
/// it after rebuilding the controls vector from config values.
fn snapshot_control_states(controls: &[SettingsControl]) -> Vec<ControlSnapshot> {
    controls
        .iter()
        .map(|c| match &c.control_type {
            ControlType::Select {
                selected, expanded, ..
            } if *expanded => ControlSnapshot::SelectEditing {
                selected: *selected,
                expanded: true,
            },
            ControlType::TextInput {
                value,
                cursor,
                editing,
            } if *editing => ControlSnapshot::TextEditing {
                value: value.clone(),
                cursor: *cursor,
            },
            ControlType::ListRow {
                display_value,
                cursor,
                editing,
                original_value,
            } if *editing => ControlSnapshot::ListRowEditing {
                value: display_value.clone(),
                cursor: *cursor,
                original: original_value.clone(),
            },
            _ => ControlSnapshot::None,
        })
        .collect()
}

/// Restore in-progress editing state that was captured before a rebuild.
fn restore_control_states(controls: &mut [SettingsControl], snapshots: &[ControlSnapshot]) {
    for (control, snapshot) in controls.iter_mut().zip(snapshots.iter()) {
        match (&mut control.control_type, snapshot) {
            (
                ControlType::Select {
                    selected, expanded, ..
                },
                ControlSnapshot::SelectEditing {
                    selected: prev_sel,
                    expanded: prev_exp,
                },
            ) => {
                *selected = *prev_sel;
                *expanded = *prev_exp;
            }
            (
                ControlType::TextInput {
                    value,
                    cursor,
                    editing,
                },
                ControlSnapshot::TextEditing {
                    value: prev_val,
                    cursor: prev_cur,
                },
            ) => {
                *value = prev_val.clone();
                *cursor = *prev_cur;
                *editing = true;
            }
            (
                ControlType::ListRow {
                    display_value,
                    cursor,
                    editing,
                    original_value,
                },
                ControlSnapshot::ListRowEditing {
                    value: prev_val,
                    cursor: prev_cur,
                    original: prev_orig,
                },
            ) => {
                *display_value = prev_val.clone();
                *cursor = *prev_cur;
                *original_value = prev_orig.clone();
                *editing = true;
            }
            _ => {}
        }
    }
}

/// Predefined UI text scale options (select control values).
pub(crate) const UI_TEXT_SCALE_OPTIONS: &[f32] = &[0.75, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0];

/// Predefined scrollback line count options (tn-ya01).
pub(crate) const SCROLLBACK_OPTIONS: &[usize] = &[1_000, 5_000, 10_000, 50_000, 100_000];

/// Cursor style option labels (tn-ya01).
pub(crate) const CURSOR_STYLE_OPTIONS: &[&str] = &["Block", "Underline", "Beam", "Hollow Block"];

/// Bell style option labels (tn-ya01). Audible is excluded (falls back to
/// Taskbar) per the task spec.
pub(crate) const BELL_STYLE_OPTIONS: &[&str] = &["Taskbar", "Visual", "None"];

/// Predefined font family options for the unified font selector (tn-0zfo).
///
/// Only Mono Nerd Font variants are listed — these are monospaced (required
/// for a terminal grid) and ship Nerd Font icons (CSD buttons, powerline,
/// devicons). Plain and non-mono variants are excluded because they render
/// poorly in the fixed-width cell grid and WSL2 font enumeration surfaces
/// fewer families, making a curated list essential (tn-a8s9).
pub(crate) const FONT_FAMILY_OPTIONS: &[&str] = &[
    "JetBrainsMono Nerd Font Mono",
    "FiraCode Nerd Font Mono",
    "CaskaydiaCove Nerd Font Mono",
    "Hack Nerd Font Mono",
    "Inconsolata Nerd Font Mono",
    "SourceCodePro Nerd Font Mono",
    "UbuntuMono Nerd Font Mono",
    "Iosevka Nerd Font Mono",
    "RobotoMono Nerd Font Mono",
    "MesloLGS Nerd Font Mono",
];

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

/// Find the closest index in [`SCROLLBACK_OPTIONS`] for a given line count.
pub(crate) fn scrollback_index(lines: usize) -> usize {
    SCROLLBACK_OPTIONS
        .iter()
        .enumerate()
        .min_by_key(|(_, opt)| (**opt as i64 - lines as i64).unsigned_abs())
        .map(|(i, _)| i)
        .unwrap_or(2) // default to 10_000 (index 2)
}

/// Map a `CursorStyle` config value to an index in [`CURSOR_STYLE_OPTIONS`].
pub(crate) fn cursor_style_index(style: &therminal_core::config::CursorStyle) -> usize {
    use therminal_core::config::CursorStyle;
    match style {
        CursorStyle::Block => 0,
        CursorStyle::Underline => 1,
        CursorStyle::Beam => 2,
        CursorStyle::HollowBlock => 3,
    }
}

/// Map a `BellStyle` config value to an index in [`BELL_STYLE_OPTIONS`].
pub(crate) fn bell_style_index(style: &therminal_core::config::BellStyle) -> usize {
    use therminal_core::config::BellStyle;
    match style {
        BellStyle::Taskbar => 0,
        BellStyle::Visual => 1,
        // Audible falls back to Taskbar in practice.
        BellStyle::Audible => 0,
        BellStyle::None => 2,
    }
}

/// Find the index in [`FONT_FAMILY_OPTIONS`] matching the given family name.
/// Returns `None` when the family is not in the predefined list (custom value).
pub(crate) fn font_family_index(family: &str) -> Option<usize> {
    FONT_FAMILY_OPTIONS
        .iter()
        .position(|&opt| opt.eq_ignore_ascii_case(family))
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
    // Font section (tn-0zfo)
    pub font_family_index: Option<usize>,
    /// Subset of FONT_FAMILY_OPTIONS that are actually installed on the system.
    pub available_font_families: Vec<String>,
    // Cursor section (tn-ya01)
    pub cursor_style_index: usize,
    pub cursor_blink: bool,
    // Notification section (tn-ya01)
    pub bell_style_index: usize,
    pub agent_waiting: bool,
    pub osc9_enabled: bool,
    // Terminal section (tn-ya01)
    pub auto_tile: bool,
    pub scrollback_index: usize,
    // Widgets section (tn-ya01)
    pub system_metrics_enabled: bool,
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
                    (ControlBinding::ToggleCursorBlink, ControlType::Toggle { value }) => {
                        *value = values.cursor_blink;
                    }
                    (ControlBinding::ToggleAgentWaiting, ControlType::Toggle { value }) => {
                        *value = values.agent_waiting;
                    }
                    (ControlBinding::ToggleOsc9Enabled, ControlType::Toggle { value }) => {
                        *value = values.osc9_enabled;
                    }
                    (ControlBinding::ToggleAutoTile, ControlType::Toggle { value }) => {
                        *value = values.auto_tile;
                    }
                    (ControlBinding::ToggleSystemMetrics, ControlType::Toggle { value }) => {
                        *value = values.system_metrics_enabled;
                    }
                    _ => {}
                }
            }
        }
        self.rebuild_font_section(values);
        self.rebuild_hotspots_section(values);
        self.rebuild_shell_section(values);
        self.rebuild_accessibility_section(values);
        self.rebuild_cursor_section(values);
        self.rebuild_notifications_section(values);
        self.rebuild_terminal_section(values);
        self.rebuild_widgets_section(values);
    }

    fn rebuild_font_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "font") {
            Some(idx) => idx,
            None => return,
        };
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);

        // Show only fonts that are actually installed on the system.
        // Fall back to the full static list if the availability check
        // returned empty (e.g. font database not yet initialised).
        let options: Vec<String> = if values.available_font_families.is_empty() {
            FONT_FAMILY_OPTIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        } else {
            values.available_font_families.clone()
        };
        // Find the current font in the filtered list (indices differ
        // from FONT_FAMILY_OPTIONS when unavailable fonts are removed).
        let current_family = values
            .font_family_index
            .map(|i| FONT_FAMILY_OPTIONS[i])
            .unwrap_or("");
        let selected = options
            .iter()
            .position(|o| o.eq_ignore_ascii_case(current_family))
            .unwrap_or(0);
        let mut controls = vec![SettingsControl::with_type(
            "Font family",
            ControlBinding::FontFamily,
            ControlType::select(options, selected),
        )];
        restore_control_states(&mut controls, &prev_states);
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

    fn rebuild_hotspots_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "hotspots") {
            Some(idx) => idx,
            None => return,
        };
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);
        let mut controls = Vec::new();
        for (i, entry) in values.editor_chain.iter().enumerate() {
            controls.push(SettingsControl::with_type(
                editor_chain_label(i),
                ControlBinding::EditorChainEntry(i),
                ControlType::list_row(entry.clone()),
            ));
        }
        controls.push(SettingsControl::with_type(
            "+ Add editor",
            ControlBinding::AddEditorChainEntry,
            ControlType::Action,
        ));
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
        controls.push(SettingsControl::with_type(
            "+ Add opener",
            ControlBinding::AddFolderOpenerEntry,
            ControlType::Action,
        ));
        restore_control_states(&mut controls, &prev_states);
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
        // Preserve in-progress Select/TextInput editing state so that
        // sync_toggle_values (called after every key event) doesn't reset
        // the user's mid-edit expanded dropdown or text cursor.
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);

        let cwd_options = vec![
            "Inherit from focused pane".to_string(),
            "Home directory".to_string(),
        ];
        let cwd_selected = values.new_pane_cwd_index;
        let mut controls = vec![
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
        restore_control_states(&mut controls, &prev_states);
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
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);

        let scale_options: Vec<String> = UI_TEXT_SCALE_OPTIONS
            .iter()
            .map(|s| format!("{:.0}%", s * 100.0))
            .collect();
        let scale_selected = values.ui_text_scale_index;
        let mut controls = vec![
            SettingsControl::with_type(
                "High contrast chrome",
                ControlBinding::ToggleHighContrast,
                ControlType::toggle(values.high_contrast),
            ),
            SettingsControl::with_type(
                "Suppress visual bell",
                ControlBinding::ToggleReducedMotion,
                ControlType::toggle(values.reduced_motion),
            ),
            SettingsControl::with_type(
                "UI text scale",
                ControlBinding::UiTextScale,
                ControlType::select(scale_options, scale_selected),
            ),
        ];
        restore_control_states(&mut controls, &prev_states);
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

    fn rebuild_cursor_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "cursor") {
            Some(idx) => idx,
            None => return,
        };
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);

        let style_options: Vec<String> = CURSOR_STYLE_OPTIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut controls = vec![
            SettingsControl::with_type(
                "Cursor style",
                ControlBinding::CursorStyle,
                ControlType::select(style_options, values.cursor_style_index),
            ),
            SettingsControl::with_type(
                "Cursor blink",
                ControlBinding::ToggleCursorBlink,
                ControlType::toggle(values.cursor_blink),
            ),
        ];
        restore_control_states(&mut controls, &prev_states);
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

    fn rebuild_notifications_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "notifications") {
            Some(idx) => idx,
            None => return,
        };
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);

        let bell_options: Vec<String> = BELL_STYLE_OPTIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let mut controls = vec![
            SettingsControl::with_type(
                "Bell style",
                ControlBinding::BellStyle,
                ControlType::select(bell_options, values.bell_style_index),
            ),
            SettingsControl::with_type(
                "Notify when agents wait",
                ControlBinding::ToggleAgentWaiting,
                ControlType::toggle(values.agent_waiting),
            ),
            SettingsControl::with_type(
                "Desktop notifications (OSC 9)",
                ControlBinding::ToggleOsc9Enabled,
                ControlType::toggle(values.osc9_enabled),
            ),
        ];
        restore_control_states(&mut controls, &prev_states);
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

    fn rebuild_terminal_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "terminal") {
            Some(idx) => idx,
            None => return,
        };
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);

        let scrollback_options: Vec<String> = SCROLLBACK_OPTIONS
            .iter()
            .map(|n| {
                // Simple comma-separated thousands formatting.
                let s = n.to_string();
                let mut result = String::new();
                for (i, ch) in s.chars().rev().enumerate() {
                    if i > 0 && i % 3 == 0 {
                        result.insert(0, ',');
                    }
                    result.insert(0, ch);
                }
                result
            })
            .collect();
        let mut controls = vec![
            SettingsControl::with_type(
                "Auto-tile on agent spawn",
                ControlBinding::ToggleAutoTile,
                ControlType::toggle(values.auto_tile),
            ),
            SettingsControl::with_type(
                "Scrollback lines",
                ControlBinding::ScrollbackLines,
                ControlType::select(scrollback_options, values.scrollback_index),
            ),
        ];
        restore_control_states(&mut controls, &prev_states);
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

    fn rebuild_widgets_section(&mut self, values: &SettingsRenderValues) {
        let section_idx = match self.sections.iter().position(|s| s.id == "widgets") {
            Some(idx) => idx,
            None => return,
        };
        let prev_states = snapshot_control_states(&self.sections[section_idx].controls);

        let mut controls = vec![SettingsControl::with_type(
            "Show system metrics",
            ControlBinding::ToggleSystemMetrics,
            ControlType::toggle(values.system_metrics_enabled),
        )];
        restore_control_states(&mut controls, &prev_states);
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
        self.register_section(SettingsSection::new("font", "Font", vec![]));
        self.register_section(SettingsSection::new("cursor", "Cursor", vec![]));
        self.register_section(SettingsSection::new("shell", "Shell", vec![]));
        self.register_section(SettingsSection::new("terminal", "Terminal", vec![]));
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
            "notifications",
            "Notifications",
            vec![],
        ));
        self.register_section(SettingsSection::new("widgets", "Widgets", vec![]));
        self.register_section(SettingsSection::new(
            "accessibility",
            "Accessibility",
            vec![],
        ));
    }
}
