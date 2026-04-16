/// A single RGBA color with u8 components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Construct from a packed 24-bit hex value (0xRRGGBB), alpha = 255.
    pub const fn from_hex(hex: u32) -> Self {
        Self {
            r: ((hex >> 16) & 0xFF) as u8,
            g: ((hex >> 8) & 0xFF) as u8,
            b: (hex & 0xFF) as u8,
            a: 0xFF,
        }
    }

    /// Construct from individual u8 components.
    pub const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Return as a `[f32; 4]` RGBA array (each channel 0.0–1.0).
    pub fn to_f32_array(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// Return as a `(u8, u8, u8, u8)` tuple.
    pub const fn to_rgba_u8(self) -> (u8, u8, u8, u8) {
        (self.r, self.g, self.b, self.a)
    }

    /// Return a 24-bit ANSI foreground escape sequence: `\x1b[38;2;R;G;Bm`.
    pub fn to_ansi_escape(self) -> String {
        format!("\x1b[38;2;{};{};{}m", self.r, self.g, self.b)
    }

    /// Relative luminance per WCAG 2.1 (sRGB linearization + BT.709 weights).
    pub fn relative_luminance(self) -> f64 {
        fn linearize(c: u8) -> f64 {
            let s = c as f64 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linearize(self.r) + 0.7152 * linearize(self.g) + 0.0722 * linearize(self.b)
    }

    /// WCAG 2.1 contrast ratio between two colors (range 1.0–21.0).
    pub fn contrast_ratio(self, other: Color) -> f64 {
        let l1 = self.relative_luminance();
        let l2 = other.relative_luminance();
        let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
        (lighter + 0.05) / (darker + 0.05)
    }
}

// ---------------------------------------------------------------------------
// Color constants matching the thermal palette
// ---------------------------------------------------------------------------

// THERMAL-COLORS-START
impl Color {
    // ── Depth ramp (Codex 2031) ─────────────────────────────────────────
    /// Deepest background — replaces old BG (#0a0010).
    pub const VOID_0: Color = Color::from_hex(0x060a12);
    /// Slightly raised surface.
    pub const VOID_1: Color = Color::from_hex(0x0d1421);
    /// Mid-depth surface.
    pub const VOID_2: Color = Color::from_hex(0x111c2d);
    /// Panel / card background.
    pub const PLATE: Color = Color::from_hex(0x18263a);
    /// Strong panel / raised element.
    pub const PLATE_STRONG: Color = Color::from_hex(0x22324a);

    // Backwards-compat aliases — old names map to the new depth ramp.
    pub const BG: Color = Self::VOID_0;
    pub const BG_LIGHT: Color = Self::VOID_1;
    pub const BG_SURFACE: Color = Self::VOID_2;

    // ── Ink (text) ──────────────────────────────────────────────────────
    /// Primary text — replaces old TEXT (#c4b5fd).
    pub const INK: Color = Color::from_hex(0xe7f0ff);
    /// Muted text — secondary labels.
    pub const INK_MUTED: Color = Color::from_hex(0xa9b8cd);
    /// Dim text — placeholders, disabled.
    pub const INK_DIM: Color = Color::from_hex(0x7b8fa9);

    // Backwards-compat aliases for old text names.
    pub const TEXT: Color = Self::INK;
    pub const TEXT_BRIGHT: Color = Self::INK;
    pub const TEXT_MUTED: Color = Self::INK_MUTED;

    // ── Semantic accents (Codex 2031) ───────────────────────────────────
    /// Success / safe / go — teal-green.
    pub const SIGNAL: Color = Color::from_hex(0x39ffb6);
    /// Human interaction / focus — blue.
    pub const FOCUS: Color = Color::from_hex(0x56a7ff);
    /// Caution / warning — amber.
    pub const WARN: Color = Color::from_hex(0xffb24f);
    /// Error / blocked / danger — coral-red.
    pub const ALERT: Color = Color::from_hex(0xff5f78);

    // ── Borders ─────────────────────────────────────────────────────────
    /// Hard border line.
    pub const LINE: Color = Color::from_hex(0x2f4564);
    /// Soft border (use with alpha in rendering contexts).
    pub const LINE_SOFT: Color = Color::from_rgba(123, 156, 193, 51); // ~0.2 alpha

    // ── Temperature spectrum (mapped to semantic values) ────────────────
    // These preserve the thermal gradient API while pointing at Codex colors.
    pub const FREEZING: Color = Self::VOID_2;
    pub const COLD: Color = Self::PLATE;
    pub const COOL: Color = Self::SIGNAL; // was blue, now success-green
    pub const MILD: Color = Color::from_hex(0x0d9488); // teal (retained)
    pub const WARM: Color = Self::WARN; // was green, now amber

    pub const HOT: Color = Color::from_hex(0xeab308); // yellow (retained)
    pub const HOTTER: Color = Color::from_hex(0xf97316); // orange (retained)
    pub const SEARING: Color = Self::ALERT; // was red, now coral-red
    pub const CRITICAL: Color = Color::from_hex(0xdc2626); // deep red (retained)

    pub const WHITE_HOT: Color = Color::from_hex(0xfef3c7); // retained

    // ── Accent aliases (backwards compat) ───────────────────────────────
    pub const ACCENT_COLD: Color = Self::FOCUS;
    pub const ACCENT_COOL: Color = Self::FOCUS;
    pub const ACCENT_NEUTRAL: Color = Self::SIGNAL;
    pub const ACCENT_WARM: Color = Self::WARN;
    pub const ACCENT_HOT: Color = Self::ALERT;

    // ── Status aliases ──────────────────────────────────────────────────
    pub const STATUS_OK: Color = Self::SIGNAL;
    pub const STATUS_WARN: Color = Self::WARN;
    pub const STATUS_ERROR: Color = Self::ALERT;
}
// THERMAL-COLORS-END

// ---------------------------------------------------------------------------
// Gradient interpolation
// ---------------------------------------------------------------------------

/// Linearly interpolate between two u8 channel values.
fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Interpolate a `Color` between two `Color` values.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: lerp_u8(a.r, b.r, t),
        g: lerp_u8(a.g, b.g, t),
        b: lerp_u8(a.b, b.b, t),
        a: lerp_u8(a.a, b.a, t),
    }
}

/// Map a heat value `t` in `[0.0, 1.0]` to a thermal-spectrum `Color`.
///
/// The gradient runs:
///   0.00 → deep-cold blue   (`COOL`)
///   0.20 → cold purple      (`COLD`)
///   0.40 → teal/mild        (`MILD`)
///   0.55 → warm green       (`WARM`)
///   0.70 → hot yellow       (`HOT`)
///   0.80 → hotter orange    (`HOTTER`)
///   0.90 → searing red      (`SEARING`)
///   1.00 → white-hot        (`WHITE_HOT`)
pub fn thermal_gradient(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);

    // Gradient stops: (position, Color)
    const STOPS: [(f32, Color); 8] = [
        (0.00, Color::COOL),
        (0.20, Color::COLD),
        (0.40, Color::MILD),
        (0.55, Color::WARM),
        (0.70, Color::HOT),
        (0.80, Color::HOTTER),
        (0.90, Color::SEARING),
        (1.00, Color::WHITE_HOT),
    ];

    // Find the surrounding pair of stops.
    for i in 0..STOPS.len() - 1 {
        let (t0, c0) = STOPS[i];
        let (t1, c1) = STOPS[i + 1];
        if t <= t1 {
            let local = (t - t0) / (t1 - t0);
            return lerp_color(c0, c1, local);
        }
    }

    Color::WHITE_HOT
}

