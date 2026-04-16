//! Settings overlay types: focus, control bindings/types, section + control
//! structs, and the `SettingsCommand` enum the App applies to runtime config.

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
    ApplyThemePreset(ThemePreset),
    // Hotspot controls (tn-avjv.5)
    EditorChainEntry(usize),
    AddEditorChainEntry,
    FolderPaneCommand,
    FolderOpenerEntry(usize),
    AddFolderOpenerEntry,
    // Shell controls (tn-avjv.6)
    DefaultShell,
    ShellArgs,
    NewPaneCwd,
    // Accessibility controls (tn-avjv.6)
    ToggleHighContrast,
    ToggleReducedMotion,
    UiTextScale,
    // Font controls (tn-0zfo)
    FontFamily,
    // Cursor controls (tn-ya01)
    CursorStyle,
    ToggleCursorBlink,
    // Notification controls (tn-ya01)
    BellStyle,
    ToggleAgentWaiting,
    ToggleOsc9Enabled,
    // Terminal controls (tn-ya01)
    ToggleAutoTile,
    ScrollbackLines,
    // Widget controls (tn-ya01)
    ToggleSystemMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // MoveUp/MoveDown wired in future keyboard shortcut pass.
pub(crate) enum SettingsCommand {
    ApplyThemePreset(ThemePreset),
    // Hotspot mutations (tn-avjv.5)
    EditorChainRemove(usize),
    EditorChainEdit(usize, String),
    EditorChainAdd(String),
    EditorChainMoveUp(usize),
    EditorChainMoveDown(usize),
    SetFolderPaneCommand(String),
    FolderOpenerRemove(usize),
    FolderOpenerEdit(usize, String),
    FolderOpenerAdd(String),
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
    // Font mutations (tn-0zfo)
    SetFontFamily(String),
    // Cursor mutations (tn-ya01)
    SetCursorStyle(usize),
    ToggleCursorBlink,
    // Notification mutations (tn-ya01)
    SetBellStyle(usize),
    ToggleAgentWaiting,
    ToggleOsc9Enabled,
    // Terminal mutations (tn-ya01)
    ToggleAutoTile,
    SetScrollbackLines(usize),
    // Widget mutations (tn-ya01)
    ToggleSystemMetrics,
}

impl ControlBinding {
    pub(super) fn command(&self) -> SettingsCommand {
        match self {
            Self::ApplyThemePreset(preset) => SettingsCommand::ApplyThemePreset(*preset),
            Self::EditorChainEntry(idx) => SettingsCommand::EditorChainEdit(*idx, String::new()),
            Self::AddEditorChainEntry => SettingsCommand::EditorChainAdd(String::new()),
            Self::FolderOpenerEntry(idx) => SettingsCommand::FolderOpenerEdit(*idx, String::new()),
            Self::AddFolderOpenerEntry => SettingsCommand::FolderOpenerAdd(String::new()),
            Self::FolderPaneCommand => SettingsCommand::SetFolderPaneCommand(String::new()),
            Self::DefaultShell => SettingsCommand::SetDefaultShell(String::new()),
            Self::ShellArgs => SettingsCommand::SetShellArgs(String::new()),
            Self::NewPaneCwd => SettingsCommand::SetNewPaneCwd(0),
            Self::ToggleHighContrast => SettingsCommand::ToggleHighContrast,
            Self::ToggleReducedMotion => SettingsCommand::ToggleReducedMotion,
            Self::UiTextScale => SettingsCommand::SetUiTextScale(0),
            Self::FontFamily => SettingsCommand::SetFontFamily(String::new()),
            // Cursor (tn-ya01)
            Self::CursorStyle => SettingsCommand::SetCursorStyle(0),
            Self::ToggleCursorBlink => SettingsCommand::ToggleCursorBlink,
            // Notifications (tn-ya01)
            Self::BellStyle => SettingsCommand::SetBellStyle(0),
            Self::ToggleAgentWaiting => SettingsCommand::ToggleAgentWaiting,
            Self::ToggleOsc9Enabled => SettingsCommand::ToggleOsc9Enabled,
            // Terminal (tn-ya01)
            Self::ToggleAutoTile => SettingsCommand::ToggleAutoTile,
            Self::ScrollbackLines => SettingsCommand::SetScrollbackLines(0),
            // Widgets (tn-ya01)
            Self::ToggleSystemMetrics => SettingsCommand::ToggleSystemMetrics,
        }
    }

    /// Build the remove command for ListRow entries (fired on Delete key).
    pub(super) fn remove_command(&self) -> Option<SettingsCommand> {
        match self {
            Self::EditorChainEntry(idx) => Some(SettingsCommand::EditorChainRemove(*idx)),
            Self::FolderOpenerEntry(idx) => Some(SettingsCommand::FolderOpenerRemove(*idx)),
            _ => None,
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
        expanded: bool,
    },
    TextInput {
        value: String,
        cursor: usize,
        editing: bool,
    },
    ListRow {
        display_value: String,
        editing: bool,
        cursor: usize,
        original_value: String,
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
        Self::Select {
            options,
            selected,
            expanded: false,
        }
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
        let value: String = display_value.into();
        Self::ListRow {
            original_value: value.clone(),
            cursor: 0,
            editing: false,
            display_value: value,
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
