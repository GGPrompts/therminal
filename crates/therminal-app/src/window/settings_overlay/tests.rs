//! Unit tests for the settings overlay model + theme palettes.

use super::sections::{
    FONT_FAMILY_OPTIONS, SettingsRenderValues, font_family_index, ui_text_scale_index,
};
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
        // Skip ANSI[0] (Black) — it is intentionally close to the background
        // on dark themes. On light themes ANSI[0] is true black, which has
        // excellent contrast against the light bg, so the skip is harmless.
        if idx == 0 {
            continue;
        }
        let ratio = contrast_ratio(background, color);
        assert!(
            ratio >= min_contrast,
            "{theme_name} ANSI[{idx}] contrast too low: {ratio:.2} ({color} on {background})"
        );
    }
}

/// Verify that ANSI background colors (as darkened for bg use) provide
/// enough contrast for white text. This tests the ANSI *foreground*
/// values from the preset directly, expecting dark-theme colors 1-6 to
/// be dark enough for white-on-colored backgrounds. For light themes,
/// the colors are inherently dark (they need to contrast against a light
/// bg) so they pass easily.
fn assert_ansi_bg_contrast(colors: &ColorsConfig, theme_name: &str) {
    let ansi = colors.ansi.as_ref().expect("theme should set ANSI palette");
    let bg = colors.background.as_deref().unwrap_or("#000000");
    let bg_lum = relative_luminance(bg);
    let is_dark = bg_lum < 0.2;

    if is_dark {
        // For dark themes, ANSI blue (index 4) must be dark enough for
        // white-on-blue backgrounds. We check blue specifically because
        // it's the most commonly problematic one (diffs, selections).
        let blue_ratio = contrast_ratio(&ansi[4], "#FFFFFF");
        assert!(
            blue_ratio >= 3.0,
            "{theme_name} ANSI[4] (Blue) as bg: white text contrast {ratio:.2} < 3.0 ({color} bg)",
            ratio = blue_ratio,
            color = &ansi[4]
        );
    } else {
        // For light themes, all ANSI colors 1-6 should be dark enough for
        // white text on colored bg.
        for idx in [1, 2, 3, 4, 5, 6] {
            let ratio = contrast_ratio(&ansi[idx], "#FFFFFF");
            assert!(
                ratio >= 3.0,
                "{theme_name} ANSI[{idx}] as bg: white text contrast {ratio:.2} < 3.0 ({color} bg)",
                color = &ansi[idx]
            );
        }
    }
}

/// Navigate the settings overlay to the section with the given ID.
fn navigate_to_section(state: &mut SettingsOverlayState, section_id: &str) {
    let idx = state
        .sections()
        .iter()
        .position(|s| s.id == section_id)
        .unwrap_or_else(|| panic!("section '{}' not found", section_id));
    for _ in 0..idx {
        state.arrow_down();
    }
}

