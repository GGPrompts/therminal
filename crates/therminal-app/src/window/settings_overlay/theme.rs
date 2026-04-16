//! Theme preset palettes: `apply_theme_preset`.
//!
//! Each preset overwrites both the terminal-cell colors (`background`,
//! `foreground`, `cursor`, `ansi`) and the full set of chrome role fields
//! from tn-g7oo (`chrome_*`, `hotspot_*`). Setting only the cell colors —
//! as the pre-tn-2xwr version did — left the chrome locked to the bundled
//! Codex 2031 defaults regardless of which preset the user picked, so the
//! status bar, pane headers, tab bar, and CSD strip would not re-skin.
//!
//! Derivations handled by `ColorsConfig::chrome_palette` remain in play:
//!
//! - `chrome_focus_border` → `separator_focus` + `tab_active_underline`
//! - `chrome_header_bg`    → `tab_active_bg`
//! - `chrome_status_bar_bg` → `tab_bar_bg`
//! - `hotspot_url`          → `hyperlink`
//!
//! so presets set primary roles only and let the resolver fan them out.

use therminal_core::config::ColorsConfig;

use super::types::ThemePreset;

/// Full chrome + cell color set for a single theme preset.
struct ThemePalette {
    // Terminal cells
    background: &'static str,
    foreground: &'static str,
    foreground_bright: &'static str,
    foreground_muted: &'static str,
    surface: &'static str,
    cursor: &'static str,
    selection: &'static str,
    ansi: [&'static str; 16],

    // Chrome role overrides (tn-g7oo) — picks per preset so each preset
    // produces a visually coherent chrome, not the dark Codex defaults.
    chrome_focus_border: &'static str,
    chrome_separator: &'static str,
    chrome_header_bg: &'static str,
    chrome_header_bg_dim: &'static str,
    chrome_status_bar_bg: &'static str,
    chrome_csd_close: &'static str,
    chrome_fg: &'static str,
    chrome_fg_muted: &'static str,
    chrome_fg_focus: &'static str,
    chrome_fg_warn: &'static str,
    chrome_fg_alert: &'static str,

    // Hotspot underline colors
    hotspot_filepath: &'static str,
    hotspot_url: &'static str,
    hotspot_error: &'static str,
    hotspot_gitref: &'static str,
    hotspot_issueref: &'static str,
}

pub(crate) fn apply_theme_preset(colors: &mut ColorsConfig, preset: ThemePreset) {
    let p = palette_for(preset);

    // Terminal cells
    colors.background = Some(p.background.to_string());
    colors.foreground = Some(p.foreground.to_string());
    colors.foreground_bright = Some(p.foreground_bright.to_string());
    colors.foreground_muted = Some(p.foreground_muted.to_string());
    colors.surface = Some(p.surface.to_string());
    colors.cursor = Some(p.cursor.to_string());
    colors.selection = Some(p.selection.to_string());
    colors.ansi = Some(p.ansi.iter().map(|c| (*c).to_string()).collect());

    // Chrome roles
    colors.chrome_focus_border = Some(p.chrome_focus_border.to_string());
    colors.chrome_separator = Some(p.chrome_separator.to_string());
    colors.chrome_header_bg = Some(p.chrome_header_bg.to_string());
    colors.chrome_header_bg_dim = Some(p.chrome_header_bg_dim.to_string());
    colors.chrome_status_bar_bg = Some(p.chrome_status_bar_bg.to_string());
    colors.chrome_csd_close = Some(p.chrome_csd_close.to_string());
    colors.chrome_fg = Some(p.chrome_fg.to_string());
    colors.chrome_fg_muted = Some(p.chrome_fg_muted.to_string());
    colors.chrome_fg_focus = Some(p.chrome_fg_focus.to_string());
    colors.chrome_fg_warn = Some(p.chrome_fg_warn.to_string());
    colors.chrome_fg_alert = Some(p.chrome_fg_alert.to_string());

    // Hotspot underlines
    colors.hotspot_filepath = Some(p.hotspot_filepath.to_string());
    colors.hotspot_url = Some(p.hotspot_url.to_string());
    colors.hotspot_error = Some(p.hotspot_error.to_string());
    colors.hotspot_gitref = Some(p.hotspot_gitref.to_string());
    colors.hotspot_issueref = Some(p.hotspot_issueref.to_string());

    // Leave `chrome_tab_bar_bg`, `chrome_tab_active_bg`, and
    // `chrome_hyperlink` as None so the propagation rules in
    // `ColorsConfig::chrome_palette` do their job (tab bar follows status
    // bar, tab active follows header, hyperlink follows hotspot_url).
    colors.chrome_tab_bar_bg = None;
    colors.chrome_tab_active_bg = None;
    colors.chrome_hyperlink = None;
}

fn palette_for(preset: ThemePreset) -> ThemePalette {
    match preset {
        // ── Original Therminal ──────────────────────────────────────────
        // Dark theme with Codex 2031 aesthetics. ANSI colors are now
        // semantically correct: green is green, magenta is distinct from
        // blue, cyan is distinct from green. Chrome accent uses a
        // dedicated blue that decouples from ANSI[4].
        ThemePreset::OriginalTherminal => ThemePalette {
            background: "#060a12",
            foreground: "#e7f0ff",
            foreground_bright: "#ffffff",
            foreground_muted: "#7b8fa9",
            surface: "#111c2d",
            cursor: "#fef3c7",
            selection: "#56a7ff",
            ansi: [
                "#1a1a2e", // 0  Black       — near-black with slight blue tint
                "#ff5f6d", // 1  Red         — coral red, readable on dark bg
                "#4ec970", // 2  Green       — true green, distinct from cyan
                "#e7c547", // 3  Yellow      — warm yellow, good contrast
                "#4a8fd9", // 4  Blue        — medium blue, dark enough for white-on-blue bg
                "#c97bdb", // 5  Magenta     — distinct purple-pink, clearly not blue
                "#56c8d8", // 6  Cyan        — teal-cyan, distinct from green
                "#d3dce6", // 7  White       — silver-white
                "#6b7f99", // 8  Bright Black — grey, readable on dark bg
                "#ff8fa0", // 9  Bright Red  — lighter coral
                "#74e39a", // 10 Bright Green — bright true green
                "#ffd75f", // 11 Bright Yellow — bright gold
                "#79b8f8", // 12 Bright Blue — lighter blue
                "#dda0ee", // 13 Bright Magenta — lighter purple-pink
                "#7edce8", // 14 Bright Cyan — lighter teal
                "#f0f4fc", // 15 Bright White — near-white
            ],
            chrome_focus_border: "#56a7ff",
            chrome_separator: "#1a2333",
            chrome_header_bg: "#0e1520",
            chrome_header_bg_dim: "#080d16",
            chrome_status_bar_bg: "#080d16",
            chrome_csd_close: "#ff5f6d",
            chrome_fg: "#e7f0ff",
            chrome_fg_muted: "#7b8fa9",
            chrome_fg_focus: "#56a7ff",
            chrome_fg_warn: "#e7c547",
            chrome_fg_alert: "#ff5f6d",
            hotspot_filepath: "#4ec970",
            hotspot_url: "#56a7ff",
            hotspot_error: "#ff5f6d",
            hotspot_gitref: "#e7c547",
            hotspot_issueref: "#b49bff",
        },

        // ── Paper ───────────────────────────────────────────────────────
        // Warm-tinted light theme. All ANSI colors are dark enough for
        // WCAG AA (4.5:1) contrast against the cream background. Bright
        // variants are modestly brighter but still readable.
        ThemePreset::Paper => ThemePalette {
            background: "#f2eede",
            foreground: "#000000",
            foreground_bright: "#000000",
            foreground_muted: "#5f6673",
            surface: "#e6e1cf",
            cursor: "#000000",
            selection: "#c4d7f2",
            ansi: [
                "#000000", // 0  Black
                "#c41a16", // 1  Red         — strong red, readable on cream
                "#007400", // 2  Green       — true dark green
                "#7a6400", // 3  Yellow      — dark gold/olive, readable
                "#0e3a8c", // 4  Blue        — dark navy, white-on-blue safe
                "#a020a0", // 5  Magenta     — true purple-magenta
                "#007070", // 6  Cyan        — dark teal
                "#4d4d4d", // 7  White       — dark grey (foreground role on light bg)
                "#666666", // 8  Bright Black — medium grey
                "#a82a22", // 9  Bright Red  — deeper red, high contrast
                "#005e00", // 10 Bright Green — deeper green
                "#6b5500", // 11 Bright Yellow — deeper gold
                "#0c3280", // 12 Bright Blue — deeper blue
                "#8a1a8a", // 13 Bright Magenta — deeper magenta
                "#005e5e", // 14 Bright Cyan — deeper teal
                "#1a1a1a", // 15 Bright White — near-black (text on light bg)
            ],
            chrome_focus_border: "#1659b7",
            chrome_separator: "#c9c2ad",
            chrome_header_bg: "#e6e1cf",
            chrome_header_bg_dim: "#dcd6c1",
            chrome_status_bar_bg: "#dcd6c1",
            chrome_csd_close: "#c41a16",
            chrome_fg: "#1a1a1a",
            chrome_fg_muted: "#5f6673",
            chrome_fg_focus: "#1659b7",
            chrome_fg_warn: "#7a6400",
            chrome_fg_alert: "#c41a16",
            hotspot_filepath: "#007070",
            hotspot_url: "#1659b7",
            hotspot_error: "#c41a16",
            hotspot_gitref: "#826b00",
            hotspot_issueref: "#7b2fa0",
        },

        // ── Tokyo Night Light ───────────────────────────────────────────
        // Cool-toned light theme. ANSI colors from the Tokyo Night
        // palette, darkened for WCAG AA against the light grey background.
        ThemePreset::TokyoNightLight => ThemePalette {
            background: "#D5D6DB",
            foreground: "#343B58",
            foreground_bright: "#1a1e36",
            foreground_muted: "#6b7089",
            surface: "#c5c6cc",
            cursor: "#343B58",
            selection: "#99a7df",
            ansi: [
                "#0F0F14", // 0  Black
                "#8c4351", // 1  Red         — muted red, readable on light
                "#2b6242", // 2  Green       — dark forest green
                "#735212", // 3  Yellow      — dark amber, high contrast
                "#34548a", // 4  Blue        — navy blue, white-on-blue safe
                "#6b3a90", // 5  Magenta     — dark purple, distinct from blue
                "#0e5560", // 6  Cyan        — dark teal, distinct from green
                "#343B58", // 7  White       — dark slate (text on light)
                "#525870", // 8  Bright Black — darker grey for contrast
                "#7a3340", // 9  Bright Red  — deeper red
                "#1f5035", // 10 Bright Green — deeper green
                "#614510", // 11 Bright Yellow — deeper amber
                "#2a4275", // 12 Bright Blue — deeper blue
                "#582d78", // 13 Bright Magenta — deeper purple
                "#0a444e", // 14 Bright Cyan — deeper teal
                "#1a1e36", // 15 Bright White — near-black
            ],
            chrome_focus_border: "#34548A",
            chrome_separator: "#b9bac0",
            chrome_header_bg: "#c9cad0",
            chrome_header_bg_dim: "#bfc0c6",
            chrome_status_bar_bg: "#bfc0c6",
            chrome_csd_close: "#8C4351",
            chrome_fg: "#1f2937",
            chrome_fg_muted: "#6b7089",
            chrome_fg_focus: "#34548A",
            chrome_fg_warn: "#735212",
            chrome_fg_alert: "#8C4351",
            hotspot_filepath: "#0e5560",
            hotspot_url: "#34548A",
            hotspot_error: "#8C4351",
            hotspot_gitref: "#735212",
            hotspot_issueref: "#6b3a90",
        },

        // ── Tomorrow Night Bright ───────────────────────────────────────
        // High-contrast dark theme based on Chris Kempson's Tomorrow
        // palette. All ANSI colors are bright enough for good readability
        // on true black. Blue is darkened enough for white-on-blue bg.
        ThemePreset::TomorrowNightBright => ThemePalette {
            background: "#000000",
            foreground: "#EAEAEA",
            foreground_bright: "#FFFFFF",
            foreground_muted: "#969896",
            surface: "#1a1a1a",
            cursor: "#EAEAEA",
            selection: "#5c7ab5",
            ansi: [
                "#2A2A2A", // 0  Black       — dark grey
                "#D54E53", // 1  Red         — tomato red
                "#B9CA4A", // 2  Green       — lime green (already correct)
                "#E7C547", // 3  Yellow      — gold
                "#5580B0", // 4  Blue        — medium-dark blue, bg-safe
                "#C397D8", // 5  Magenta     — lavender purple
                "#70C0B1", // 6  Cyan        — seafoam teal
                "#EAEAEA", // 7  White       — near-white
                "#969896", // 8  Bright Black — grey
                "#E88388", // 9  Bright Red  — soft red
                "#D0E17D", // 10 Bright Green — light lime
                "#F2DF7E", // 11 Bright Yellow — light gold
                "#7EAFE9", // 12 Bright Blue — brighter blue
                "#D8B4E8", // 13 Bright Magenta — light lavender
                "#96D8CC", // 14 Bright Cyan — light seafoam
                "#FFFFFF", // 15 Bright White
            ],
            chrome_focus_border: "#7AA6DA",
            chrome_separator: "#2A2A2A",
            chrome_header_bg: "#151515",
            chrome_header_bg_dim: "#0a0a0a",
            chrome_status_bar_bg: "#0a0a0a",
            chrome_csd_close: "#D54E53",
            chrome_fg: "#EAEAEA",
            chrome_fg_muted: "#969896",
            chrome_fg_focus: "#7AA6DA",
            chrome_fg_warn: "#E7C547",
            chrome_fg_alert: "#D54E53",
            hotspot_filepath: "#70C0B1",
            hotspot_url: "#7AA6DA",
            hotspot_error: "#D54E53",
            hotspot_gitref: "#E7C547",
            hotspot_issueref: "#C397D8",
        },

        // ── Hemisu Dark ─────────────────────────────────────────────────
        // Desaturated dark theme with characteristic green cursor. ANSI
        // colors are cleaned up: yellow is now a proper warm tone (was a
        // muddy brown), blue is darkened for bg-safety, all remain
        // readable on true black.
        ThemePreset::HemisuDark => ThemePalette {
            background: "#000000",
            foreground: "#FFFFFF",
            foreground_bright: "#FFFFFF",
            foreground_muted: "#888888",
            surface: "#1a1a1a",
            cursor: "#BAFFAA",
            selection: "#4d7a44",
            ansi: [
                "#444444", // 0  Black       — dark grey
                "#FF0054", // 1  Red         — hot pink-red
                "#B1D630", // 2  Green       — lime green (already correct)
                "#D4A520", // 3  Yellow      — goldenrod, proper warm yellow
                "#4B88C7", // 4  Blue        — medium blue, bg-safe for white text
                "#B576BC", // 5  Magenta     — mauve purple
                "#569A9F", // 6  Cyan        — muted teal
                "#EDEDED", // 7  White       — near-white
                "#777777", // 8  Bright Black — grey
                "#FF5C84", // 9  Bright Red  — lighter hot pink
                "#BAFFAA", // 10 Bright Green — bright lime
                "#F0D060", // 11 Bright Yellow — bright gold
                "#7DB4E6", // 12 Bright Blue — lighter blue
                "#DEB3DF", // 13 Bright Magenta — light mauve
                "#83C5C9", // 14 Bright Cyan — lighter teal
                "#FFFFFF", // 15 Bright White
            ],
            chrome_focus_border: "#67BEE3",
            chrome_separator: "#2a2a2a",
            chrome_header_bg: "#141414",
            chrome_header_bg_dim: "#0a0a0a",
            chrome_status_bar_bg: "#0a0a0a",
            chrome_csd_close: "#FF0054",
            chrome_fg: "#EDEDED",
            chrome_fg_muted: "#777777",
            chrome_fg_focus: "#67BEE3",
            chrome_fg_warn: "#D4A520",
            chrome_fg_alert: "#FF0054",
            hotspot_filepath: "#569A9F",
            hotspot_url: "#67BEE3",
            hotspot_error: "#FF0054",
            hotspot_gitref: "#D4A520",
            hotspot_issueref: "#B576BC",
        },
    }
}
