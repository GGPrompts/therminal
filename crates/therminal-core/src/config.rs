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
    /// Terminal OSC sequence interceptor settings.
    pub terminal: TerminalConfig,
}

impl TherminalConfig {
    /// Load config from the default path, falling back to defaults if the
    /// file does not exist or contains errors.
    ///
    /// When the config file doesn't exist, a fully-commented default config
    /// is written to disk so users can discover all available options.
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    /// Load config from a specific path.
    ///
    /// If the file doesn't exist, writes a commented default config and
    /// returns [`TherminalConfig::default`].
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
                info!(?path, "no config file found, writing default config");
                let defaults = Self::default();
                if let Err(write_err) = defaults.save_default_to(path) {
                    warn!(?path, %write_err, "failed to write default config");
                }
                defaults
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

    /// Write a fully-commented default config to the default path.
    ///
    /// All values are commented out (`#`-prefixed) so the file acts as
    /// documentation rather than live overrides.  Users can uncomment and
    /// edit any line to take effect on the next hot-reload.
    pub fn save_default() -> std::io::Result<()> {
        Self::default().save_default_to(&config_path())
    }

    /// Write a fully-commented default config to `path` (creates parent dirs).
    pub fn save_default_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, Self::default_config_text())
    }

    /// Return the fully-commented default config as a TOML string.
    ///
    /// Every line is either a comment or a commented-out value so that the
    /// file round-trips back to defaults when un-commented.
    pub fn default_config_text() -> String {
        let d = Self::default();
        let mut out = String::new();

        out.push_str("# Therminal config — hot-reloaded on save\n");
        out.push_str("# Uncomment and edit any value to override the default.\n");
        out.push('\n');

        // ── [general] ───────────────────────────────────────────────────────
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [general] — Window geometry, shell, scrollback, and environment.\n");
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("[general]\n");
        out.push_str(&format!("# title = {:?}\n", d.general.title));
        out.push_str(&format!("# window_width = {}\n", d.general.window_width));
        out.push_str(&format!("# window_height = {}\n", d.general.window_height));
        out.push_str(&format!(
            "# scrollback_lines = {}\n",
            d.general.scrollback_lines
        ));
        out.push_str("# shell = \"\"  # empty = user's default shell\n");
        out.push_str(&format!("# padding = {}\n", d.general.padding));
        out.push_str("# [general.env]  # extra PTY environment variables\n");
        out.push_str("# MY_VAR = \"value\"\n");
        out.push('\n');

        // ── [font] ───────────────────────────────────────────────────────────
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [font] — Font family, size, and rendering options.\n");
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("[font]\n");
        out.push_str(&format!("# family = {:?}\n", d.font.family));
        out.push_str(&format!("# size = {}\n", d.font.size));
        out.push_str(&format!(
            "# line_height_scale = {}\n",
            d.font.line_height_scale
        ));
        out.push_str("# extra_fallbacks = [\"Noto Color Emoji\"]\n");
        out.push_str(&format!("# nerd_font = {}\n", d.font.nerd_font));
        out.push_str(&format!("# ui_font_family = {:?}\n", d.font.ui_font_family));
        out.push_str(&format!(
            "# display_font_family = {:?}\n",
            d.font.display_font_family
        ));
        out.push('\n');

        // ── [colors] ─────────────────────────────────────────────────────────
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [colors] — Terminal color palette overrides (hex: \"#RRGGBB\").\n");
        out.push_str("# Leave a field absent (or comment it out) to use the built-in palette.\n");
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("[colors]\n");
        out.push_str("# background = \"#060a12\"\n");
        out.push_str("# foreground = \"#e7f0ff\"\n");
        out.push_str("# foreground_bright = \"#e7f0ff\"\n");
        out.push_str("# foreground_muted = \"#a9b8cd\"\n");
        out.push_str("# surface = \"#18263a\"\n");
        out.push_str("# cursor = \"#56a7ff\"\n");
        out.push_str("# selection = \"#22324a\"\n");
        out.push_str("# ansi = [  # 16-entry ANSI palette override\n");
        out.push_str("#   \"#000000\", \"#cc0000\", \"#00cc00\", \"#cccc00\",\n");
        out.push_str("#   \"#0000cc\", \"#cc00cc\", \"#00cccc\", \"#cccccc\",\n");
        out.push_str("#   \"#888888\", \"#ff5555\", \"#55ff55\", \"#ffff55\",\n");
        out.push_str("#   \"#5555ff\", \"#ff55ff\", \"#55ffff\", \"#ffffff\",\n");
        out.push_str("# ]\n");
        out.push('\n');

        // ── [terminal] ───────────────────────────────────────────────────────
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [terminal] — OSC sequence interceptor.\n");
        out.push_str(
            "# Controls which escape-sequence families are intercepted for AI-awareness\n",
        );
        out.push_str("# and shell integration.  Disable a family only if a third-party tool\n");
        out.push_str("# conflicts.  All families are enabled by default.\n");
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("[terminal]\n");
        out.push_str(&format!(
            "# osc_633 = {}  # VS Code shell integration\n",
            d.terminal.osc_633
        ));
        out.push_str(&format!(
            "# osc_133 = {}  # FinalTerm shell integration\n",
            d.terminal.osc_133
        ));
        out.push_str(&format!(
            "# osc_7 = {}   # current working directory\n",
            d.terminal.osc_7
        ));
        out.push_str(&format!(
            "# osc_1337 = {}  # iTerm2 extensions\n",
            d.terminal.osc_1337
        ));
        out.push('\n');

        // ── [trust] ──────────────────────────────────────────────────────────
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [trust] — AI agent trust tiers.\n");
        out.push_str("# default_tier: \"sandboxed\" | \"supervised\" | \"trusted\"\n");
        out.push_str("# agent_scan_interval: seconds between process-tree scans (0 = disabled).\n");
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("[trust]\n");
        out.push_str(&format!(
            "# default_tier = {:?}\n",
            Self::trust_tier_str(&d.trust.default_tier)
        ));
        out.push_str(&format!(
            "# show_agent_indicator = {}\n",
            d.trust.show_agent_indicator
        ));
        out.push_str(&format!(
            "# agent_scan_interval = {}  # seconds (0 = disabled)\n",
            d.trust.agent_scan_interval
        ));
        out.push('\n');
        out.push_str(
            "# Per-agent overrides — tier: \"sandboxed\" | \"supervised\" | \"trusted\"\n",
        );
        out.push_str("# [trust.agents.claude]\n");
        out.push_str("# tier = \"trusted\"\n");
        out.push_str("# allowed_tools = [\"read_file\", \"write_file\"]\n");
        out.push('\n');

        // ── [keybindings] ────────────────────────────────────────────────────
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [keybindings] — Key-action bindings merged on top of built-in defaults.\n");
        out.push_str("# Actions: copy | paste | font_size_up | font_size_down | font_size_reset\n");
        out.push_str("#          split_horizontal | split_vertical | split_auto | close_pane\n");
        out.push_str("#          resize_grow | resize_shrink | focus_next | focus_prev\n");
        out.push_str("#          focus_up | focus_down | focus_left | focus_right | zoom_pane\n");
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("[keybindings]\n");
        out.push_str("# [[keybindings.bindings]]\n");
        out.push_str("# key = \"ctrl+shift+c\"\n");
        out.push_str("# action = \"copy\"\n");
        out.push('\n');
        out.push_str("# [[keybindings.bindings]]\n");
        out.push_str("# key = \"ctrl+shift+v\"\n");
        out.push_str("# action = \"paste\"\n");
        out.push('\n');
        out.push_str("# [[keybindings.bindings]]\n");
        out.push_str("# key = \"ctrl+plus\"\n");
        out.push_str("# action = \"font_size_up\"\n");
        out.push('\n');
        out.push_str("# [[keybindings.bindings]]\n");
        out.push_str("# key = \"ctrl+minus\"\n");
        out.push_str("# action = \"font_size_down\"\n");
        out.push('\n');
        out.push_str("# [[keybindings.bindings]]\n");
        out.push_str("# key = \"ctrl+0\"\n");
        out.push_str("# action = \"font_size_reset\"\n");
        out.push('\n');

        // ── [profiles] ───────────────────────────────────────────────────────
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [profiles] — Named session profiles with per-profile overrides.\n");
        out.push_str(
            "# ─────────────────────────────────────────────────────────────────────────\n",
        );
        out.push_str("# [profiles.dev]\n");
        out.push_str("# shell = \"/bin/zsh\"\n");
        out.push_str("# working_directory = \"~/dev\"\n");
        out.push_str("# font_size = 14.0\n");
        out.push_str("# scrollback_lines = 50000\n");
        out.push_str("# [profiles.dev.env]\n");
        out.push_str("# EDITOR = \"nvim\"\n");
        out.push('\n');

        out
    }

    /// Return the TOML string representation of a [`TrustTier`] variant.
    fn trust_tier_str(tier: &TrustTier) -> &'static str {
        match tier {
            TrustTier::Sandboxed => "sandboxed",
            TrustTier::Supervised => "supervised",
            TrustTier::Trusted => "trusted",
        }
    }

    /// Serialise the current effective config to a TOML string.
    ///
    /// Used by `therminal --print-config`.
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|e| format!("# serialization error: {e}\n"))
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
    /// Primary font family name for the terminal grid. Empty string uses platform default.
    pub family: String,
    /// Font size in points (before DPI scaling).
    pub size: f32,
    /// Line-height multiplier (applied to `size`).
    pub line_height_scale: f32,
    /// Extra fallback font families.
    pub extra_fallbacks: Vec<String>,
    /// Whether to try Nerd Font variant of the primary family.
    pub nerd_font: bool,
    /// UI chrome font family (tabs, status bar, menus). Reserved for future use.
    pub ui_font_family: String,
    /// Display/brand font family (splash, about). Reserved for future use.
    pub display_font_family: String,
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
            ui_font_family: "IBM Plex Sans".to_string(),
            display_font_family: "Chakra Petch".to_string(),
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Split the focused pane horizontally (side-by-side).
    SplitHorizontal,
    /// Split the focused pane vertically (top/bottom).
    SplitVertical,
    /// Split the focused pane in auto-detected direction.
    SplitAuto,
    /// Close the currently focused pane.
    ClosePane,
    /// Grow the focused pane's split ratio.
    ResizeGrow,
    /// Shrink the focused pane's split ratio.
    ResizeShrink,
    /// Move focus to the next pane.
    FocusNext,
    /// Move focus to the previous pane.
    FocusPrev,
    /// Move focus up.
    FocusUp,
    /// Move focus down.
    FocusDown,
    /// Move focus left.
    FocusLeft,
    /// Move focus right.
    FocusRight,
    /// Toggle focused pane fullscreen (zoom).
    ZoomPane,
    /// Show the keybinding help overlay.
    ShowHelp,
}

