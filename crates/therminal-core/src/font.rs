//! Cross-platform font discovery and fallback chain for cosmic-text.
//!
//! [`FontConfig`] describes the user's desired font family, size, and fallback
//! order.  [`build_font_system`] turns that config into a fully-loaded
//! [`FontSystem`] with the platform's system fonts plus the configured
//! monospace family set on the underlying fontdb database.
//!
//! # Fallback order
//!
//! When cosmic-text cannot find a glyph in the primary family it walks the
//! fallback list in order:
//!
//! 1. User-specified family (from config)
//! 2. Nerd Font variant (if available)
//! 3. Emoji font
//! 4. CJK fallback
//! 5. Platform monospace default
//!
//! All of these are *hints* — cosmic-text resolves them through fontdb which
//! delegates to the platform font backend (fontconfig / CoreText / DirectWrite).

use cosmic_text::FontSystem;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Platform defaults
// ---------------------------------------------------------------------------

/// Primary monospace font family per platform.
#[cfg(target_os = "linux")]
pub const PLATFORM_MONOSPACE: &str = "monospace";

#[cfg(target_os = "macos")]
pub const PLATFORM_MONOSPACE: &str = "Menlo";

#[cfg(target_os = "windows")]
pub const PLATFORM_MONOSPACE: &str = "Cascadia Mono";

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub const PLATFORM_MONOSPACE: &str = "monospace";

/// Secondary monospace fallback (used when the primary is not installed).
#[cfg(target_os = "macos")]
const PLATFORM_MONOSPACE_FALLBACK: &str = "SF Mono";

#[cfg(target_os = "windows")]
const PLATFORM_MONOSPACE_FALLBACK: &str = "Consolas";

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const PLATFORM_MONOSPACE_FALLBACK: &str = "DejaVu Sans Mono";

/// Common Nerd-Font–patched family name suffix.
const NERD_FONT_SUFFIX: &str = " Nerd Font Mono";

/// Well-known emoji fonts per platform.
#[cfg(target_os = "linux")]
const EMOJI_FONT: &str = "Noto Color Emoji";

#[cfg(target_os = "macos")]
const EMOJI_FONT: &str = "Apple Color Emoji";

#[cfg(target_os = "windows")]
const EMOJI_FONT: &str = "Segoe UI Emoji";

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const EMOJI_FONT: &str = "Noto Color Emoji";

/// CJK fallback font.
#[cfg(target_os = "macos")]
const CJK_FONT: &str = "Hiragino Sans";

#[cfg(not(target_os = "macos"))]
const CJK_FONT: &str = "Noto Sans CJK SC";

// ---------------------------------------------------------------------------
// FontConfig
// ---------------------------------------------------------------------------

/// Font configuration for the terminal.
///
/// Serialisable so it can live in a TOML/JSON config file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// Primary font family name.  If `None`, uses [`PLATFORM_MONOSPACE`].
    pub family: Option<String>,
    /// Font size in points.
    pub size: f32,
    /// Line-height multiplier (applied to `size`).
    pub line_height_scale: f32,
    /// Extra families to try before the built-in fallback chain.
    /// Evaluated in order after `family`.
    pub extra_fallbacks: Vec<String>,
    /// Whether to try a Nerd Font variant of the primary family.
    pub nerd_font: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: None,
            size: 14.0,
            line_height_scale: 1.2,
            extra_fallbacks: Vec::new(),
            nerd_font: true,
        }
    }
}

impl FontConfig {
    /// Computed line height in points.
    pub fn line_height(&self) -> f32 {
        self.size * self.line_height_scale
    }

    /// The effective primary family name, resolving `None` to the platform
    /// default.
    pub fn effective_family(&self) -> &str {
        self.family.as_deref().unwrap_or(PLATFORM_MONOSPACE)
    }

