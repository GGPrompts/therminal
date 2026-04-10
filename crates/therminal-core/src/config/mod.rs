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
pub use config_text::CONFIG_TEMPLATE_VERSION;
pub use keybindings::*;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table};
use tracing::{info, warn};

use crate::font::FontConfig as CoreFontConfig;
use crate::palette::Color;

// ── Config file path ─────────────────────────────────────────────────────

/// Default config filename.
const CONFIG_FILENAME: &str = "therminal.toml";

fn merged_toml_with_existing(
    existing: &str,
    replacement: &str,
) -> Result<String, toml_edit::TomlError> {
    let mut existing_doc: DocumentMut = existing.parse()?;
    let replacement_doc: DocumentMut = replacement.parse()?;
    merge_table(existing_doc.as_table_mut(), replacement_doc.as_table());
    Ok(existing_doc.to_string())
}

fn merge_table(dst: &mut Table, src: &Table) {
    let keys: Vec<String> = src.iter().map(|(k, _)| k.to_string()).collect();
    for key in keys {
        let Some(src_item) = src.get(&key) else {
            continue;
        };

        match src_item {
            Item::Table(src_table) => {
                if !matches!(dst.get(&key), Some(Item::Table(_))) {
                    dst.insert(&key, Item::Table(src_table.clone()));
                    continue;
                }
                if let Some(Item::Table(dst_table)) = dst.get_mut(&key) {
                    merge_table(dst_table, src_table);
                }
            }
            Item::Value(src_value) => {
                if let Some(dst_item) = dst.get_mut(&key)
                    && let Some(dst_value) = dst_item.as_value_mut()
                {
                    *dst_value = src_value.clone();
                    continue;
                }
                dst.insert(&key, src_item.clone());
            }
            _ => {
                dst.insert(&key, src_item.clone());
            }
        }
    }
}

/// In-memory state machine for settings editing.
///
/// Tracks three states:
/// - `saved`: last persisted config snapshot.
/// - `draft`: current editable form state.
/// - `applied`: last runtime-applied state.
#[derive(Debug, Clone)]
pub struct ConfigEditSession {
    saved: TherminalConfig,
    draft: TherminalConfig,
    applied: TherminalConfig,
}

impl ConfigEditSession {
    /// Initialize an edit session from an already-loaded persisted config.
    pub fn from_saved(saved: TherminalConfig) -> Self {
        Self {
            draft: saved.clone(),
            applied: saved.clone(),
            saved,
        }
    }

    /// Returns the persisted snapshot captured by this session.
    pub fn saved(&self) -> &TherminalConfig {
        &self.saved
    }

    /// Returns the current mutable draft state.
    pub fn draft_mut(&mut self) -> &mut TherminalConfig {
        &mut self.draft
    }

    /// Returns the current draft state.
    pub fn draft(&self) -> &TherminalConfig {
        &self.draft
    }

    /// Returns the currently applied runtime state.
    pub fn applied(&self) -> &TherminalConfig {
        &self.applied
    }

    /// Apply the draft to runtime state (no disk write).
    pub fn apply_draft(&mut self) {
        self.applied = self.draft.clone();
    }

    /// Persist the draft to `path`, then mark all session states as saved.
    pub fn save_draft_to(&mut self, path: &Path) -> std::io::Result<()> {
        self.draft.save_to(path)?;
        self.saved = self.draft.clone();
        self.applied = self.draft.clone();
        Ok(())
    }

    /// Revert draft + applied back to the last saved snapshot.
    pub fn discard_draft(&mut self) {
        self.draft = self.saved.clone();
        self.applied = self.saved.clone();
    }

    /// True when draft differs from saved snapshot.
    pub fn is_dirty(&self) -> bool {
        toml::to_string(&self.draft).ok() != toml::to_string(&self.saved).ok()
    }
}

/// Return the full path to the Therminal config file.
pub fn config_path() -> PathBuf {
    therminal_runtime::paths::config_dir().join(CONFIG_FILENAME)
}

// ── Template version detection ────────────────────────────────────────────

/// Number of lines from the top of a config file to scan for the
/// `template_version` marker. The marker is emitted on line 2 of a freshly
/// generated file (right after the `# Therminal config` header), but we
/// allow some slack so users who hand-edit the header don't immediately
/// trip the "Unversioned" path.
const TEMPLATE_VERSION_SCAN_LINES: usize = 30;

/// Result of comparing a user's `therminal.toml` against the current
/// default template version (see [`CONFIG_TEMPLATE_VERSION`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConfigTemplateStatus {
    /// The file matches the current template version (or was freshly
    /// generated by therminal). Nothing to do.
    #[default]
    UpToDate,
    /// The file has a `template_version` marker older than the current one.
    Outdated {
        /// The version found in the file.
        found: u32,
        /// The current template version.
        current: u32,
    },
    /// The file has no `template_version` marker at all (pre-versioning era).
    Unversioned,
}