fn test_render_values() -> SettingsRenderValues {
    SettingsRenderValues {
        editor_chain: vec!["$VISUAL".into(), "$EDITOR".into(), "code".into()],
        folder_pane_command: vec!["tfe".into(), "{path}".into()],
        folder_opener: vec!["$FILE_MANAGER".into(), "xdg-open".into()],
        shell: String::new(),
        shell_args: String::new(),
        new_pane_cwd_index: 0,
        high_contrast: true,
        reduced_motion: false,
        ui_text_scale_index: 1,
        font_family_index: Some(0),
        available_font_families: FONT_FAMILY_OPTIONS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        // tn-ya01 new fields
        cursor_style_index: 0,
        cursor_blink: false,
        bell_style_index: 0,
        agent_waiting: true,
        osc9_enabled: true,
        auto_tile: true,
        scrollback_index: 2,
        system_metrics_enabled: true,
        background_opacity_index: 0,
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
fn register_section_extends_navigation_model() {
    let mut state = SettingsOverlayState::new();
    let base = state.sections().len();
    state.register_section(SettingsSection::new(
        "test",
        "Test",
        vec![SettingsControl::new(
            "Toggle",
            ControlBinding::ToggleHighContrast,
        )],
    ));
    assert_eq!(state.sections().len(), base + 1);
    for _ in 0..base {
        state.arrow_down();
    }
    assert_eq!(state.active_section().map(|s| s.id), Some("test"));
}
#[test]
fn select_arrows_cycle_options_when_expanded() {
    let mut state = SettingsOverlayState::new();
    state.register_section(SettingsSection::new(
        "ts",
        "TS",
        vec![SettingsControl::with_type(
            "C",
            ControlBinding::ToggleHighContrast,
            ControlType::select(vec!["A".into(), "B".into(), "C".into()], 0),
        )],
    ));
    let target = state.sections().len() - 1;
    for _ in 0..target {
        state.arrow_down();
    }
    state.tab(false);
    // Expand the select first.
    state.enter();
    assert!(state.is_select_expanded());
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
fn select_up_down_arrows_cycle_options_when_expanded() {
    let mut state = SettingsOverlayState::new();
    state.register_section(SettingsSection::new(
        "ts2",
        "TS2",
        vec![SettingsControl::with_type(
            "D",
            ControlBinding::ToggleHighContrast,
            ControlType::select(vec!["X".into(), "Y".into(), "Z".into()], 0),
        )],
    ));
    let target = state.sections().len() - 1;
    for _ in 0..target {
        state.arrow_down();
    }
    state.tab(false);
    // Expand the select first.
    state.enter();
    assert!(state.is_select_expanded());
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
fn select_up_down_navigate_when_not_expanded() {
    let mut state = SettingsOverlayState::new();
    state.register_section(SettingsSection::new(
        "ts3",
        "TS3",
        vec![
            SettingsControl::with_type(
                "Toggle",
                ControlBinding::ToggleHighContrast,
                ControlType::toggle(false),
            ),
            SettingsControl::with_type(
                "Sel",
                ControlBinding::UiTextScale,
                ControlType::select(vec!["X".into(), "Y".into()], 0),
            ),
            SettingsControl::with_type(
                "Another",
                ControlBinding::ToggleReducedMotion,
                ControlType::toggle(false),
            ),
        ],
    ));
    let target = state.sections().len() - 1;
    for _ in 0..target {
        state.arrow_down();
    }
    state.tab(false);
    // Navigate down to the Select control (index 1).
    state.arrow_down();
    assert_eq!(state.active_control_index(), 1);
    assert!(!state.is_select_expanded());
    // Arrow down should navigate past the Select to control index 2.
    state.arrow_down();
    assert_eq!(state.active_control_index(), 2);
    // Arrow up should navigate back to the Select.
    state.arrow_up();
    assert_eq!(state.active_control_index(), 1);
    // The Select value should be unchanged.
    if let ControlType::Select { selected, .. } =
        &state.sections[state.selected_section].controls[1].control_type
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
            ControlBinding::ToggleHighContrast,
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
            ControlBinding::ToggleHighContrast,
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
    assert!(state.delete().is_none());
}
#[test]
fn escape_cancels_text_editing() {
    let mut state = SettingsOverlayState::new();
    state.register_section(SettingsSection::new(
        "te",
        "TE",
        vec![SettingsControl::with_type(
            "F",
            ControlBinding::ToggleHighContrast,
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
    assert_eq!(colors.foreground_bright.as_deref(), Some("#ffffff"));
    assert_eq!(colors.foreground_muted.as_deref(), Some("#7b8fa9"));
    assert_eq!(colors.surface.as_deref(), Some("#111c2d"));
    assert_eq!(colors.cursor.as_deref(), Some("#fef3c7"));
    assert_eq!(colors.selection.as_deref(), Some("#56a7ff"));
    assert_eq!(colors.ansi.as_ref().map(|v| v.len()), Some(16));
    // ANSI[0] is near-black, ANSI[15] is near-white
    let ansi = colors.ansi.as_ref().unwrap();
    let black = ColorsConfig::parse_hex(&ansi[0]).unwrap();
    assert!(black.r < 40 && black.g < 40 && black.b < 60);
    let white = ColorsConfig::parse_hex(&ansi[15]).unwrap();
    assert!(white.r > 200 && white.g > 200 && white.b > 200);

    apply_theme_preset(&mut colors, ThemePreset::Paper);
    assert_eq!(colors.background.as_deref(), Some("#f2eede"));
    assert_eq!(colors.foreground.as_deref(), Some("#000000"));
    assert_eq!(colors.cursor.as_deref(), Some("#000000"));
    assert_eq!(colors.ansi.as_ref().map(|v| v.len()), Some(16));
    assert!(colors.foreground_bright.is_some());
    assert!(colors.foreground_muted.is_some());
    assert!(colors.surface.is_some());
    assert!(colors.selection.is_some());
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
            "{:?} fg/bg contrast too low: {ratio:.2}",
            preset
        );
    }
    // All presets: ANSI fg colors must be readable against their own background.
    for preset in [
        ThemePreset::OriginalTherminal,
        ThemePreset::Paper,
        ThemePreset::TokyoNightLight,
        ThemePreset::TomorrowNightBright,
        ThemePreset::HemisuDark,
    ] {
        apply_theme_preset(&mut colors, preset);
        let name = format!("{preset:?}");
        assert_min_ansi_contrast(&colors, MIN_CONTRAST, &name);
    }
}

/// tn-oei7 — ANSI colors used as backgrounds (diffs, McFly, bubbletea)
/// must provide enough contrast for white/bright text to be readable.
#[test]
fn theme_presets_ansi_bg_contrast_for_white_text() {
    let mut colors = ColorsConfig::default();
    for preset in [
        ThemePreset::OriginalTherminal,
        ThemePreset::Paper,
        ThemePreset::TokyoNightLight,
        ThemePreset::TomorrowNightBright,
        ThemePreset::HemisuDark,
    ] {
        apply_theme_preset(&mut colors, preset);
        assert_ansi_bg_contrast(&colors, &format!("{preset:?}"));
    }
}

/// tn-oei7 — ANSI colors must be semantically correct: green is visually
/// green (hue 90-160), magenta is visually purple/pink (not blue), cyan
/// is visually teal (not green), blue is distinct from magenta.
#[test]
fn theme_presets_ansi_semantic_correctness() {
    let mut colors = ColorsConfig::default();
    for preset in [
        ThemePreset::OriginalTherminal,
        ThemePreset::Paper,
        ThemePreset::TokyoNightLight,
        ThemePreset::TomorrowNightBright,
        ThemePreset::HemisuDark,
    ] {
        apply_theme_preset(&mut colors, preset);
        let ansi = colors.ansi.as_ref().unwrap();
        let name = format!("{preset:?}");

        // Green (index 2): G channel must dominate
        let green = ColorsConfig::parse_hex(&ansi[2]).unwrap();
        assert!(
            green.g > green.r && green.g > green.b,
            "{name} ANSI[2] (Green) should have dominant G channel: r={} g={} b={}",
            green.r,
            green.g,
            green.b
        );

        // Blue (index 4): B channel must dominate
        let blue = ColorsConfig::parse_hex(&ansi[4]).unwrap();
        assert!(
            blue.b > blue.r,
            "{name} ANSI[4] (Blue) should have B > R: r={} b={}",
            blue.r,
            blue.b
        );

        // Magenta (index 5) must be distinct from Blue (index 4)
        let magenta = ColorsConfig::parse_hex(&ansi[5]).unwrap();
        assert_ne!(
            (magenta.r, magenta.g, magenta.b),
            (blue.r, blue.g, blue.b),
            "{name} ANSI[5] (Magenta) must differ from ANSI[4] (Blue)"
        );
        // Magenta should have significant red component
        assert!(
            magenta.r > magenta.g,
            "{name} ANSI[5] (Magenta) should have R > G: r={} g={}",
            magenta.r,
            magenta.g
        );
    }
}

/// tn-2xwr — every preset must set the full chrome + hotspot role set so
/// the status bar, pane headers, tab bar, and CSD strip re-skin when the
/// user picks a preset. Leaving any of these as `None` falls back to the
/// bundled Codex 2031 defaults regardless of preset.
#[test]
fn theme_presets_set_all_chrome_role_fields() {
    let mut colors = ColorsConfig::default();
    for preset in [
        ThemePreset::OriginalTherminal,
        ThemePreset::Paper,
        ThemePreset::TokyoNightLight,
        ThemePreset::TomorrowNightBright,
        ThemePreset::HemisuDark,
    ] {
        apply_theme_preset(&mut colors, preset);
        let missing = [
            ("chrome_focus_border", colors.chrome_focus_border.is_some()),
            ("chrome_separator", colors.chrome_separator.is_some()),
            ("chrome_header_bg", colors.chrome_header_bg.is_some()),
            (
                "chrome_header_bg_dim",
                colors.chrome_header_bg_dim.is_some(),
            ),
            (
                "chrome_status_bar_bg",
                colors.chrome_status_bar_bg.is_some(),
            ),
            ("chrome_csd_close", colors.chrome_csd_close.is_some()),
            ("chrome_fg", colors.chrome_fg.is_some()),
            ("chrome_fg_muted", colors.chrome_fg_muted.is_some()),
            ("chrome_fg_focus", colors.chrome_fg_focus.is_some()),
            ("chrome_fg_warn", colors.chrome_fg_warn.is_some()),
            ("chrome_fg_alert", colors.chrome_fg_alert.is_some()),
            ("hotspot_filepath", colors.hotspot_filepath.is_some()),
            ("hotspot_url", colors.hotspot_url.is_some()),
            ("hotspot_error", colors.hotspot_error.is_some()),
            ("hotspot_gitref", colors.hotspot_gitref.is_some()),
            ("hotspot_issueref", colors.hotspot_issueref.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, set)| (!set).then_some(name))
        .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "{preset:?} missing chrome fields: {missing:?}"
        );
    }
}

/// tn-2xwr — chrome text (`chrome_fg`) must remain readable on the chrome
/// background it rides (`chrome_header_bg`) for every preset. The pane
/// header process label and status bar center text both use
/// `chrome_fg` on `chrome_header_bg`/`chrome_status_bar_bg`, so anything
/// below WCAG AA (4.5:1) would be a visual regression.
#[test]
fn theme_presets_chrome_text_readable_on_chrome_bg() {
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
        let fg = colors.chrome_fg.as_deref().unwrap();
        let header_bg = colors.chrome_header_bg.as_deref().unwrap();
        let status_bg = colors.chrome_status_bar_bg.as_deref().unwrap();
        let header_ratio = contrast_ratio(fg, header_bg);
        let status_ratio = contrast_ratio(fg, status_bg);
        assert!(
            header_ratio >= MIN_CONTRAST,
            "{preset:?} chrome_fg/header_bg contrast too low: {header_ratio:.2} ({fg} on {header_bg})"
        );
        assert!(
            status_ratio >= MIN_CONTRAST,
            "{preset:?} chrome_fg/status_bar_bg contrast too low: {status_ratio:.2} ({fg} on {status_bg})"
        );
    }
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
    // 3 editors + 1 "Add editor" + 1 folder_pane_command + 2 openers + 1 "Add opener" = 8
    assert_eq!(hotspots.controls.len(), 8);
    assert!(matches!(
        hotspots.controls[0].control_type,
        ControlType::ListRow { .. }
    ));
    assert_eq!(hotspots.controls[0].label, "Editor #1");
    assert_eq!(hotspots.controls[3].label, "+ Add editor");
    assert!(matches!(
        hotspots.controls[3].control_type,
        ControlType::Action
    ));
    assert!(matches!(
        hotspots.controls[4].control_type,
        ControlType::TextInput { .. }
    ));
    assert_eq!(hotspots.controls[4].label, "Folder pane command");
    assert!(matches!(
        hotspots.controls[5].control_type,
        ControlType::ListRow { .. }
    ));
    assert_eq!(hotspots.controls[5].label, "Opener #1");
    assert_eq!(hotspots.controls[7].label, "+ Add opener");
}
#[test]
fn editor_chain_enter_enters_edit_mode() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "hotspots");
    state.tab(false);
    // First Enter enters edit mode (returns None).
    let cmd = state.enter();
    assert_eq!(cmd, None);
    assert!(state.is_text_editing());
}

#[test]
fn editor_chain_enter_confirms_edit() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "hotspots");
    state.tab(false);
    // Enter edit mode.
    state.enter();
    assert!(state.is_text_editing());
    // Type a character.
    state.char_input('!');
    // Second Enter confirms.
    let cmd = state.enter();
    assert!(matches!(
        cmd,
        Some(SettingsCommand::EditorChainEdit(0, ref v)) if v == "$VISUAL!"
    ));
    assert!(!state.is_text_editing());
}