    /// Build the full fallback chain.
    ///
    /// Returns an ordered list of family names that should be tried when
    /// looking for glyphs.  The first entry is the primary family.
    pub fn fallback_chain(&self) -> Vec<String> {
        let mut chain = Vec::with_capacity(8);

        // 1. Primary family
        let primary = self.effective_family().to_owned();
        chain.push(primary.clone());

        // 2. Nerd Font variant of the primary
        if self.nerd_font {
            chain.push(format!("{primary}{NERD_FONT_SUFFIX}"));
        }

        // 3. User-specified extra fallbacks
        chain.extend(self.extra_fallbacks.iter().cloned());

        // 4. Emoji
        chain.push(EMOJI_FONT.to_owned());

        // 5. CJK
        chain.push(CJK_FONT.to_owned());

        // 6. Platform monospace fallback (if different from primary)
        if primary != PLATFORM_MONOSPACE {
            chain.push(PLATFORM_MONOSPACE.to_owned());
        }
        if primary != PLATFORM_MONOSPACE_FALLBACK {
            chain.push(PLATFORM_MONOSPACE_FALLBACK.to_owned());
        }

        chain
    }
}

// ---------------------------------------------------------------------------
// FontSystem builder
// ---------------------------------------------------------------------------

/// Create a [`FontSystem`] configured according to `config`.
///
/// This loads all system fonts (via fontconfig / CoreText / DirectWrite) and
/// sets the fontdb monospace family to the user's chosen primary so that
/// `Family::Monospace` resolves to the right typeface.
pub fn build_font_system(config: &FontConfig) -> FontSystem {
    let mut font_system = FontSystem::new();

    // Tell fontdb which family name to resolve for the generic `monospace`
    // keyword.  This affects `Family::Monospace` in cosmic-text / glyphon.
    let db = font_system.db_mut();
    db.set_monospace_family(config.effective_family());

    tracing::info!(
        family = config.effective_family(),
        size = config.size,
        fallbacks = ?config.fallback_chain(),
        "font system initialised"
    );

    font_system
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_platform_monospace() {
        let cfg = FontConfig::default();
        assert_eq!(cfg.effective_family(), PLATFORM_MONOSPACE);
    }

    #[test]
    fn custom_family_overrides_default() {
        let cfg = FontConfig {
            family: Some("JetBrains Mono".into()),
            ..Default::default()
        };
        assert_eq!(cfg.effective_family(), "JetBrains Mono");
    }

    #[test]
    fn fallback_chain_contains_expected_entries() {
        let cfg = FontConfig {
            family: Some("Fira Code".into()),
            extra_fallbacks: vec!["Iosevka".into()],
            ..Default::default()
        };
        let chain = cfg.fallback_chain();

        // Primary
        assert_eq!(chain[0], "Fira Code");
        // Nerd font variant
        assert!(chain.iter().any(|f| f.contains("Nerd Font")));
        // Extra fallback
        assert!(chain.contains(&"Iosevka".into()));
        // Emoji
        assert!(chain.contains(&EMOJI_FONT.into()));
        // CJK
        assert!(chain.contains(&CJK_FONT.into()));
        // Platform default still present as fallback
        assert!(chain.contains(&PLATFORM_MONOSPACE.into()));
    }

    #[test]
    fn nerd_font_disabled_removes_nerd_entry() {
        let cfg = FontConfig {
            nerd_font: false,
            ..Default::default()
        };
        let chain = cfg.fallback_chain();
        assert!(!chain.iter().any(|f| f.contains("Nerd Font")));
    }

    #[test]
    fn line_height_computed_correctly() {
        let cfg = FontConfig {
            size: 16.0,
            line_height_scale: 1.5,
            ..Default::default()
        };
        assert!((cfg.line_height() - 24.0).abs() < f32::EPSILON);
    }

    #[test]
    fn build_font_system_does_not_panic() {
        let cfg = FontConfig::default();
        let _fs = build_font_system(&cfg);
    }

    #[test]
    fn serde_round_trip() {
        let cfg = FontConfig {
            family: Some("Hack".into()),
            size: 18.0,
            line_height_scale: 1.3,
            extra_fallbacks: vec!["Inconsolata".into()],
            nerd_font: false,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: FontConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.family.as_deref(), Some("Hack"));
        assert_eq!(decoded.extra_fallbacks.len(), 1);
        assert!(!decoded.nerd_font);
    }
}
