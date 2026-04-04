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
    // Void / Background
    pub const BG: Color = Color::from_hex(0x0a0010);
    pub const BG_LIGHT: Color = Color::from_hex(0x0f0018);
    pub const BG_SURFACE: Color = Color::from_hex(0x120822);

    // Cold spectrum
    pub const FREEZING: Color = Color::from_hex(0x1a0030);
    pub const COLD: Color = Color::from_hex(0x2d1b69);
    pub const COOL: Color = Color::from_hex(0x1e3a8a);

    // Neutral
    pub const MILD: Color = Color::from_hex(0x0d9488);
    pub const WARM: Color = Color::from_hex(0x22c55e);

    // Hot spectrum
    pub const HOT: Color = Color::from_hex(0xeab308);
    pub const HOTTER: Color = Color::from_hex(0xf97316);
    pub const SEARING: Color = Color::from_hex(0xef4444);
    pub const CRITICAL: Color = Color::from_hex(0xdc2626);

    // White-hot
    pub const WHITE_HOT: Color = Color::from_hex(0xfef3c7);

    // Text
    pub const TEXT: Color = Color::from_hex(0xc4b5fd);
    pub const TEXT_BRIGHT: Color = Color::from_hex(0xe9e0ff);
    pub const TEXT_MUTED: Color = Color::from_hex(0x9b8dd1);

    // Accents
    pub const ACCENT_COLD: Color = Color::from_hex(0x818cf8);
    pub const ACCENT_COOL: Color = Color::from_hex(0x3b82f6);
    pub const ACCENT_NEUTRAL: Color = Color::from_hex(0x14b8a6);
    pub const ACCENT_WARM: Color = Color::from_hex(0xf59e0b);
    pub const ACCENT_HOT: Color = Color::from_hex(0xef4444);

    // Status
    pub const STATUS_OK: Color = Color::from_hex(0x22c55e);
    pub const STATUS_WARN: Color = Color::from_hex(0xf59e0b);
    pub const STATUS_ERROR: Color = Color::from_hex(0xef4444);
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
        // BG constant: 0x0a0010
        let c = Color::from_hex(0x0a0010);
        assert_eq!((c.r, c.g, c.b), (0x0a, 0x00, 0x10));
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
        assert_contrast("TEXT", Color::TEXT, WCAG_AA_TEXT);
        assert_contrast("TEXT_BRIGHT", Color::TEXT_BRIGHT, WCAG_AA_TEXT);
        assert_contrast("TEXT_MUTED", Color::TEXT_MUTED, WCAG_AA_TEXT);
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
    fn accent_colors_meet_wcag_aa_large() {
        assert_contrast("ACCENT_COLD", Color::ACCENT_COLD, WCAG_AA_LARGE);
        assert_contrast("ACCENT_COOL", Color::ACCENT_COOL, WCAG_AA_LARGE);
        assert_contrast("ACCENT_NEUTRAL", Color::ACCENT_NEUTRAL, WCAG_AA_LARGE);
        assert_contrast("ACCENT_WARM", Color::ACCENT_WARM, WCAG_AA_LARGE);
        assert_contrast("ACCENT_HOT", Color::ACCENT_HOT, WCAG_AA_LARGE);
    }

    #[test]
    fn status_colors_meet_wcag_aa_for_text() {
        assert_contrast("STATUS_OK", Color::STATUS_OK, WCAG_AA_TEXT);
        assert_contrast("STATUS_WARN", Color::STATUS_WARN, WCAG_AA_TEXT);
        assert_contrast("STATUS_ERROR", Color::STATUS_ERROR, WCAG_AA_TEXT);
    }
}

// THERMAL-PALETTE-COLORS-START
impl ThermalPalette {
    // Void / Background
    pub const BG: [f32; 4] = Self::hex(0x0a, 0x00, 0x10);
    pub const BG_LIGHT: [f32; 4] = Self::hex(0x0f, 0x00, 0x18);
    pub const BG_SURFACE: [f32; 4] = Self::hex(0x12, 0x08, 0x22);

    // Cold spectrum
    pub const FREEZING: [f32; 4] = Self::hex(0x1a, 0x00, 0x30);
    pub const COLD: [f32; 4] = Self::hex(0x2d, 0x1b, 0x69);
    pub const COOL: [f32; 4] = Self::hex(0x1e, 0x3a, 0x8a);

    // Neutral
    pub const MILD: [f32; 4] = Self::hex(0x0d, 0x94, 0x88);
    pub const WARM: [f32; 4] = Self::hex(0x22, 0xc5, 0x5e);

    // Hot spectrum
    pub const HOT: [f32; 4] = Self::hex(0xea, 0xb3, 0x08);
    pub const HOTTER: [f32; 4] = Self::hex(0xf9, 0x73, 0x16);
    pub const SEARING: [f32; 4] = Self::hex(0xef, 0x44, 0x44);
    pub const CRITICAL: [f32; 4] = Self::hex(0xdc, 0x26, 0x26);

    // White-hot
    pub const WHITE_HOT: [f32; 4] = Self::hex(0xfe, 0xf3, 0xc7);

    // Text
    pub const TEXT: [f32; 4] = Self::hex(0xc4, 0xb5, 0xfd);
    pub const TEXT_BRIGHT: [f32; 4] = Self::hex(0xe9, 0xe0, 0xff);
    pub const TEXT_MUTED: [f32; 4] = Self::hex(0x9b, 0x8d, 0xd1);

    // Accents
    pub const ACCENT_COLD: [f32; 4] = Self::hex(0x81, 0x8c, 0xf8);
    pub const ACCENT_COOL: [f32; 4] = Self::hex(0x3b, 0x82, 0xf6);
    pub const ACCENT_NEUTRAL: [f32; 4] = Self::hex(0x14, 0xb8, 0xa6);
    pub const ACCENT_WARM: [f32; 4] = Self::hex(0xf5, 0x9e, 0x0b);
    pub const ACCENT_HOT: [f32; 4] = Self::hex(0xef, 0x44, 0x44);

    // Status
    pub const STATUS_OK: [f32; 4] = Self::hex(0x22, 0xc5, 0x5e);
    pub const STATUS_WARN: [f32; 4] = Self::hex(0xf5, 0x9e, 0x0b);
    pub const STATUS_ERROR: [f32; 4] = Self::hex(0xef, 0x44, 0x44);
    // THERMAL-PALETTE-COLORS-END

    const fn hex(r: u8, g: u8, b: u8) -> [f32; 4] {
        [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
    }
}