/// Parse the first ~30 lines of a config file for a `template_version = N`
/// comment, and compare against the current template version.
///
/// This is a pure function — it takes the file contents as `&str` and does
/// no IO. The marker is expected as a comment line of the form
/// `# template_version = 1` (with optional surrounding whitespace and an
/// optional trailing comment after the number). Anything else is treated as
/// the absence of the marker.
///
/// Returns:
/// - [`ConfigTemplateStatus::UpToDate`] if the marker is present and matches
///   `current_version`, OR if the file's marker is **newer** than what this
///   build knows about (we assume forward compatibility — better to stay
///   silent than to nag users running a config that came from a newer
///   build).
/// - [`ConfigTemplateStatus::Outdated`] if the marker is present and the
///   parsed version is strictly less than `current_version`.
/// - [`ConfigTemplateStatus::Unversioned`] if no marker is found in the
///   first [`TEMPLATE_VERSION_SCAN_LINES`] lines.
pub fn check_config_template_status(
    file_contents: &str,
    current_version: u32,
) -> ConfigTemplateStatus {
    for line in file_contents.lines().take(TEMPLATE_VERSION_SCAN_LINES) {
        if let Some(found) = parse_template_version_marker(line) {
            if found >= current_version {
                return ConfigTemplateStatus::UpToDate;
            }
            return ConfigTemplateStatus::Outdated {
                found,
                current: current_version,
            };
        }
    }
    ConfigTemplateStatus::Unversioned
}