impl KeyAction {
    /// Human-readable description of what this action does.
    pub fn description(&self) -> &'static str {
        match self {
            KeyAction::Copy => "Copy selection",
            KeyAction::Paste => "Paste from clipboard",
            KeyAction::FontSizeUp => "Increase font size",
            KeyAction::FontSizeDown => "Decrease font size",
            KeyAction::FontSizeReset => "Reset font size",
            KeyAction::SplitHorizontal => "Split pane horizontally",
            KeyAction::SplitVertical => "Split pane vertically",
            KeyAction::SplitAuto => "Auto split pane",
            KeyAction::ClosePane => "Close focused pane",
            KeyAction::ResizeGrow => "Grow pane ratio",
            KeyAction::ResizeShrink => "Shrink pane ratio",
            KeyAction::FocusNext => "Focus next pane",
            KeyAction::FocusPrev => "Focus previous pane",
            KeyAction::FocusUp => "Focus pane above",
            KeyAction::FocusDown => "Focus pane below",
            KeyAction::FocusLeft => "Focus pane left",
            KeyAction::FocusRight => "Focus pane right",
            KeyAction::ZoomPane => "Toggle pane zoom",
            KeyAction::ShowHelp => "Show keybinding help",
        }
    }

    /// Which section this action belongs to in the help overlay.
    pub fn section(&self) -> &'static str {
        match self {
            KeyAction::SplitHorizontal
            | KeyAction::SplitVertical
            | KeyAction::SplitAuto
            | KeyAction::ClosePane
            | KeyAction::ResizeGrow
            | KeyAction::ResizeShrink
            | KeyAction::FocusNext
            | KeyAction::FocusPrev
            | KeyAction::FocusUp
            | KeyAction::FocusDown
            | KeyAction::FocusLeft
            | KeyAction::FocusRight
            | KeyAction::ZoomPane => "Pane Management",
            KeyAction::FontSizeUp | KeyAction::FontSizeDown | KeyAction::FontSizeReset => "Font",
            KeyAction::Copy | KeyAction::Paste | KeyAction::ShowHelp => "General",
        }
    }
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
                // Clipboard
                Keybinding {
                    key: "ctrl+shift+c".to_string(),
                    action: KeyAction::Copy,
                },
                Keybinding {
                    key: "ctrl+shift+v".to_string(),
                    action: KeyAction::Paste,
                },
                // Font sizing
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
                // Pane splits
                Keybinding {
                    key: "ctrl+shift+h".to_string(),
                    action: KeyAction::SplitHorizontal,
                },
                Keybinding {
                    key: "ctrl+shift+d".to_string(),
                    action: KeyAction::SplitVertical,
                },
                Keybinding {
                    key: "ctrl+shift+enter".to_string(),
                    action: KeyAction::SplitAuto,
                },
                Keybinding {
                    key: "alt+enter".to_string(),
                    action: KeyAction::SplitAuto,
                },
                // Pane management
                Keybinding {
                    key: "ctrl+shift+w".to_string(),
                    action: KeyAction::ClosePane,
                },
                Keybinding {
                    key: "ctrl+shift+=".to_string(),
                    action: KeyAction::ResizeGrow,
                },
                Keybinding {
                    key: "ctrl+shift+-".to_string(),
                    action: KeyAction::ResizeShrink,
                },
                // Focus movement (directional arrows)
                Keybinding {
                    key: "ctrl+shift+up".to_string(),
                    action: KeyAction::FocusUp,
                },
                Keybinding {
                    key: "ctrl+shift+down".to_string(),
                    action: KeyAction::FocusDown,
                },
                Keybinding {
                    key: "ctrl+shift+left".to_string(),
                    action: KeyAction::FocusLeft,
                },
                Keybinding {
                    key: "ctrl+shift+right".to_string(),
                    action: KeyAction::FocusRight,
                },
                // Focus cycling
                Keybinding {
                    key: "ctrl+shift+n".to_string(),
                    action: KeyAction::FocusNext,
                },
                Keybinding {
                    key: "ctrl+shift+p".to_string(),
                    action: KeyAction::FocusPrev,
                },
                // Zoom
                Keybinding {
                    key: "ctrl+shift+z".to_string(),
                    action: KeyAction::ZoomPane,
                },
                // Help overlay
                Keybinding {
                    key: "ctrl+shift+/".to_string(),
                    action: KeyAction::ShowHelp,
                },
            ],
        }
    }
}

