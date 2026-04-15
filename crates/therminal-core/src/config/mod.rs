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
pub mod profiles;

// Re-export all public types so that `therminal_core::config::*` paths
// continue to work unchanged after the split.
pub use config_text::CONFIG_TEMPLATE_VERSION;
pub use keybindings::*;
pub use profiles::{ProfileResolveError, ResolvedProfile};

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table};
use tracing::{info, warn};

use crate::font::FontConfig as CoreFontConfig;
use crate::palette::{ChromePalette, Color};

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
    /// Accessibility settings (tn-avjv.6).
    pub accessibility: AccessibilityConfig,
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
                    config.clamp_trust_settings();
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

        // Validate profile working directories and new launcher fields.
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
            // Warn if both `shell` and `command` are set (command wins).
            if profile.shell.is_some() && profile.command.is_some() {
                warn!(
                    profile = %name,
                    "profile has both `shell` and `command` set; `command` takes priority"
                );
            }
            // Warn if `color` is present but not a valid hex string.
            if let Some(ref color) = profile.color
                && ColorsConfig::parse_hex(color).is_none()
            {
                warn!(
                    profile = %name,
                    color = %color,
                    "profile `color` is not a valid hex color (#RRGGBB or #RGB)"
                );
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

    /// Clamp security-sensitive trust settings to safe values.
    ///
    /// `[trust] auto_approve_tier` is a footgun: setting it to `Trusted`
    /// silently auto-approves every MCP tool call, including destructive
    /// `Admin`-tier ones (`sessions.destroy`, `panes.destroy`, etc.),
    /// bypassing the GUI confirmation flow entirely. tn-t5il enforces an
    /// invariant at config-load time:
    ///
    /// > `auto_approve_tier` may never exceed `Supervised`.
    ///
    /// If a user sets `auto_approve_tier = "Trusted"` (or any future
    /// value above `Supervised`), this method clamps it down to
    /// `Supervised` and emits a prominent `tracing::warn!` so the
    /// override is loud and visible on every startup. The user's TOML
    /// is **not** rewritten — clamp-and-warn is friendlier than
    /// refuse-to-load and avoids destroying user intent, but the warn
    /// hits the log loudly enough that nobody can claim ignorance.
    ///
    /// Users who really want "trust everything" should set the agent's
    /// `tier = "trusted"` explicitly under `[trust.agents.<name>]`,
    /// which is per-agent and audit-logged on every call.
    pub fn clamp_trust_settings(&mut self) {
        if let Some(requested) = self.trust.auto_approve_tier
            && requested > TrustTier::Supervised
        {
            warn!(
                requested = %requested,
                clamped_to = %TrustTier::Supervised,
                "[trust] auto_approve_tier = \"{requested}\" would silently \
                 auto-approve destructive Admin-tier MCP tool calls \
                 (sessions.destroy, panes.destroy, etc.) and bypass the \
                 GUI confirmation flow. Clamping to \"Supervised\" — see \
                 tn-t5il and the doc comment on TrustConfig::auto_approve_tier \
                 for details. To grant full trust to a specific agent, \
                 set [trust.agents.<name>] tier = \"trusted\" instead."
            );
            self.trust.auto_approve_tier = Some(TrustTier::Supervised);
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

/// Working directory policy for newly split panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NewPaneCwd {
    /// Inherit the cwd of the focused pane at split time.
    #[default]
    Inherit,
    /// Always use the user's home directory.
    Home,
}

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
    /// Extra arguments passed to the shell on startup.
    pub shell_args: Vec<String>,
    /// Working directory policy for newly split panes.
    pub new_pane_cwd: NewPaneCwd,
    /// Extra environment variables set in the PTY.
    pub env: HashMap<String, String>,
    /// Padding in pixels around the terminal grid.
    pub padding: f32,
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
            shell_args: Vec::new(),
            new_pane_cwd: NewPaneCwd::default(),
            env: HashMap::new(),
            padding: 4.0,
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
    /// UI chrome font family (tabs, status bar, pane headers, CSD, overlays).
    ///
    /// When non-empty, all chrome text renders with this family while the
    /// terminal grid keeps using `family`. Falls back to `family` when empty.
    pub ui_font_family: String,
    /// Display/brand font family (splash, about).
    ///
    /// **Not yet wired.** Will be wired when the splash/about screen ships.
    /// Parsed and round-tripped through TOML but not consumed by any renderer
    /// today.
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
///
/// **Chrome roles (tn-g7oo)**: the `chrome_*` and `hotspot_*` families
/// override individual roles in the runtime [`crate::palette::ChromePalette`]
/// (pane headers, separators, focus border, status bar, tab bar, hotspot
/// underlines, ...). Defaults derive from the bundled Codex 2031 palette
/// via [`ChromePalette::default`], so themes only need to set the roles
/// they want to recolor. Hot-reload picks these up automatically because
/// `apply_color_overrides` rebuilds `GridRenderer.chrome_palette` from this
/// config on every config change.
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

    // ── Chrome role overrides (tn-g7oo) ──────────────────────────────
    /// Pane focus border color (3 px outline around the focused pane).
    pub chrome_focus_border: Option<HexColor>,
    /// Separator color between adjacent panes.
    pub chrome_separator: Option<HexColor>,
    /// Pane header background — focused pane.
    pub chrome_header_bg: Option<HexColor>,
    /// Pane header background — unfocused pane.
    pub chrome_header_bg_dim: Option<HexColor>,
    /// Status bar background fill.
    pub chrome_status_bar_bg: Option<HexColor>,
    /// Workspace tab bar background fill.
    pub chrome_tab_bar_bg: Option<HexColor>,
    /// Active workspace tab background.
    pub chrome_tab_active_bg: Option<HexColor>,
    /// CSD close button hover color.
    pub chrome_csd_close: Option<HexColor>,
    /// Primary chrome text color (pane header process labels, status bar
    /// center text). Themes that re-skin chrome backgrounds should set
    /// this to something readable against the new background.
    pub chrome_fg: Option<HexColor>,
    /// Muted chrome text color (pane indices, button labels).
    pub chrome_fg_muted: Option<HexColor>,
    /// Focus-accent chrome text (workspace number, agent indicator,
    /// active topic branch).
    pub chrome_fg_focus: Option<HexColor>,
    /// Warning chrome text (git detached HEAD, ...).
    pub chrome_fg_warn: Option<HexColor>,
    /// Alert chrome text (close button, fatal errors).
    pub chrome_fg_alert: Option<HexColor>,

    // ── Hotspot underline overrides (tn-g7oo) ────────────────────────
    /// Dotted underline color for `FilePath` hotspots.
    pub hotspot_filepath: Option<HexColor>,
    /// Dotted underline color for `Url` hotspots (click-to-open URLs).
    ///
    /// Setting this field also overrides the OSC 8 hyperlink underline color
    /// unless `chrome_hyperlink` is separately set.  Set `chrome_hyperlink`
    /// to decouple the two.
    pub hotspot_url: Option<HexColor>,
    /// OSC 8 hyperlink underline color, independent of `hotspot_url`.
    ///
    /// When unset, the hyperlink underline inherits the `hotspot_url`
    /// override (or the palette default when neither is set).  Set this to
    /// override OSC 8 underlines without affecting click-to-open URL hotspot
    /// styling.
    pub chrome_hyperlink: Option<HexColor>,
    /// Dotted underline color for `ErrorLocation` hotspots.
    pub hotspot_error: Option<HexColor>,
    /// Dotted underline color for `GitRef` hotspots.
    pub hotspot_gitref: Option<HexColor>,
    /// Dotted underline color for `IssueRef` hotspots.
    pub hotspot_issueref: Option<HexColor>,
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

    /// Resolve the 16-entry ANSI palette from config into a
    /// `[Color; 16]` array, or `None` if the user did not set one (or
    /// the configured vector is malformed).
    ///
    /// Used by [`Self::chrome_palette`] so derivation can pull accent
    /// colors from the theme's own ANSI palette instead of falling back
    /// to the bundled Codex accents (tn-n3vl).
    fn resolved_ansi(&self) -> Option<[Color; 16]> {
        let ansi = self.ansi.as_ref()?;
        if ansi.len() != 16 {
            return None;
        }
        let mut out = [Color::from_hex(0); 16];
        for (idx, hex) in ansi.iter().enumerate() {
            out[idx] = Self::parse_hex(hex)?;
        }
        Some(out)
    }

    /// Build a runtime [`ChromePalette`] by starting from the bundled
    /// defaults (or a palette derived from the configured cell colors)
    /// and applying any `chrome_*` / `hotspot_*` overrides set in this
    /// config.
    ///
    /// **Derivation (tn-n3vl)**: when the user has set any of
    /// `[colors] background`, `foreground`, or `ansi`, the base palette
    /// is computed by [`ChromePalette::derive_from_cells`] so the
    /// chrome (status bar, pane headers, tab bar, CSD strip, hotspot
    /// underlines) tracks the theme automatically. When none of those
    /// fields are set, the base is the bundled [`ChromePalette::default`]
    /// so the bundled Codex 2031 look is preserved bit-for-bit.
    ///
    /// Each explicit `chrome_*` / `hotspot_*` override is treated
    /// independently and layered on top of the base: invalid hex
    /// strings fall back to the base value for that role. Alpha bake-in
    /// for the few translucent roles (focus border, selection, exit
    /// stripes) is preserved — overriding `chrome_focus_border` updates
    /// only the RGB channels and reuses the default 0.92 alpha.
    pub fn chrome_palette(&self) -> ChromePalette {
        // ── Derive-from-cells base (tn-n3vl) ─────────────────────────────
        // Only derive when the user has actually overridden a cell color.
        // Otherwise stay bit-for-bit identical to `ChromePalette::default`
        // — any regression in the default path is a test failure in
        // `chrome_palette_default_matches_default_chrome_palette`.
        let ansi = self.resolved_ansi();
        let cells_overridden =
            self.background.is_some() || self.foreground.is_some() || ansi.is_some();
        let mut p = if cells_overridden {
            ChromePalette::derive_from_cells(
                self.background_color(),
                self.foreground_color(),
                ansi.as_ref(),
            )
        } else {
            ChromePalette::default()
        };

        // Helper: replace just the RGB channels of an existing [f32; 4],
        // preserving the alpha that's baked into the default.
        fn rgb_into(slot: &mut [f32; 4], color: Color) {
            let [r, g, b, _] = color.to_f32_array();
            slot[0] = r;
            slot[1] = g;
            slot[2] = b;
        }

        // Chrome roles. Each override is a single role-by-role replacement —
        // a few derived defaults (separator_focus, tab_bar_bg, tab_active_bg,
        // tab_active_underline) follow their parents so that overriding the
        // primary role re-skins the dependents in step.
        if let Some(c) = self
            .chrome_focus_border
            .as_deref()
            .and_then(Self::parse_hex)
        {
            rgb_into(&mut p.focus_border, c);
            // Separators adjacent to the focused pane track focus_border.
            rgb_into(&mut p.separator_focus, c);
            // Active workspace tab underline tracks focus_border.
            rgb_into(&mut p.tab_active_underline, c);
        }
        if let Some(c) = self.chrome_separator.as_deref().and_then(Self::parse_hex) {
            p.separator = c.to_f32_array();
        }
        if let Some(c) = self.chrome_header_bg.as_deref().and_then(Self::parse_hex) {
            p.header_bg = c.to_f32_array();
            // Active workspace tab background tracks the header background.
            p.tab_active_bg = c.to_f32_array();
        }
        if let Some(c) = self
            .chrome_header_bg_dim
            .as_deref()
            .and_then(Self::parse_hex)
        {
            p.header_bg_dim = c.to_f32_array();
        }
        if let Some(c) = self
            .chrome_status_bar_bg
            .as_deref()
            .and_then(Self::parse_hex)
        {
            p.status_bar_bg = c.to_f32_array();
            // Tab bar background defaults to the status bar background, so
            // an override of `chrome_status_bar_bg` automatically re-skins
            // the tab bar too.
            p.tab_bar_bg = c.to_f32_array();
        }
        if let Some(c) = self.chrome_tab_bar_bg.as_deref().and_then(Self::parse_hex) {
            p.tab_bar_bg = c.to_f32_array();
        }
        if let Some(c) = self
            .chrome_tab_active_bg
            .as_deref()
            .and_then(Self::parse_hex)
        {
            p.tab_active_bg = c.to_f32_array();
        }
        if let Some(c) = self.chrome_csd_close.as_deref().and_then(Self::parse_hex) {
            p.csd_close = c.to_f32_array();
        }

        // Chrome text-color roles. These are stored as `Color` (u8 channels)
        // so chrome modules can build per-state alpha-modulated GlyphColors.
        if let Some(c) = self.chrome_fg.as_deref().and_then(Self::parse_hex) {
            p.chrome_fg = c;
        }
        if let Some(c) = self.chrome_fg_muted.as_deref().and_then(Self::parse_hex) {
            p.chrome_fg_muted = c;
        }
        if let Some(c) = self.chrome_fg_focus.as_deref().and_then(Self::parse_hex) {
            p.chrome_fg_focus = c;
        }
        if let Some(c) = self.chrome_fg_warn.as_deref().and_then(Self::parse_hex) {
            p.chrome_fg_warn = c;
        }
        if let Some(c) = self.chrome_fg_alert.as_deref().and_then(Self::parse_hex) {
            p.chrome_fg_alert = c;
        }

        // Cursor + selection: respect existing terminal-color overrides so
        // those config fields stay backwards-compatible — they used to be
        // applied in `GridRenderer::resolved_*_color`.
        if let Some(c) = self.cursor.as_deref().and_then(Self::parse_hex) {
            rgb_into(&mut p.cursor, c);
        }
        if let Some(c) = self.selection.as_deref().and_then(Self::parse_hex) {
            rgb_into(&mut p.selection, c);
        }

        // Hotspot underline overrides (full RGBA — these all default to
        // alpha = 1.0 so a hex override produces the right value).
        if let Some(c) = self.hotspot_filepath.as_deref().and_then(Self::parse_hex) {
            p.hotspot_filepath = c.to_f32_array();
        }
        if let Some(c) = self.hotspot_url.as_deref().and_then(Self::parse_hex) {
            p.hotspot_url = c.to_f32_array();
            // Hyperlink underline (OSC 8) defaults to the URL hotspot color.
            // Overridden below by `chrome_hyperlink` when that field is set.
            p.hyperlink = c.to_f32_array();
        }
        // Allow OSC 8 hyperlink underlines to be overridden independently of
        // the click-to-open URL hotspot color.  Runs after the `hotspot_url`
        // block so it always wins when both fields are present.
        if let Some(c) = self.chrome_hyperlink.as_deref().and_then(Self::parse_hex) {
            p.hyperlink = c.to_f32_array();
        }
        if let Some(c) = self.hotspot_error.as_deref().and_then(Self::parse_hex) {
            p.hotspot_error = c.to_f32_array();
        }
        if let Some(c) = self.hotspot_gitref.as_deref().and_then(Self::parse_hex) {
            p.hotspot_gitref = c.to_f32_array();
        }
        if let Some(c) = self.hotspot_issueref.as_deref().and_then(Self::parse_hex) {
            p.hotspot_issueref = c.to_f32_array();
        }

        p
    }
}

// ── Section: Profiles ────────────────────────────────────────────────────

/// A named session profile with optional overrides.
///
/// Resolved to PTY spawn parameters via [`profiles::resolve_profile`], which
/// produces a [`ResolvedProfile`] suitable for conversion to
/// `therminal_terminal::pty::SpawnOptions` at the call site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileConfig {
    /// Shell command override for this profile.
    pub shell: Option<String>,
    /// Extra arguments passed to the shell binary.
    ///
    /// Only meaningful when `shell` is set; ignored when `command` is used.
    #[serde(default)]
    pub shell_args: Vec<String>,
    /// Freeform command alternative to `shell` + `shell_args`.
    ///
    /// When set, this string is executed instead of the shell binary.
    /// If both `shell` and `command` are set, `command` wins and a
    /// validation warning is emitted.
    pub command: Option<String>,
    /// Whether to inject shell-integration scripts for this profile.
    ///
    /// `None` = auto: `true` when the profile uses `shell`, `false` when
    /// it uses `command` (commands like `docker exec` or `ssh` are
    /// unlikely to benefit from shell integration).
    pub shell_integration: Option<bool>,
    /// Working directory override.
    pub working_directory: Option<String>,
    /// Extra environment variables for this profile.
    pub env: HashMap<String, String>,
    /// Font size override for this profile.
    pub font_size: Option<f32>,
    /// Scrollback lines override.
    pub scrollback_lines: Option<usize>,
    /// Nerd Font glyph for the launcher overlay tile (e.g. "\u{f489}").
    pub icon: Option<String>,
    /// Hex color for the launcher overlay tile background (`#RRGGBB` or `#RGB`).
    pub color: Option<String>,
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
    /// Intercept OSC 7337 sequences (shell PID reporting).
    pub osc_7337: bool,
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
            osc_7337: true,
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

impl std::fmt::Display for TrustTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandboxed => write!(f, "Sandboxed"),
            Self::Supervised => write!(f, "Supervised"),
            Self::Trusted => write!(f, "Trusted"),
        }
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
    /// Interval in seconds between process-tree scans for agent detection.
    ///
    /// Set to `0` to disable automatic process-tree scanning.
    /// Default is `3` seconds.
    pub agent_scan_interval: u64,
    /// Maximum number of destructive MCP operations (destroy_session, etc.)
    /// allowed per agent per minute. Set to `0` to disable rate limiting.
    /// Default is `5`.
    pub destructive_rate_limit: u32,
    /// Auto-approve trust escalations up to this tier without prompting the
    /// user via the GUI confirmation flow.
    ///
    /// Tier semantics (security-sensitive):
    /// - `None` (default): every escalation prompts. Safe; the GUI is
    ///   always in the loop.
    /// - `Some(Sandboxed)`: only Observer-tier reads auto-approve. The
    ///   incoming agent must already be at Sandboxed or above; this is
    ///   effectively a no-op for read-only tools that wouldn't prompt
    ///   anyway.
    /// - `Some(Supervised)`: Observer + Writer tools auto-approve. This is
    ///   the maximum allowed value — see the clamp note below. Useful for
    ///   trusted local agents that you don't want to babysit, while still
    ///   prompting for destructive Admin-tier operations
    ///   (`sessions.destroy`, `panes.destroy`, etc.).
    /// - `Some(Trusted)`: would silently auto-approve **all** tools
    ///   including destructive Admin-tier ones. **This is unsafe and is
    ///   clamped to `Supervised` at config load time** (tn-t5il) — a
    ///   `tracing::warn!` is emitted on every startup that loads such a
    ///   config so the override is loud and visible. To get effective
    ///   "trust everything" behavior, set the relevant agent's `tier =
    ///   "trusted"` explicitly under `[trust.agents.<name>]` instead;
    ///   that path is per-agent and audit-logged.
    pub auto_approve_tier: Option<TrustTier>,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_tier: TrustTier::Supervised,
            agents: HashMap::new(),
            agent_scan_interval: 3,
            destructive_rate_limit: 5,
            auto_approve_tier: None,
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

    /// Ordered list of TUI git tool names probed on `PATH` at startup
    /// (and on config reload). Tools that resolve get a context-menu
    /// entry on git commit hash hotspots; clicking the entry splits a
    /// new pane and runs the tool against the hash (tn-fzr0).
    ///
    /// Default: `["lazygit", "gitlogue", "tig"]`. The order controls
    /// the order of menu entries — first listed appears at the top.
    /// Each entry is a bare binary name; arguments are not configurable
    /// here because the invocation form differs per tool. The currently
    /// supported invocations are:
    ///
    /// - `lazygit --filter <hash>`
    /// - `gitlogue -c <hash>`
    /// - `tig show <hash>`
    ///
    /// Unknown tool names are silently ignored. To disable git-tool
    /// menu entries entirely, set this to `[]`.
    pub git_tools: Vec<String>,
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
    /// System resource metrics displayed in the status bar right section
    /// (tn-l6y3). Shows CPU usage and memory consumption for the host
    /// and optionally for the WSL environment when running on Windows.
    pub system_metrics: SystemMetricsConfig,
}

