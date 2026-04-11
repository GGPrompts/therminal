//! Chrome button dimensions.
//!
//! Color constants used to live here as compile-time `[f32; 4]` values
//! derived from `PaletteColor::*`. Those have been replaced by the runtime
//! [`therminal_core::palette::ChromePalette`] (tn-g7oo) — chrome modules now
//! read each color through `&renderer.chrome_palette.<role>` so theme
//! reloads re-skin the UI immediately. The button dimension constants
//! below are still compile-time values because they describe layout, not
//! color, and don't depend on the active theme.

/// Width of each header button in pixels.
pub(crate) const HEADER_BUTTON_WIDTH: f32 = 24.0;

/// Right-side margin for header buttons.
pub(crate) const HEADER_BUTTON_MARGIN: f32 = 4.0;
