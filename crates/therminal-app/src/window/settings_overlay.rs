//! Settings overlay model + renderer.
//!
//! This module provides:
//! - A reusable section/control registration model (`SettingsOverlayState`).
//! - Deterministic keyboard navigation state transitions.
//! - Control type variants (Toggle, Select, TextInput, ListRow) with per-control state.
//! - Control bindings (`SettingsCommand`) that the app applies to runtime config.
//! - A simple two-pane overlay renderer (left nav, right controls) with focus ring.

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

    pub(crate) fn menu_label(self) -> &'static str {
        match self {
            Self::OriginalTherminal => "Original Therminal",
            Self::Paper => "Paper",
            Self::TokyoNightLight => "Tokyo Night Light",
            Self::TomorrowNightBright => "Tomorrow Night Bright",
            Self::HemisuDark => "Hemisu Dark",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlBinding {
    TogglePaneHeaders,
    ToggleStatusBar,
    ToggleTabBar,
    ApplyThemePreset(ThemePreset),
    // Hotspot controls (tn-avjv.5)
    EditorChainEntry(usize),
    FolderPaneCommand,
    FolderOpenerEntry(usize),
    // Shell controls (tn-avjv.6)
    DefaultShell,
    ShellArgs,
    NewPaneCwd,
    // Accessibility controls (tn-avjv.6)
    ToggleHighContrast,
    ToggleReducedMotion,
    UiTextScale,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // MoveUp/MoveDown wired in future keyboard shortcut pass.
pub(crate) enum SettingsCommand {
    TogglePaneHeaders,
    ToggleStatusBar,
    ToggleTabBar,
    ApplyThemePreset(ThemePreset),
    // Hotspot mutations (tn-avjv.5)
    EditorChainRemove(usize),
    EditorChainMoveUp(usize),
    EditorChainMoveDown(usize),
    SetFolderPaneCommand(String),
    FolderOpenerRemove(usize),
    FolderOpenerMoveUp(usize),
    FolderOpenerMoveDown(usize),
    // Shell mutations (tn-avjv.6)
    SetDefaultShell(String),
    SetShellArgs(String),
    SetNewPaneCwd(usize),
    // Accessibility mutations (tn-avjv.6)
    ToggleHighContrast,
    ToggleReducedMotion,
    SetUiTextScale(usize),
}

impl ControlBinding {
    fn command(&self) -> SettingsCommand {
        match self {
            Self::TogglePaneHeaders => SettingsCommand::TogglePaneHeaders,
            Self::ToggleStatusBar => SettingsCommand::ToggleStatusBar,
            Self::ToggleTabBar => SettingsCommand::ToggleTabBar,
            Self::ApplyThemePreset(preset) => SettingsCommand::ApplyThemePreset(*preset),
            Self::EditorChainEntry(idx) => SettingsCommand::EditorChainRemove(*idx),
            Self::FolderOpenerEntry(idx) => SettingsCommand::FolderOpenerRemove(*idx),
            Self::FolderPaneCommand => SettingsCommand::SetFolderPaneCommand(String::new()),
            Self::DefaultShell => SettingsCommand::SetDefaultShell(String::new()),
            Self::ShellArgs => SettingsCommand::SetShellArgs(String::new()),
            Self::NewPaneCwd => SettingsCommand::SetNewPaneCwd(0),
            Self::ToggleHighContrast => SettingsCommand::ToggleHighContrast,
            Self::ToggleReducedMotion => SettingsCommand::ToggleReducedMotion,
            Self::UiTextScale => SettingsCommand::SetUiTextScale(0),
        }
    }
}

// -- Control type variants --

/// Visual/interactive type for a settings control.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants used by downstream settings sections (tn-avjv.5, tn-avjv.6).
pub(crate) enum ControlType {
    Toggle {
        value: bool,
    },
    Select {
        options: Vec<String>,
        selected: usize,
    },
    TextInput {
        value: String,
        cursor: usize,
        editing: bool,
    },
    ListRow {
        display_value: String,
    },
    Action,
}

#[allow(dead_code)]
impl ControlType {
    pub(crate) fn toggle(initial: bool) -> Self {
        Self::Toggle { value: initial }
    }

    pub(crate) fn select(options: Vec<String>, initial: usize) -> Self {
        let selected = if options.is_empty() {
            0
        } else {
            initial.min(options.len() - 1)
        };
        Self::Select { options, selected }
    }

    pub(crate) fn text_input(initial: impl Into<String>) -> Self {
        let value: String = initial.into();
        let cursor = value.len();
        Self::TextInput {
            value,
            cursor,
            editing: false,
        }
    }

    pub(crate) fn list_row(display_value: impl Into<String>) -> Self {
        Self::ListRow {
            display_value: display_value.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsControl {
    pub label: &'static str,
    pub binding: ControlBinding,
    pub control_type: ControlType,
}

impl SettingsControl {
    pub(crate) fn new(label: &'static str, binding: ControlBinding) -> Self {
        Self {
            label,
            binding,
            control_type: ControlType::Action,
        }
    }

    pub(crate) fn with_type(
        label: &'static str,
        binding: ControlBinding,
        control_type: ControlType,
    ) -> Self {
        Self {
            label,
            binding,
            control_type,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsSection {
    #[allow(dead_code)]
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
    panel_rect: Option<[f32; 4]>,
}

impl SettingsOverlayState {
    pub(crate) fn new() -> Self {
        let mut s = Self {
            sections: Vec::new(),
            selected_section: 0,
            selected_control_by_section: Vec::new(),
            focus: SettingsFocus::Navigation,
            panel_rect: None,
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
                if let ControlType::TextInput { editing, .. } = &mut control.control_type {
                    *editing = false;
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
        if self.focus == SettingsFocus::Controls {
            if self.text_cursor_left() {
                return;
            }
            if self.active_control_is_select() {
                self.cycle_select(-1);
                return;
            }
        }
        self.focus = SettingsFocus::Navigation;
    }

    pub(crate) fn arrow_right(&mut self) {
        if self.focus == SettingsFocus::Controls {
            if self.text_cursor_right() {
                return;
            }
            if self.active_control_is_select() {
                self.cycle_select(1);
                return;
            }
        }
        self.focus = SettingsFocus::Controls;
    }

    pub(crate) fn enter(&mut self) -> Option<SettingsCommand> {
        match self.focus {
            SettingsFocus::Navigation => {
                self.focus = SettingsFocus::Controls;
                None
            }
            SettingsFocus::Controls => {
                let idx = self.active_control_index();
                let section = self.sections.get_mut(self.selected_section)?;
                let control = section.controls.get_mut(idx)?;
                match &mut control.control_type {
                    ControlType::Toggle { value } => {
                        *value = !*value;
                        Some(control.binding.command())
                    }
                    ControlType::Select { .. } => {
                        self.cycle_select(1);
                        let section = self.sections.get(self.selected_section)?;
                        let control = section.controls.get(idx)?;
                        Some(self.select_command_for(control))
                    }
                    ControlType::TextInput { value, editing, .. } => {
                        if *editing {
                            *editing = false;
                            Some(match &control.binding {
                                ControlBinding::FolderPaneCommand => {
                                    SettingsCommand::SetFolderPaneCommand(value.clone())
                                }
                                ControlBinding::DefaultShell => {
                                    SettingsCommand::SetDefaultShell(value.clone())
                                }
                                ControlBinding::ShellArgs => {
                                    SettingsCommand::SetShellArgs(value.clone())
                                }
                                other => other.command(),
                            })
                        } else {
                            *editing = true;
                            None
                        }
                    }
                    ControlType::ListRow { .. } | ControlType::Action => {
                        Some(control.binding.command())
                    }
                }
            }
        }
    }

    pub(crate) fn space(&mut self) -> Option<SettingsCommand> {
        if self.focus != SettingsFocus::Controls {
            return None;
        }
        let idx = self.active_control_index();
        let section = self.sections.get_mut(self.selected_section)?;
        let control = section.controls.get_mut(idx)?;
        match &mut control.control_type {
            ControlType::Toggle { value } => {
                *value = !*value;
                Some(control.binding.command())
            }
            ControlType::Select { .. } => {
                self.cycle_select(1);
                let section = self.sections.get(self.selected_section)?;
                let control = section.controls.get(idx)?;
                Some(self.select_command_for(control))
            }
            ControlType::TextInput {
                value,
                cursor,
                editing,
            } => {
                if *editing {
                    value.insert(*cursor, ' ');
                    *cursor += 1;
                }
                None
            }
            _ => None,
        }
    }

    pub(crate) fn char_input(&mut self, ch: char) -> bool {
        if self.focus != SettingsFocus::Controls {
            return false;
        }
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return false;
        };
        if let ControlType::TextInput {
            value,
            cursor,
            editing,
        } = &mut control.control_type
            && *editing
        {
            value.insert(*cursor, ch);
            *cursor += ch.len_utf8();
            return true;
        }
        false
    }

    pub(crate) fn backspace(&mut self) -> bool {
        if self.focus != SettingsFocus::Controls {
            return false;
        }
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return false;
        };
        if let ControlType::TextInput {
            value,
            cursor,
            editing,
        } = &mut control.control_type
            && *editing
            && *cursor > 0
        {
            let prev = value[..*cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            value.drain(prev..*cursor);
            *cursor = prev;
            return true;
        }
        false
    }

    pub(crate) fn delete(&mut self) -> bool {
        if self.focus != SettingsFocus::Controls {
            return false;
        }
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return false;
        };
        if let ControlType::TextInput {
            value,
            cursor,
            editing,
        } = &mut control.control_type
            && *editing
            && *cursor < value.len()
        {
            let next = value[*cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| *cursor + i)
                .unwrap_or(value.len());
            value.drain(*cursor..next);
            return true;
        }
        false
    }

    fn text_cursor_left(&mut self) -> bool {
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return false;
        };
        if let ControlType::TextInput {
            value,
            cursor,
            editing,
        } = &mut control.control_type
            && *editing
            && *cursor > 0
        {
            *cursor = value[..*cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            return true;
        }
        false
    }

    fn text_cursor_right(&mut self) -> bool {
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return false;
        };
        if let ControlType::TextInput {
            value,
            cursor,
            editing,
        } = &mut control.control_type
            && *editing
            && *cursor < value.len()
        {
            *cursor = value[*cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| *cursor + i)
                .unwrap_or(value.len());
            return true;
        }
        false
    }

    pub(crate) fn is_text_editing(&self) -> bool {
        let Some(section) = self.sections.get(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get(self.active_control_index()) else {
            return false;
        };
        matches!(
            control.control_type,
            ControlType::TextInput { editing: true, .. }
        )
    }

    pub(crate) fn cancel_text_editing(&mut self) -> bool {
        let ctrl_idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(ctrl_idx) else {
            return false;
        };
        if let ControlType::TextInput { editing, .. } = &mut control.control_type
            && *editing
        {
            *editing = false;
            return true;
        }
        false
    }

    fn cycle_select(&mut self, delta: i32) {
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return;
        };
        if let ControlType::Select { options, selected } = &mut control.control_type {
            if options.is_empty() {
                return;
            }
            let len = options.len() as i32;
            *selected = ((*selected as i32 + delta).rem_euclid(len)) as usize;
        }
    }

    /// Produce a [`SettingsCommand`] for a `Select` control, embedding the
    /// current `selected` index into bindings that need it.
    fn select_command_for(&self, control: &SettingsControl) -> SettingsCommand {
        let selected = match &control.control_type {
            ControlType::Select { selected, .. } => *selected,
            _ => 0,
        };
        match &control.binding {
            ControlBinding::NewPaneCwd => SettingsCommand::SetNewPaneCwd(selected),
            ControlBinding::UiTextScale => SettingsCommand::SetUiTextScale(selected),
            _ => control.binding.command(),
        }
    }

    fn active_control_is_select(&self) -> bool {
        self.sections
            .get(self.selected_section)
            .and_then(|s| s.controls.get(self.active_control_index()))
            .is_some_and(|c| matches!(c.control_type, ControlType::Select { .. }))
    }

    pub(crate) fn sync_toggle_values(&mut self, values: &SettingsRenderValues) {
        for section in &mut self.sections {
            for control in &mut section.controls {
                match (&control.binding, &mut control.control_type) {
                    (ControlBinding::TogglePaneHeaders, ControlType::Toggle { value }) => {
                        *value = values.show_pane_headers;
                    }
                    (ControlBinding::ToggleStatusBar, ControlType::Toggle { value }) => {
                        *value = values.show_status_bar;
                    }
                    (ControlBinding::ToggleTabBar, ControlType::Toggle { value }) => {
                        *value = values.show_tab_bar;
                    }
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
                SettingsControl::with_type(
                    "Show pane headers",
                    ControlBinding::TogglePaneHeaders,
                    ControlType::toggle(true),
                ),
                SettingsControl::with_type(
                    "Show status bar",
                    ControlBinding::ToggleStatusBar,
                    ControlType::toggle(true),
                ),
                SettingsControl::with_type(
                    "Show tab bar",
                    ControlBinding::ToggleTabBar,
                    ControlType::toggle(true),
                ),
            ],
        ));
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

impl Default for SettingsOverlayState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsRenderValues {
    pub show_pane_headers: bool,
    pub show_status_bar: bool,
    pub show_tab_bar: bool,
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

fn editor_chain_label(index: usize) -> &'static str {
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

fn folder_opener_label(index: usize) -> &'static str {
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
                "#000000", "#b13a24", "#216609", "#7a5f00", "#1659b7", "#5c21a5", "#106c66",
                "#5f6673", "#555555", "#b13a24", "#216609", "#7a5f00", "#1659b7", "#5c21a5",
                "#106c66", "#5f6673",
            ],
        ),
        ThemePreset::TokyoNightLight => (
            "#D5D6DB",
            "#4B5563",
            "#4B5563",
            [
                "#0F0F14", "#8C4351", "#485E30", "#7B4F10", "#34548A", "#5A4A78", "#0F4B6E",
                "#343B58", "#4B5563", "#8C4351", "#485E30", "#7B4F10", "#34548A", "#5A4A78",
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
    state: &mut SettingsOverlayState,
    _values: SettingsRenderValues,
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
    state.set_panel_rect(panel_x, panel_y, panel_w, panel_h);

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

    let ctrl_row_h = 36.0_f32;
    let ctrl_start_y = panel_y + 112.0;
    let focus_ring_border = [0.34, 0.65, 1.0, 0.6];
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
        let text_cursor_color = [0.34, 0.65, 1.0, 0.9];
        let value_col_x = content_x + 28.0 + (panel_w - nav_w - 56.0) * 0.55;
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
                ControlType::TextInput {
                    cursor, editing, ..
                } => {
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
                        let char_w = 9.0_f32;
                        let cursor_x =
                            value_col_x + 4.0 + (*cursor as f32 * char_w).min(field_w - 8.0);
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
                ControlType::ListRow { .. } | ControlType::Action => {}
            }
        }
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
    let signal = GlyphColor::rgba(
        PaletteColor::SIGNAL.r,
        PaletteColor::SIGNAL.g,
        PaletteColor::SIGNAL.b,
        255,
    );

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
    let hint = if state.is_text_editing() {
        "Type to edit, Enter to confirm, Esc to cancel"
    } else {
        "Tab/Shift+Tab focus, Arrows move, Enter/Space activate, Esc close"
    };
    add_text(
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
                } => {
                    add_text(
                        truncate_for_width(&format!("{marker} {}", control.label), label_width),
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
                ControlType::ListRow { display_value } => {
                    add_text(
                        truncate_for_width(&format!("{marker} {}", control.label), label_width),
                        content_x + 28.0,
                        row_y,
                        label_width,
                        32.0,
                        metrics,
                        row_color,
                        Weight::NORMAL,
                    );
                    add_text(
                        truncate_for_width(display_value, value_width),
                        value_col_x + 4.0,
                        row_y + 2.0,
                        value_width,
                        24.0,
                        metrics,
                        muted,
                        Weight::NORMAL,
                    );
                }
                ControlType::Action => {
                    add_text(
                        truncate_for_width(&format!("{marker} {}", control.label), row_width),
                        content_x + 28.0,
                        row_y,
                        row_width,
                        32.0,
                        metrics,
                        row_color,
                        Weight::NORMAL,
                    );
                }
            }
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
        0.2126 * srgb_to_linear(color.r as f64 / 255.0)
            + 0.7152 * srgb_to_linear(color.g as f64 / 255.0)
            + 0.0722 * srgb_to_linear(color.b as f64 / 255.0)
    }
    fn contrast_ratio(a: &str, b: &str) -> f64 {
        let l1 = relative_luminance(a);
        let l2 = relative_luminance(b);
        let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }
    fn assert_min_ansi_contrast(colors: &ColorsConfig, min_contrast: f64, theme_name: &str) {
        let background = colors.background.as_deref().unwrap_or("#000000");
        let ansi = colors.ansi.as_ref().expect("theme should set ANSI palette");
        for (idx, color) in ansi.iter().enumerate() {
            let ratio = contrast_ratio(background, color);
            assert!(
                ratio >= min_contrast,
                "{theme_name} ANSI[{idx}] contrast too low: {ratio:.2} ({color} on {background})"
            );
        }
    }

    fn test_render_values() -> SettingsRenderValues {
        SettingsRenderValues {
            show_pane_headers: true,
            show_status_bar: true,
            show_tab_bar: true,
            editor_chain: vec!["$VISUAL".into(), "$EDITOR".into(), "code".into()],
            folder_pane_command: vec!["tfe".into(), "{path}".into()],
            folder_opener: vec!["$FILE_MANAGER".into(), "xdg-open".into()],
            shell: String::new(),
            shell_args: String::new(),
            new_pane_cwd_index: 0,
            high_contrast: false,
            reduced_motion: false,
            ui_text_scale_index: 1,
        }
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
        state.sync_toggle_values(&test_render_values());
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
    fn enter_from_nav_then_enter_toggles() {
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
        for _ in 0..base {
            state.arrow_down();
        }
        assert_eq!(state.active_section().map(|s| s.id), Some("test"));
    }
    #[test]
    fn toggle_space_flips_value() {
        let mut state = SettingsOverlayState::new();
        state.tab(false);
        assert_eq!(state.space(), Some(SettingsCommand::TogglePaneHeaders));
    }
    #[test]
    fn select_arrows_cycle_options() {
        let mut state = SettingsOverlayState::new();
        state.register_section(SettingsSection::new(
            "ts",
            "TS",
            vec![SettingsControl::with_type(
                "C",
                ControlBinding::ToggleStatusBar,
                ControlType::select(vec!["A".into(), "B".into(), "C".into()], 0),
            )],
        ));
        let target = state.sections().len() - 1;
        for _ in 0..target {
            state.arrow_down();
        }
        state.tab(false);
        state.arrow_right();
        if let ControlType::Select { selected, .. } =
            &state.sections[state.selected_section].controls[0].control_type
        {
            assert_eq!(*selected, 1);
        }
        state.arrow_left();
        if let ControlType::Select { selected, .. } =
            &state.sections[state.selected_section].controls[0].control_type
        {
            assert_eq!(*selected, 0);
        }
    }
    #[test]
    fn text_input_enter_edits_then_confirms() {
        let mut state = SettingsOverlayState::new();
        state.register_section(SettingsSection::new(
            "tt",
            "TT",
            vec![SettingsControl::with_type(
                "N",
                ControlBinding::ToggleStatusBar,
                ControlType::text_input("hello"),
            )],
        ));
        let target = state.sections().len() - 1;
        for _ in 0..target {
            state.arrow_down();
        }
        state.tab(false);
        assert!(!state.is_text_editing());
        assert_eq!(state.enter(), None);
        assert!(state.is_text_editing());
        assert!(state.char_input('!'));
        assert!(state.enter().is_some());
        assert!(!state.is_text_editing());
        if let ControlType::TextInput { value, .. } =
            &state.sections[state.selected_section].controls[0].control_type
        {
            assert_eq!(value, "hello!");
        }
    }
    #[test]
    fn text_input_backspace() {
        let mut state = SettingsOverlayState::new();
        state.register_section(SettingsSection::new(
            "tb",
            "TB",
            vec![SettingsControl::with_type(
                "P",
                ControlBinding::ToggleStatusBar,
                ControlType::text_input("abc"),
            )],
        ));
        let target = state.sections().len() - 1;
        for _ in 0..target {
            state.arrow_down();
        }
        state.tab(false);
        state.enter();
        assert!(state.backspace());
        if let ControlType::TextInput { value, cursor, .. } =
            &state.sections[state.selected_section].controls[0].control_type
        {
            assert_eq!(value, "ab");
            assert_eq!(*cursor, 2);
        }
        assert!(!state.delete());
    }
    #[test]
    fn escape_cancels_text_editing() {
        let mut state = SettingsOverlayState::new();
        state.register_section(SettingsSection::new(
            "te",
            "TE",
            vec![SettingsControl::with_type(
                "F",
                ControlBinding::ToggleStatusBar,
                ControlType::text_input(""),
            )],
        ));
        let target = state.sections().len() - 1;
        for _ in 0..target {
            state.arrow_down();
        }
        state.tab(false);
        state.enter();
        assert!(state.is_text_editing());
        assert!(state.cancel_text_editing());
        assert!(!state.is_text_editing());
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
        const MIN_CONTRAST: f64 = 4.5;
        let mut colors = ColorsConfig::default();
        for preset in [
            ThemePreset::OriginalTherminal,
            ThemePreset::Paper,
            ThemePreset::TokyoNightLight,
            ThemePreset::TomorrowNightBright,
            ThemePreset::HemisuDark,
        ] {
            apply_theme_preset(&mut colors, preset);
            let ratio = contrast_ratio(
                colors.background.as_deref().unwrap_or("#000000"),
                colors.foreground.as_deref().unwrap_or("#ffffff"),
            );
            assert!(
                ratio >= MIN_CONTRAST,
                "{:?} contrast too low: {ratio:.2}",
                preset
            );
        }
        apply_theme_preset(&mut colors, ThemePreset::Paper);
        assert_min_ansi_contrast(&colors, MIN_CONTRAST, "Paper");
        apply_theme_preset(&mut colors, ThemePreset::TokyoNightLight);
        assert_min_ansi_contrast(&colors, MIN_CONTRAST, "TokyoNightLight");
    }
    #[test]
    fn hotspots_section_is_registered() {
        let state = SettingsOverlayState::new();
        assert!(state.sections().iter().any(|s| s.id == "hotspots"));
    }
    #[test]
    fn hotspots_section_rebuilds_from_config() {
        let mut state = SettingsOverlayState::new();
        state.sync_toggle_values(&test_render_values());
        let hotspots = state
            .sections()
            .iter()
            .find(|s| s.id == "hotspots")
            .unwrap();
        assert_eq!(hotspots.controls.len(), 6);
        assert!(matches!(
            hotspots.controls[0].control_type,
            ControlType::ListRow { .. }
        ));
        assert_eq!(hotspots.controls[0].label, "Editor #1");
        assert!(matches!(
            hotspots.controls[3].control_type,
            ControlType::TextInput { .. }
        ));
        assert_eq!(hotspots.controls[3].label, "Folder pane command");
        assert!(matches!(
            hotspots.controls[4].control_type,
            ControlType::ListRow { .. }
        ));
        assert_eq!(hotspots.controls[4].label, "Opener #1");
    }
    #[test]
    fn editor_chain_remove_produces_command() {
        let mut state = SettingsOverlayState::new();
        state.sync_toggle_values(&test_render_values());
        // Navigate to "hotspots" section (index 2: layout=0, shell=1, hotspots=2).
        state.arrow_down();
        state.arrow_down();
        state.tab(false);
        let cmd = state.enter();
        assert_eq!(cmd, Some(SettingsCommand::EditorChainRemove(0)));
    }
    #[test]
    fn folder_pane_command_text_edit_produces_command() {
        let mut state = SettingsOverlayState::new();
        state.sync_toggle_values(&test_render_values());
        // Navigate to "hotspots" section (index 2: layout=0, shell=1, hotspots=2).
        state.arrow_down();
        state.arrow_down();
        state.tab(false);
        state.arrow_down();
        state.arrow_down();
        state.arrow_down();
        assert_eq!(state.enter(), None);
        assert!(state.is_text_editing());
        let cmd = state.enter();
        assert!(matches!(
            cmd,
            Some(SettingsCommand::SetFolderPaneCommand(_))
        ));
    }
}