/// Generate a Vec of `n` Colors evenly sampled across the thermal spectrum.
pub fn thermal_gradient_lut(n: usize) -> Vec<Color> {
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![thermal_gradient(0.0)];
    }
    (0..n)
        .map(|i| thermal_gradient(i as f32 / (n - 1) as f32))
        .collect()
}

/// Convert a heat value `t` in `[0.0, 1.0]` directly to a `[f32; 4]` RGBA array.
pub fn thermal_gradient_f32(t: f32) -> [f32; 4] {
    thermal_gradient(t).to_f32_array()
}

/// Map a heat level to a descriptive temperature string.
pub fn heat_label(t: f32) -> &'static str {
    let t = t.clamp(0.0, 1.0);
    if t < 0.15 {
        "CRYO"
    } else if t < 0.30 {
        "COLD"
    } else if t < 0.50 {
        "MILD"
    } else if t < 0.65 {
        "WARM"
    } else if t < 0.80 {
        "HOT"
    } else if t < 0.92 {
        "SEARING"
    } else {
        "WHITE-HOT"
    }
}

// ---------------------------------------------------------------------------
// Chrome palette — runtime, theme-aware
// ---------------------------------------------------------------------------

/// Runtime, theme-aware palette of chrome and overlay roles (tn-g7oo).
///
/// Chrome rendering (pane headers, separators, focus borders, status bar,
/// tab bar, CSD buttons, hotspot underlines) used to read its colors from
/// compile-time `Color::*` constants. That meant a `[colors]` theme override
/// re-skinned only the terminal cells; the surrounding chrome stayed pinned
/// to the dark Codex 2031 palette.
///
/// `ChromePalette` is the runtime substitute. Each `[f32; 4]` field holds an
/// RGBA value with the alpha already baked in (so renderers can pass it
/// straight to `pixel_rect_to_ndc` / glyphon). The defaults derive from the
/// existing `Color::*` constants — they reproduce the previous look bit-for-bit
/// when no overrides are present. Themes can override individual roles via the
/// `[colors]` section in `therminal.toml` (see `ColorsConfig::chrome_*` and
/// `ColorsConfig::hotspot_*` fields).
///
/// All fields are public so chrome modules can read them directly without
/// going through accessors. The struct is owned by `GridRenderer` and
/// rebuilt on `apply_color_overrides`, which is called from
/// `apply_config()` and `init()` — the existing hot-reload pipeline picks
/// up theme changes for free.
#[derive(Debug, Clone, Copy)]
pub struct ChromePalette {
    // ── Pane headers / separators / focus border ───────────────────────
    /// Focused pane border (3 px outline) and the separator color used
    /// when the focused pane is adjacent to a split.
    pub focus_border: [f32; 4],
    /// Separators between adjacent panes (when neither is focused).
    pub separator: [f32; 4],
    /// Separators adjacent to the focused pane (slightly stronger than
    /// `focus_border`'s alpha — read by `draw_split_separator`).
    pub separator_focus: [f32; 4],
    /// Pane header background — focused pane.
    pub header_bg: [f32; 4],
    /// Pane header background — unfocused pane (dimmed).
    pub header_bg_dim: [f32; 4],
    /// Exit-code stripe color when the last command exited 0.
    pub exit_ok: [f32; 4],
    /// Exit-code stripe color when the last command exited non-zero.
    pub exit_error: [f32; 4],

    // ── Bottom status bar / workspace tab bar ──────────────────────────
    /// Status bar background fill.
    pub status_bar_bg: [f32; 4],
    /// Workspace tab bar background fill (defaults to `status_bar_bg`).
    pub tab_bar_bg: [f32; 4],
    /// Active workspace tab background fill (defaults to `header_bg`).
    pub tab_active_bg: [f32; 4],
    /// 2 px underline drawn beneath the active workspace tab (defaults
    /// to `focus_border`).
    pub tab_active_underline: [f32; 4],

    // ── Client-side decorations (CSD) ──────────────────────────────────
    /// Hover background tint of the close button (the only CSD button
    /// with a dedicated color — others use `csd_button_hover`).
    pub csd_close: [f32; 4],
    /// Hover background tint of all non-close CSD buttons (a near-white
    /// translucent overlay).
    pub csd_button_hover: [f32; 4],

    // ── Selection / cursor ─────────────────────────────────────────────
    /// Selection-highlight rect color (alpha already applied).
    pub selection: [f32; 4],
    /// Cursor block / underline color.
    pub cursor: [f32; 4],

    // ── Chrome text colors ─────────────────────────────────────────────
    // These are stored as `Color` (u8 channels) so chrome modules can
    // build glyphon `GlyphColor::rgba` values with their own per-state
    // alpha modulations (focused vs unfocused, hover vs idle, ...).
    /// Primary chrome text — pane header process labels, status bar
    /// center text, etc. Defaults to `Color::INK`.
    pub chrome_fg: Color,
    /// Muted chrome text — pane indices, button labels, status-bar muted
    /// fields. Defaults to `Color::INK_MUTED`.
    pub chrome_fg_muted: Color,
    /// Focus-accent chrome text — workspace number highlight, agent
    /// indicator, claude badge, git branch on a topic branch. Defaults to
    /// `Color::FOCUS`.
    pub chrome_fg_focus: Color,
    /// Warning chrome text — git detached state, etc. Defaults to
    /// `Color::WARN`.
    pub chrome_fg_warn: Color,
    /// Alert chrome text — close-button glyph, fatal errors. Defaults to
    /// `Color::ALERT`.
    pub chrome_fg_alert: Color,

    // ── Hyperlink + click-to-open hotspot underlines ───────────────────
    /// Solid hyperlink underline (OSC 8 + regex URLs in cell text).
    pub hyperlink: [f32; 4],
    /// Dotted underline color for `HotspotKind::FilePath` cells.
    pub hotspot_filepath: [f32; 4],
    /// Dotted underline color for `HotspotKind::Url` cells.
    pub hotspot_url: [f32; 4],
    /// Dotted underline color for `HotspotKind::ErrorLocation` cells.
    pub hotspot_error: [f32; 4],
    /// Dotted underline color for `HotspotKind::GitRef` cells.
    pub hotspot_gitref: [f32; 4],
    /// Dotted underline color for `HotspotKind::IssueRef` cells.
    pub hotspot_issueref: [f32; 4],
}

