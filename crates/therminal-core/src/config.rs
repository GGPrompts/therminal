//! TOML configuration system for Therminal.
//!
//! Defines [`TherminalConfig`] with sections for general settings, font,
//! colors, keybindings, profiles, and trust tiers.  Config is loaded from
//! `therminal_runtime::paths::config_dir() / "therminal.toml"` with sensible
//! defaults that match the current hardcoded values.
//!
//! Use [`ConfigWatcher`] for hot-reload via filesystem notifications.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::font::FontConfig as CoreFontConfig;
use crate::palette::Color;

// ── Config file path ─────────────────────────────────────────────────────

/// Default config filename.
const CONFIG_FILENAME: &str = "therminal.toml";

/// Return the full path to the Therminal config file.
pub fn config_path() -> PathBuf {
    therminal_runtime::paths::config_dir().join(CONFIG_FILENAME)
}

// ── Top-level config ─────────────────────────────────────────────────────

/// Root configuration for Therminal, deserialized from `therminal.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TherminalConfig {
    /// General window and behavior settings.
    pub general: GeneralConfig,
    /// Font family, size, and rendering options.
    pub font: FontConfig,
    /// Terminal color palette overrides.
    pub colors: ColorsConfig,
    /// Keybinding mappings.
    pub keybindings: KeybindingsConfig,
    /// Named session profiles.
    pub profiles: HashMap<String, ProfileConfig>,
    /// Agent trust tier settings.
    pub trust: TrustConfig,
}

impl TherminalConfig {
    /// Load config from the default path, falling back to defaults if the
    /// file does not exist or contains errors.
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    /// Load config from a specific path.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<TherminalConfig>(&contents) {
                Ok(config) => {
                    info!(?path, "loaded config");
                    config
                }
                Err(e) => {
                    warn!(?path, %e, "failed to parse config, using defaults");
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(?path, "no config file found, using defaults");
                Self::default()
            }
            Err(e) => {
                warn!(?path, %e, "failed to read config, using defaults");
                Self::default()
            }
        }
    }

    /// Write the current config to the default path (creates parent dirs).
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&config_path())
    }

    /// Write the current config to a specific path.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, contents)
    }

    /// Build a [`CoreFontConfig`] from the config's font section.
    pub fn to_core_font_config(&self) -> CoreFontConfig {
        CoreFontConfig {
            family: if self.font.family.is_empty() {
                None
            } else {
                Some(self.font.family.clone())
            },
            size: self.font.size,
            line_height_scale: self.font.line_height_scale,
            extra_fallbacks: self.font.extra_fallbacks.clone(),
            nerd_font: self.font.nerd_font,
        }
    }
}

// ── Section: General ─────────────────────────────────────────────────────

/// General window and behavior settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Window title.
    pub title: String,
    /// Default window width in logical pixels.
    pub window_width: f64,
    /// Default window height in logical pixels.
    pub window_height: f64,
    /// Scrollback history size (lines).
    pub scrollback_lines: usize,
    /// Shell command to run. If empty, uses the user's default shell.
    pub shell: String,
    /// Extra environment variables set in the PTY.
    pub env: HashMap<String, String>,
    /// Padding in pixels around the terminal grid.
    pub padding: f32,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            title: "Therminal".to_string(),
            window_width: 1280.0,
            window_height: 800.0,
            scrollback_lines: 10_000,
            shell: String::new(),
            env: HashMap::new(),
            padding: 4.0,
        }
    }
}

// ── Section: Font ────────────────────────────────────────────────────────

/// Font rendering configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    /// Primary font family name. Empty string uses platform default.
    pub family: String,
    /// Font size in points (before DPI scaling).
    pub size: f32,
    /// Line-height multiplier (applied to `size`).
    pub line_height_scale: f32,
    /// Extra fallback font families.
    pub extra_fallbacks: Vec<String>,
    /// Whether to try Nerd Font variant of the primary family.
    pub nerd_font: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            // Matches the grid_renderer DEFAULT_FONT_FAMILY
            family: "JetBrainsMono Nerd Font Mono".to_string(),
            // Matches grid_renderer DEFAULT_FONT_SIZE
            size: 17.0,
            // Matches grid_renderer LINE_HEIGHT_RATIO
            line_height_scale: 1.375,
            extra_fallbacks: vec!["Noto Color Emoji".to_string()],
            nerd_font: true,
        }
    }
}

