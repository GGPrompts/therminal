//! TOML configuration system for Therminal.
//!
//! Defines [`TherminalConfig`] with sections for general settings, font,
//! colors, keybindings, profiles, and trust tiers.  Config is loaded from
//! `therminal_runtime::paths::config_dir() / "therminal.toml"` with sensible
//! defaults that match the current hardcoded values.
//!
//! Use [`ConfigWatcher`] for hot-reload via filesystem notifications.

mod config_text;
pub mod keybindings;

// Re-export all public types so that `therminal_core::config::*` paths
// continue to work unchanged after the split.
pub use keybindings::*;

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
    /// MCP server settings.
    pub mcp: McpConfig,
    /// Bell behavior settings.
    pub bell: BellConfig,
    /// Notification settings.
    pub notifications: NotificationConfig,
    /// Hotspot (clickable file/URL) settings.
    pub hotspots: HotspotsConfig,
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
                    config.validate_paths();
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
        config_text::default_config_text()
    }

    /// Serialise the current effective config to a TOML string.
    ///
    /// Used by `therminal --print-config`.
    pub fn to_toml_string(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_else(|e| format!("# serialization error: {e}\n"))
    }

    /// Validate path-related config fields and emit warnings for suspicious
    /// values.  Called after loading; never fails hard.
    pub fn validate_paths(&self) {
        // Validate mcp.socket_path if explicitly set.
        if !self.mcp.socket_path.is_empty() {
            let p = Path::new(&self.mcp.socket_path);
            if p.is_relative() {
                warn!(
                    path = %self.mcp.socket_path,
                    "mcp.socket_path is a relative path; consider using an absolute path"
                );
            }
            // Check for characters that are generally invalid in paths.
            if self.mcp.socket_path.contains('\0') {
                warn!(
                    path = %self.mcp.socket_path,
                    "mcp.socket_path contains null bytes"
                );
            }
        }

        // Validate profile working directories.
        for (name, profile) in &self.profiles {
            if let Some(ref dir) = profile.working_directory
                && !dir.is_empty()
            {
                let p = Path::new(dir);
                if p.is_relative() {
                    warn!(
                        profile = %name,
                        path = %dir,
                        "profile working_directory is a relative path; consider using an absolute path"
                    );
                }
                if !p.exists() {
                    warn!(
                        profile = %name,
                        path = %dir,
                        "profile working_directory does not exist"
                    );
                }
            }
        }
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
    /// Whether to show the status bar at the bottom of the window.
    pub show_status_bar: bool,
    /// Whether to show the workspace tab bar at the top of the window.
    pub show_tab_bar: bool,
    /// Use client-side decorations (custom title bar with window controls).
    /// Default: true on Linux and Windows, false on macOS.
    pub use_csd: bool,
    /// Automatically split panes when AI agents spawn subprocesses.
    pub auto_tile: bool,
    /// Debounce interval (ms) for auto-tile spawn/exit events to avoid layout thrashing.
    pub auto_tile_debounce_ms: u64,
}

/// Platform-specific default for client-side decorations.
/// Enabled on Linux and Windows (replaces native title bar with integrated tabs).
/// Disabled on macOS where the native title bar is expected.
fn default_use_csd() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
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
            show_status_bar: true,
            show_tab_bar: true,
            use_csd: default_use_csd(),
            auto_tile: true,
            auto_tile_debounce_ms: 200,
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
    /// UI chrome font family (tabs, status bar, menus).
    ///
    /// **Not yet wired.** Reserved for Phase 3 chrome rendering. Parsed and
    /// round-tripped through TOML but not consumed by any renderer today.
    pub ui_font_family: String,
    /// Display/brand font family (splash, about).
    ///
    /// **Not yet wired.** Reserved for Phase 3 splash/about screen. Parsed
    /// and round-tripped through TOML but not consumed by any renderer today.
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

// ── Section: Profiles ────────────────────────────────────────────────────

