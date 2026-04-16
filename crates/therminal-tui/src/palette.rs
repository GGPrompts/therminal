//! Color palette for the TUI — dark theme tuned for terminal backgrounds.
//!
//! Constants use Ratatui's `Color::Rgb` directly. The palette is inspired
//! by therminal's Codex 2031 palette but optimised for 24-bit terminal
//! output rather than GPU surfaces.

use ratatui::style::Color;

// -- Backgrounds --

pub const BG: Color = Color::Rgb(16, 16, 20);
pub const BG_SURFACE: Color = Color::Rgb(26, 26, 32);

// -- Text --

pub const TEXT: Color = Color::Rgb(180, 180, 190);
pub const TEXT_BRIGHT: Color = Color::Rgb(230, 230, 235);
pub const TEXT_MUTED: Color = Color::Rgb(100, 100, 112);

// -- Accents --

pub const ACCENT_COOL: Color = Color::Rgb(80, 140, 220);
pub const ACCENT_WARM: Color = Color::Rgb(220, 140, 60);
pub const WARM: Color = Color::Rgb(200, 160, 60);
pub const HOT: Color = Color::Rgb(230, 100, 50);

// -- Status --

pub const STATUS_OK: Color = Color::Rgb(60, 180, 100);
pub const STATUS_WARN: Color = Color::Rgb(220, 180, 40);
pub const STATUS_ERROR: Color = Color::Rgb(200, 60, 60);