// ── Section: Colors ──────────────────────────────────────────────────────

/// Hex color string (e.g. "#0a0010" or "0a0010").
type HexColor = String;

/// Terminal color palette configuration.
///
/// All fields are optional hex strings. When `None`, the built-in thermal
/// palette constant is used.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    /// Background color.
    pub background: Option<HexColor>,
    /// Main text color.
    pub foreground: Option<HexColor>,
    /// Bright text color.
    pub foreground_bright: Option<HexColor>,
    /// Muted text color.
    pub foreground_muted: Option<HexColor>,
    /// Surface/panel background.
    pub surface: Option<HexColor>,
    /// Cursor color.
    pub cursor: Option<HexColor>,
    /// Selection background color.
    pub selection: Option<HexColor>,

    /// ANSI 16-color overrides (indices 0-15).
    pub ansi: Option<Vec<HexColor>>,
}

impl ColorsConfig {
    /// Parse a hex color string into a [`Color`].
    ///
    /// Accepts formats: `"#RRGGBB"`, `"RRGGBB"`, `"#RGB"`.
    pub fn parse_hex(s: &str) -> Option<Color> {
        let s = s.strip_prefix('#').unwrap_or(s);
        let hex = u32::from_str_radix(s, 16).ok()?;
        if s.len() == 6 {
            Some(Color::from_hex(hex))
        } else if s.len() == 3 {
            // Expand #RGB to #RRGGBB
            let r = ((hex >> 8) & 0xF) as u8;
            let g = ((hex >> 4) & 0xF) as u8;
            let b = (hex & 0xF) as u8;
            Some(Color::from_rgba(r << 4 | r, g << 4 | g, b << 4 | b, 255))
        } else {
            None
        }
    }

    /// Resolve background color, falling back to palette default.
    pub fn background_color(&self) -> Color {
        self.background
            .as_deref()
            .and_then(Self::parse_hex)
            .unwrap_or(Color::BG)
    }

    /// Resolve foreground color, falling back to palette default.
    pub fn foreground_color(&self) -> Color {
        self.foreground
            .as_deref()
            .and_then(Self::parse_hex)
            .unwrap_or(Color::TEXT)
    }
}

// ── Section: Keybindings ─────────────────────────────────────────────────

/// Typed action for a keybinding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    /// Copy selected text to the clipboard.
    Copy,
    /// Paste text from the clipboard.
    Paste,
    /// Increase the font size by one step.
    FontSizeUp,
    /// Decrease the font size by one step.
    FontSizeDown,
    /// Reset the font size to the configured default.
    FontSizeReset,
}

/// A single keybinding entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybinding {
    /// Key combination (e.g. "ctrl+shift+c", "ctrl+plus").
    pub key: String,
    /// Action to perform when this keybinding is triggered.
    pub action: KeyAction,
}

/// Keybinding configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    /// List of keybinding overrides. These are merged on top of defaults.
    pub bindings: Vec<Keybinding>,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            bindings: vec![
                Keybinding {
                    key: "ctrl+shift+c".to_string(),
                    action: KeyAction::Copy,
                },
                Keybinding {
                    key: "ctrl+shift+v".to_string(),
                    action: KeyAction::Paste,
                },
                Keybinding {
                    key: "ctrl+plus".to_string(),
                    action: KeyAction::FontSizeUp,
                },
                Keybinding {
                    key: "ctrl+minus".to_string(),
                    action: KeyAction::FontSizeDown,
                },
                Keybinding {
                    key: "ctrl+0".to_string(),
                    action: KeyAction::FontSizeReset,
                },
            ],
        }
    }
}

// ── Section: Profiles ────────────────────────────────────────────────────

/// A named session profile with optional overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    /// Shell command override for this profile.
    pub shell: Option<String>,
    /// Working directory override.
    pub working_directory: Option<String>,
    /// Extra environment variables for this profile.
    pub env: HashMap<String, String>,
    /// Font size override for this profile.
    pub font_size: Option<f32>,
    /// Scrollback lines override.
    pub scrollback_lines: Option<usize>,
}

// ── Section: Trust ───────────────────────────────────────────────────────

/// Trust tier assigned to an AI agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Minimal permissions; agent actions are heavily restricted.
    Sandboxed,
    /// Actions require user confirmation.
    Supervised,
    /// Full access; agent is treated as trusted.
    Trusted,
}