/// A named session profile with optional overrides.
///
/// **Not yet wired.** Profiles are parsed and round-tripped through TOML but
/// there is no profile-selection UI or CLI flag to activate a profile yet.
/// Planned for Phase 3 session management. The struct is kept so that users
/// can start writing profiles in their config file in advance.
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
    /// Intercept OSC 9 sequences (desktop notifications).
    pub osc_9: bool,
    /// Intercept OSC 1337 sequences (iTerm2).
    pub osc_1337: bool,
    /// Intercept OSC 7777 sequences (cooperative agent self-reporting).
    pub osc_7777: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            osc_633: true,
            osc_133: true,
            osc_7: true,
            osc_9: true,
            osc_1337: true,
            osc_7777: true,
        }
    }
}

// ── Section: Trust ───────────────────────────────────────────────────────

/// Trust tier assigned to an AI agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Minimal permissions; agent actions are heavily restricted.
    Sandboxed,
    /// Actions require user confirmation.
    Supervised,
    /// Full access; agent is treated as trusted.
    Trusted,
}

impl TrustTier {
    /// Numeric privilege level for comparison.
    /// Higher values grant more permissions.
    fn level(self) -> u8 {
        match self {
            Self::Sandboxed => 0,
            Self::Supervised => 1,
            Self::Trusted => 2,
        }
    }

    /// Returns `true` if this tier has at least the given required tier's
    /// privilege level.
    pub fn has_access(self, required: Self) -> bool {
        self.level() >= required.level()
    }
}

impl PartialOrd for TrustTier {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TrustTier {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.level().cmp(&other.level())
    }
}

/// Agent trust tier configuration.
///
/// Controls what level of access AI agents have when detected in the
/// terminal session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustConfig {
    /// Default trust tier for unknown agents.
    ///
    /// Used by the MCP server to gate tool access when the connecting agent
    /// is not listed in `agents`. Sandboxed = read-only, Supervised = read
    /// + write, Trusted = full access including destructive operations.
    pub default_tier: TrustTier,
    /// Per-agent trust overrides, keyed by agent name (matched against the
    /// MCP client's `Implementation.name` from the `initialize` handshake).
    ///
    /// Agents not listed here fall back to `default_tier`.
    pub agents: HashMap<String, AgentTrust>,
    /// Whether to show visual indicators when agents are detected.
    pub show_agent_indicator: bool,
    /// Interval in seconds between process-tree scans for agent detection.
    ///
    /// Set to `0` to disable automatic process-tree scanning.
    /// Default is `3` seconds.
    pub agent_scan_interval: u64,
    /// Maximum number of destructive MCP operations (destroy_session, etc.)
    /// allowed per agent per minute. Set to `0` to disable rate limiting.
    /// Default is `5`.
    pub destructive_rate_limit: u32,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_tier: TrustTier::Supervised,
            agents: HashMap::new(),
            show_agent_indicator: true,
            agent_scan_interval: 3,
            destructive_rate_limit: 5,
        }
    }
}

/// Trust settings for a specific agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTrust {
    /// Trust tier for this agent.
    pub tier: TrustTier,
    /// Optional per-agent MCP tool allowlist. When `Some`, the tool name
    /// must match one of the entries exactly (in addition to passing the
    /// tier check). `None` means no per-tool restriction. Enforced in
    /// `therminal-daemon/src/trust.rs::check_tool_access`.
    pub allowed_tools: Option<Vec<String>>,
}

// ── Section: MCP ────────────────────────────────────────────────────────

/// MCP (Model Context Protocol) server configuration.
///
/// When enabled, the daemon exposes session management tools via the MCP
/// protocol on a dedicated Unix socket, allowing external tools (Claude
/// Code, TUIs, dashboards) to interact with terminal sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Whether the MCP server is enabled.
    pub enabled: bool,
    /// Custom socket path for the MCP server.
    ///
    /// If empty, defaults to the runtime directory socket path
    /// (`<runtime_dir>/mcp.sock`).
    pub socket_path: String,
}

