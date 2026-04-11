//! Unit tests for the settings overlay model + theme palettes.

use super::sections::{SettingsRenderValues, ui_text_scale_index};
use super::state::SettingsOverlayState;
use super::theme::apply_theme_preset;
use super::types::{
    ControlBinding, ControlType, SettingsCommand, SettingsControl, SettingsFocus, SettingsSection,
    ThemePreset,
};
use therminal_core::config::ColorsConfig;

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
fn select_up_down_arrows_cycle_options() {
    let mut state = SettingsOverlayState::new();
    state.register_section(SettingsSection::new(
        "ts2",
        "TS2",
        vec![SettingsControl::with_type(
            "D",
            ControlBinding::ToggleStatusBar,
            ControlType::select(vec!["X".into(), "Y".into(), "Z".into()], 0),
        )],
    ));
    let target = state.sections().len() - 1;
    for _ in 0..target {
        state.arrow_down();
    }
    state.tab(false);
    // Down arrow should cycle forward.
    state.arrow_down();
    if let ControlType::Select { selected, .. } =
        &state.sections[state.selected_section].controls[0].control_type
    {
        assert_eq!(*selected, 1);
    }
    // Up arrow should cycle backward.
    state.arrow_up();
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
#[test]
fn shell_section_is_registered() {
    let state = SettingsOverlayState::new();
    assert!(state.sections().iter().any(|s| s.id == "shell"));
}
#[test]
fn shell_section_rebuilds_from_config() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    let shell = state.sections().iter().find(|s| s.id == "shell").unwrap();
    assert_eq!(shell.controls.len(), 3);
    assert_eq!(shell.controls[0].label, "Default shell");
    assert!(matches!(
        shell.controls[0].control_type,
        ControlType::TextInput { .. }
    ));
    assert_eq!(shell.controls[1].label, "Shell arguments");
    assert!(matches!(
        shell.controls[1].control_type,
        ControlType::TextInput { .. }
    ));
    assert_eq!(shell.controls[2].label, "New pane cwd");
    assert!(matches!(
        shell.controls[2].control_type,
        ControlType::Select { .. }
    ));
}
#[test]
fn shell_default_shell_text_edit_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    // Navigate to "shell" section (index 1: layout=0, shell=1).
    state.arrow_down();
    state.tab(false);
    // Enter to start editing the "Default shell" text input.
    assert_eq!(state.enter(), None);
    assert!(state.is_text_editing());
    state.char_input('/');
    let cmd = state.enter();
    assert!(matches!(cmd, Some(SettingsCommand::SetDefaultShell(_))));
}
#[test]
fn shell_new_pane_cwd_select_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    // Navigate to "shell" section (index 1), then to "New pane cwd" control (index 2).
    state.arrow_down();
    state.tab(false);
    state.arrow_down();
    state.arrow_down();
    // Enter cycles the select and returns a command.
    let cmd = state.enter();
    assert!(matches!(cmd, Some(SettingsCommand::SetNewPaneCwd(1))));
}
#[test]
fn accessibility_section_is_registered() {
    let state = SettingsOverlayState::new();
    assert!(state.sections().iter().any(|s| s.id == "accessibility"));
}
#[test]
fn accessibility_section_rebuilds_from_config() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    let a11y = state
        .sections()
        .iter()
        .find(|s| s.id == "accessibility")
        .unwrap();
    assert_eq!(a11y.controls.len(), 3);
    assert_eq!(a11y.controls[0].label, "High contrast mode");
    assert!(matches!(
        a11y.controls[0].control_type,
        ControlType::Toggle { value: false }
    ));
    assert_eq!(a11y.controls[1].label, "Reduced motion");
    assert!(matches!(
        a11y.controls[1].control_type,
        ControlType::Toggle { value: false }
    ));
    assert_eq!(a11y.controls[2].label, "UI text scale");
    assert!(matches!(
        a11y.controls[2].control_type,
        ControlType::Select { .. }
    ));
}
#[test]
fn accessibility_high_contrast_toggle_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    // Navigate to "accessibility" section (index 4: layout=0, shell=1,
    // hotspots=2, themes=3, accessibility=4).
    for _ in 0..4 {
        state.arrow_down();
    }
    state.tab(false);
    let cmd = state.enter();
    assert_eq!(cmd, Some(SettingsCommand::ToggleHighContrast));
}
#[test]
fn accessibility_ui_text_scale_select_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    // Navigate to "accessibility" section, control index 2 (UI text scale).
    for _ in 0..4 {
        state.arrow_down();
    }
    state.tab(false);
    state.arrow_down();
    state.arrow_down();
    let cmd = state.enter();
    // Cycles from index 1 (100%) to index 2 (125%).
    assert_eq!(cmd, Some(SettingsCommand::SetUiTextScale(2)));
}
#[test]
fn ui_text_scale_index_finds_exact_match() {
    assert_eq!(ui_text_scale_index(1.0), 1);
    assert_eq!(ui_text_scale_index(0.75), 0);
    assert_eq!(ui_text_scale_index(3.0), 6);
}
#[test]
fn ui_text_scale_index_finds_closest_match() {
    // 0.9 is between 0.75 and 1.0, closer to 1.0.
    assert_eq!(ui_text_scale_index(0.9), 1);
    // 1.1 is between 1.0 and 1.25, closer to 1.0.
    assert_eq!(ui_text_scale_index(1.1), 1);
    // 1.15 is exactly between 1.0 and 1.25 — should pick 1.25 (or either).
    let idx = ui_text_scale_index(1.15);
    assert!(idx == 1 || idx == 2);
}