/// Agent trust tier configuration.
///
/// Controls what level of access AI agents have when detected in the
/// terminal session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustConfig {
    /// Default trust tier for unknown agents.
    pub default_tier: TrustTier,
    /// Per-agent trust overrides, keyed by agent process name.
    pub agents: HashMap<String, AgentTrust>,
    /// Whether to show visual indicators when agents are detected.
    pub show_agent_indicator: bool,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_tier: TrustTier::Supervised,
            agents: HashMap::new(),
            show_agent_indicator: true,
        }
    }
}

/// Trust settings for a specific agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTrust {
    /// Trust tier for this agent.
    pub tier: TrustTier,
    /// Optional list of allowed MCP tool patterns.
    pub allowed_tools: Option<Vec<String>>,
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_through_toml() {
        let config = TherminalConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(decoded.general.title, "Therminal");
        assert_eq!(decoded.font.size, 17.0);
        assert_eq!(decoded.trust.default_tier, TrustTier::Supervised);
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_fields() {
        let toml_str = r#"
[font]
size = 20.0
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.font.size, 20.0);
        // Other font fields use defaults
        assert_eq!(config.font.family, "JetBrainsMono Nerd Font Mono");
        assert!(config.font.nerd_font);
        // General section uses full defaults
        assert_eq!(config.general.title, "Therminal");
    }

    #[test]
    fn parse_hex_6_digit() {
        let c = ColorsConfig::parse_hex("#0a0010").unwrap();
        assert_eq!(c, Color::BG);
    }

    #[test]
    fn parse_hex_without_hash() {
        let c = ColorsConfig::parse_hex("c4b5fd").unwrap();
        assert_eq!(c, Color::TEXT);
    }

    #[test]
    fn parse_hex_3_digit() {
        let c = ColorsConfig::parse_hex("#fff").unwrap();
        assert_eq!(c.r, 255);
        assert_eq!(c.g, 255);
        assert_eq!(c.b, 255);
    }

    #[test]
    fn parse_hex_invalid_returns_none() {
        assert!(ColorsConfig::parse_hex("xyz").is_none());
        assert!(ColorsConfig::parse_hex("").is_none());
    }

    #[test]
    fn load_from_nonexistent_returns_defaults() {
        let config = TherminalConfig::load_from(Path::new("/nonexistent/therminal.toml"));
        assert_eq!(config.general.title, "Therminal");
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");

        let mut config = TherminalConfig::default();
        config.general.title = "Test Terminal".to_string();
        config.font.size = 22.0;
        config.save_to(&path).unwrap();

        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(loaded.general.title, "Test Terminal");
        assert_eq!(loaded.font.size, 22.0);
    }

    #[test]
    fn profiles_deserialize() {
        let toml_str = r#"
[profiles.dev]
shell = "/bin/zsh"
font_size = 14.0

[profiles.server]
shell = "/bin/bash"
working_directory = "/srv"
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(config.profiles["dev"].shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(
            config.profiles["server"].working_directory.as_deref(),
            Some("/srv")
        );
    }

    #[test]
    fn trust_config_deserialize() {
        let toml_str = r#"
[trust]
default_tier = "sandboxed"
show_agent_indicator = false

[trust.agents.claude]
tier = "trusted"
allowed_tools = ["read_file", "write_file"]
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.trust.default_tier, TrustTier::Sandboxed);
        assert!(!config.trust.show_agent_indicator);
        assert_eq!(config.trust.agents["claude"].tier, TrustTier::Trusted);
    }

    #[test]
    fn to_core_font_config_maps_fields() {
        let config = TherminalConfig::default();
        let core = config.to_core_font_config();
        assert_eq!(core.size, 17.0);
        assert!(core.nerd_font);
    }

    #[test]
    fn colors_fallback_to_palette_defaults() {
        let colors = ColorsConfig::default();
        assert_eq!(colors.background_color(), Color::BG);
        assert_eq!(colors.foreground_color(), Color::TEXT);
    }

    #[test]
    fn colors_override_palette() {
        let colors = ColorsConfig {
            background: Some("#ff0000".to_string()),
            ..Default::default()
        };
        let bg = colors.background_color();
        assert_eq!(bg.r, 255);
        assert_eq!(bg.g, 0);
        assert_eq!(bg.b, 0);
    }

    #[test]
    fn keybindings_default_has_copy_paste() {
        let kb = KeybindingsConfig::default();
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::Copy));
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::Paste));
    }
}