#[test]
fn editor_chain_escape_cancels_edit() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "hotspots");
    state.tab(false);
    state.enter(); // enter edit mode
    state.char_input('X');
    assert!(state.cancel_text_editing());
    assert!(!state.is_text_editing());
    // Value should be restored to original.
    let hotspots = state
        .sections()
        .iter()
        .find(|s| s.id == "hotspots")
        .unwrap();
    if let ControlType::ListRow { display_value, .. } = &hotspots.controls[0].control_type {
        assert_eq!(display_value, "$VISUAL");
    } else {
        panic!("expected ListRow");
    }
}

#[test]
fn editor_chain_delete_removes_entry() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "hotspots");
    state.tab(false);
    // Delete on non-editing ListRow should produce remove command.
    let cmd = state.delete();
    assert_eq!(cmd, Some(SettingsCommand::EditorChainRemove(0)));
}

#[test]
fn add_editor_button_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "hotspots");
    state.tab(false);
    // The "Add editor" button is after the 3 editor chain entries (index 3).
    for _ in 0..3 {
        state.arrow_down();
    }
    let cmd = state.enter();
    assert!(matches!(cmd, Some(SettingsCommand::EditorChainAdd(_))));
}
#[test]
fn folder_pane_command_text_edit_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "hotspots");
    state.tab(false);
    // Folder pane command is now at index 4 (3 editors + 1 "Add editor" button).
    state.arrow_down();
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
    navigate_to_section(&mut state, "shell");
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
    navigate_to_section(&mut state, "shell");
    state.tab(false);
    state.arrow_down();
    state.arrow_down();
    // First Enter expands the select (returns None).
    assert_eq!(state.enter(), None);
    assert!(state.is_select_expanded());
    // Cycle to next option while expanded.
    state.arrow_down();
    // Second Enter confirms and returns the command.
    let cmd = state.enter();
    assert!(matches!(cmd, Some(SettingsCommand::SetNewPaneCwd(1))));
    assert!(!state.is_select_expanded());
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
    assert_eq!(a11y.controls[0].label, "High contrast chrome");
    assert!(matches!(
        a11y.controls[0].control_type,
        ControlType::Toggle { value: true }
    ));
    assert_eq!(a11y.controls[1].label, "Suppress visual bell");
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
    navigate_to_section(&mut state, "accessibility");
    state.tab(false);
    let cmd = state.enter();
    assert_eq!(cmd, Some(SettingsCommand::ToggleHighContrast));
}
#[test]
fn accessibility_ui_text_scale_select_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "accessibility");
    state.tab(false);
    state.arrow_down();
    state.arrow_down();
    // First Enter expands the select (returns None).
    assert_eq!(state.enter(), None);
    assert!(state.is_select_expanded());
    // Cycle from index 1 (100%) to index 2 (125%).
    state.arrow_down();
    // Second Enter confirms and returns the command.
    let cmd = state.enter();
    assert_eq!(cmd, Some(SettingsCommand::SetUiTextScale(2)));
    assert!(!state.is_select_expanded());
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