/// Parse a single line for a `# template_version = N` marker, returning the
/// parsed `N` if the line matches the expected shape.
///
/// The marker must:
/// 1. Start with `#` (it lives in a TOML comment so it can't be a real key).
/// 2. Have `template_version` as the first non-whitespace token after the `#`.
/// 3. Be followed by `=` (with arbitrary surrounding whitespace).
/// 4. Be followed by an unsigned integer.
///
/// Anything after the integer (e.g. an inline `# explanation` tail) is
/// ignored. A leading `[` (TOML section header) or any non-`#` line short
/// circuits with `None` so we don't accidentally match TOML keys named
/// `template_version`.
fn parse_template_version_marker(line: &str) -> Option<u32> {
    let trimmed = line.trim_start();
    let after_hash = trimmed.strip_prefix('#')?;
    let after_hash = after_hash.trim_start();
    let rest = after_hash.strip_prefix("template_version")?;
    // The next char must be whitespace or `=`, otherwise this is some other
    // identifier like `template_version_extended`.
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    let rest = rest.trim_start();
    // Take leading digits only — stop at the first non-digit so trailing
    // comment text or whitespace doesn't break parsing.
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

/// Emit a tracing message describing the result of a template-version scan.
///
/// Up-to-date files log at `info!` (silent in normal logs); unversioned and
/// outdated files log at `warn!` so they show up in default tracing output
/// alongside the GUI status-bar hint.
fn log_template_status(path: &Path, status: &ConfigTemplateStatus) {
    match status {
        ConfigTemplateStatus::UpToDate => {
            info!(
                ?path,
                template_version = CONFIG_TEMPLATE_VERSION,
                "config template up to date"
            );
        }
        ConfigTemplateStatus::Unversioned => {
            warn!(
                ?path,
                current = CONFIG_TEMPLATE_VERSION,
                "config has no template_version marker — predates tn-3ge3 versioning; \
                 run `therminal --print-config` to see the latest defaults"
            );
        }
        ConfigTemplateStatus::Outdated { found, current } => {
            warn!(
                ?path,
                found,
                current,
                "config template_version is older than current — \
                 run `therminal --print-config` to see the latest defaults"
            );
        }
    }
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
    /// Daemon auto-spawn settings.
    pub daemon: DaemonConfig,
    /// Bell behavior settings.
    pub bell: BellConfig,
    /// Notification settings.
    pub notifications: NotificationConfig,
    /// Hotspot (clickable file/URL) settings.
    pub hotspots: HotspotsConfig,
    /// Semantic pattern-matching engine settings (tn-yrjd).
    pub patterns: PatternsConfig,
    /// Sibling delegate profiles for spawning isolated AI agent siblings (tn-ztv3).
    pub delegate: DelegateConfig,
    /// Overlay widget settings (tn-x85k).
    pub widgets: WidgetsConfig,
    /// Result of the template-version scan performed by [`Self::load_from`].
    ///
    /// Computed in-process and never round-tripped through TOML
    /// (`#[serde(skip)]`). Defaults to [`ConfigTemplateStatus::UpToDate`] so
    /// callers that build a [`TherminalConfig`] without going through
    /// [`Self::load_from`] (tests, defaults, programmatic construction) get
    /// the silent / no-hint behaviour.
    #[serde(skip)]
    pub template_status: ConfigTemplateStatus,
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
                Ok(mut config) => {
                    info!(?path, "loaded config");
                    // F8 (tn-97j6): detect whether the user's TOML actually
                    // mentions `attach_mode` so we can distinguish explicit
                    // setting from the default-flip to Remote.
                    config.mcp.attach_mode_explicit = contents
                        .lines()
                        .any(|l| l.trim_start().starts_with("attach_mode"));
                    // tn-3ge3: detect whether the user's file predates the
                    // current default template so the GUI can hint at the
                    // status bar without rewriting their file.
                    config.template_status =
                        check_config_template_status(&contents, CONFIG_TEMPLATE_VERSION);
                    log_template_status(path, &config.template_status);
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
        let replacement = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        let contents = match std::fs::read_to_string(path) {
            Ok(existing) => match merged_toml_with_existing(&existing, &replacement) {
                Ok(merged) => merged,
                Err(_) => replacement,
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => replacement,
            Err(err) => return Err(err),
        };
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
    /// Used by `therminal --print-config`. The output is prefixed with the
    /// `template_version` comment marker so that users who redirect this
    /// command into their `therminal.toml` (the documented upgrade path
    /// surfaced by the status bar hint, tn-3ge3) end up with a file that
    /// scans as [`ConfigTemplateStatus::UpToDate`] on the next launch.
    pub fn to_toml_string(&self) -> String {
        let body = toml::to_string_pretty(self)
            .unwrap_or_else(|e| format!("# serialization error: {e}\n"));
        format!(
            "# Therminal config — generated by `therminal --print-config`\n\
             # template_version = {CONFIG_TEMPLATE_VERSION}   # used by upgrade detection, do not edit manually\n\
             \n\
             {body}"
        )
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

        // Validate delegate profile commands (must be non-empty).
        for (name, profile) in &self.delegate.profiles {
            if profile.command.is_empty() {
                warn!(
                    profile = %name,
                    "delegate profile has an empty command; it will be unusable"
                );
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
    /// Whether to show the per-pane header strip in multi-pane layouts.
    /// When false, headers are hidden even with multiple panes; focus is
    /// indicated solely by the focus border, and footer surfaces focused-pane
    /// info. Single-pane layouts never show a header regardless.
    pub show_pane_headers: bool,
    /// Whether to show the workspace tab bar at the top of the window.
    pub show_tab_bar: bool,
    /// Use client-side decorations (custom title bar with window controls).
    /// Default: true on Linux and Windows, false on macOS.
    pub use_csd: bool,
    /// Automatically split panes when AI agents spawn subprocesses.
    pub auto_tile: bool,
    /// Debounce interval (ms) for auto-tile spawn/exit events to avoid layout thrashing.
    pub auto_tile_debounce_ms: u64,
    /// Scope filter for the Claude subagent swarm watcher.
    /// `All` shows subagents from any Claude Code session on the machine.
    /// `Current` restricts to subagents whose parent session belongs to a
    /// Claude Code process running under one of THIS therminal instance's panes.
    pub swarm_watch_scope: SwarmWatchScope,
}

/// Scope filter for the Claude subagent swarm watcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SwarmWatchScope {
    /// Show subagents from any Claude Code session on the machine.
    #[default]
    All,
    /// Only show subagents whose parent session is owned by this therminal instance.
    Current,
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
            show_pane_headers: true,
            show_tab_bar: true,
            use_csd: default_use_csd(),
            auto_tile: true,
            auto_tile_debounce_ms: 200,
            swarm_watch_scope: SwarmWatchScope::All,
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
    /// How the GUI attaches its panes to the terminal backend.
    ///
    /// - `Local` (default): the GUI owns its own portable_pty `Child`
    ///   processes via `PaneBackendKind::Terminal`. Byte-identical to
    ///   pre-tn-5ps8 behaviour.
    /// - `Remote`: the GUI subscribes to `DaemonEvent::PaneOutput` from
    ///   the daemon and forwards input/resize over IPC via
    ///   `PaneBackendKind::RemotePty`. Requires a running daemon.
    ///
    /// This is the migration switch for epic tn-382v. Local mode stays
    /// available for one release while remote mode bakes in.
    #[serde(default)]
    pub attach_mode: AttachMode,
    /// F8 (tn-97j6): tracks whether `attach_mode` was explicitly set in
    /// the user's config file or filled in from `AttachMode::default()`.
    /// `#[serde(skip)]` so it never round-trips through TOML — `load_from`
    /// post-processes the raw text to set this. Used by the GUI init log
    /// to call out the silent default-flip from Local → Remote.
    #[serde(skip)]
    pub attach_mode_explicit: bool,
}

/// Strategy used by the GUI to obtain PTY-backed panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachMode {
    /// GUI spawns and owns local PTY children (legacy escape hatch).
    /// Kept for one release after tn-beez so users can opt out if the
    /// daemon-driven path regresses for their workflow.
    Local,
    /// GUI streams PTY bytes from the daemon over IPC (default as of
    /// tn-beez Phase B — the GUI is a daemon client end-to-end).
    #[default]
    Remote,
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
            attach_mode: AttachMode::default(),
            attach_mode_explicit: false,
        }
    }
}

// ── Section: Daemon ─────────────────────────────────────────────────────

/// Daemon spawn / discovery settings.
///
/// The GUI is a daemon client by default (tn-beez). When the daemon is not
/// already running, the GUI auto-spawns it (tn-txs8). `binary_path` lets
/// users override the resolution chain when their `therminal-daemon` is
/// installed somewhere unusual.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Explicit absolute path to the `therminal-daemon` binary. When set,
    /// the GUI's auto-spawn helper uses this path verbatim instead of the
    /// "next to current exe" / `PATH` resolution chain. `None` (the default
    /// — represented by an empty string in TOML for round-tripping) means
    /// "auto-detect".
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_pathbuf"
    )]
    pub binary_path: Option<std::path::PathBuf>,
}