impl Default for ChromePalette {
    /// Build the default chrome palette by deriving from the bundled
    /// Codex 2031 `Color::*` constants. Reproduces the pre-tn-g7oo look
    /// bit-for-bit so the existing dark theme is unchanged.
    fn default() -> Self {
        Self {
            focus_border: with_alpha(Color::FOCUS, 0.92),
            separator: Color::LINE.to_f32_array(),
            separator_focus: with_alpha(Color::FOCUS, 0.82),
            header_bg: Color::VOID_2.to_f32_array(),
            header_bg_dim: Color::VOID_0.to_f32_array(),
            exit_ok: with_alpha(Color::STATUS_OK, 0.90),
            exit_error: with_alpha(Color::STATUS_ERROR, 0.90),

            status_bar_bg: Color::VOID_0.to_f32_array(),
            tab_bar_bg: Color::VOID_0.to_f32_array(),
            tab_active_bg: Color::VOID_2.to_f32_array(),
            tab_active_underline: with_alpha(Color::FOCUS, 0.92),

            csd_close: [0.85, 0.25, 0.25, 1.0],
            csd_button_hover: [1.0, 1.0, 1.0, 0.1],

            selection: with_alpha(Color::ACCENT_COOL, 0.45),
            cursor: with_alpha(Color::WHITE_HOT, 0.85),

            chrome_fg: Color::INK,
            chrome_fg_muted: Color::INK_MUTED,
            chrome_fg_focus: Color::FOCUS,
            chrome_fg_warn: Color::WARN,
            chrome_fg_alert: Color::ALERT,

            hyperlink: Color::ACCENT_COOL.to_f32_array(),
            hotspot_filepath: Color::ACCENT_NEUTRAL.to_f32_array(),
            hotspot_url: Color::ACCENT_COOL.to_f32_array(),
            hotspot_error: Color::STATUS_ERROR.to_f32_array(),
            hotspot_gitref: Color::HOT.to_f32_array(),
            // Distinct purple/indigo — no palette constant covers this hue.
            hotspot_issueref: [0.706, 0.557, 1.0, 1.0],
        }
    }
}

impl ChromePalette {
    /// Derive a chrome palette from resolved terminal cell colors
    /// (tn-n3vl).
    ///
    /// When the user sets only `[colors] background`/`foreground`/`ansi`
    /// in `therminal.toml`, this builds a visually coherent chrome from
    /// those cell colors instead of falling back to the bundled Codex
    /// 2031 defaults — so any theme (hand-rolled, preset, or future
    /// import) gets a matching status bar, pane headers, tab bar, CSD
    /// strip, and hotspot underlines without the user having to learn
    /// the full set of `chrome_*` role keys.
    ///
    /// Explicit `chrome_*` / `hotspot_*` overrides still win: they are
    /// applied on top of this derived palette by
    /// `ColorsConfig::chrome_palette` after `derive_from_cells` returns.
    ///
    /// ## Algorithm
    ///
    /// - **Chrome backgrounds** are produced by mixing `bg` toward `fg`
    ///   by small amounts in sRGB space. Because `fg` is the contrastive
    ///   complement of `bg`, the mix direction automatically inverts for
    ///   light vs dark themes — on a dark theme `header_bg` ends up
    ///   *lighter* than `bg`; on a light theme, *darker*. This matches
    ///   the intent of the bundled defaults where `VOID_2` is slightly
    ///   lighter than `VOID_0`.
    ///
    /// - **Chrome text** (`chrome_fg`, `chrome_fg_muted`) uses `fg`
    ///   directly and `fg` blended toward `bg` respectively, so text on
    ///   the derived header remains readable.
    ///
    /// - **Accents** (`focus_border`, `fg_focus`, `warn`, `alert`,
    ///   hotspot underlines) come from the ANSI palette when the user
    ///   supplied one. Without an ANSI override, we fall back to the
    ///   Codex accent constants (`Color::FOCUS`, `Color::WARN`, etc.) —
    ///   which is exactly what `ChromePalette::default` does, so this
    ///   method's behavior degrades gracefully when only bg/fg are set.
    ///
    /// - **Alpha bake-in** is preserved for the translucent roles
    ///   (`focus_border`, `separator_focus`, `selection`, `cursor`,
    ///   `exit_ok`, `exit_error`) — their alpha matches
    ///   `ChromePalette::default` exactly.
    pub fn derive_from_cells(bg: Color, fg: Color, ansi: Option<&[Color; 16]>) -> Self {
        // Mix ratios — chosen to roughly reproduce the Codex defaults on
        // the bundled dark palette (VOID_0 → VOID_2 is about an 8% shift
        // toward INK in sRGB space).
        let header_bg = mix_srgb(bg, fg, 0.08);
        let header_bg_dim = mix_srgb(bg, fg, 0.04);
        let status_bar_bg = header_bg_dim;
        let separator = mix_srgb(bg, fg, 0.20);
        let chrome_fg_muted = mix_srgb(fg, bg, 0.45);

        // Accent pulls: prefer ANSI palette indices when the user set
        // them, otherwise use the Codex constants (same as Default).
        let (accent_focus, accent_warn, accent_alert, ansi_cyan, ansi_magenta) = match ansi {
            Some(ansi) => (ansi[4], ansi[3], ansi[1], ansi[6], ansi[5]),
            None => (Color::FOCUS, Color::WARN, Color::ALERT, Color::MILD, {
                // No palette constant covers the hotspot_issueref purple
                // — keep the same RGB as `ChromePalette::default`
                // (0.706, 0.557, 1.0) rounded back to u8 channels.
                Color::from_rgba(180, 142, 255, 255)
            }),
        };

        // The green-ish hotspot_gitref default is yellow/amber; on Codex
        // that's `Color::HOT` (#eab308). With ANSI overrides we use
        // index 3 (yellow), which is what the Codex 2031 palette itself
        // puts at that slot anyway.
        let accent_gitref = match ansi {
            Some(ansi) => ansi[3],
            None => Color::HOT,
        };

        Self {
            focus_border: with_alpha(accent_focus, 0.92),
            separator: separator.to_f32_array(),
            separator_focus: with_alpha(accent_focus, 0.82),
            header_bg: header_bg.to_f32_array(),
            header_bg_dim: header_bg_dim.to_f32_array(),
            exit_ok: with_alpha(ansi.map(|a| a[2]).unwrap_or(Color::STATUS_OK), 0.90),
            exit_error: with_alpha(accent_alert, 0.90),

            status_bar_bg: status_bar_bg.to_f32_array(),
            // Propagation: tab_bar_bg follows status_bar_bg, tab_active_bg
            // follows header_bg, tab_active_underline follows focus_border.
            tab_bar_bg: status_bar_bg.to_f32_array(),
            tab_active_bg: header_bg.to_f32_array(),
            tab_active_underline: with_alpha(accent_focus, 0.92),

            csd_close: accent_alert.to_f32_array(),
            // csd_button_hover is a theme-invariant translucent white —
            // same as the default. It works on both light and dark
            // because alpha=0.1.
            csd_button_hover: [1.0, 1.0, 1.0, 0.1],

            selection: with_alpha(accent_focus, 0.45),
            cursor: with_alpha(fg, 0.85),

            chrome_fg: fg,
            chrome_fg_muted,
            chrome_fg_focus: accent_focus,
            chrome_fg_warn: accent_warn,
            chrome_fg_alert: accent_alert,

            hyperlink: accent_focus.to_f32_array(),
            hotspot_filepath: ansi_cyan.to_f32_array(),
            hotspot_url: accent_focus.to_f32_array(),
            hotspot_error: accent_alert.to_f32_array(),
            hotspot_gitref: accent_gitref.to_f32_array(),
            hotspot_issueref: ansi_magenta.to_f32_array(),
        }
    }
}