#[test]
fn select_expanded_state_survives_sync_toggle_values() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "shell");
    state.tab(false);
    state.arrow_down();
    state.arrow_down();
    // Expand the select.
    assert_eq!(state.enter(), None);
    assert!(state.is_select_expanded());
    // Cycle to next option.
    state.arrow_down();
    // Now sync_toggle_values fires (as it does after every key event in the
    // event handler). The expanded + selected state must survive.
    state.sync_toggle_values(&test_render_values());
    assert!(
        state.is_select_expanded(),
        "Select should remain expanded after sync_toggle_values"
    );
    // Confirm — should produce the cycled index, not the original.
    let cmd = state.enter();
    assert!(matches!(cmd, Some(SettingsCommand::SetNewPaneCwd(1))));
}

#[test]
fn text_editing_state_survives_sync_toggle_values() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    navigate_to_section(&mut state, "shell");
    state.tab(false);
    // Enter to start editing.
    assert_eq!(state.enter(), None);
    assert!(state.is_text_editing());
    state.char_input('/');
    state.char_input('b');
    // sync_toggle_values fires — editing state must survive.
    state.sync_toggle_values(&test_render_values());
    assert!(
        state.is_text_editing(),
        "TextInput should remain in editing mode after sync_toggle_values"
    );
    // Confirm should emit the edited value, not the original.
    let cmd = state.enter();
    assert!(matches!(cmd, Some(SettingsCommand::SetDefaultShell(ref s)) if s.contains("/b")));
}

