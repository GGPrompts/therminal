//! TOML schema for pattern packs.
//!
//! These are the raw, on-disk representations deserialized from
//! `*.toml` files. They are validated and compiled into
//! [`crate::semantic_patterns::CompiledPattern`] / [`CompiledPack`] at load
//! time (see `loader.rs`).
//!
//! The schema matches `docs/pattern-matching-spec.md` — any changes here
//! must also update the SPEC and the authoring guide.

use serde::Deserialize;
use std::collections::HashMap;

/// A pattern pack file as parsed from TOML.
///
/// `pack_name` is optional; if omitted the filename stem is used. All
/// `[[pattern]]` tables live under the `pattern` field via TOML's array-of-
/// tables syntax.
#[derive(Debug, Clone, Deserialize)]
pub struct PatternPackToml {
    /// Optional explicit pack name. Must match `[a-z0-9_-]+` when present.
    #[serde(default)]
    pub pack_name: Option<String>,
    /// Optional human-readable description.
    #[serde(default)]
    pub pack_description: Option<String>,
    /// The list of `[[pattern]]` rules in this pack. May be empty.
    #[serde(default, rename = "pattern")]
    pub patterns: Vec<PatternToml>,
}

/// A single `[[pattern]]` rule as parsed from TOML.
#[derive(Debug, Clone, Deserialize)]
pub struct PatternToml {
    /// Unique identifier within the pack. Must match `[a-z0-9_-]+`.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// The regex source string. Rust `regex` crate syntax.
    #[serde(rename = "match")]
    pub match_regex: String,
    /// Scope enum: `finalized_line`, `prompt_boundary`, or `region`.
    pub scope: String,
    /// Action enum: `hotspot`, `widget`, or `emit_event`.
    pub action: String,

    /// Optional pane scoping: either a string (`"claude"`) or a table
    /// (`{ command = "cargo" }`).
    #[serde(default)]
    pub applies_to: Option<AppliesToToml>,

    /// Hotspot sub-table. Present iff `action = "hotspot"`.
    #[serde(default, rename = "hotspot")]
    pub hotspot: Option<HotspotActionToml>,

    /// Widget sub-table. Present iff `action = "widget"`.
    #[serde(default, rename = "widget")]
    pub widget: Option<WidgetActionToml>,

    /// Emit-event sub-table. Optional — if absent the event carries only
    /// the default body (captures + matched_text).
    #[serde(default, rename = "emit_event")]
    pub emit_event: Option<EmitEventActionToml>,
}

/// `applies_to` TOML shape: either a bare string (harness name) or a table.
///
/// Serde's untagged enum lets us accept both forms from the same field.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AppliesToToml {
    /// `applies_to = "claude"` — harness-scoped.
    Harness(String),
    /// `[pattern.applies_to]` with `command = "cargo"` — command-scoped.
    Table(AppliesToTable),
}

/// Expanded `applies_to` table form.
#[derive(Debug, Clone, Deserialize)]
pub struct AppliesToTable {
    /// Restrict to panes where this harness is active. Valid values are
    /// `claude`, `codex`, `copilot`, `aider` (matched against
    /// `crate::state_inference::AgentType::as_str`).
    #[serde(default)]
    pub harness: Option<String>,
    /// Restrict to OSC 633 command regions whose initiating command matches
    /// this prefix.
    #[serde(default)]
    pub command: Option<String>,
    /// Reserved for v2 (`applies_to.shell = "bash"`). Ignored in v1.
    #[serde(default)]
    pub shell: Option<String>,
}

/// Hotspot action configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct HotspotActionToml {
    /// `open_editor`, `open_url`, or `emit_event`.
    pub on_click: String,
    /// Required for `open_editor` / `open_url`. Supports `{capture}` refs.
    #[serde(default)]
    pub target: Option<String>,
    /// Optional tooltip text. Supports `{capture}` refs.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional hotspot kind hint. Defaults to `"pattern"`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// Widget action configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WidgetActionToml {
    /// `badge`, `gauge`, `sparkline`, or `card`.
    pub kind: String,
    /// `inline`, `line_right`, or `overlay`. Default: `line_right`.
    #[serde(default)]
    pub anchor: Option<String>,
    /// Text label. Required for `badge`, optional for `card`. Supports
    /// `{capture}` refs.
    #[serde(default)]
    pub label: Option<String>,
    /// Numeric value for `gauge` / `sparkline`. Supports `{capture}` refs;
    /// parsed as f64 at match time.
    #[serde(default)]
    pub value: Option<String>,
    /// Max for `gauge`. Supports `{capture}` refs. Default `"100"`.
    #[serde(default)]
    pub max: Option<String>,
    /// Color table: capture-value → CSS color name / hex string.
    #[serde(default)]
    pub color: HashMap<String, String>,
    /// Card title. `card` kind only. Supports `{capture}` refs.
    #[serde(default)]
    pub title: Option<String>,
    /// Card body. `card` kind only. Supports `{capture}` refs.
    #[serde(default)]
    pub body: Option<String>,
}

/// Emit-event action configuration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmitEventActionToml {
    /// Static key/value pairs merged into the emitted event body alongside
    /// `captures`. Values support `{capture}` refs.
    #[serde(default)]
    pub extra: HashMap<String, String>,
}
