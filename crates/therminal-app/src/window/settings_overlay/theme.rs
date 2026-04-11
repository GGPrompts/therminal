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
    cursor: &'static str,
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
    colors.cursor = Some(p.cursor.to_string());
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
        ThemePreset::OriginalTherminal => ThemePalette {
            background: "#060a12",
            foreground: "#e7f0ff",
            cursor: "#fef3c7",
            ansi: [
                "#060a12", "#ff5f78", "#ffb24f", "#eab308", "#56a7ff", "#56a7ff", "#39ffb6",
                "#e7f0ff", "#7b8fa9", "#ff7d8f", "#59ffc7", "#f97316", "#56a7ff", "#e7f0ff",
                "#4ce5cc", "#fef3c7",
            ],
            chrome_focus_border: "#56a7ff",
            chrome_separator: "#1a2333",
            chrome_header_bg: "#0e1520",
            chrome_header_bg_dim: "#080d16",
            chrome_status_bar_bg: "#080d16",
            chrome_csd_close: "#ff5f78",
            chrome_fg: "#e7f0ff",
            chrome_fg_muted: "#7b8fa9",
            chrome_fg_focus: "#56a7ff",
            chrome_fg_warn: "#eab308",
            chrome_fg_alert: "#ff5f78",
            hotspot_filepath: "#39ffb6",
            hotspot_url: "#56a7ff",
            hotspot_error: "#ff5f78",
            hotspot_gitref: "#eab308",
            hotspot_issueref: "#b49bff",
        },
        ThemePreset::Paper => ThemePalette {
            background: "#f2eede",
            foreground: "#000000",
            cursor: "#000000",
            ansi: [
                "#000000", "#b13a24", "#216609", "#7a5f00", "#1659b7", "#5c21a5", "#106c66",
                "#5f6673", "#555555", "#b13a24", "#216609", "#7a5f00", "#1659b7", "#5c21a5",
                "#106c66", "#5f6673",
            ],
            chrome_focus_border: "#1659b7",
            chrome_separator: "#c9c2ad",
            chrome_header_bg: "#e6e1cf",
            chrome_header_bg_dim: "#dcd6c1",
            chrome_status_bar_bg: "#dcd6c1",
            chrome_csd_close: "#b13a24",
            chrome_fg: "#1a1a1a",
            chrome_fg_muted: "#5f6673",
            chrome_fg_focus: "#1659b7",
            chrome_fg_warn: "#7a5f00",
            chrome_fg_alert: "#b13a24",
            hotspot_filepath: "#106c66",
            hotspot_url: "#1659b7",
            hotspot_error: "#b13a24",
            hotspot_gitref: "#7a5f00",
            hotspot_issueref: "#5c21a5",
        },
        ThemePreset::TokyoNightLight => ThemePalette {
            background: "#D5D6DB",
            foreground: "#4B5563",
            cursor: "#4B5563",
            ansi: [
                "#0F0F14", "#8C4351", "#485E30", "#7B4F10", "#34548A", "#5A4A78", "#0F4B6E",
                "#343B58", "#4B5563", "#8C4351", "#485E30", "#7B4F10", "#34548A", "#5A4A78",
                "#0F4B6E", "#343B58",
            ],
            chrome_focus_border: "#34548A",
            chrome_separator: "#b9bac0",
            chrome_header_bg: "#c9cad0",
            chrome_header_bg_dim: "#bfc0c6",
            chrome_status_bar_bg: "#bfc0c6",
            chrome_csd_close: "#8C4351",
            chrome_fg: "#1f2937",
            chrome_fg_muted: "#4B5563",
            chrome_fg_focus: "#34548A",
            chrome_fg_warn: "#7B4F10",
            chrome_fg_alert: "#8C4351",
            hotspot_filepath: "#0F4B6E",
            hotspot_url: "#34548A",
            hotspot_error: "#8C4351",
            hotspot_gitref: "#7B4F10",
            hotspot_issueref: "#5A4A78",
        },
        ThemePreset::TomorrowNightBright => ThemePalette {
            background: "#000000",
            foreground: "#EAEAEA",
            cursor: "#EAEAEA",
            ansi: [
                "#2A2A2A", "#D54E53", "#B9CA4A", "#E7C547", "#7AA6DA", "#C397D8", "#70C0B1",
                "#EAEAEA", "#969896", "#D54E53", "#B9CA4A", "#E7C547", "#7AA6DA", "#C397D8",
                "#70C0B1", "#FFFFFF",
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
        ThemePreset::HemisuDark => ThemePalette {
            background: "#000000",
            foreground: "#FFFFFF",
            cursor: "#BAFFAA",
            ansi: [
                "#444444", "#FF0054", "#B1D630", "#9D895E", "#67BEE3", "#B576BC", "#569A9F",
                "#EDEDED", "#777777", "#D65E75", "#BAFFAA", "#ECE1C8", "#9FD3E5", "#DEB3DF",
                "#B6E0E5", "#FFFFFF",
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
            chrome_fg_warn: "#9D895E",
            chrome_fg_alert: "#FF0054",
            hotspot_filepath: "#569A9F",
            hotspot_url: "#67BEE3",
            hotspot_error: "#FF0054",
            hotspot_gitref: "#9D895E",
            hotspot_issueref: "#B576BC",
        },
    }
}