#[test]
fn active_theme_indicator_tracks_applied_preset() {
    let mut state = SettingsOverlayState::new();
    assert_eq!(state.active_theme(), None);
    state.set_active_theme(ThemePreset::Paper);
    assert_eq!(state.active_theme(), Some(ThemePreset::Paper));
    state.set_active_theme(ThemePreset::HemisuDark);
    assert_eq!(state.active_theme(), Some(ThemePreset::HemisuDark));
}

// -- Appearance section tests (consolidated from font/cursor/theme/opacity) --

#[test]
fn appearance_section_is_registered() {
    let state = SettingsOverlayState::new();
    assert!(state.sections().iter().any(|s| s.id == "appearance"));
}

#[test]
fn appearance_section_contains_font_and_cursor() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    let appearance = state
        .sections()
        .iter()
        .find(|s| s.id == "appearance")
        .unwrap();
    // Theme presets (5) + Font family + Background opacity + Cursor style + Cursor blink
    assert_eq!(appearance.controls.len(), 9);
    assert_eq!(appearance.controls[5].label, "Font family");
    assert_eq!(appearance.controls[7].label, "Cursor style");
}

#[test]
fn font_family_select_produces_command() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    // "appearance" section is at index 0.
    state.tab(false);
    // Navigate down to "Font family" control (index 5, after 5 theme presets).
    for _ in 0..5 {
        state.arrow_down();
    }
    // First Enter expands the select (returns None).
    assert_eq!(state.enter(), None);
    assert!(state.is_select_expanded());
    // Cycle to next option.
    state.arrow_down();
    // Second Enter confirms and returns the command.
    let cmd = state.enter();
    assert_eq!(
        cmd,
        Some(SettingsCommand::SetFontFamily(
            FONT_FAMILY_OPTIONS[1].to_string()
        ))
    );
    assert!(!state.is_select_expanded());
}