impl McpConfig {
    /// Return the effective socket path, falling back to the default
    /// runtime directory path if `socket_path` is empty.
    pub fn resolved_socket_path(&self) -> std::path::PathBuf {
        if self.socket_path.is_empty() {
            therminal_runtime::paths::socket_path("mcp")
        } else {
            std::path::PathBuf::from(&self.socket_path)
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket_path: String::new(),
        }
    }
}

// ── Section: Bell ───────────────────────────────────────────────────────

/// Bell style determines how BEL (`\x07`) is presented to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BellStyle {
    /// Flash the taskbar/dock icon.
    Taskbar,
    /// Brief screen invert/flash.
    Visual,
    /// System audible bell (not yet implemented; falls back to taskbar).
    Audible,
    /// Ignore the bell entirely.
    None,
}

/// Bell behavior configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BellConfig {
    /// How to present BEL characters to the user.
    pub style: BellStyle,
    /// Duration of the visual bell flash in milliseconds (only used when
    /// `style = "visual"`).
    pub visual_bell_duration_ms: u64,
}

impl Default for BellConfig {
    fn default() -> Self {
        Self {
            style: BellStyle::Taskbar,
            visual_bell_duration_ms: 150,
        }
    }
}

// ── Section: Notifications ──────────────────────────────────────────────

/// Desktop notification settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    /// Send a desktop notification when an agent transitions to
    /// `AwaitingInput` (i.e. the agent is waiting for the user).
    pub agent_waiting: bool,
    /// Send desktop notifications for OSC 9 sequences. When `false`,
    /// OSC 9 events are still parsed (subject to `terminal.osc_9`) but
    /// do not trigger a desktop notification. Enforced in
    /// `therminal-app/src/window/mod.rs` in the `UserEvent::DesktopNotification`
    /// handler.
    pub osc9_enabled: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            agent_waiting: true,
            osc9_enabled: true,
        }
    }
}

// ── Section: Hotspots ──────────────────────────────────────────────────

/// Hotspot (clickable terminal content) settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct HotspotsConfig {
    /// Ordered list of editor commands to try when opening a file hotspot.
    ///
    /// The first entry whose executable is found on `PATH` is used. The
    /// special tokens `$VISUAL` and `$EDITOR` (case-sensitive) are
    /// substituted with the values of those environment variables at
    /// launch time and skipped if unset.
    ///
    /// If the entire chain fails, `open::that` is used as a last resort.
    pub editor_chain: Vec<String>,
}