// ── Binding parser ──────────────────────────────────────────────────────

/// Modifier flags produced by [`parse_binding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ParsedModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

/// Key identifier produced by [`parse_binding`].
///
/// This is a platform-independent representation that maps 1:1 to
/// `winit::keyboard::Key` variants.  The conversion happens in the app
/// crate so that `therminal-core` stays free of windowing dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ParsedKey {
    /// A single printable character (a-z, 0-9, punctuation).
    Character(String),
    /// A named key (Enter, Tab, Escape, Arrow*, F1-F12, etc.).
    Named(ParsedNamedKey),
}

/// Named (non-character) keys recognized by the binding parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParsedNamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Space,
}

/// Parse a keybinding string like `"ctrl+shift+h"` into modifiers and key.
///
/// Returns `None` (and logs a warning) if the binding string is invalid.
///
/// Supported modifier names: `ctrl`, `shift`, `alt`, `super`.
/// Supported key names: `a`-`z`, `0`-`9`, `f1`-`f12`, arrow keys,
/// `enter`, `tab`, `escape`, `backspace`, `delete`, `insert`, `home`,
/// `end`, `pageup`, `pagedown`, `space`, and single-character
/// punctuation (`+`, `-`, `=`, `[`, `]`, `\`, `;`, `'`, `,`, `.`, `/`).
///
/// The special token `plus` is treated as the `+` character key, and
/// `minus` as the `-` character key, so they can coexist with the `+`
/// separator.
pub fn parse_binding(binding: &str) -> Option<(ParsedModifiers, ParsedKey)> {
    let parts: Vec<&str> = binding.split('+').collect();
    if parts.is_empty() {
        warn!(binding, "empty keybinding string");
        return None;
    }

    let mut mods = ParsedModifiers::default();
    let mut key_string: Option<String> = None;

    for (i, part) in parts.iter().enumerate() {
        let lower = part.trim().to_lowercase();
        match lower.as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "alt" | "option" => mods.alt = true,
            "super" | "meta" | "cmd" | "win" => mods.super_key = true,
            _ => {
                // Must be the key component — should be the last part.
                if i != parts.len() - 1 {
                    // There are more parts after this non-modifier.
                    // Join everything from here with '+' (handles e.g.
                    // accidental extra '+' in the binding string).
                    key_string = Some(parts[i..].join("+"));
                    break;
                }
                key_string = Some(part.trim().to_string());
            }
        }
    }

    let key_str = match key_string.as_deref() {
        Some(k) if !k.is_empty() => k,
        _ => {
            warn!(binding, "keybinding has no key component");
            return None;
        }
    };

    let key_lower = key_str.to_lowercase();
    let parsed_key = match key_lower.as_str() {
        // Named keys
        "enter" | "return" => ParsedKey::Named(ParsedNamedKey::Enter),
        "tab" => ParsedKey::Named(ParsedNamedKey::Tab),
        "escape" | "esc" => ParsedKey::Named(ParsedNamedKey::Escape),
        "backspace" => ParsedKey::Named(ParsedNamedKey::Backspace),
        "delete" | "del" => ParsedKey::Named(ParsedNamedKey::Delete),
        "insert" | "ins" => ParsedKey::Named(ParsedNamedKey::Insert),
        "home" => ParsedKey::Named(ParsedNamedKey::Home),
        "end" => ParsedKey::Named(ParsedNamedKey::End),
        "pageup" | "page_up" => ParsedKey::Named(ParsedNamedKey::PageUp),
        "pagedown" | "page_down" => ParsedKey::Named(ParsedNamedKey::PageDown),
        "up" | "arrowup" => ParsedKey::Named(ParsedNamedKey::ArrowUp),
        "down" | "arrowdown" => ParsedKey::Named(ParsedNamedKey::ArrowDown),
        "left" | "arrowleft" => ParsedKey::Named(ParsedNamedKey::ArrowLeft),
        "right" | "arrowright" => ParsedKey::Named(ParsedNamedKey::ArrowRight),
        "space" => ParsedKey::Named(ParsedNamedKey::Space),
        "f1" => ParsedKey::Named(ParsedNamedKey::F1),
        "f2" => ParsedKey::Named(ParsedNamedKey::F2),
        "f3" => ParsedKey::Named(ParsedNamedKey::F3),
        "f4" => ParsedKey::Named(ParsedNamedKey::F4),
        "f5" => ParsedKey::Named(ParsedNamedKey::F5),
        "f6" => ParsedKey::Named(ParsedNamedKey::F6),
        "f7" => ParsedKey::Named(ParsedNamedKey::F7),
        "f8" => ParsedKey::Named(ParsedNamedKey::F8),
        "f9" => ParsedKey::Named(ParsedNamedKey::F9),
        "f10" => ParsedKey::Named(ParsedNamedKey::F10),
        "f11" => ParsedKey::Named(ParsedNamedKey::F11),
        "f12" => ParsedKey::Named(ParsedNamedKey::F12),
        // Aliases for punctuation that conflicts with the '+' separator
        "plus" => ParsedKey::Character("+".to_string()),
        "minus" => ParsedKey::Character("-".to_string()),
        // Single-character keys (letters, digits, punctuation)
        s if s.len() == 1 => ParsedKey::Character(s.to_string()),
        _ => {
            warn!(binding, key = key_str, "unrecognized key in keybinding");
            return None;
        }
    };

    Some((mods, parsed_key))
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