/// Build a `[f32; 4]` from a `Color` with an explicit alpha (0.0–1.0).
/// Used by `ChromePalette` defaults so the alpha bake-in stays in one spot.
fn with_alpha(color: Color, alpha: f32) -> [f32; 4] {
    let [r, g, b, _] = color.to_f32_array();
    [r, g, b, alpha]
}

/// Linear interpolation between two sRGB `Color`s in sRGB channel space.
///
/// `t` is the weight of `b` (0.0 = pure `a`, 1.0 = pure `b`). Alpha
/// channels are ignored — the result is opaque.
///
/// sRGB channel mixing is not perceptually uniform — a proper derivation
/// would convert to linear RGB or OKLCH before interpolating. For the
/// small shifts used by `derive_from_cells` (4–20%) the error is
/// imperceptible, and sRGB keeps the dependency tree lean.
fn mix_srgb(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    let mix = |ca: u8, cb: u8| -> u8 {
        let blended = ca as f32 * inv + cb as f32 * t;
        blended.round().clamp(0.0, 255.0) as u8
    };
    Color {
        r: mix(a.r, b.r),
        g: mix(a.g, b.g),
        b: mix(a.b, b.b),
        a: 255,
    }
}

// ---------------------------------------------------------------------------
// Legacy [f32; 4] palette (kept for wgpu compatibility)
// ---------------------------------------------------------------------------