impl Default for HotspotsConfig {
    fn default() -> Self {
        // On WSL2, prefer VS Code first — most users have it installed via
        // the Windows host and `$EDITOR` is frequently unset in GUI launches.
        let is_wsl = std::env::var_os("WSL_DISTRO_NAME").is_some();
        let chain: Vec<&str> = if is_wsl {
            vec!["code", "$VISUAL", "$EDITOR", "nvim", "vim", "nano"]
        } else {
            vec!["$VISUAL", "$EDITOR", "code", "nvim", "vim", "nano"]
        };
        Self {
            editor_chain: chain.into_iter().map(String::from).collect(),
        }
    }
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
    fn hotspots_config_round_trips() {
        let mut config = TherminalConfig::default();
        config.hotspots.editor_chain =
            vec!["hx".to_string(), "$EDITOR".to_string(), "nano".to_string()];
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            decoded.hotspots.editor_chain,
            vec!["hx".to_string(), "$EDITOR".to_string(), "nano".to_string()]
        );
    }

    #[test]
    fn hotspots_default_chain_contains_fallbacks() {
        let d = HotspotsConfig::default();
        // `nano` is always the last-resort entry regardless of platform.
        assert_eq!(d.editor_chain.last().map(String::as_str), Some("nano"));
        // Env tokens are included so unset-$EDITOR users still get a chain.
        assert!(d.editor_chain.iter().any(|s| s == "$EDITOR"));
        assert!(d.editor_chain.iter().any(|s| s == "$VISUAL"));
    }

    #[test]
    fn hotspots_default_chain_wsl_puts_code_first() {
        // Simulate WSL detection by constructing the chain the same way
        // `HotspotsConfig::default` does. We can't mutate the process env
        // safely in parallel tests, so assert the shape of both branches.
        let wsl_chain: Vec<&str> = vec!["code", "$VISUAL", "$EDITOR", "nvim", "vim", "nano"];
        let non_wsl_chain: Vec<&str> = vec!["$VISUAL", "$EDITOR", "code", "nvim", "vim", "nano"];
        assert_eq!(wsl_chain[0], "code");
        assert_ne!(non_wsl_chain[0], "code");

        // And confirm the real default matches one of the two shapes.
        let actual_owned: Vec<String> = HotspotsConfig::default().editor_chain;
        let wsl_owned: Vec<String> = wsl_chain.iter().map(|s| s.to_string()).collect();
        let non_wsl_owned: Vec<String> = non_wsl_chain.iter().map(|s| s.to_string()).collect();
        assert!(
            actual_owned == wsl_owned || actual_owned == non_wsl_owned,
            "unexpected default chain: {:?}",
            actual_owned
        );
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
    fn new_key_actions_round_trip_through_toml() {
        let config = TherminalConfig::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert!(
            decoded
                .keybindings
                .bindings
                .iter()
                .any(|b| b.action == KeyAction::SplitHorizontal)
        );
        assert!(
            decoded
                .keybindings
                .bindings
                .iter()
                .any(|b| b.action == KeyAction::ZoomPane)
        );
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

    // ── Config wiring audit tests ───────────────────────────────────────────
    //
    // Each test verifies that a config field set to a non-default value
    // survives load_from() and produces the expected value at the point
    // where the app would consume it.

    /// Shell override round-trips and is available for PTY spawn.
    #[test]
    fn shell_override_round_trips_through_toml() {
        let toml_str = r#"
[general]
shell = "/usr/bin/fish"
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.shell, "/usr/bin/fish");

        // Round-trip through file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(
            loaded.general.shell, "/usr/bin/fish",
            "shell override must survive save/load round-trip"
        );
    }

    /// Empty shell string means "use the user's default shell".
    #[test]
    fn empty_shell_means_default() {
        let config = TherminalConfig::default();
        assert!(
            config.general.shell.is_empty(),
            "default shell should be empty (meaning: use user's login shell)"
        );
    }

    /// Extra env vars round-trip and are available for PTY spawn.
    #[test]
    fn env_vars_round_trip_through_toml() {
        let toml_str = r#"
[general.env]
EDITOR = "nvim"
MY_VAR = "hello world"
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.env.len(), 2);
        assert_eq!(config.general.env["EDITOR"], "nvim");
        assert_eq!(config.general.env["MY_VAR"], "hello world");

        // Round-trip through file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(loaded.general.env["EDITOR"], "nvim");
        assert_eq!(loaded.general.env["MY_VAR"], "hello world");
    }

    /// Padding round-trips and non-default value is preserved.
    #[test]
    fn padding_round_trips_through_toml() {
        let toml_str = r#"
[general]
padding = 12.0
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.padding, 12.0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(
            loaded.general.padding, 12.0,
            "padding override must survive save/load round-trip"
        );
    }

    /// Scrollback lines round-trips and non-default value is preserved.
    #[test]
    fn scrollback_lines_round_trips_through_toml() {
        let toml_str = r#"
[general]
scrollback_lines = 100000
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.general.scrollback_lines, 100_000);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(
            loaded.general.scrollback_lines, 100_000,
            "scrollback_lines override must survive save/load round-trip"
        );
    }

    /// show_status_bar round-trips with non-default value.
    #[test]
    fn show_status_bar_round_trips_through_toml() {
        let toml_str = r#"
[general]
show_status_bar = false
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert!(
            !config.general.show_status_bar,
            "show_status_bar should be false when set explicitly"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);
        assert!(
            !loaded.general.show_status_bar,
            "show_status_bar override must survive save/load round-trip"
        );
    }

    /// Dead fields: ui_font_family and display_font_family are parsed but
    /// not consumed by to_core_font_config (which is the only consumer path).
    #[test]
    fn reserved_font_fields_are_not_in_core_font_config() {
        let mut config = TherminalConfig::default();
        config.font.ui_font_family = "Custom UI Font".to_string();
        config.font.display_font_family = "Custom Display Font".to_string();

        let core = config.to_core_font_config();

        // CoreFontConfig has no ui_font_family or display_font_family fields,
        // so these values are intentionally dropped. This test documents that
        // these fields exist in config but are not yet wired to any consumer.
        assert_eq!(core.family.as_deref(), Some("JetBrainsMono Nerd Font Mono"));
        assert_eq!(core.size, 17.0);
        // The reserved fields round-trip through TOML though.
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(decoded.font.ui_font_family, "Custom UI Font");
        assert_eq!(decoded.font.display_font_family, "Custom Display Font");
    }

    /// Dead fields: profiles are parsed but not consumed by any app code.
    #[test]
    fn profiles_round_trip_but_are_not_consumed() {
        let toml_str = r#"
[profiles.dev]
shell = "/bin/zsh"
font_size = 14.0
scrollback_lines = 50000

[profiles.dev.env]
EDITOR = "nvim"
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.profiles.len(), 1);
        let dev = &config.profiles["dev"];
        assert_eq!(dev.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(dev.font_size, Some(14.0));
        assert_eq!(dev.scrollback_lines, Some(50_000));
        assert_eq!(dev.env["EDITOR"], "nvim");

        // Round-trip through file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(loaded.profiles["dev"].shell.as_deref(), Some("/bin/zsh"));
    }

    // ── Path validation tests ───────────────────────────────────────────────

    /// validate_paths does not panic on a default config (all paths empty).
    #[test]
    fn validate_paths_default_config_no_panic() {
        let config = TherminalConfig::default();
        config.validate_paths(); // should not panic
    }

    /// validate_paths does not panic when socket_path is an absolute path.
    #[test]
    fn validate_paths_absolute_socket_path_ok() {
        let mut config = TherminalConfig::default();
        config.mcp.socket_path = "/tmp/therminal-mcp.sock".to_string();
        config.validate_paths(); // should not panic or produce errors
    }

    /// validate_paths warns (but does not panic) for a relative socket_path.
    #[test]
    fn validate_paths_relative_socket_path_warns() {
        let mut config = TherminalConfig::default();
        config.mcp.socket_path = "relative/mcp.sock".to_string();
        // This should emit a tracing::warn but not panic.
        config.validate_paths();
    }

    /// validate_paths warns for socket_path containing null bytes.
    #[test]
    fn validate_paths_socket_path_with_null_warns() {
        let mut config = TherminalConfig::default();
        config.mcp.socket_path = "/tmp/bad\0path.sock".to_string();
        config.validate_paths(); // should warn but not panic
    }

    /// validate_paths warns when a profile working_directory does not exist.
    #[test]
    fn validate_paths_nonexistent_working_directory_warns() {
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "test".to_string(),
            ProfileConfig {
                working_directory: Some("/nonexistent/path/that/does/not/exist".to_string()),
                ..Default::default()
            },
        );
        config.validate_paths(); // should warn but not panic
    }

    /// validate_paths warns for a relative working_directory.
    #[test]
    fn validate_paths_relative_working_directory_warns() {
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "dev".to_string(),
            ProfileConfig {
                working_directory: Some("relative/dir".to_string()),
                ..Default::default()
            },
        );
        config.validate_paths(); // should warn but not panic
    }

    /// validate_paths accepts a working_directory that actually exists.
    #[test]
    fn validate_paths_existing_working_directory_ok() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "test".to_string(),
            ProfileConfig {
                working_directory: Some(dir.path().to_string_lossy().to_string()),
                ..Default::default()
            },
        );
        config.validate_paths(); // should not warn about non-existence
    }

    /// validate_paths skips empty working_directory values.
    #[test]
    fn validate_paths_empty_working_directory_skipped() {
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "empty".to_string(),
            ProfileConfig {
                working_directory: Some(String::new()),
                ..Default::default()
            },
        );
        config.validate_paths(); // should not warn
    }
}
