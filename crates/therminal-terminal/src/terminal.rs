//! Shared terminal size type.
//!
//! Provides [`TerminalSize`] which implements `alacritty_terminal::grid::Dimensions`,
//! used by both desktop and mobile Terminal wrappers when creating or resizing
//! the alacritty `Term`.
//!
//! The full `Terminal` wrapper struct lives in downstream crates because it
//! depends on platform-specific details (kitty graphics on desktop, bell
//! detection on Android, etc.).

/// Default terminal width in columns.
pub const DEFAULT_COLS: usize = 120;
/// Default terminal height in screen lines.
pub const DEFAULT_ROWS: usize = 36;

/// Grid dimensions for `Term::new` and `Term::resize`.
///
/// This is a simple (columns, lines) pair that satisfies alacritty's
/// `Dimensions` trait.  Both therminal-conductor and therminal-term use this
/// identically.
pub struct TerminalSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl TerminalSize {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }
}

// NOTE: The `Dimensions` trait impl requires `alacritty_terminal` as a
// dependency.  Since therminal-terminal intentionally avoids pulling in
// alacritty_terminal (to keep the crate lightweight and independent),
// downstream crates should implement `Dimensions` for `TerminalSize`
// themselves, or use a newtype wrapper.  The struct fields are public
// so this is trivial:
//
//   impl Dimensions for TerminalSize { ... }
//
// If we later decide to add alacritty_terminal as an optional dep,
// the impl can move here behind a feature flag.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_new() {
        let size = TerminalSize::new(80, 24);
        assert_eq!(size.columns, 80);
        assert_eq!(size.screen_lines, 24);
    }

    #[test]
    fn default_constants() {
        assert_eq!(DEFAULT_COLS, 120);
        assert_eq!(DEFAULT_ROWS, 36);
    }
}