// ── Section: Terminal ────────────────────────────────────────────────────

/// Configuration for the terminal sequence interceptor.
///
/// Controls which OSC escape-sequence families are intercepted for AI
/// awareness and shell integration.  All families are enabled by default;
/// disable individual families only if a third-party tool conflicts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// Intercept OSC 633 sequences (VS Code shell integration).
    pub osc_633: bool,
    /// Intercept OSC 133 sequences (FinalTerm shell integration).
    pub osc_133: bool,
    /// Intercept OSC 7 sequences (current working directory).
    pub osc_7: bool,
    /// Intercept OSC 1337 sequences (iTerm2).
    pub osc_1337: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            osc_633: true,
            osc_133: true,
            osc_7: true,
            osc_1337: true,
        }
    }
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
    /// Interval in seconds between process-tree scans for agent detection.
    ///
    /// Set to `0` to disable automatic process-tree scanning.
    /// Default is `3` seconds.
    pub agent_scan_interval: u64,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_tier: TrustTier::Supervised,
            agents: HashMap::new(),
            show_agent_indicator: true,
            agent_scan_interval: 3,
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
        let c = ColorsConfig::parse_hex("#060a12").unwrap();
        assert_eq!(c, Color::BG);
    }

    #[test]
    fn parse_hex_without_hash() {
        let c = ColorsConfig::parse_hex("e7f0ff").unwrap();
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

    #[test]
    fn keybindings_default_has_pane_actions() {
        let kb = KeybindingsConfig::default();
        assert!(kb
            .bindings
            .iter()
            .any(|b| b.action == KeyAction::SplitHorizontal));
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::ClosePane));
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::ZoomPane));
        assert!(kb.bindings.iter().any(|b| b.action == KeyAction::FocusNext));
    }

    #[test]
    fn parse_binding_ctrl_shift_h() {
        let (mods, key) = parse_binding("ctrl+shift+h").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert!(!mods.alt);
        assert!(!mods.super_key);
        assert_eq!(key, ParsedKey::Character("h".to_string()));
    }

    #[test]
    fn parse_binding_ctrl_plus() {
        let (mods, key) = parse_binding("ctrl+plus").unwrap();
        assert!(mods.ctrl);
        assert!(!mods.shift);
        assert_eq!(key, ParsedKey::Character("+".to_string()));
    }

    #[test]
    fn parse_binding_ctrl_shift_enter() {
        let (mods, key) = parse_binding("ctrl+shift+enter").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::Enter));
    }

    #[test]
    fn parse_binding_arrow_keys() {
        let (mods, key) = parse_binding("ctrl+shift+up").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::ArrowUp));
    }

    #[test]
    fn parse_binding_function_keys() {
        let (_, key) = parse_binding("f12").unwrap();
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::F12));
    }

    #[test]
    fn parse_binding_equals_sign() {
        let (mods, key) = parse_binding("ctrl+shift+=").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Character("=".to_string()));
    }

    #[test]
    fn parse_binding_minus_sign() {
        let (mods, key) = parse_binding("ctrl+shift+-").unwrap();
        assert!(mods.ctrl);
        assert!(mods.shift);
        assert_eq!(key, ParsedKey::Character("-".to_string()));
    }

    #[test]
    fn parse_binding_invalid_returns_none() {
        assert!(parse_binding("").is_none());
        assert!(parse_binding("ctrl+shift+foobar").is_none());
    }

    #[test]
    fn parse_binding_alt_enter() {
        let (mods, key) = parse_binding("alt+enter").unwrap();
        assert!(!mods.ctrl);
        assert!(!mods.shift);
        assert!(mods.alt);
        assert!(!mods.super_key);
        assert_eq!(key, ParsedKey::Named(ParsedNamedKey::Enter));
    }

    #[test]
    fn parse_binding_alt_super() {
        let (mods, key) = parse_binding("alt+super+a").unwrap();
        assert!(!mods.ctrl);
        assert!(!mods.shift);
        assert!(mods.alt);
        assert!(mods.super_key);
        assert_eq!(key, ParsedKey::Character("a".to_string()));
    }

    #[test]
    fn new_key_actions_round_trip_through_toml() {
        let config = TherminalConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert!(decoded
            .keybindings
            .bindings
            .iter()
            .any(|b| b.action == KeyAction::SplitHorizontal));
        assert!(decoded
            .keybindings
            .bindings
            .iter()
            .any(|b| b.action == KeyAction::ZoomPane));
    }

    #[test]
    fn terminal_config_defaults_all_enabled() {
        let config = TerminalConfig::default();
        assert!(config.osc_633);
        assert!(config.osc_133);
        assert!(config.osc_7);
        assert!(config.osc_1337);
    }

    #[test]
    fn terminal_config_deserialize_partial() {
        let toml_str = r#"
[terminal]
osc_633 = false
osc_1337 = false
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.terminal.osc_633);
        assert!(config.terminal.osc_133); // defaults to true
        assert!(config.terminal.osc_7); // defaults to true
        assert!(!config.terminal.osc_1337);
    }

    #[test]
    fn terminal_config_round_trips_through_toml() {
        let mut config = TherminalConfig::default();
        config.terminal.osc_633 = false;
        config.terminal.osc_1337 = false;

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();

        assert!(!decoded.terminal.osc_633);
        assert!(decoded.terminal.osc_133);
        assert!(decoded.terminal.osc_7);
        assert!(!decoded.terminal.osc_1337);
    }

    #[test]
    fn trust_config_default_scan_interval() {
        let trust = TrustConfig::default();
        assert_eq!(trust.agent_scan_interval, 3);
    }

    #[test]
    fn trust_config_scan_interval_deserialize() {
        let toml_str = r#"
[trust]
agent_scan_interval = 10
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.trust.agent_scan_interval, 10);
    }

    #[test]
    fn trust_config_scan_interval_zero_disabled() {
        let toml_str = r#"
[trust]
agent_scan_interval = 0
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.trust.agent_scan_interval, 0);
    }

    #[test]
    fn trust_config_scan_interval_round_trips() {
        let mut config = TherminalConfig::default();
        config.trust.agent_scan_interval = 5;

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(decoded.trust.agent_scan_interval, 5);
    }

    #[test]
    fn default_config_text_has_header() {
        let text = TherminalConfig::default_config_text();
        assert!(text.starts_with("# Therminal config — hot-reloaded on save\n"));
    }

    #[test]
    fn default_config_text_all_lines_are_comments_or_empty() {
        let text = TherminalConfig::default_config_text();
        for line in text.lines() {
            assert!(
                line.is_empty() || line.starts_with('#') || line.starts_with('['),
                "unexpected non-comment line: {line:?}"
            );
        }
    }

    #[test]
    fn default_config_text_parses_as_empty_toml() {
        // Since every value line is commented out, the text should parse
        // successfully and yield the same result as an empty TOML document
        // (i.e. all defaults).
        let text = TherminalConfig::default_config_text();
        let config: TherminalConfig =
            toml::from_str(&text).expect("default config text must parse");
        assert_eq!(config.general.title, "Therminal");
        assert_eq!(config.font.size, 17.0);
        assert_eq!(config.trust.default_tier, TrustTier::Supervised);
        assert_eq!(config.trust.agent_scan_interval, 3);
        assert!(config.terminal.osc_633);
        assert!(config.terminal.osc_133);
        assert!(config.terminal.osc_7);
        assert!(config.terminal.osc_1337);
    }

    #[test]
    fn save_default_to_writes_parseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");

        TherminalConfig::default()
            .save_default_to(&path)
            .expect("save_default_to should succeed");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("# Therminal config"));
        let _config: TherminalConfig =
            toml::from_str(&contents).expect("written default config must parse");
    }

    #[test]
    fn load_from_missing_file_writes_default_and_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");

        assert!(!path.exists());
        let config = TherminalConfig::load_from(&path);

        // Returns defaults.
        assert_eq!(config.general.title, "Therminal");
        // Wrote the commented default config to disk.
        assert!(path.exists());
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.starts_with("# Therminal config"));
    }

    #[test]
    fn to_toml_string_produces_valid_toml() {
        let config = TherminalConfig::default();
        let s = config.to_toml_string();
        let _: TherminalConfig =
            toml::from_str(&s).expect("to_toml_string must produce valid TOML");
    }

    // ── Config-to-renderer mapping tests ─────────────────────────────────────

    /// Task 1: Font config change produces correct CoreFontConfig fields.
    #[test]
    fn font_config_change_produces_correct_core_font_config() {
        let mut config = TherminalConfig::default();
        config.font.family = "Fira Code".to_string();
        config.font.size = 20.0;
        config.font.line_height_scale = 1.5;
        config.font.nerd_font = false;
        config.font.extra_fallbacks = vec!["Iosevka".to_string()];

        let core = config.to_core_font_config();

        // Family is mapped through the Some() wrapper when non-empty.
        assert_eq!(core.family.as_deref(), Some("Fira Code"));
        assert_eq!(core.size, 20.0);
        assert_eq!(core.line_height_scale, 1.5);
        assert!(!core.nerd_font);
        assert_eq!(core.extra_fallbacks, vec!["Iosevka".to_string()]);
    }

    /// Empty family string maps to None in CoreFontConfig (use platform default).
    #[test]
    fn empty_font_family_maps_to_none_in_core_font_config() {
        let mut config = TherminalConfig::default();
        config.font.family = String::new();

        let core = config.to_core_font_config();
        assert!(
            core.family.is_none(),
            "empty family should map to None so platform default is used"
        );
    }

    /// Default font config round-trips to expected CoreFontConfig defaults.
    #[test]
    fn default_font_config_maps_to_correct_core_font_config() {
        let config = TherminalConfig::default();
        let core = config.to_core_font_config();

        // Default family is JetBrainsMono Nerd Font Mono (non-empty → Some).
        assert_eq!(core.family.as_deref(), Some("JetBrainsMono Nerd Font Mono"));
        assert_eq!(core.size, 17.0);
        assert_eq!(core.line_height_scale, 1.375);
        assert!(core.nerd_font);
    }

    /// Task 2: Color overrides with hex values produce the correct Color structs.
    #[test]
    fn color_hex_overrides_produce_correct_color_values() {
        let colors = ColorsConfig {
            background: Some("#1a2b3c".to_string()),
            foreground: Some("#aabbcc".to_string()),
            cursor: Some("#39ffb6".to_string()),
            ..Default::default()
        };

        let bg = colors.background_color();
        assert_eq!(bg.r, 0x1a);
        assert_eq!(bg.g, 0x2b);
        assert_eq!(bg.b, 0x3c);

        let fg = colors.foreground_color();
        assert_eq!(fg.r, 0xaa);
        assert_eq!(fg.g, 0xbb);
        assert_eq!(fg.b, 0xcc);

        // cursor uses parse_hex directly; verify against known Codex SIGNAL color.
        let cursor = colors
            .cursor
            .as_deref()
            .and_then(ColorsConfig::parse_hex)
            .expect("cursor should parse");
        assert_eq!(cursor, Color::SIGNAL); // SIGNAL = #39ffb6
    }

    /// Task 3: Invalid hex color string falls back to the palette default.
    #[test]
    fn invalid_hex_color_falls_back_to_palette_default() {
        let colors = ColorsConfig {
            background: Some("not-a-hex".to_string()),
            foreground: Some("#GGGGGG".to_string()), // invalid hex digits
            ..Default::default()
        };

        // Should silently fall back to the Codex 2031 palette constants.
        assert_eq!(
            colors.background_color(),
            Color::BG,
            "invalid hex background should fall back to Color::BG (VOID_0)"
        );
        assert_eq!(
            colors.foreground_color(),
            Color::TEXT,
            "invalid hex foreground should fall back to Color::TEXT (INK)"
        );
    }

    /// Task 4: Partial config (only [font] section) preserves all other section defaults.
    #[test]
    fn partial_font_config_preserves_other_section_defaults() {
        let toml_str = r#"
[font]
family = "Hack"
size = 16.0
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        let core = config.to_core_font_config();

        // Font section was overridden.
        assert_eq!(core.family.as_deref(), Some("Hack"));
        assert_eq!(core.size, 16.0);

        // line_height_scale still uses the font default (not the core default).
        assert_eq!(
            core.line_height_scale,
            FontConfig::default().line_height_scale
        );

        // All other sections remain at their defaults.
        assert_eq!(config.general.title, "Therminal");
        assert_eq!(config.general.padding, 4.0);
        assert_eq!(config.general.scrollback_lines, 10_000);
        assert_eq!(config.trust.default_tier, TrustTier::Supervised);
        assert!(config.trust.show_agent_indicator);
        assert_eq!(config.trust.agent_scan_interval, 3);
        assert!(config.terminal.osc_633);
        assert!(config.terminal.osc_133);
        assert!(config.terminal.osc_7);
        assert!(config.terminal.osc_1337);
        // Colors section should be all None (defaults).
        assert!(config.colors.background.is_none());
        assert!(config.colors.foreground.is_none());
    }

    /// Task 5: Invalid TOML logs a warning and returns defaults — does not panic.
    #[test]
    fn invalid_toml_returns_defaults_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");

        std::fs::write(&path, "this is not [ valid toml !!@#$%").unwrap();

        // Must not panic; must return the full defaults.
        let config = TherminalConfig::load_from(&path);
        assert_eq!(
            config.general.title, "Therminal",
            "invalid TOML should fall back to default title"
        );
        assert_eq!(
            config.font.size, 17.0,
            "invalid TOML should fall back to default font size"
        );
        assert_eq!(
            config.trust.default_tier,
            TrustTier::Supervised,
            "invalid TOML should fall back to default trust tier"
        );
    }

    /// Task 6: Write a fully-populated config, load it back, and verify all
    /// fields survived the round-trip faithfully.
    #[test]
    fn full_config_round_trip_write_load_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");

        let mut original = TherminalConfig::default();
        // General
        original.general.title = "RT Terminal".to_string();
        original.general.window_width = 1920.0;
        original.general.window_height = 1080.0;
        original.general.scrollback_lines = 50_000;
        original.general.padding = 8.0;
        // Font
        original.font.family = "Cascadia Code".to_string();
        original.font.size = 15.0;
        original.font.line_height_scale = 1.4;
        original.font.nerd_font = false;
        original.font.extra_fallbacks = vec!["Noto Color Emoji".to_string()];
        // Colors
        original.colors.background = Some("#0d1421".to_string());
        original.colors.foreground = Some("#e7f0ff".to_string());
        original.colors.cursor = Some("#56a7ff".to_string());
        // Trust
        original.trust.default_tier = TrustTier::Sandboxed;
        original.trust.show_agent_indicator = false;
        original.trust.agent_scan_interval = 10;
        // Terminal
        original.terminal.osc_633 = false;
        original.terminal.osc_1337 = false;

        original.save_to(&path).expect("save_to should succeed");

        let loaded = TherminalConfig::load_from(&path);

        // Verify general
        assert_eq!(loaded.general.title, "RT Terminal");
        assert_eq!(loaded.general.window_width, 1920.0);
        assert_eq!(loaded.general.window_height, 1080.0);
        assert_eq!(loaded.general.scrollback_lines, 50_000);
        assert_eq!(loaded.general.padding, 8.0);
        // Verify font
        assert_eq!(loaded.font.family, "Cascadia Code");
        assert_eq!(loaded.font.size, 15.0);
        assert_eq!(loaded.font.line_height_scale, 1.4);
        assert!(!loaded.font.nerd_font);
        assert_eq!(loaded.font.extra_fallbacks, vec!["Noto Color Emoji"]);
        // Verify colors
        assert_eq!(
            loaded.colors.background.as_deref(),
            Some("#0d1421"),
            "background color should survive round-trip"
        );
        assert_eq!(loaded.colors.foreground.as_deref(), Some("#e7f0ff"));
        assert_eq!(loaded.colors.cursor.as_deref(), Some("#56a7ff"));
        // Verify parsed color values from loaded config.
        let bg = loaded.colors.background_color();
        assert_eq!(
            bg,
            Color::VOID_1,
            "loaded background should parse to VOID_1"
        );
        let fg = loaded.colors.foreground_color();
        assert_eq!(fg, Color::INK, "loaded foreground should parse to INK");
        // Verify trust
        assert_eq!(loaded.trust.default_tier, TrustTier::Sandboxed);
        assert!(!loaded.trust.show_agent_indicator);
        assert_eq!(loaded.trust.agent_scan_interval, 10);
        // Verify terminal
        assert!(!loaded.terminal.osc_633);
        assert!(loaded.terminal.osc_133);
        assert!(loaded.terminal.osc_7);
        assert!(!loaded.terminal.osc_1337);
    }
}