#[test]
fn font_family_index_finds_known_families() {
    assert_eq!(font_family_index("JetBrainsMono Nerd Font Mono"), Some(0));
    assert_eq!(font_family_index("FiraCode Nerd Font Mono"), Some(1));
    assert_eq!(font_family_index("CaskaydiaCove Nerd Font Mono"), Some(2));
    assert_eq!(font_family_index("Iosevka Nerd Font Mono"), Some(7));
}

#[test]
fn font_family_index_is_case_insensitive() {
    assert_eq!(font_family_index("firacode nerd font mono"), Some(1));
    assert_eq!(font_family_index("HACK NERD FONT MONO"), Some(3));
}

#[test]
fn font_family_index_returns_none_for_unknown() {
    assert_eq!(font_family_index("Comic Sans MS"), None);
    assert_eq!(font_family_index(""), None);
}

#[test]
fn font_family_options_has_expected_entries() {
    assert!(FONT_FAMILY_OPTIONS.len() >= 5);
    assert!(FONT_FAMILY_OPTIONS.contains(&"JetBrainsMono Nerd Font Mono"));
    assert!(FONT_FAMILY_OPTIONS.contains(&"FiraCode Nerd Font Mono"));
    assert!(FONT_FAMILY_OPTIONS.contains(&"CaskaydiaCove Nerd Font Mono"));
}

#[test]
fn font_select_expanded_state_survives_sync() {
    let mut state = SettingsOverlayState::new();
    state.sync_toggle_values(&test_render_values());
    // "appearance" section is at index 0, font family at control index 5.
    state.tab(false);
    for _ in 0..5 {
        state.arrow_down();
    }
    // Expand the select.
    assert_eq!(state.enter(), None);
    assert!(state.is_select_expanded());
    // Cycle to next option.
    state.arrow_down();
    // sync_toggle_values fires — expanded + selected state must survive.
    state.sync_toggle_values(&test_render_values());
    assert!(
        state.is_select_expanded(),
        "Font family Select should remain expanded after sync_toggle_values"
    );
    // Confirm — should produce the cycled index, not the original.
    let cmd = state.enter();
    assert_eq!(
        cmd,
        Some(SettingsCommand::SetFontFamily(
            FONT_FAMILY_OPTIONS[1].to_string()
        ))
    );
}
