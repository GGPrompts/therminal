//! Shared terminal size type and graphics events.
//!
//! Provides [`TerminalSize`] which implements `alacritty_terminal::grid::Dimensions`,
//! used by both desktop and mobile Terminal wrappers when creating or resizing
//! the alacritty `Term`.
//!
//! Also provides [`GraphicsEvent`] — the event stream produced by the Kitty
//! graphics APC parser in [`crate::graphics`]. Emitted by the interceptor when
//! an APC string terminates; downstream renderers consume these to display
//! images without needing the raw PTY bytes.
//!
//! The full `Terminal` wrapper struct lives in downstream crates because it
//! depends on platform-specific details (kitty graphics on desktop, bell
//! detection on Android, etc.).

use crate::graphics::{DeleteScope, GraphicsFormat, GraphicsMedium, RawGraphicsCommand};

/// Default terminal width in columns.
pub const DEFAULT_COLS: usize = 120;
/// Default terminal height in screen lines.
pub const DEFAULT_ROWS: usize = 36;

/// Events produced by the Kitty graphics APC parser.
///
/// These mirror the four protocol actions the parser recognises today
/// (`a=t/T`, `a=p`, `a=d`, `a=q`). Payloads are **not decoded** here — the
/// raw bytes and the format metadata are handed off to the downstream
/// decoder (tn-0htm) which turns them into pixel data.
///
/// Every variant carries the original [`RawGraphicsCommand`] so the daemon /
/// renderer can always reach back to the full key=value map without having
/// to re-parse the APC payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphicsEvent {
    /// `a=t` or `a=T` — transmit image data.
    ///
    /// The payload is the (possibly reassembled) base64-encoded bytes for a
    /// single image. When `display` is `true` the transmission was `a=T`
    /// (transmit-and-display); the renderer should draw the image at the
    /// current cursor position after decoding.
    GraphicsTransmit {
        image_id: Option<u32>,
        placement_id: Option<u32>,
        format: GraphicsFormat,
        medium: GraphicsMedium,
        width_px: Option<u32>,
        height_px: Option<u32>,
        /// Raw (base64-encoded) payload bytes accumulated across `m=1`
        /// chunks. Decoder is responsible for base64 + format-specific work.
        payload: Vec<u8>,
        /// `true` iff the action byte was `T` (transmit-and-display).
        display: bool,
        command: RawGraphicsCommand,
    },
    /// `a=p` — display a previously transmitted image.
    GraphicsDisplay {
        image_id: Option<u32>,
        placement_id: Option<u32>,
        rows: Option<u32>,
        cols: Option<u32>,
        z_index: Option<i32>,
        command: RawGraphicsCommand,
    },
    /// `a=d` — delete one or more images / placements.
    GraphicsDelete {
        scope: DeleteScope,
        command: RawGraphicsCommand,
    },
    /// `a=q` — feature / capability query. The terminal replies with `OK`
    /// through the APC response envelope.
    GraphicsQuery {
        image_id: Option<u32>,
        command: RawGraphicsCommand,
    },
}

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
