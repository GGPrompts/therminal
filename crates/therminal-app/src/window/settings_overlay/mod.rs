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
//!   `rebuild_accessibility_section`, `seed_defaults`),
//!   `SettingsRenderValues`, label helpers,
//!   `UI_TEXT_SCALE_OPTIONS`, `ui_text_scale_index`,
//!   `FONT_FAMILY_OPTIONS`, `font_family_index`.
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
    FONT_FAMILY_OPTIONS, SettingsRenderValues, UI_TEXT_SCALE_OPTIONS, font_family_index,
    ui_text_scale_index,
};
pub(crate) use state::SettingsOverlayState;
pub(crate) use theme::apply_theme_preset;
pub(crate) use types::SettingsCommand;
