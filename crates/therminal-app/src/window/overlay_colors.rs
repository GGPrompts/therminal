//! Opaque overlay surfaces keep readability independent of underlying terminal content.
use therminal_core::palette::{ChromePalette, Color};

pub(super) fn opaque(mut color: [f32; 4]) -> [f32; 4] {
    color[3] = 1.0;
    color
}

fn color(rgba: [f32; 4]) -> Color {
    Color::from_rgba(
        (rgba[0] * 255.0).round() as u8,
        (rgba[1] * 255.0).round() as u8,
        (rgba[2] * 255.0).round() as u8,
        255,
    )
}

/// Preserve the theme foreground when readable; otherwise choose black or white.
/// Both the background and returned text are opaque, so the ratio is stable.
pub(super) fn readable_text(background: [f32; 4], preferred: Color) -> glyphon::Color {
    let bg = color(background);

    let fg = if preferred.contrast_ratio(bg) >= 4.5 {
        preferred
    } else {
        let black = Color::from_hex(0);
        let white = Color::from_hex(0xffffff);
        if black.contrast_ratio(bg) > white.contrast_ratio(bg) {
            black
        } else {
            white
        }
    };
    glyphon::Color::rgba(fg.r, fg.g, fg.b, 255)
}

pub(super) fn panel(palette: &ChromePalette) -> [f32; 4] {
    opaque(palette.header_bg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_remains_readable_on_dark_light_and_colored_surfaces() {
        // Include custom profile colors and themes with low-contrast foregrounds.
        for bg in [
            0x0e1520, 0xfafafa, 0x404d66, 0x2563eb, 0x5e6ad2, 0xffee00, 0x777777,
        ] {
            for fg in [0xe7f0ff, 0x101010, 0x56a7ff, bg] {
                let background = Color::from_hex(bg);
                let text = readable_text(background.to_f32_array(), Color::from_hex(fg));
                let text = Color::from_rgba(text.r(), text.g(), text.b(), text.a());
                assert_eq!(text.a, 255);
                assert!(
                    text.contrast_ratio(background) >= 4.5,
                    "{bg:06x} / {fg:06x}"
                );
            }
        }
    }
}