/// The thermal/FLIR color palette used across all components.
pub struct ThermalPalette;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    // --- Color::from_hex ---

    #[test]
    fn from_hex_pure_red() {
        let c = Color::from_hex(0xFF0000);
        assert_eq!(c.r, 0xFF);
        assert_eq!(c.g, 0x00);
        assert_eq!(c.b, 0x00);
        assert_eq!(c.a, 0xFF);
    }

    #[test]
    fn from_hex_pure_green() {
        let c = Color::from_hex(0x00FF00);
        assert_eq!((c.r, c.g, c.b, c.a), (0x00, 0xFF, 0x00, 0xFF));
    }

    #[test]
    fn from_hex_pure_blue() {
        let c = Color::from_hex(0x0000FF);
        assert_eq!((c.r, c.g, c.b, c.a), (0x00, 0x00, 0xFF, 0xFF));
    }

    #[test]
    fn from_hex_black() {
        let c = Color::from_hex(0x000000);
        assert_eq!((c.r, c.g, c.b, c.a), (0x00, 0x00, 0x00, 0xFF));
    }

    #[test]
    fn from_hex_white() {
        let c = Color::from_hex(0xFFFFFF);
        assert_eq!((c.r, c.g, c.b, c.a), (0xFF, 0xFF, 0xFF, 0xFF));
    }

    #[test]
    fn from_hex_arbitrary() {
        // VOID_0 constant: 0x060a12
        let c = Color::from_hex(0x060a12);
        assert_eq!((c.r, c.g, c.b), (0x06, 0x0a, 0x12));
    }

    // --- Color::from_rgba ---

    #[test]
    fn from_rgba_round_trips() {
        let c = Color::from_rgba(10, 20, 30, 200);
        assert_eq!((c.r, c.g, c.b, c.a), (10, 20, 30, 200));
    }

    #[test]
    fn from_rgba_zero_alpha() {
        let c = Color::from_rgba(255, 128, 0, 0);
        assert_eq!(c.a, 0);
    }

    // --- Color::to_f32_array ---

    #[test]
    fn to_f32_array_black() {
        let arr = Color::from_rgba(0, 0, 0, 255).to_f32_array();
        assert_eq!(arr, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn to_f32_array_white() {
        let arr = Color::from_rgba(255, 255, 255, 255).to_f32_array();
        // Each channel should be 1.0.
        for &ch in &arr {
            assert!((ch - 1.0).abs() < 1e-6, "channel was {ch}");
        }
    }

    #[test]
    fn to_f32_array_half_red() {
        // 128 / 255 ≈ 0.50196
        let arr = Color::from_rgba(128, 0, 0, 255).to_f32_array();
        assert!((arr[0] - 128.0 / 255.0).abs() < 1e-6);
        assert_eq!(arr[1], 0.0);
        assert_eq!(arr[2], 0.0);
        assert_eq!(arr[3], 1.0);
    }

    #[test]
    fn to_f32_array_zero_alpha() {
        let arr = Color::from_rgba(255, 255, 255, 0).to_f32_array();
        assert_eq!(arr[3], 0.0);
    }

    // --- Color::to_rgba_u8 ---

    #[test]
    fn to_rgba_u8_round_trips() {
        let c = Color::from_rgba(1, 2, 3, 4);
        assert_eq!(c.to_rgba_u8(), (1, 2, 3, 4));
    }

    // --- Color::to_ansi_escape ---

    #[test]
    fn to_ansi_escape_format() {
        let c = Color::from_rgba(255, 128, 0, 255);
        assert_eq!(c.to_ansi_escape(), "\x1b[38;2;255;128;0m");
    }

    #[test]
    fn to_ansi_escape_black() {
        let c = Color::from_rgba(0, 0, 0, 255);
        assert_eq!(c.to_ansi_escape(), "\x1b[38;2;0;0;0m");
    }

    // --- thermal_gradient ---

    #[test]
    fn thermal_gradient_at_zero_returns_cool() {
        assert_eq!(thermal_gradient(0.0), Color::COOL);
    }

    #[test]
    fn thermal_gradient_at_one_returns_white_hot() {
        assert_eq!(thermal_gradient(1.0), Color::WHITE_HOT);
    }

    #[test]
    fn thermal_gradient_clamps_below_zero() {
        assert_eq!(thermal_gradient(-1.0), thermal_gradient(0.0));
    }

    #[test]
    fn thermal_gradient_clamps_above_one() {
        assert_eq!(thermal_gradient(2.0), thermal_gradient(1.0));
    }

    #[test]
    fn thermal_gradient_midpoint_is_between_stops() {
        // At t=0.20 the result is COLD exactly (stop boundary).
        assert_eq!(thermal_gradient(0.20), Color::COLD);
    }

    #[test]
    fn thermal_gradient_at_0_40_is_mild() {
        assert_eq!(thermal_gradient(0.40), Color::MILD);
    }

    #[test]
    fn thermal_gradient_at_0_55_is_warm() {
        assert_eq!(thermal_gradient(0.55), Color::WARM);
    }

    #[test]
    fn thermal_gradient_at_0_70_is_hot() {
        assert_eq!(thermal_gradient(0.70), Color::HOT);
    }

    #[test]
    fn thermal_gradient_at_0_80_is_hotter() {
        assert_eq!(thermal_gradient(0.80), Color::HOTTER);
    }

    #[test]
    fn thermal_gradient_at_0_90_is_searing() {
        assert_eq!(thermal_gradient(0.90), Color::SEARING);
    }

    #[test]
    fn thermal_gradient_interpolates_between_stops() {
        // At t=0.10 (half-way between COOL@0.0 and COLD@0.20) we should get
        // a color whose red channel is between the two stop values.
        let mid = thermal_gradient(0.10);
        let cool_r = Color::COOL.r;
        let cold_r = Color::COLD.r;
        let lo = cool_r.min(cold_r);
        let hi = cool_r.max(cold_r);
        assert!(mid.r >= lo && mid.r <= hi, "r={} not in [{lo},{hi}]", mid.r);
    }

    // --- thermal_gradient_lut ---

    #[test]
    fn thermal_gradient_lut_empty() {
        assert!(thermal_gradient_lut(0).is_empty());
    }

    #[test]
    fn thermal_gradient_lut_single() {
        let lut = thermal_gradient_lut(1);
        assert_eq!(lut.len(), 1);
        assert_eq!(lut[0], thermal_gradient(0.0));
    }

    #[test]
    fn thermal_gradient_lut_two_endpoints() {
        let lut = thermal_gradient_lut(2);
        assert_eq!(lut.len(), 2);
        assert_eq!(lut[0], thermal_gradient(0.0));
        assert_eq!(lut[1], thermal_gradient(1.0));
    }

    #[test]
    fn thermal_gradient_lut_length() {
        for n in [3, 8, 16, 256] {
            assert_eq!(thermal_gradient_lut(n).len(), n);
        }
    }

    // --- thermal_gradient_f32 ---

    #[test]
    fn thermal_gradient_f32_at_zero() {
        let arr = thermal_gradient_f32(0.0);
        let expected = Color::COOL.to_f32_array();
        for i in 0..4 {
            assert!((arr[i] - expected[i]).abs() < 1e-6);
        }
    }

    #[test]
    fn thermal_gradient_f32_at_one() {
        let arr = thermal_gradient_f32(1.0);
        let expected = Color::WHITE_HOT.to_f32_array();
        for i in 0..4 {
            assert!((arr[i] - expected[i]).abs() < 1e-6);
        }
    }

    // --- heat_label ---

    #[test]
    fn heat_label_cryo() {
        assert_eq!(heat_label(0.0), "CRYO");
        assert_eq!(heat_label(0.14), "CRYO");
    }

    #[test]
    fn heat_label_cold() {
        assert_eq!(heat_label(0.15), "COLD");
        assert_eq!(heat_label(0.29), "COLD");
    }

    #[test]
    fn heat_label_mild() {
        assert_eq!(heat_label(0.30), "MILD");
        assert_eq!(heat_label(0.49), "MILD");
    }

    #[test]
    fn heat_label_warm() {
        assert_eq!(heat_label(0.50), "WARM");
        assert_eq!(heat_label(0.64), "WARM");
    }

    #[test]
    fn heat_label_hot() {
        assert_eq!(heat_label(0.65), "HOT");
        assert_eq!(heat_label(0.79), "HOT");
    }

    #[test]
    fn heat_label_searing() {
        assert_eq!(heat_label(0.80), "SEARING");
        assert_eq!(heat_label(0.91), "SEARING");
    }

    #[test]
    fn heat_label_white_hot() {
        assert_eq!(heat_label(0.92), "WHITE-HOT");
        assert_eq!(heat_label(1.0), "WHITE-HOT");
    }

    #[test]
    fn heat_label_clamps_negative() {
        assert_eq!(heat_label(-5.0), "CRYO");
    }

    #[test]
    fn heat_label_clamps_above_one() {
        assert_eq!(heat_label(99.0), "WHITE-HOT");
    }

    // --- ThermalPalette legacy constants ---

    #[test]
    fn thermal_palette_bg_alpha_is_one() {
        assert_eq!(ThermalPalette::BG[3], 1.0);
    }

    #[test]
    fn thermal_palette_bg_matches_color_bg() {
        let expected = Color::BG.to_f32_array();
        let palette = ThermalPalette::BG;
        for i in 0..4 {
            assert!(
                (palette[i] - expected[i]).abs() < 1e-6,
                "channel {i}: palette={} expected={}",
                palette[i],
                expected[i]
            );
        }
    }

    #[test]
    fn thermal_palette_searing_matches_color_searing() {
        let expected = Color::SEARING.to_f32_array();
        let palette = ThermalPalette::SEARING;
        for i in 0..4 {
            assert!((palette[i] - expected[i]).abs() < 1e-6);
        }
    }

    // --- WCAG contrast ratio ---

    #[test]
    fn contrast_ratio_black_white_is_21() {
        let black = Color::from_hex(0x000000);
        let white = Color::from_hex(0xFFFFFF);
        let ratio = black.contrast_ratio(white);
        assert!(
            (ratio - 21.0).abs() < 0.1,
            "expected ~21:1, got {ratio:.2}:1"
        );
    }

    #[test]
    fn contrast_ratio_same_color_is_1() {
        let ratio = Color::SEARING.contrast_ratio(Color::SEARING);
        assert!((ratio - 1.0).abs() < 0.01, "expected 1:1, got {ratio:.2}:1");
    }

    #[test]
    fn contrast_ratio_is_symmetric() {
        let a = Color::TEXT.contrast_ratio(Color::BG);
        let b = Color::BG.contrast_ratio(Color::TEXT);
        assert!((a - b).abs() < 0.001);
    }

    // --- Palette contrast compliance ---
    // WCAG AA: 4.5:1 for normal text, 3.0:1 for large text / UI elements.
    // All foreground colors must meet at least 3.0:1 against BG.
    // Text colors (TEXT, TEXT_BRIGHT, TEXT_MUTED) must meet 4.5:1.

    const WCAG_AA_TEXT: f64 = 4.5;
    const WCAG_AA_LARGE: f64 = 3.0;

    /// Helper: assert a color meets a minimum contrast ratio against BG.
    fn assert_contrast(name: &str, color: Color, min_ratio: f64) {
        let ratio = color.contrast_ratio(Color::BG);
        assert!(
            ratio >= min_ratio,
            "{name} ({:02x}{:02x}{:02x}) contrast {ratio:.2}:1 < {min_ratio}:1 against BG",
            color.r,
            color.g,
            color.b,
        );
    }

    #[test]
    fn text_colors_meet_wcag_aa() {
        assert_contrast("INK", Color::INK, WCAG_AA_TEXT);
        assert_contrast("INK_MUTED", Color::INK_MUTED, WCAG_AA_TEXT);
        // INK_DIM is for placeholders/disabled — only needs large-text ratio.
        assert_contrast("INK_DIM", Color::INK_DIM, WCAG_AA_LARGE);
    }

    #[test]
    fn hot_spectrum_meets_wcag_aa_large() {
        assert_contrast("HOT", Color::HOT, WCAG_AA_LARGE);
        assert_contrast("HOTTER", Color::HOTTER, WCAG_AA_LARGE);
        assert_contrast("SEARING", Color::SEARING, WCAG_AA_LARGE);
        assert_contrast("CRITICAL", Color::CRITICAL, WCAG_AA_LARGE);
        assert_contrast("WHITE_HOT", Color::WHITE_HOT, WCAG_AA_LARGE);
    }

    #[test]
    fn neutral_spectrum_meets_wcag_aa_large() {
        assert_contrast("MILD", Color::MILD, WCAG_AA_LARGE);
        assert_contrast("WARM", Color::WARM, WCAG_AA_LARGE);
    }

    #[test]
    fn semantic_accents_meet_wcag_aa_large() {
        assert_contrast("SIGNAL", Color::SIGNAL, WCAG_AA_LARGE);
        assert_contrast("FOCUS", Color::FOCUS, WCAG_AA_LARGE);
        assert_contrast("WARN", Color::WARN, WCAG_AA_LARGE);
        assert_contrast("ALERT", Color::ALERT, WCAG_AA_LARGE);
    }

    #[test]
    fn status_colors_meet_wcag_aa_for_text() {
        assert_contrast("STATUS_OK", Color::STATUS_OK, WCAG_AA_TEXT);
        assert_contrast("STATUS_WARN", Color::STATUS_WARN, WCAG_AA_TEXT);
        assert_contrast("STATUS_ERROR", Color::STATUS_ERROR, WCAG_AA_TEXT);
    }

    // --- ChromePalette (tn-g7oo) ---

    #[test]
    fn chrome_palette_default_focus_border_matches_focus_constant() {
        let p = ChromePalette::default();
        let focus = Color::FOCUS.to_f32_array();
        // RGB channels match Color::FOCUS; alpha was lowered to 0.92.
        assert!((p.focus_border[0] - focus[0]).abs() < 1e-6);
        assert!((p.focus_border[1] - focus[1]).abs() < 1e-6);
        assert!((p.focus_border[2] - focus[2]).abs() < 1e-6);
        assert!((p.focus_border[3] - 0.92).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_default_separator_matches_line() {
        let p = ChromePalette::default();
        assert_eq!(p.separator, Color::LINE.to_f32_array());
    }

    #[test]
    fn chrome_palette_default_header_bg_matches_void_2() {
        let p = ChromePalette::default();
        assert_eq!(p.header_bg, Color::VOID_2.to_f32_array());
    }

    #[test]
    fn chrome_palette_default_header_bg_dim_matches_void_0() {
        let p = ChromePalette::default();
        assert_eq!(p.header_bg_dim, Color::VOID_0.to_f32_array());
    }

    #[test]
    fn chrome_palette_default_status_bar_bg_matches_void_0() {
        let p = ChromePalette::default();
        assert_eq!(p.status_bar_bg, Color::VOID_0.to_f32_array());
    }

    #[test]
    fn chrome_palette_default_tab_bar_bg_tracks_status_bar_bg() {
        let p = ChromePalette::default();
        assert_eq!(p.tab_bar_bg, p.status_bar_bg);
    }

    #[test]
    fn chrome_palette_default_tab_active_bg_tracks_header_bg() {
        let p = ChromePalette::default();
        assert_eq!(p.tab_active_bg, p.header_bg);
    }

    #[test]
    fn chrome_palette_default_tab_active_underline_tracks_focus_border() {
        let p = ChromePalette::default();
        assert_eq!(p.tab_active_underline, p.focus_border);
    }

    #[test]
    fn chrome_palette_default_exit_ok_alpha_is_baked() {
        let p = ChromePalette::default();
        assert!((p.exit_ok[3] - 0.90).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_default_exit_error_alpha_is_baked() {
        let p = ChromePalette::default();
        assert!((p.exit_error[3] - 0.90).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_default_csd_close_is_red() {
        let p = ChromePalette::default();
        // r > g, r > b, fully opaque
        assert!(p.csd_close[0] > p.csd_close[1]);
        assert!(p.csd_close[0] > p.csd_close[2]);
        assert!((p.csd_close[3] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_default_csd_button_hover_is_translucent_white() {
        let p = ChromePalette::default();
        assert!((p.csd_button_hover[0] - 1.0).abs() < 1e-6);
        assert!((p.csd_button_hover[1] - 1.0).abs() < 1e-6);
        assert!((p.csd_button_hover[2] - 1.0).abs() < 1e-6);
        assert!(p.csd_button_hover[3] > 0.0 && p.csd_button_hover[3] < 0.5);
    }

    #[test]
    fn chrome_palette_default_selection_alpha_baked() {
        let p = ChromePalette::default();
        assert!((p.selection[3] - 0.45).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_default_cursor_alpha_baked() {
        let p = ChromePalette::default();
        assert!((p.cursor[3] - 0.85).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_default_hotspot_kinds_are_distinct() {
        let p = ChromePalette::default();
        let colors = [
            p.hotspot_filepath,
            p.hotspot_url,
            p.hotspot_error,
            p.hotspot_gitref,
            p.hotspot_issueref,
        ];
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "hotspot kind colors at index {i} and {j} must be distinct"
                );
            }
        }
    }

    #[test]
    fn chrome_palette_default_hyperlink_matches_url_hotspot() {
        // The default hyperlink underline (OSC 8) and the URL-kind hotspot
        // underline both default to ACCENT_COOL so OSC 8 + regex URLs match.
        let p = ChromePalette::default();
        assert_eq!(p.hyperlink, p.hotspot_url);
    }

    // ── ChromePalette::derive_from_cells (tn-n3vl) ──────────────────────

    #[test]
    fn derive_from_cells_dark_theme_header_bg_lighter_than_bg() {
        // Dark theme: bg is near-black, fg is near-white. Mixing toward
        // fg should produce a *lighter* header background.
        let bg = Color::from_hex(0x000000);
        let fg = Color::from_hex(0xffffff);
        let p = ChromePalette::derive_from_cells(bg, fg, None);
        // header_bg luminance > bg luminance on a dark theme.
        let header_bg = Color::from_rgba(
            (p.header_bg[0] * 255.0) as u8,
            (p.header_bg[1] * 255.0) as u8,
            (p.header_bg[2] * 255.0) as u8,
            255,
        );
        assert!(
            header_bg.relative_luminance() > bg.relative_luminance(),
            "derived header_bg should be lighter than a dark bg"
        );
        // And it should not have inverted past the foreground.
        assert!(header_bg.relative_luminance() < fg.relative_luminance());
    }

    #[test]
    fn derive_from_cells_light_theme_header_bg_darker_than_bg() {
        // Light theme: bg is near-white, fg is near-black. Mixing bg
        // toward fg should now produce a *darker* header background —
        // the sign flips automatically because fg flipped.
        let bg = Color::from_hex(0xf0f0f0);
        let fg = Color::from_hex(0x000000);
        let p = ChromePalette::derive_from_cells(bg, fg, None);
        let header_bg = Color::from_rgba(
            (p.header_bg[0] * 255.0) as u8,
            (p.header_bg[1] * 255.0) as u8,
            (p.header_bg[2] * 255.0) as u8,
            255,
        );
        assert!(
            header_bg.relative_luminance() < bg.relative_luminance(),
            "derived header_bg should be darker than a light bg"
        );
        assert!(header_bg.relative_luminance() > fg.relative_luminance());
    }

    #[test]
    fn derive_from_cells_header_bg_dim_closer_to_bg_than_header_bg() {
        // `header_bg_dim` should sit between `bg` and `header_bg` — it
        // is a smaller shift toward fg than `header_bg`.
        let bg = Color::from_hex(0x060a12);
        let fg = Color::from_hex(0xe7f0ff);
        let p = ChromePalette::derive_from_cells(bg, fg, None);
        let header = Color::from_rgba(
            (p.header_bg[0] * 255.0) as u8,
            (p.header_bg[1] * 255.0) as u8,
            (p.header_bg[2] * 255.0) as u8,
            255,
        );
        let dim = Color::from_rgba(
            (p.header_bg_dim[0] * 255.0) as u8,
            (p.header_bg_dim[1] * 255.0) as u8,
            (p.header_bg_dim[2] * 255.0) as u8,
            255,
        );
        let bg_to_dim = (dim.relative_luminance() - bg.relative_luminance()).abs();
        let bg_to_header = (header.relative_luminance() - bg.relative_luminance()).abs();
        assert!(
            bg_to_dim < bg_to_header,
            "header_bg_dim ({:.4}) should be closer to bg than header_bg ({:.4})",
            bg_to_dim,
            bg_to_header
        );
    }

    #[test]
    fn derive_from_cells_no_ansi_falls_back_to_codex_accents() {
        // Without an ANSI override, accents should match the bundled
        // Codex constants — so themes that set only bg/fg get the same
        // focus/warn/alert colors as today.
        let bg = Color::from_hex(0x060a12);
        let fg = Color::from_hex(0xe7f0ff);
        let p = ChromePalette::derive_from_cells(bg, fg, None);
        assert_eq!(p.chrome_fg_focus, Color::FOCUS);
        assert_eq!(p.chrome_fg_warn, Color::WARN);
        assert_eq!(p.chrome_fg_alert, Color::ALERT);
    }

    #[test]
    fn derive_from_cells_with_ansi_picks_accents_from_indices() {
        // With an ANSI palette, accents should come from the expected
        // indices: focus=4 (blue), warn=3 (yellow), alert=1 (red),
        // filepath=6 (cyan), issueref=5 (magenta).
        let bg = Color::from_hex(0x000000);
        let fg = Color::from_hex(0xffffff);
        let mut ansi = [Color::from_hex(0); 16];
        ansi[1] = Color::from_hex(0xabcdef); // red slot
        ansi[3] = Color::from_hex(0x123456); // yellow slot
        ansi[4] = Color::from_hex(0xfedcba); // blue slot
        ansi[5] = Color::from_hex(0xdeadbe); // magenta slot
        ansi[6] = Color::from_hex(0xbadc0f); // cyan slot
        let p = ChromePalette::derive_from_cells(bg, fg, Some(&ansi));
        assert_eq!(p.chrome_fg_focus, ansi[4]);
        assert_eq!(p.chrome_fg_warn, ansi[3]);
        assert_eq!(p.chrome_fg_alert, ansi[1]);
        // Hotspot underlines
        assert_eq!(p.hotspot_filepath, ansi[6].to_f32_array());
        assert_eq!(p.hotspot_url, ansi[4].to_f32_array());
        assert_eq!(p.hotspot_error, ansi[1].to_f32_array());
        assert_eq!(p.hotspot_gitref, ansi[3].to_f32_array());
        assert_eq!(p.hotspot_issueref, ansi[5].to_f32_array());
        // CSD close tracks alert
        assert_eq!(p.csd_close, ansi[1].to_f32_array());
    }

    #[test]
    fn derive_from_cells_preserves_alpha_bake_in() {
        // The translucent roles must keep the same alphas as
        // `ChromePalette::default` so chrome that assumes those alphas
        // (split separator, exit-code stripe, selection rect, cursor)
        // keeps working.
        let p = ChromePalette::derive_from_cells(
            Color::from_hex(0x000000),
            Color::from_hex(0xffffff),
            None,
        );
        assert!((p.focus_border[3] - 0.92).abs() < 1e-6);
        assert!((p.separator_focus[3] - 0.82).abs() < 1e-6);
        assert!((p.tab_active_underline[3] - 0.92).abs() < 1e-6);
        assert!((p.exit_ok[3] - 0.90).abs() < 1e-6);
        assert!((p.exit_error[3] - 0.90).abs() < 1e-6);
        assert!((p.selection[3] - 0.45).abs() < 1e-6);
        assert!((p.cursor[3] - 0.85).abs() < 1e-6);
    }

    #[test]
    fn derive_from_cells_chrome_fg_muted_between_fg_and_bg() {
        // Muted chrome text should sit between fg and bg in luminance
        // space so it reads as "secondary" relative to primary text.
        let bg = Color::from_hex(0x060a12);
        let fg = Color::from_hex(0xe7f0ff);
        let p = ChromePalette::derive_from_cells(bg, fg, None);
        let muted_lum = p.chrome_fg_muted.relative_luminance();
        assert!(muted_lum > bg.relative_luminance());
        assert!(muted_lum < fg.relative_luminance());
    }

    #[test]
    fn derive_from_cells_tab_propagations_track_base_roles() {
        // tab_bar_bg follows status_bar_bg, tab_active_bg follows
        // header_bg. Derivation must establish these at build time — the
        // config-layer override propagations only fire when explicit
        // chrome_* keys are present.
        let p = ChromePalette::derive_from_cells(
            Color::from_hex(0x060a12),
            Color::from_hex(0xe7f0ff),
            None,
        );
        assert_eq!(p.tab_bar_bg, p.status_bar_bg);
        assert_eq!(p.tab_active_bg, p.header_bg);
    }

    #[test]
    fn derive_from_cells_chrome_text_readable_on_chrome_header_bg() {
        // WCAG AA (4.5:1) for chrome_fg on derived chrome_header_bg.
        // Verifies both a dark and a light case — the whole point of
        // deriving from cells is that `chrome_fg = fg` stays readable
        // because `header_bg` is a tiny shift from the theme's own bg.
        for (bg, fg, name) in [
            (
                Color::from_hex(0x060a12),
                Color::from_hex(0xe7f0ff),
                "dark codex",
            ),
            (
                Color::from_hex(0xf5f5f5),
                Color::from_hex(0x1a1a1a),
                "light",
            ),
            (
                Color::from_hex(0x000000),
                Color::from_hex(0xffffff),
                "pure bw",
            ),
            (
                Color::from_hex(0xf2eede),
                Color::from_hex(0x000000),
                "paper",
            ),
        ] {
            let p = ChromePalette::derive_from_cells(bg, fg, None);
            let header_bg = Color::from_rgba(
                (p.header_bg[0] * 255.0) as u8,
                (p.header_bg[1] * 255.0) as u8,
                (p.header_bg[2] * 255.0) as u8,
                255,
            );
            let ratio = p.chrome_fg.contrast_ratio(header_bg);
            assert!(
                ratio >= 4.5,
                "{name}: chrome_fg on derived header_bg contrast {ratio:.2} < 4.5 \
                 (fg={fg:?}, header_bg={header_bg:?})"
            );
        }
    }

    // ── Retro Terminal theme contrast compliance (tn-gsiy) ───────────────

    /// Helper: assert two hex colors meet a minimum WCAG contrast ratio.
    fn assert_hex_contrast(label: &str, fg_hex: u32, bg_hex: u32, min_ratio: f64) {
        let fg = Color::from_hex(fg_hex);
        let bg = Color::from_hex(bg_hex);
        let ratio = fg.contrast_ratio(bg);
        assert!(
            ratio >= min_ratio,
            "{label}: #{fg_hex:06x} vs #{bg_hex:06x} contrast {ratio:.2}:1 < {min_ratio}:1 (WCAG AA)"
        );
    }

    #[test]
    fn retro_ansi_black_darker_than_background() {
        // ANSI black (#050d05) must be visually distinct from the terminal
        // background (#0a120a). In reverse-video mode the terminal renders
        // text in the background color on an ANSI-black cell background; if
        // black equals the background the cell becomes invisible.
        // We require a minimum 1.5:1 separation so the two shades are at
        // least perceivable as different even without reverse video.
        let ansi_black = Color::from_hex(0x050d05);
        let bg = Color::from_hex(0x0a120a);
        // ANSI black must be darker than the background
        assert!(
            ansi_black.relative_luminance() < bg.relative_luminance(),
            "ANSI black #050d05 should be darker than background #0a120a"
        );
        // The phosphor foreground (#33ff33) on ANSI black must be highly
        // legible — meeting WCAG AA large-text minimum of 3:1.
        assert_hex_contrast(
            "retro fg (#33ff33) on ANSI black (#050d05)",
            0x33ff33,
            0x050d05,
            WCAG_AA_LARGE,
        );
    }

    #[test]
    fn retro_chrome_fg_muted_meets_wcag_aa_on_header_bg() {
        // chrome_fg_muted (#44a844) must meet WCAG AA (4.5:1) against the
        // focused pane header background (#0f200f). This is the primary
        // chrome contrast pair that was failing before tn-gsiy.
        assert_hex_contrast(
            "retro chrome_fg_muted (#44a844) on chrome_header_bg (#0f200f)",
            0x44a844,
            0x0f200f,
            WCAG_AA_TEXT,
        );
    }

    #[test]
    fn retro_chrome_fg_muted_meets_wcag_aa_on_header_bg_dim() {
        // chrome_fg_muted (#44a844) must also meet WCAG AA (4.5:1) against
        // the unfocused pane header background (#0a150a).
        assert_hex_contrast(
            "retro chrome_fg_muted (#44a844) on chrome_header_bg_dim (#0a150a)",
            0x44a844,
            0x0a150a,
            WCAG_AA_TEXT,
        );
    }

    #[test]
    fn retro_foreground_muted_meets_wcag_aa_on_background() {
        // foreground_muted (#2a902a) must meet WCAG AA (4.5:1) against the
        // terminal background (#0a120a). This secondary text color is used
        // for dimmed terminal output and prompt hints.
        assert_hex_contrast(
            "retro foreground_muted (#2a902a) on background (#0a120a)",
            0x2a902a,
            0x0a120a,
            WCAG_AA_TEXT,
        );
    }

    #[test]
    fn retro_primary_fg_meets_wcag_aa_on_background() {
        // Primary foreground (#33ff33) must meet WCAG AA (4.5:1) against
        // the terminal background — baseline sanity for the whole theme.
        assert_hex_contrast(
            "retro fg (#33ff33) on background (#0a120a)",
            0x33ff33,
            0x0a120a,
            WCAG_AA_TEXT,
        );
    }

    #[test]
    fn retro_chrome_fg_meets_wcag_aa_on_header_bg() {
        // Primary chrome text (#33ff33) must meet WCAG AA (4.5:1) against
        // the focused pane header background (#0f200f).
        assert_hex_contrast(
            "retro chrome_fg (#33ff33) on chrome_header_bg (#0f200f)",
            0x33ff33,
            0x0f200f,
            WCAG_AA_TEXT,
        );
    }
}

// THERMAL-PALETTE-COLORS-START
impl ThermalPalette {
    // Depth ramp (Codex 2031)
    pub const VOID_0: [f32; 4] = Self::hex(0x06, 0x0a, 0x12);
    pub const VOID_1: [f32; 4] = Self::hex(0x0d, 0x14, 0x21);
    pub const VOID_2: [f32; 4] = Self::hex(0x11, 0x1c, 0x2d);
    pub const PLATE: [f32; 4] = Self::hex(0x18, 0x26, 0x3a);
    pub const PLATE_STRONG: [f32; 4] = Self::hex(0x22, 0x32, 0x4a);

    // Backwards-compat aliases
    pub const BG: [f32; 4] = Self::VOID_0;
    pub const BG_LIGHT: [f32; 4] = Self::VOID_1;
    pub const BG_SURFACE: [f32; 4] = Self::VOID_2;

    // Ink (text)
    pub const INK: [f32; 4] = Self::hex(0xe7, 0xf0, 0xff);
    pub const INK_MUTED: [f32; 4] = Self::hex(0xa9, 0xb8, 0xcd);
    pub const INK_DIM: [f32; 4] = Self::hex(0x7b, 0x8f, 0xa9);

    pub const TEXT: [f32; 4] = Self::INK;
    pub const TEXT_BRIGHT: [f32; 4] = Self::INK;
    pub const TEXT_MUTED: [f32; 4] = Self::INK_MUTED;

    // Semantic accents (Codex 2031)
    pub const SIGNAL: [f32; 4] = Self::hex(0x39, 0xff, 0xb6);
    pub const FOCUS: [f32; 4] = Self::hex(0x56, 0xa7, 0xff);
    pub const WARN: [f32; 4] = Self::hex(0xff, 0xb2, 0x4f);
    pub const ALERT: [f32; 4] = Self::hex(0xff, 0x5f, 0x78);

    // Borders
    pub const LINE: [f32; 4] = Self::hex(0x2f, 0x45, 0x64);

    // Temperature spectrum
    pub const FREEZING: [f32; 4] = Self::VOID_2;
    pub const COLD: [f32; 4] = Self::PLATE;
    pub const COOL: [f32; 4] = Self::SIGNAL;
    pub const MILD: [f32; 4] = Self::hex(0x0d, 0x94, 0x88);
    pub const WARM: [f32; 4] = Self::WARN;

    pub const HOT: [f32; 4] = Self::hex(0xea, 0xb3, 0x08);
    pub const HOTTER: [f32; 4] = Self::hex(0xf9, 0x73, 0x16);
    pub const SEARING: [f32; 4] = Self::ALERT;
    pub const CRITICAL: [f32; 4] = Self::hex(0xdc, 0x26, 0x26);

    pub const WHITE_HOT: [f32; 4] = Self::hex(0xfe, 0xf3, 0xc7);

    // Accent aliases
    pub const ACCENT_COLD: [f32; 4] = Self::FOCUS;
    pub const ACCENT_COOL: [f32; 4] = Self::FOCUS;
    pub const ACCENT_NEUTRAL: [f32; 4] = Self::SIGNAL;
    pub const ACCENT_WARM: [f32; 4] = Self::WARN;
    pub const ACCENT_HOT: [f32; 4] = Self::ALERT;

    // Status aliases
    pub const STATUS_OK: [f32; 4] = Self::SIGNAL;
    pub const STATUS_WARN: [f32; 4] = Self::WARN;
    pub const STATUS_ERROR: [f32; 4] = Self::ALERT;
    // THERMAL-PALETTE-COLORS-END

    const fn hex(r: u8, g: u8, b: u8) -> [f32; 4] {
        [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
    }
}
