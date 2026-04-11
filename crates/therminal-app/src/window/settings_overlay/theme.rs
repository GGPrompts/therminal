//! Theme preset palettes: `apply_theme_preset`.

use therminal_core::config::ColorsConfig;

use super::types::ThemePreset;

pub(crate) fn apply_theme_preset(colors: &mut ColorsConfig, preset: ThemePreset) {
    let (background, foreground, cursor, ansi): (&str, &str, &str, [&str; 16]) = match preset {
        ThemePreset::OriginalTherminal => (
            "#060a12",
            "#e7f0ff",
            "#fef3c7",
            [
                "#060a12", "#ff5f78", "#ffb24f", "#eab308", "#56a7ff", "#56a7ff", "#39ffb6",
                "#e7f0ff", "#7b8fa9", "#ff7d8f", "#59ffc7", "#f97316", "#56a7ff", "#e7f0ff",
                "#4ce5cc", "#fef3c7",
            ],
        ),
        ThemePreset::Paper => (
            "#f2eede",
            "#000000",
            "#000000",
            [
                "#000000", "#b13a24", "#216609", "#7a5f00", "#1659b7", "#5c21a5", "#106c66",
                "#5f6673", "#555555", "#b13a24", "#216609", "#7a5f00", "#1659b7", "#5c21a5",
                "#106c66", "#5f6673",
            ],
        ),
        ThemePreset::TokyoNightLight => (
            "#D5D6DB",
            "#4B5563",
            "#4B5563",
            [
                "#0F0F14", "#8C4351", "#485E30", "#7B4F10", "#34548A", "#5A4A78", "#0F4B6E",
                "#343B58", "#4B5563", "#8C4351", "#485E30", "#7B4F10", "#34548A", "#5A4A78",
                "#0F4B6E", "#343B58",
            ],
        ),
        ThemePreset::TomorrowNightBright => (
            "#000000",
            "#EAEAEA",
            "#EAEAEA",
            [
                "#2A2A2A", "#D54E53", "#B9CA4A", "#E7C547", "#7AA6DA", "#C397D8", "#70C0B1",
                "#EAEAEA", "#969896", "#D54E53", "#B9CA4A", "#E7C547", "#7AA6DA", "#C397D8",
                "#70C0B1", "#FFFFFF",
            ],
        ),
        ThemePreset::HemisuDark => (
            "#000000",
            "#FFFFFF",
            "#BAFFAA",
            [
                "#444444", "#FF0054", "#B1D630", "#9D895E", "#67BEE3", "#B576BC", "#569A9F",
                "#EDEDED", "#777777", "#D65E75", "#BAFFAA", "#ECE1C8", "#9FD3E5", "#DEB3DF",
                "#B6E0E5", "#FFFFFF",
            ],
        ),
    };
    colors.background = Some(background.to_string());
    colors.foreground = Some(foreground.to_string());
    colors.cursor = Some(cursor.to_string());
    colors.ansi = Some(ansi.iter().map(|c| (*c).to_string()).collect());
}