/// Position of the agent timeline bar relative to the window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// Configuration for system resource metrics in the status bar (tn-l6y3).
///
/// When enabled, a background thread polls CPU and memory usage at a
/// configurable interval and the status bar right section shows a
/// compact summary like "38% 7.4G" (Linux) or "Win 38% 7.4G | WSL 0.8
/// 2.1G" (Windows native with WSL panes).
///
/// All fields are wired into the rendering code via
/// `system_metrics::spawn_metrics_poller`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemMetricsConfig {
    /// Whether system metrics are shown in the status bar.
    /// Default: true (baked in by default per "surface over flags" philosophy).
    pub enabled: bool,
    /// Polling interval in milliseconds. Lower values give more
    /// responsive readings but consume more CPU. Default: 2000.
    pub poll_interval_ms: u64,
    /// Whether to show WSL-side metrics when running on Windows.
    /// When `true`, the poller shells out to `wsl.exe` to read
    /// `/proc/loadavg` and `/proc/meminfo` from the default distro.
    /// Default: auto-detect (enabled on Windows when WSL is present).
    pub show_wsl: Option<bool>,
    /// How often to run the WSL subprocess probe, in milliseconds.
    ///
    /// The WSL probe spawns `wsl.exe -e sh -c '...'` which has a
    /// non-trivial startup cost (50–200 ms). Load average and memory
    /// stats change slowly, so probing far less often than the host
    /// poll is fine. Default: 10000 (10 seconds).
    pub wsl_poll_interval_ms: u64,
}