fn deserialize_optional_pathbuf<'de, D>(
    deserializer: D,
) -> Result<Option<std::path::PathBuf>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.and_then(|s| if s.is_empty() { None } else { Some(s.into()) }))
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

    /// Command (argv form) to spawn in a new pane when a directory hotspot
    /// is clicked.
    ///
    /// The literal token `{path}` in any argument is substituted with the
    /// clicked directory before the command is spawned. The new pane's
    /// working directory is also set to the clicked path so that even when
    /// the command's binary is missing the user lands in the right place.
    ///
    /// Default: `["tfe", "{path}"]` (Terminal File Explorer). Override with
    /// any TUI file explorer that accepts a path argument: yazi, ranger,
    /// nnn, lf, broot, mc, etc.
    ///
    /// An empty list disables the in-pane spawn entirely — clicks then
    /// fall back to the [`folder_opener`](Self::folder_opener) chain.
    pub folder_pane_command: Vec<String>,

    /// Ordered list of "reveal in file manager" commands tried for the
    /// secondary directory action ("Open in file manager" in the right-click
    /// menu).
    ///
    /// The first entry whose executable is found on `PATH` is used. The
    /// `$FILE_MANAGER` token is expanded from the env var at launch time
    /// and skipped if unset. Each command receives the directory as its
    /// last argument. If the entire chain fails, `open::that` is used as
    /// the last resort, which delegates to `xdg-open` / `open` / `explorer`
    /// depending on the platform.
    pub folder_opener: Vec<String>,
}

// ── Section: Patterns ───────────────────────────────────────────────────

/// Semantic pattern-matching engine settings (tn-yrjd).
///
/// Controls the pattern-pack surface from
/// `docs/pattern-matching-spec.md`. Every field declared here is wired to
/// code in `therminal-daemon::ensure` → `PatternEngine::new`; fields that
/// would otherwise be dead config are kept to the `directory` /
/// `max_patterns` / `slow_pattern_threshold_us` / `slow_strike_limit`
/// tuple defined by the performance-model doc §7.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PatternsConfig {
    /// Master enable toggle for the pattern engine. When `false`, packs
    /// still load so `terminal.patterns.stats` can report them, but the
    /// runtime `process_*` calls return empty vecs. Default: `true`.
    pub enabled: bool,
    /// Override the user pattern-pack directory. When unset (the default)
    /// the engine falls back to `<config_dir>/patterns` — i.e.
    /// `~/.config/therminal/patterns/` on Linux.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_pathbuf"
    )]
    pub directory: Option<PathBuf>,
    /// Global cap on the total number of loaded patterns across all packs
    /// combined. SPEC §4.1 default: 500. Set low to catch accidental DoS
    /// from runaway pack generation.
    pub max_patterns: usize,
    /// A pattern match that takes more than this many microseconds on a
    /// single input is logged as slow and counted against its strike
    /// limit. SPEC §3.1 default: 1000 (1 ms).
    pub slow_pattern_threshold_us: u64,
    /// Number of consecutive slow strikes before a pattern is disabled
    /// for the remainder of the daemon session. SPEC §3.2 default: 3.
    pub slow_strike_limit: u32,
}

// ── Section: Delegate ────────────────────────────────────────────────────

/// Top-level container for sibling delegate configuration (tn-ztv3).
///
/// Sibling delegates are isolated AI agent processes (e.g. Claude Code
/// sub-instances) that can be spawned into a new pane with a known role,
/// working directory policy, and MCP/permission envelope.  Each named
/// profile under `[delegate.profiles.<name>]` describes one such role.
///
/// At config load time the profiles are deserialized and validated (empty
/// `command` is warned); no runtime spawning happens here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DelegateConfig {
    /// Named delegate profiles, keyed by a short identifier (e.g.
    /// `"planner"`, `"browser-research"`, `"adversarial-review"`).
    pub profiles: HashMap<String, DelegateProfileConfig>,
}

