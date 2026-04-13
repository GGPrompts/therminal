//! Keyboard navigation and editing event handlers for the settings overlay.
//!
//! All methods here are pure state mutations on `SettingsOverlayState` —
//! they don't touch the renderer or the App config directly. Each event
//! that produces an actionable change returns a `SettingsCommand` that
//! the App applies to runtime config separately.

use super::state::SettingsOverlayState;
use super::types::{ControlBinding, ControlType, SettingsCommand, SettingsFocus};

impl SettingsOverlayState {
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
            SettingsFocus::Controls => {
                if self.active_control_is_select_expanded() {
                    self.cycle_select(-1);
                } else {
                    self.move_control(-1);
                }
            }
        }
    }

    pub(crate) fn arrow_down(&mut self) {
        match self.focus {
            SettingsFocus::Navigation => self.move_section(1),
            SettingsFocus::Controls => {
                if self.active_control_is_select_expanded() {
                    self.cycle_select(1);
                } else {
                    self.move_control(1);
                }
            }
        }
    }

    pub(crate) fn arrow_left(&mut self) {
        if self.focus == SettingsFocus::Controls {
            if self.text_cursor_left() {
                return;
            }
            if self.active_control_is_select_expanded() {
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
            if self.active_control_is_select_expanded() {
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
                    ControlType::Select {
                        expanded, selected, ..
                    } => {
                        if *expanded {
                            // Confirm selection and collapse.
                            *expanded = false;
                            let sel = *selected;
                            Some(Self::select_command_inline(&control.binding, sel))
                        } else {
                            // Expand the dropdown for arrow key cycling.
                            *expanded = true;
                            None
                        }
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
                    ControlType::ListRow {
                        display_value,
                        editing,
                        cursor,
                        original_value,
                    } => {
                        if *editing {
                            *editing = false;
                            let val = display_value.clone();
                            Some(match &control.binding {
                                ControlBinding::EditorChainEntry(i) => {
                                    SettingsCommand::EditorChainEdit(*i, val)
                                }
                                ControlBinding::FolderOpenerEntry(i) => {
                                    SettingsCommand::FolderOpenerEdit(*i, val)
                                }
                                other => other.command(),
                            })
                        } else {
                            *editing = true;
                            *cursor = display_value.len();
                            *original_value = display_value.clone();
                            None
                        }
                    }
                    ControlType::Action => Some(control.binding.command()),
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
            ControlType::Select {
                expanded, selected, ..
            } => {
                if *expanded {
                    // Confirm selection and collapse.
                    *expanded = false;
                    let sel = *selected;
                    Some(Self::select_command_inline(&control.binding, sel))
                } else {
                    // Expand the dropdown for arrow key cycling.
                    *expanded = true;
                    None
                }
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
            ControlType::ListRow {
                display_value,
                cursor,
                editing,
                ..
            } => {
                if *editing {
                    display_value.insert(*cursor, ' ');
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
        match &mut control.control_type {
            ControlType::TextInput {
                value,
                cursor,
                editing,
            } if *editing => {
                value.insert(*cursor, ch);
                *cursor += ch.len_utf8();
                return true;
            }
            ControlType::ListRow {
                display_value,
                cursor,
                editing,
                ..
            } if *editing => {
                display_value.insert(*cursor, ch);
                *cursor += ch.len_utf8();
                return true;
            }
            _ => {}
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
        match &mut control.control_type {
            ControlType::TextInput {
                value,
                cursor,
                editing,
            } if *editing && *cursor > 0 => {
                let prev = value[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                value.drain(prev..*cursor);
                *cursor = prev;
                true
            }
            ControlType::ListRow {
                display_value,
                cursor,
                editing,
                ..
            } if *editing && *cursor > 0 => {
                let prev = display_value[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                display_value.drain(prev..*cursor);
                *cursor = prev;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn delete(&mut self) -> Option<SettingsCommand> {
        if self.focus != SettingsFocus::Controls {
            return None;
        }
        let idx = self.active_control_index();
        let section = self.sections.get_mut(self.selected_section)?;
        let control = section.controls.get_mut(idx)?;
        match &mut control.control_type {
            ControlType::TextInput {
                value,
                cursor,
                editing,
            } if *editing && *cursor < value.len() => {
                let next = value[*cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| *cursor + i)
                    .unwrap_or(value.len());
                value.drain(*cursor..next);
                None
            }
            ControlType::ListRow {
                display_value,
                cursor,
                editing,
                ..
            } => {
                if *editing && *cursor < display_value.len() {
                    // Delete char at cursor while editing.
                    let next = display_value[*cursor..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| *cursor + i)
                        .unwrap_or(display_value.len());
                    display_value.drain(*cursor..next);
                    None
                } else if !*editing {
                    // Delete key on non-editing ListRow removes the entry.
                    control.binding.remove_command()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn text_cursor_left(&mut self) -> bool {
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return false;
        };
        match &mut control.control_type {
            ControlType::TextInput {
                value,
                cursor,
                editing,
            } if *editing && *cursor > 0 => {
                *cursor = value[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                true
            }
            ControlType::ListRow {
                display_value,
                cursor,
                editing,
                ..
            } if *editing && *cursor > 0 => {
                *cursor = display_value[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                true
            }
            _ => false,
        }
    }

    fn text_cursor_right(&mut self) -> bool {
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return false;
        };
        match &mut control.control_type {
            ControlType::TextInput {
                value,
                cursor,
                editing,
            } if *editing && *cursor < value.len() => {
                *cursor = value[*cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| *cursor + i)
                    .unwrap_or(value.len());
                true
            }
            ControlType::ListRow {
                display_value,
                cursor,
                editing,
                ..
            } if *editing && *cursor < display_value.len() => {
                *cursor = display_value[*cursor..]
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| *cursor + i)
                    .unwrap_or(display_value.len());
                true
            }
            _ => false,
        }
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
                | ControlType::ListRow { editing: true, .. }
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
        match &mut control.control_type {
            ControlType::TextInput { editing, .. } if *editing => {
                *editing = false;
                true
            }
            ControlType::ListRow {
                display_value,
                editing,
                original_value,
                ..
            } if *editing => {
                *display_value = original_value.clone();
                *editing = false;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn is_select_expanded(&self) -> bool {
        let Some(section) = self.sections.get(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get(self.active_control_index()) else {
            return false;
        };
        matches!(
            control.control_type,
            ControlType::Select { expanded: true, .. }
        )
    }

    pub(crate) fn cancel_select_expanded(&mut self) -> bool {
        let ctrl_idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return false;
        };
        let Some(control) = section.controls.get_mut(ctrl_idx) else {
            return false;
        };
        if let ControlType::Select { expanded, .. } = &mut control.control_type
            && *expanded
        {
            *expanded = false;
            return true;
        }
        false
    }

    pub(super) fn cycle_select(&mut self, delta: i32) {
        let idx = self.active_control_index();
        let Some(section) = self.sections.get_mut(self.selected_section) else {
            return;
        };
        let Some(control) = section.controls.get_mut(idx) else {
            return;
        };
        if let ControlType::Select {
            options, selected, ..
        } = &mut control.control_type
        {
            if options.is_empty() {
                return;
            }
            let len = options.len() as i32;
            *selected = ((*selected as i32 + delta).rem_euclid(len)) as usize;
        }
    }

    /// Produce a [`SettingsCommand`] for a `Select` control, embedding the
    /// current `selected` index into bindings that need it.
    fn select_command_inline(binding: &ControlBinding, selected: usize) -> SettingsCommand {
        match binding {
            ControlBinding::NewPaneCwd => SettingsCommand::SetNewPaneCwd(selected),
            ControlBinding::UiTextScale => SettingsCommand::SetUiTextScale(selected),
            _ => binding.command(),
        }
    }

    fn active_control_is_select_expanded(&self) -> bool {
        self.sections
            .get(self.selected_section)
            .and_then(|s| s.controls.get(self.active_control_index()))
            .is_some_and(|c| matches!(c.control_type, ControlType::Select { expanded: true, .. }))
    }

    pub(super) fn move_section(&mut self, delta: i32) {
        if self.sections.is_empty() {
            return;
        }
        let len = self.sections.len() as i32;
        let next = (self.selected_section as i32 + delta).rem_euclid(len);
        self.selected_section = next as usize;
        self.clamp_selection();
    }

    pub(super) fn move_control(&mut self, delta: i32) {
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

    pub(super) fn clamp_selection(&mut self) {
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
}
