//! Settings overlay model + renderer.
//!
//! Split into focused submodules:
//! - [`types`] — `SettingsFocus`, `ThemePreset`, `ControlBinding`,
//!   `SettingsCommand`, `ControlType`, `SettingsControl`, `SettingsSection`.
//! - [`state`] — `SettingsOverlayState` struct, constructor, accessors,
//!   panel rect tracking, navigation reset, section registration.
//! - [`nav`] — keyboard navigation: tab / arrows / enter / space /
//!   char_input / backspace / delete / text cursor / cycle helpers.
//! - [`sections`] — section seeding + value sync + rebuild helpers
//!   (`sync_toggle_values`, `rebuild_font_section`,
//!   `rebuild_hotspots_section`, `rebuild_shell_section`,
//!   `rebuild_accessibility_section`, `rebuild_cursor_section`,
//!   `rebuild_notifications_section`, `rebuild_terminal_section`,
//!   `rebuild_widgets_section`, `seed_defaults`),
//!   `SettingsRenderValues`, label helpers,
//!   `UI_TEXT_SCALE_OPTIONS`, `ui_text_scale_index`,
//!   `FONT_FAMILY_OPTIONS`, `font_family_index`,
//!   `CURSOR_STYLE_OPTIONS`, `cursor_style_index`,
//!   `BELL_STYLE_OPTIONS`, `bell_style_index`,
//!   `SCROLLBACK_OPTIONS`, `scrollback_index`.
//! - [`theme`] — `apply_theme_preset`.
//! - [`renderer`] — `draw_settings_overlay` and its private helpers.

mod nav;
mod renderer;
mod sections;
mod state;
#[cfg(test)]
mod tests;
mod theme;
mod types;

pub(crate) use renderer::draw_settings_overlay;
pub(crate) use sections::{
    FONT_FAMILY_OPTIONS, SCROLLBACK_OPTIONS, SettingsRenderValues, UI_TEXT_SCALE_OPTIONS,
    bell_style_index, cursor_style_index, font_family_index, scrollback_index, ui_text_scale_index,
};
pub(crate) use state::SettingsOverlayState;
pub(crate) use theme::apply_theme_preset;
pub(crate) use types::SettingsCommand;