/// Working-directory policy for a delegate profile.
///
/// Controls where the delegate process's cwd is set when it is spawned.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkingDirMode {
    /// Inherit the cwd of the pane that triggered the spawn.
    #[default]
    Same,
    /// Use the nearest git worktree root discovered by walking up from the
    /// triggering pane's cwd. Falls back to `Same` if no `.git` is found.
    Worktree,
    /// Create a temporary directory under `<runtime_dir>/scratch/<random>`
    /// and use that as the cwd. The directory is removed when the delegate
    /// exits.
    #[serde(rename = "scratch/{random}")]
    ScratchRandom,
}

/// Configuration for a single sibling delegate profile (tn-ztv3).
///
/// All fields except `command` are optional and fall back to safe defaults.
/// Unknown fields produce a deserialization error (`deny_unknown_fields`)
/// so typos surface immediately at config load time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DelegateProfileConfig {
    /// Human-readable description shown in the UI and in `therminal agents` output.
    pub description: String,
    /// Command template used to launch the delegate process.
    ///
    /// This is a **required** field — an empty string will produce a
    /// load-time warning and the profile will be unusable. Template tokens:
    ///
    /// - `{pane_id}` — the ID of the pane that triggered the spawn
    /// - `{session_id}` — the daemon session ID
    /// - `{cwd}` — the resolved working directory (after `working_dir` policy)
    ///
    /// Example: `"claude --pane {pane_id} --role planner"`
    pub command: String,
    /// Working-directory policy for the spawned process.
    ///
    /// Defaults to `"same"` (inherit caller's cwd).
    pub working_dir: WorkingDirMode,
    /// Allowlist of MCP tool domains that the delegate may call.
    ///
    /// An empty list means no additional MCP capabilities beyond what the
    /// daemon's trust tier already grants.  Each entry is a tool-name prefix
    /// (e.g. `"terminal.panes"`, `"terminal.sessions"`).
    pub mcp_enabled: Vec<String>,
    /// Permission mode string forwarded to the delegate at spawn time (e.g.
    /// `"bypassPermissions"`, `"default"`, `"plan"`).
    ///
    /// The value is passed verbatim; no validation is performed beyond
    /// ensuring it is a non-null string.
    pub permission_mode: String,
}

impl Default for DelegateProfileConfig {
    fn default() -> Self {
        Self {
            description: String::new(),
            command: String::new(),
            working_dir: WorkingDirMode::default(),
            mcp_enabled: Vec::new(),
            permission_mode: "default".to_string(),
        }
    }
}

// ── Section: Widgets ─────────────────────────────────────────────────────

/// Top-level container for overlay widget configuration (tn-x85k).
///
/// Each widget gets its own subsection under `[widgets]`. This sets the
/// precedent for future widgets (context gauges, tool-call cards, etc.)
/// to follow the same pattern: `[widgets.<name>]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetsConfig {
    /// Agent timeline bar: shows recent tool activity color-coded by
    /// category, with subagent entries visually distinguished.
    pub agent_timeline: AgentTimelineConfig,
}

/// Position of the agent timeline bar relative to the window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimelinePosition {
    /// Top-right corner of the window, below the tab bar.
    TopRight,
    /// Bottom-right corner of the window, above the status bar.
    #[default]
    BottomRight,
    /// Bottom-center of the window, above the status bar.
    BottomCenter,
}

/// Configuration for the agent timeline overlay widget (tn-x85k).
///
/// All fields are wired into the rendering code. `enabled` controls
/// whether the widget is drawn at all; the keybinding toggle
/// (`ToggleAgentTimeline`) flips a runtime flag that works independently
/// of this config value. When `enabled = false` (the default, per
/// Philosophy B), the timeline is hidden until the user presses the
/// toggle keybinding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentTimelineConfig {
    /// Whether the timeline widget is visible on startup.
    /// Default: false (Philosophy B — ship behind a keybinding).
    pub enabled: bool,
    /// Height of the timeline bar in physical pixels.
    pub height_px: u32,
    /// Maximum number of tool entries to keep in the ring buffer.
    pub max_entries: usize,
    /// Position of the timeline bar relative to the window.
    pub position: TimelinePosition,
}

impl Default for AgentTimelineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            height_px: 48,
            max_entries: 500,
            position: TimelinePosition::default(),
        }
    }
}