impl Default for SystemMetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: 2000,
            show_wsl: None, // auto-detect
            wsl_poll_interval_ms: 10_000,
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

// ── Section: Accessibility ─────────────────────────────────────────────

/// Accessibility settings (tn-avjv.6).
///
/// Controls for high-contrast mode, reduced-motion preferences, and
/// UI chrome text scaling. These settings affect the chrome layer
/// (status bar, pane headers, tab bar, overlays) but NOT terminal cell
/// text, which is governed by `[font]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessibilityConfig {
    /// Enable high-contrast mode. Increases contrast for UI chrome
    /// elements (borders, headers, status bar text).
    pub high_contrast: bool,
    /// Reduce motion: disables cursor blink and any animated transitions.
    pub reduced_motion: bool,
    /// Scale factor for UI chrome text (status bar, pane headers, tab
    /// bar, overlays). `1.0` is the default size. Does NOT affect
    /// terminal cell text — use `[font].size` for that.
    ///
    /// Clamped to `0.5..=3.0` at load time.
    pub ui_text_scale: f32,
}

impl Default for AccessibilityConfig {
    fn default() -> Self {
        Self {
            high_contrast: false,
            reduced_motion: false,
            ui_text_scale: 1.0,
        }
    }
}

impl Default for HotspotsConfig {
    fn default() -> Self {
        // $VISUAL / $EDITOR always win when set — they represent an explicit
        // user preference.  `code` is a popular fallback but should never
        // override the user's chosen editor.
        let chain: Vec<&str> = vec!["$VISUAL", "$EDITOR", "code", "nvim", "vim", "nano"];

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
            git_tools: vec![
                "lazygit".to_string(),
                "gitlogue".to_string(),
                "tig".to_string(),
            ],
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
    fn hotspots_default_git_tools_includes_known_tools() {
        // tn-fzr0: default git_tools must contain the three tools whose
        // invocations the app knows about. Order matters because it
        // controls menu placement.
        let d = HotspotsConfig::default();
        assert_eq!(
            d.git_tools,
            vec![
                "lazygit".to_string(),
                "gitlogue".to_string(),
                "tig".to_string(),
            ]
        );
    }

    #[test]
    fn hotspots_git_tools_round_trips() {
        let mut config = TherminalConfig::default();
        config.hotspots.git_tools = vec!["tig".to_string(), "lazygit".to_string()];
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            decoded.hotspots.git_tools,
            vec!["tig".to_string(), "lazygit".to_string()]
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
    fn accessibility_config_round_trips() {
        let mut config = TherminalConfig::default();
        config.accessibility.high_contrast = true;
        config.accessibility.reduced_motion = true;
        config.accessibility.ui_text_scale = 1.5;
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert!(decoded.accessibility.high_contrast);
        assert!(decoded.accessibility.reduced_motion);
        assert!((decoded.accessibility.ui_text_scale - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn accessibility_defaults_are_sensible() {
        let d = AccessibilityConfig::default();
        assert!(!d.high_contrast);
        assert!(!d.reduced_motion);
        assert!((d.ui_text_scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn general_shell_args_round_trips() {
        let mut config = TherminalConfig::default();
        config.general.shell_args = vec!["--login".to_string(), "--norc".to_string()];
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            decoded.general.shell_args,
            vec!["--login".to_string(), "--norc".to_string()]
        );
    }

    #[test]
    fn general_new_pane_cwd_round_trips() {
        let mut config = TherminalConfig::default();
        config.general.new_pane_cwd = NewPaneCwd::Home;
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let decoded: TherminalConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(decoded.general.new_pane_cwd, NewPaneCwd::Home);
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
        assert_eq!(config.trust.agents["claude"].tier, TrustTier::Trusted);
    }

    /// tn-t5il: `auto_approve_tier` greater than `Supervised` is unsafe
    /// because it would silently auto-approve destructive Admin-tier MCP
    /// tool calls (`sessions.destroy`, `panes.destroy`, etc.) and bypass
    /// the GUI confirmation flow. The clamp at config-load time forces
    /// the effective value down to `Supervised`.
    #[test]
    fn clamp_trust_settings_clamps_trusted_to_supervised() {
        let mut config = TherminalConfig::default();
        config.trust.auto_approve_tier = Some(TrustTier::Trusted);
        config.clamp_trust_settings();
        assert_eq!(
            config.trust.auto_approve_tier,
            Some(TrustTier::Supervised),
            "auto_approve_tier = Trusted must be clamped to Supervised"
        );
    }

    /// `clamp_trust_settings` must NOT touch values that are already at or
    /// below the safe ceiling — they're explicit user intent.
    #[test]
    fn clamp_trust_settings_preserves_safe_values() {
        for safe in [
            None,
            Some(TrustTier::Sandboxed),
            Some(TrustTier::Supervised),
        ] {
            let mut config = TherminalConfig::default();
            config.trust.auto_approve_tier = safe;
            config.clamp_trust_settings();
            assert_eq!(
                config.trust.auto_approve_tier, safe,
                "safe value {safe:?} must be preserved"
            );
        }
    }

    /// End-to-end: a TOML file with `auto_approve_tier = "trusted"` must
    /// land at `Supervised` after `load_from` runs the post-parse clamp.
    /// This is the tn-t5il invariant the daemon's trust gate relies on.
    #[test]
    fn load_from_clamps_unsafe_auto_approve_tier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust-clamp.toml");
        std::fs::write(
            &path,
            r#"
[trust]
auto_approve_tier = "trusted"
"#,
        )
        .unwrap();

        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(
            loaded.trust.auto_approve_tier,
            Some(TrustTier::Supervised),
            "load_from must clamp `auto_approve_tier = trusted` down to Supervised"
        );
    }

    /// Same end-to-end load path but for the safe value — `Supervised`
    /// in the user's TOML must round-trip unchanged.
    #[test]
    fn load_from_preserves_supervised_auto_approve_tier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trust-supervised.toml");
        std::fs::write(
            &path,
            r#"
[trust]
auto_approve_tier = "supervised"
"#,
        )
        .unwrap();

        let loaded = TherminalConfig::load_from(&path);
        assert_eq!(
            loaded.trust.auto_approve_tier,
            Some(TrustTier::Supervised),
            "load_from must preserve `auto_approve_tier = supervised`"
        );
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

    // ── ProfileConfig launcher-field tests (tn-zpxv) ──────────────────────

    #[test]
    fn profiles_new_fields_deserialize() {
        let toml_str = r##"
[profiles.docker]
command = "docker exec -it my-app /bin/bash"
icon = "\uf308"
color = "#0db7ed"

[profiles.dev]
shell = "/bin/zsh"
shell_args = ["-l", "--no-rcs"]
shell_integration = true
font_size = 14.0

[profiles.ssh]
command = "ssh prod-server"
shell_integration = false
icon = "\uf489"
color = "#d4443e"
"##;
        let config: TherminalConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.profiles.len(), 3);

        let docker = &config.profiles["docker"];
        assert_eq!(
            docker.command.as_deref(),
            Some("docker exec -it my-app /bin/bash")
        );
        assert!(docker.shell.is_none());
        assert!(docker.shell_args.is_empty());
        assert!(docker.shell_integration.is_none());
        assert_eq!(docker.icon.as_deref(), Some("\u{f308}"));
        assert_eq!(docker.color.as_deref(), Some("#0db7ed"));

        let dev = &config.profiles["dev"];
        assert_eq!(dev.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(dev.shell_args, vec!["-l", "--no-rcs"]);
        assert_eq!(dev.shell_integration, Some(true));
        assert!(dev.command.is_none());
        assert!(dev.icon.is_none());
        assert!(dev.color.is_none());

        let ssh = &config.profiles["ssh"];
        assert_eq!(ssh.command.as_deref(), Some("ssh prod-server"));
        assert_eq!(ssh.shell_integration, Some(false));
    }

    #[test]
    fn profiles_new_fields_round_trip() {
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "docker".to_string(),
            ProfileConfig {
                command: Some("docker exec -it app bash".to_string()),
                icon: Some("\u{f308}".to_string()),
                color: Some("#0db7ed".to_string()),
                ..Default::default()
            },
        );
        config.profiles.insert(
            "pwsh".to_string(),
            ProfileConfig {
                shell: Some("pwsh".to_string()),
                shell_args: vec!["-NoLogo".to_string()],
                shell_integration: Some(true),
                ..Default::default()
            },
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        config.save_to(&path).unwrap();
        let loaded = TherminalConfig::load_from(&path);

        let docker = &loaded.profiles["docker"];
        assert_eq!(docker.command.as_deref(), Some("docker exec -it app bash"));
        assert_eq!(docker.icon.as_deref(), Some("\u{f308}"));
        assert_eq!(docker.color.as_deref(), Some("#0db7ed"));
        assert!(docker.shell.is_none());
        assert!(docker.shell_args.is_empty());

        let pwsh = &loaded.profiles["pwsh"];
        assert_eq!(pwsh.shell.as_deref(), Some("pwsh"));
        assert_eq!(pwsh.shell_args, vec!["-NoLogo"]);
        assert_eq!(pwsh.shell_integration, Some(true));
    }

    /// validate_paths warns when both shell and command are set.
    #[test]
    fn validate_paths_profile_shell_and_command_warns() {
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "conflict".to_string(),
            ProfileConfig {
                shell: Some("/bin/bash".to_string()),
                command: Some("docker exec -it app bash".to_string()),
                ..Default::default()
            },
        );
        // Should warn but not panic.
        config.validate_paths();
    }

    /// validate_paths warns on invalid hex color.
    #[test]
    fn validate_paths_profile_invalid_color_warns() {
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "bad-color".to_string(),
            ProfileConfig {
                color: Some("not-a-color".to_string()),
                ..Default::default()
            },
        );
        // Should warn but not panic.
        config.validate_paths();
    }

    /// validate_paths accepts valid hex colors in profiles.
    #[test]
    fn validate_paths_profile_valid_color_ok() {
        let mut config = TherminalConfig::default();
        config.profiles.insert(
            "good-6".to_string(),
            ProfileConfig {
                color: Some("#0db7ed".to_string()),
                ..Default::default()
            },
        );
        config.profiles.insert(
            "good-3".to_string(),
            ProfileConfig {
                color: Some("#abc".to_string()),
                ..Default::default()
            },
        );
        // Should not warn.
        config.validate_paths();
    }

    /// Default ProfileConfig has empty shell_args and None for new fields.
    #[test]
    fn profile_config_default_new_fields() {
        let p = ProfileConfig::default();
        assert!(p.shell_args.is_empty());
        assert!(p.command.is_none());
        assert!(p.shell_integration.is_none());
        assert!(p.icon.is_none());
        assert!(p.color.is_none());
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

    // ── ColorsConfig::chrome_palette (tn-g7oo) ──────────────────────────

    #[test]
    fn chrome_palette_default_matches_default_chrome_palette() {
        let colors = ColorsConfig::default();
        let derived = colors.chrome_palette();
        let default = ChromePalette::default();
        // Field-by-field RGBA equality.
        assert_eq!(derived.focus_border, default.focus_border);
        assert_eq!(derived.separator, default.separator);
        assert_eq!(derived.header_bg, default.header_bg);
        assert_eq!(derived.header_bg_dim, default.header_bg_dim);
        assert_eq!(derived.status_bar_bg, default.status_bar_bg);
        assert_eq!(derived.tab_bar_bg, default.tab_bar_bg);
        assert_eq!(derived.tab_active_bg, default.tab_active_bg);
        assert_eq!(derived.tab_active_underline, default.tab_active_underline);
        assert_eq!(derived.exit_ok, default.exit_ok);
        assert_eq!(derived.exit_error, default.exit_error);
        assert_eq!(derived.csd_close, default.csd_close);
        assert_eq!(derived.csd_button_hover, default.csd_button_hover);
        assert_eq!(derived.selection, default.selection);
        assert_eq!(derived.cursor, default.cursor);
        assert_eq!(derived.hyperlink, default.hyperlink);
        assert_eq!(derived.hotspot_filepath, default.hotspot_filepath);
        assert_eq!(derived.hotspot_url, default.hotspot_url);
        assert_eq!(derived.hotspot_error, default.hotspot_error);
        assert_eq!(derived.hotspot_gitref, default.hotspot_gitref);
        assert_eq!(derived.hotspot_issueref, default.hotspot_issueref);
    }

    #[test]
    fn chrome_palette_focus_border_override_preserves_alpha() {
        let colors = ColorsConfig {
            chrome_focus_border: Some("#ffffff".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        assert!((p.focus_border[0] - 1.0).abs() < 1e-6);
        assert!((p.focus_border[1] - 1.0).abs() < 1e-6);
        assert!((p.focus_border[2] - 1.0).abs() < 1e-6);
        // Default 0.92 alpha is preserved through the override.
        assert!((p.focus_border[3] - 0.92).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_focus_border_override_propagates_to_separator_focus() {
        let colors = ColorsConfig {
            chrome_focus_border: Some("#abcdef".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        assert_eq!(p.focus_border[0], p.separator_focus[0]);
        assert_eq!(p.focus_border[1], p.separator_focus[1]);
        assert_eq!(p.focus_border[2], p.separator_focus[2]);
        // Tab active underline also tracks focus border.
        assert_eq!(p.focus_border[0], p.tab_active_underline[0]);
        assert_eq!(p.focus_border[1], p.tab_active_underline[1]);
        assert_eq!(p.focus_border[2], p.tab_active_underline[2]);
    }

    #[test]
    fn chrome_palette_status_bar_bg_override_propagates_to_tab_bar_bg() {
        let colors = ColorsConfig {
            chrome_status_bar_bg: Some("#ffeedd".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        let expected = [
            0xff as f32 / 255.0,
            0xee as f32 / 255.0,
            0xdd as f32 / 255.0,
            1.0,
        ];
        assert_eq!(p.status_bar_bg, expected);
        assert_eq!(p.tab_bar_bg, expected);
    }

    #[test]
    fn chrome_palette_tab_bar_bg_override_does_not_affect_status_bar_bg() {
        let colors = ColorsConfig {
            chrome_tab_bar_bg: Some("#112233".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        // tab_bar_bg gets the override; status_bar_bg keeps the default.
        let expected_tab = [
            0x11 as f32 / 255.0,
            0x22 as f32 / 255.0,
            0x33 as f32 / 255.0,
            1.0,
        ];
        assert_eq!(p.tab_bar_bg, expected_tab);
        assert_eq!(p.status_bar_bg, ChromePalette::default().status_bar_bg);
    }

    #[test]
    fn chrome_palette_header_bg_override_propagates_to_tab_active_bg() {
        let colors = ColorsConfig {
            chrome_header_bg: Some("#444444".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        let expected = [0x44 as f32 / 255.0; 3];
        assert!((p.header_bg[0] - expected[0]).abs() < 1e-6);
        assert_eq!(p.header_bg, p.tab_active_bg);
    }

    #[test]
    fn chrome_palette_selection_override_preserves_alpha() {
        let colors = ColorsConfig {
            selection: Some("#11ff22".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        assert!((p.selection[0] - 0x11 as f32 / 255.0).abs() < 1e-6);
        assert!((p.selection[1] - 1.0).abs() < 1e-6);
        assert!((p.selection[2] - 0x22 as f32 / 255.0).abs() < 1e-6);
        // Selection highlight alpha (0.45) is preserved through the override.
        assert!((p.selection[3] - 0.45).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_cursor_override_preserves_alpha() {
        let colors = ColorsConfig {
            cursor: Some("#abcdef".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        assert!((p.cursor[0] - 0xab as f32 / 255.0).abs() < 1e-6);
        // Cursor alpha (0.85) is preserved.
        assert!((p.cursor[3] - 0.85).abs() < 1e-6);
    }

    #[test]
    fn chrome_palette_hotspot_url_override_propagates_to_hyperlink() {
        let colors = ColorsConfig {
            hotspot_url: Some("#00ff00".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        let expected = [0.0, 1.0, 0.0, 1.0];
        assert_eq!(p.hotspot_url, expected);
        // Hyperlink underline (OSC 8) tracks the URL hotspot color.
        assert_eq!(p.hyperlink, expected);
    }

    #[test]
    fn chrome_hyperlink_override_decouples_from_hotspot_url() {
        // hotspot_url = green, chrome_hyperlink = red — they must be
        // independent: p.hotspot_url == green, p.hyperlink == red.
        let colors = ColorsConfig {
            hotspot_url: Some("#00ff00".into()),
            chrome_hyperlink: Some("#ff0000".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        assert_eq!(
            p.hotspot_url,
            [0.0, 1.0, 0.0, 1.0],
            "hotspot_url should be green"
        );
        assert_eq!(
            p.hyperlink,
            [1.0, 0.0, 0.0, 1.0],
            "hyperlink should be red (chrome_hyperlink wins)"
        );
    }

    #[test]
    fn chrome_palette_invalid_hex_falls_back_to_default() {
        let colors = ColorsConfig {
            chrome_focus_border: Some("not-a-color".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        // Bad hex is silently ignored — default focus_border is used.
        assert_eq!(p.focus_border, ChromePalette::default().focus_border);
    }

    #[test]
    fn chrome_palette_unrelated_overrides_dont_cross_contaminate() {
        // Setting one role must not perturb the others.
        let colors = ColorsConfig {
            chrome_csd_close: Some("#00ff00".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        let default = ChromePalette::default();
        assert_ne!(p.csd_close, default.csd_close);
        // Everything else is unchanged.
        assert_eq!(p.focus_border, default.focus_border);
        assert_eq!(p.header_bg, default.header_bg);
        assert_eq!(p.status_bar_bg, default.status_bar_bg);
        assert_eq!(p.csd_button_hover, default.csd_button_hover);
    }

    // ── Derive-from-cells integration (tn-n3vl) ──────────────────────

    #[test]
    fn chrome_palette_derives_when_only_background_set() {
        // Setting only `background` (no chrome_*) should shift every
        // chrome background away from the Codex defaults, so a user who
        // hand-picks a single terminal bg gets a matching chrome.
        let colors = ColorsConfig {
            background: Some("#101820".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        let default = ChromePalette::default();
        // header_bg / header_bg_dim / status_bar_bg should all have
        // shifted from the bundled VOID_0/VOID_2 defaults.
        assert_ne!(p.header_bg, default.header_bg);
        assert_ne!(p.header_bg_dim, default.header_bg_dim);
        assert_ne!(p.status_bar_bg, default.status_bar_bg);
        // And the propagation rules still hold at build time.
        assert_eq!(p.tab_bar_bg, p.status_bar_bg);
        assert_eq!(p.tab_active_bg, p.header_bg);
    }

    #[test]
    fn chrome_palette_unset_cells_preserves_default_bit_for_bit() {
        // The no-override case must stay bit-for-bit identical to
        // `ChromePalette::default` — this is the invariant protected by
        // `chrome_palette_default_matches_default_chrome_palette`
        // above. Adding an extra assertion here keeps the intent
        // visible next to the new derive-from-cells tests.
        let colors = ColorsConfig::default();
        let p = colors.chrome_palette();
        let default = ChromePalette::default();
        assert_eq!(p.header_bg, default.header_bg);
        assert_eq!(p.chrome_fg, default.chrome_fg);
    }

    #[test]
    fn chrome_palette_explicit_chrome_header_bg_wins_over_derivation() {
        // Even when derivation is active (because `background` is set),
        // an explicit `chrome_header_bg` must override the derived value.
        let colors = ColorsConfig {
            background: Some("#000000".into()),
            chrome_header_bg: Some("#ff00ff".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        let expected = [1.0, 0.0, 1.0, 1.0];
        assert_eq!(p.header_bg, expected);
        // And the propagation from the explicit override still fires.
        assert_eq!(p.tab_active_bg, expected);
    }

    #[test]
    fn chrome_palette_light_theme_derives_darker_chrome_bg() {
        // On a light theme (bg = near-white, fg = near-black), the
        // derived header_bg should be *darker* than the window bg — the
        // sign of the luminance shift inverts automatically because the
        // derivation mixes bg toward fg.
        let colors = ColorsConfig {
            background: Some("#f5f5f5".into()),
            foreground: Some("#202020".into()),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        // header_bg luminance < background luminance.
        let header_lum = 0.2126 * p.header_bg[0] as f64
            + 0.7152 * p.header_bg[1] as f64
            + 0.0722 * p.header_bg[2] as f64;
        let bg_lum = 0.2126 * (0xf5 as f64 / 255.0)
            + 0.7152 * (0xf5 as f64 / 255.0)
            + 0.0722 * (0xf5 as f64 / 255.0);
        assert!(
            header_lum < bg_lum,
            "derived header_bg ({header_lum:.4}) should be darker than light bg ({bg_lum:.4})"
        );
    }

    #[test]
    fn chrome_palette_ansi_override_drives_accent_picks() {
        // Setting an ANSI palette (with a valid 16-entry vec) should
        // route accent picks through those indices.
        let ansi: Vec<HexColor> = vec![
            "#000000".into(),
            "#ff0000".into(),
            "#00ff00".into(),
            "#ffff00".into(),
            "#0000ff".into(),
            "#ff00ff".into(),
            "#00ffff".into(),
            "#ffffff".into(),
            "#808080".into(),
            "#ff8080".into(),
            "#80ff80".into(),
            "#ffff80".into(),
            "#8080ff".into(),
            "#ff80ff".into(),
            "#80ffff".into(),
            "#ffffff".into(),
        ];
        let colors = ColorsConfig {
            ansi: Some(ansi),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        // focus from idx 4 = blue
        assert_eq!(p.chrome_fg_focus, Color::from_hex(0x0000ff));
        // warn from idx 3 = yellow
        assert_eq!(p.chrome_fg_warn, Color::from_hex(0xffff00));
        // alert from idx 1 = red
        assert_eq!(p.chrome_fg_alert, Color::from_hex(0xff0000));
    }

    #[test]
    fn chrome_palette_malformed_ansi_falls_back_to_default_accents() {
        // A partial/malformed ANSI vector should not crash and should
        // keep the bundled accents — `resolved_ansi` returns None when
        // the vector doesn't have exactly 16 entries.
        let colors = ColorsConfig {
            background: Some("#060a12".into()),
            ansi: Some(vec!["#ff0000".into(), "#00ff00".into()]),
            ..ColorsConfig::default()
        };
        let p = colors.chrome_palette();
        // Derivation still fires because `background` is set, but the
        // accent picks fall back to Color::FOCUS / WARN / ALERT because
        // ansi was rejected.
        assert_eq!(p.chrome_fg_focus, Color::FOCUS);
        assert_eq!(p.chrome_fg_warn, Color::WARN);
        assert_eq!(p.chrome_fg_alert, Color::ALERT);
    }
}