impl Default for PatternsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: None,
            max_patterns: 500,
            slow_pattern_threshold_us: 1000,
            slow_strike_limit: 3,
        }
    }
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

        // Cross-platform external folder-open chain. The platform `open`
        // crate covers the last-resort fallback (xdg-open / open / explorer);
        // these explicit entries let the user override with a preferred
        // file manager and gracefully degrade to xdg-open's default.
        let folder_opener: Vec<&str> = if cfg!(target_os = "macos") {
            vec!["$FILE_MANAGER", "open"]
        } else if cfg!(target_os = "windows") {
            vec!["$FILE_MANAGER", "explorer"]
        } else {
            vec!["$FILE_MANAGER", "xdg-open", "nautilus", "dolphin", "thunar"]
        };

        Self {
            editor_chain: chain.into_iter().map(String::from).collect(),
            folder_pane_command: vec!["tfe".to_string(), "{path}".to_string()],
            folder_opener: folder_opener.into_iter().map(String::from).collect(),
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
    fn hotspots_default_folder_pane_command_is_tfe() {
        let d = HotspotsConfig::default();
        assert_eq!(
            d.folder_pane_command,
            vec!["tfe".to_string(), "{path}".to_string()]
        );
    }

    #[test]
    fn hotspots_default_folder_opener_includes_file_manager_token() {
        let d = HotspotsConfig::default();
        assert!(
            d.folder_opener.iter().any(|s| s == "$FILE_MANAGER"),
            "folder_opener default chain must include $FILE_MANAGER"
        );
        // Platform sanity: at least one extra fallback is provided.
        assert!(
            d.folder_opener.len() >= 2,
            "folder_opener default chain must contain at least one fallback"
        );
    }

    #[test]
    fn hotspots_folder_fields_round_trip() {
        let mut config = TherminalConfig::default();
        config.hotspots.folder_pane_command = vec!["yazi".to_string(), "{path}".to_string()];
        config.hotspots.folder_opener = vec!["nautilus".to_string(), "xdg-open".to_string()];
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            decoded.hotspots.folder_pane_command,
            vec!["yazi".to_string(), "{path}".to_string()]
        );
        assert_eq!(
            decoded.hotspots.folder_opener,
            vec!["nautilus".to_string(), "xdg-open".to_string()]
        );
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

    #[test]
    fn save_to_preserves_existing_comments_and_unknown_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");
        let original = r#"# user header comment
[general]
# keep this comment
title = "Old Title"

[custom]
keep_me = "yes"
"#;
        std::fs::write(&path, original).unwrap();

        let mut config = TherminalConfig::load_from(&path);
        config.general.title = "New Title".to_string();
        config.save_to(&path).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("# user header comment"));
        assert!(on_disk.contains("# keep this comment"));
        assert!(on_disk.contains("title = \"New Title\""));
        assert!(on_disk.contains("[custom]"));
        assert!(on_disk.contains("keep_me = \"yes\""));
    }

    #[test]
    fn config_edit_session_tracks_dirty_apply_discard_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");

        let saved = TherminalConfig::default();
        let mut session = ConfigEditSession::from_saved(saved.clone());
        assert!(!session.is_dirty());
        assert_eq!(session.saved().general.title, "Therminal");
        assert_eq!(session.applied().general.title, "Therminal");

        session.draft_mut().general.title = "Draft Title".to_string();
        assert!(session.is_dirty());
        assert_eq!(session.applied().general.title, "Therminal");

        session.apply_draft();
        assert_eq!(session.applied().general.title, "Draft Title");

        session.discard_draft();
        assert!(!session.is_dirty());
        assert_eq!(session.draft().general.title, "Therminal");
        assert_eq!(session.applied().general.title, "Therminal");

        session.draft_mut().general.title = "Persisted Title".to_string();
        session.apply_draft();
        session.save_draft_to(&path).unwrap();

        assert!(!session.is_dirty());
        assert_eq!(session.saved().general.title, "Persisted Title");
        assert_eq!(session.applied().general.title, "Persisted Title");

        let reloaded = TherminalConfig::load_from(&path);
        assert_eq!(reloaded.general.title, "Persisted Title");
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

    /// show_pane_headers round-trips with non-default value.
    #[test]
    fn show_pane_headers_round_trips_through_toml() {
        let toml_str = r#"
[general]
show_pane_headers = false
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert!(
            !config.general.show_pane_headers,
            "show_pane_headers should be false when set explicitly"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);
        assert!(
            !loaded.general.show_pane_headers,
            "show_pane_headers override must survive save/load round-trip"
        );
        // Sanity: default is true.
        assert!(TherminalConfig::default().general.show_pane_headers);
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

    // ── Template version detection tests ───────────────────────────────────

    #[test]
    fn check_template_status_up_to_date_marker() {
        let text = "# Therminal config\n# template_version = 1\n";
        assert_eq!(
            check_config_template_status(text, 1),
            ConfigTemplateStatus::UpToDate
        );
    }

    #[test]
    fn check_template_status_outdated_by_one() {
        let text = "# Therminal config\n# template_version = 1\n";
        assert_eq!(
            check_config_template_status(text, 2),
            ConfigTemplateStatus::Outdated {
                found: 1,
                current: 2,
            }
        );
    }

    #[test]
    fn check_template_status_outdated_by_many() {
        let text = "# Therminal config\n# template_version = 1\n[general]\n";
        assert_eq!(
            check_config_template_status(text, 7),
            ConfigTemplateStatus::Outdated {
                found: 1,
                current: 7,
            }
        );
    }

    #[test]
    fn check_template_status_unversioned_no_marker() {
        let text = "# Therminal config — hot-reloaded on save\n[general]\nshell = \"/bin/zsh\"\n";
        assert_eq!(
            check_config_template_status(text, 1),
            ConfigTemplateStatus::Unversioned
        );
    }

    #[test]
    fn check_template_status_marker_at_different_position() {
        // Marker is on line 5, well within the 30-line scan window.
        let text = "# Therminal config\n# Some other comment\n#\n# Another comment\n# template_version = 3\n[general]\n";
        assert_eq!(
            check_config_template_status(text, 3),
            ConfigTemplateStatus::UpToDate
        );
    }

    #[test]
    fn check_template_status_malformed_marker_treated_as_unversioned() {
        // `template_version = abc` doesn't parse, so the marker isn't
        // recognised. Falls through to Unversioned.
        let text = "# Therminal config\n# template_version = abc\n[general]\n";
        assert_eq!(
            check_config_template_status(text, 1),
            ConfigTemplateStatus::Unversioned
        );
    }

    #[test]
    fn check_template_status_marker_with_trailing_comment() {
        // Trailing text after the digits is allowed.
        let text =
            "# Therminal config\n# template_version = 2   # used by upgrade detection\n[general]\n";
        assert_eq!(
            check_config_template_status(text, 2),
            ConfigTemplateStatus::UpToDate
        );
        assert_eq!(
            check_config_template_status(text, 3),
            ConfigTemplateStatus::Outdated {
                found: 2,
                current: 3,
            }
        );
    }

    #[test]
    fn check_template_status_marker_with_extra_whitespace() {
        let text = "# Therminal config\n#    template_version    =    1\n[general]\n";
        assert_eq!(
            check_config_template_status(text, 1),
            ConfigTemplateStatus::UpToDate
        );
    }

    #[test]
    fn check_template_status_newer_marker_assumed_compatible() {
        // If the file claims a newer version than this build knows about,
        // we stay silent rather than nagging — assume forward compatibility.
        let text = "# Therminal config\n# template_version = 99\n";
        assert_eq!(
            check_config_template_status(text, 1),
            ConfigTemplateStatus::UpToDate
        );
    }

    #[test]
    fn check_template_status_marker_outside_scan_window_unversioned() {
        // Push the marker past the 30-line scan window with leading filler.
        let mut text = String::new();
        for _ in 0..40 {
            text.push_str("# filler comment line\n");
        }
        text.push_str("# template_version = 1\n");
        assert_eq!(
            check_config_template_status(&text, 1),
            ConfigTemplateStatus::Unversioned
        );
    }

    #[test]
    fn check_template_status_non_comment_line_with_template_version_ignored() {
        // A real TOML key (not in a comment) named `template_version` must
        // not be picked up — only the `#`-prefixed marker counts.
        let text = "[general]\ntemplate_version = 5\n";
        assert_eq!(
            check_config_template_status(text, 1),
            ConfigTemplateStatus::Unversioned
        );
    }

    #[test]
    fn check_template_status_default_template_is_up_to_date() {
        // Integration check: the freshly generated default template must
        // always pass an up-to-date scan against CONFIG_TEMPLATE_VERSION.
        let text = TherminalConfig::default_config_text();
        assert_eq!(
            check_config_template_status(&text, CONFIG_TEMPLATE_VERSION),
            ConfigTemplateStatus::UpToDate
        );
    }

    #[test]
    fn check_template_status_to_toml_string_includes_marker() {
        // `--print-config` output (`to_toml_string`) must scan as up-to-date
        // so users who follow the hint and run
        // `therminal --print-config > therminal.toml` end up with a versioned
        // file. Without this, the hint is misleading.
        let text = TherminalConfig::default().to_toml_string();
        assert_eq!(
            check_config_template_status(&text, CONFIG_TEMPLATE_VERSION),
            ConfigTemplateStatus::UpToDate
        );
        // And the resulting text is still valid TOML.
        let _: TherminalConfig =
            toml::from_str(&text).expect("to_toml_string output must remain valid TOML");
    }

    #[test]
    fn check_template_status_empty_string_is_unversioned() {
        assert_eq!(
            check_config_template_status("", 1),
            ConfigTemplateStatus::Unversioned
        );
    }

    #[test]
    fn load_from_pre_versioning_file_marks_unversioned() {
        // A user file written before the template_version marker existed
        // (e.g. the user's Apr 7 Windows therminal.toml) must load cleanly
        // and report `Unversioned` so the GUI can show its hint.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");
        std::fs::write(&path, "[general]\nshell = \"/bin/zsh\"\n").unwrap();

        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(loaded.template_status, ConfigTemplateStatus::Unversioned);
        // The user's file must NOT have been modified.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "[general]\nshell = \"/bin/zsh\"\n");
    }

    #[test]
    fn load_from_freshly_generated_file_is_up_to_date() {
        // load_from on a path that doesn't exist writes the default
        // template, then a follow-up load reports UpToDate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("therminal.toml");
        let _first = TherminalConfig::load_from(&path);
        let second = TherminalConfig::load_from(&path);
        assert_eq!(second.template_status, ConfigTemplateStatus::UpToDate);
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

    // ── Delegate profile tests ────────────────────────────────────────────

    #[test]
    fn delegate_profiles_deserialize() {
        let toml_str = r#"
[delegate.profiles.planner]
description = "Strategic planner"
command = "claude --pane {pane_id} --role planner"
working_dir = "worktree"
mcp_enabled = ["terminal.panes", "terminal.sessions"]
permission_mode = "plan"

[delegate.profiles.researcher]
description = "Web researcher"
command = "claude --pane {pane_id} --role researcher"
working_dir = "scratch/{random}"
mcp_enabled = ["browser"]
permission_mode = "default"
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.delegate.profiles.len(), 2);

        let planner = &config.delegate.profiles["planner"];
        assert_eq!(planner.description, "Strategic planner");
        assert_eq!(planner.command, "claude --pane {pane_id} --role planner");
        assert_eq!(planner.working_dir, WorkingDirMode::Worktree);
        assert_eq!(
            planner.mcp_enabled,
            vec!["terminal.panes", "terminal.sessions"]
        );
        assert_eq!(planner.permission_mode, "plan");

        let researcher = &config.delegate.profiles["researcher"];
        assert_eq!(researcher.working_dir, WorkingDirMode::ScratchRandom);
        assert_eq!(researcher.mcp_enabled, vec!["browser"]);
    }

    #[test]
    fn delegate_profile_defaults_apply_when_fields_absent() {
        let toml_str = r#"
[delegate.profiles.minimal]
command = "claude --pane {pane_id}"
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        let profile = &config.delegate.profiles["minimal"];
        assert_eq!(profile.working_dir, WorkingDirMode::Same);
        assert!(profile.mcp_enabled.is_empty());
        assert_eq!(profile.permission_mode, "default");
        assert!(profile.description.is_empty());
    }

    #[test]
    fn delegate_profile_unknown_field_produces_error() {
        let toml_str = r#"
[delegate.profiles.bad]
command = "claude"
unknown_field = "oops"
"#;
        let result = toml::from_str::<TherminalConfig>(toml_str);
        assert!(
            result.is_err(),
            "unknown field in delegate profile must produce a deserialization error"
        );
    }

    #[test]
    fn delegate_unknown_top_level_field_produces_error() {
        let toml_str = r#"
[delegate]
not_a_real_field = true
"#;
        let result = toml::from_str::<TherminalConfig>(toml_str);
        assert!(
            result.is_err(),
            "unknown field in [delegate] must produce a deserialization error"
        );
    }

    #[test]
    fn delegate_config_round_trips_through_toml() {
        let mut config = TherminalConfig::default();
        let mut profile = DelegateProfileConfig::default();
        profile.description = "Test delegate".to_string();
        profile.command = "claude --role test".to_string();
        profile.working_dir = WorkingDirMode::Worktree;
        profile.mcp_enabled = vec!["terminal.panes".to_string()];
        profile.permission_mode = "plan".to_string();
        config.delegate.profiles.insert("test".to_string(), profile);

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();

        let p = &decoded.delegate.profiles["test"];
        assert_eq!(p.description, "Test delegate");
        assert_eq!(p.command, "claude --role test");
        assert_eq!(p.working_dir, WorkingDirMode::Worktree);
        assert_eq!(p.mcp_enabled, vec!["terminal.panes"]);
        assert_eq!(p.permission_mode, "plan");
    }

    #[test]
    fn working_dir_mode_same_is_default() {
        let mode = WorkingDirMode::default();
        assert_eq!(mode, WorkingDirMode::Same);
    }

    #[test]
    fn working_dir_mode_scratch_random_serializes_correctly() {
        // The TOML key for ScratchRandom is the literal string "scratch/{random}".
        let toml_str = r#"
[delegate.profiles.isolated]
command = "claude"
working_dir = "scratch/{random}"
"#;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.delegate.profiles["isolated"].working_dir,
            WorkingDirMode::ScratchRandom
        );
    }
}
