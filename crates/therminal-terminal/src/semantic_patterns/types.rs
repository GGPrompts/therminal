//! Compiled pattern types.
//!
//! These are the validated, regex-compiled versions of the TOML schema in
//! `schema.rs`. The loader converts `PatternPackToml` → `CompiledPack` once
//! at load time; match-time code never touches the TOML structs.

use regex::Regex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

// ── Scopes ──────────────────────────────────────────────────────────────

/// When a pattern runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatternScope {
    /// Once per committed terminal line (SPEC §4.1).
    FinalizedLine,
    /// Once per OSC 133/633 `D` mark, against the full command transcript
    /// (SPEC §4.2).
    PromptBoundary,
    /// Once per delimited semantic region (SPEC §4.3). Reserved in v1 — the
    /// engine accepts the scope but `process_region` is a no-op until the
    /// region-indexer hook is wired.
    Region,
}

impl PatternScope {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "finalized_line" => Some(Self::FinalizedLine),
            "prompt_boundary" => Some(Self::PromptBoundary),
            "region" => Some(Self::Region),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FinalizedLine => "finalized_line",
            Self::PromptBoundary => "prompt_boundary",
            Self::Region => "region",
        }
    }
}

// ── Pane scoping (`applies_to`) ─────────────────────────────────────────

/// Restriction on which panes a pattern runs against (SPEC §6).
#[derive(Debug, Clone)]
pub enum AppliesTo {
    /// Runs on every pane.
    Global,
    /// Runs only on panes where the named harness is active. Matched
    /// against `crate::state_inference::AgentType::as_str`.
    Harness(String),
    /// Runs only within OSC 633 regions whose initiating command matches
    /// this prefix.
    Command(String),
    /// Harness + command combined (rare, but explicit per SPEC).
    HarnessAndCommand { harness: String, command: String },
}

impl AppliesTo {
    /// True when this scope would apply to a pane whose active harness
    /// and current command are as given. `current_command` is typically the
    /// first shell token of the in-progress OSC 633 block.
    pub fn matches(&self, active_harness: Option<&str>, current_command: Option<&str>) -> bool {
        match self {
            Self::Global => true,
            Self::Harness(h) => active_harness == Some(h.as_str()),
            Self::Command(c) => match current_command {
                Some(cmd) => cmd.starts_with(c.as_str()),
                None => false,
            },
            Self::HarnessAndCommand { harness, command } => {
                active_harness == Some(harness.as_str())
                    && current_command
                        .map(|c| c.starts_with(command.as_str()))
                        .unwrap_or(false)
            }
        }
    }
}

// ── Actions ─────────────────────────────────────────────────────────────

/// Compiled action definition attached to a compiled pattern.
#[derive(Debug, Clone)]
pub enum PatternAction {
    Hotspot(HotspotAction),
    Widget(WidgetAction),
    EmitEvent(EmitEventAction),
}

impl PatternAction {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Hotspot(_) => "hotspot",
            Self::Widget(_) => "widget",
            Self::EmitEvent(_) => "emit_event",
        }
    }
}

/// Hotspot action.
#[derive(Debug, Clone)]
pub struct HotspotAction {
    pub on_click: HotspotOnClick,
    /// Raw target template with `{capture}` placeholders. `None` when the
    /// handler does not take a target (e.g. `emit_event`).
    pub target_template: Option<String>,
    pub label_template: Option<String>,
    pub kind: String,
}

/// Click-handler variants for hotspot actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotspotOnClick {
    OpenEditor,
    OpenUrl,
    EmitEvent,
}

impl HotspotOnClick {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenEditor => "open_editor",
            Self::OpenUrl => "open_url",
            Self::EmitEvent => "emit_event",
        }
    }
}

/// Widget placement action.
#[derive(Debug, Clone)]
pub struct WidgetAction {
    pub kind: WidgetKind,
    pub anchor: WidgetAnchor,
    pub label_template: Option<String>,
    pub value_template: Option<String>,
    pub max_template: Option<String>,
    pub title_template: Option<String>,
    pub body_template: Option<String>,
    pub color: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    Badge,
    Gauge,
    Sparkline,
    Card,
}

impl WidgetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Badge => "badge",
            Self::Gauge => "gauge",
            Self::Sparkline => "sparkline",
            Self::Card => "card",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetAnchor {
    Inline,
    LineRight,
    Overlay,
}

impl WidgetAnchor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::LineRight => "line_right",
            Self::Overlay => "overlay",
        }
    }
}

/// Emit-event action.
#[derive(Debug, Clone, Default)]
pub struct EmitEventAction {
    /// Static `extra` key/value pairs (values support `{capture}` refs).
    pub extra: HashMap<String, String>,
}

// ── Compiled pattern + pack ─────────────────────────────────────────────

/// A single validated, regex-compiled pattern.
///
/// Metrics (`match_count`, `miss_count`, `slow_count`, `strike_count`,
/// `total_match_us`, `last_match_ts_ms`) are stored as atomics so the
/// hot-path can update them without a lock.
#[derive(Debug)]
pub struct CompiledPattern {
    pub pack_name: String,
    pub name: String,
    pub description: Option<String>,
    pub regex: Regex,
    pub scope: PatternScope,
    pub applies_to: AppliesTo,
    pub action: PatternAction,

    // Metrics:
    pub match_count: AtomicU64,
    pub miss_count: AtomicU64,
    pub total_match_us: AtomicU64,
    pub slow_count: AtomicU64,
    pub strike_count: AtomicU64,
    pub last_match_ts_ms: AtomicU64,
    /// True when the pattern has been disabled by the slow-match circuit.
    pub disabled: std::sync::atomic::AtomicBool,
}

impl CompiledPattern {
    /// Fully-qualified id: `<pack_name>/<pattern_name>`.
    pub fn full_id(&self) -> String {
        format!("{}/{}", self.pack_name, self.name)
    }

    /// Record a successful match of `elapsed` microseconds against a
    /// 200-char normalized input (SPEC §8.3). Returns `true` if this bump
    /// took the pattern past the strike threshold and it was disabled.
    pub fn record_success(
        &self,
        elapsed_us: u64,
        slow_threshold_us: u64,
        strike_limit: u32,
    ) -> bool {
        self.match_count.fetch_add(1, Ordering::Relaxed);
        self.total_match_us.fetch_add(elapsed_us, Ordering::Relaxed);
        self.last_match_ts_ms
            .store(unix_ms_now(), Ordering::Relaxed);
        self.update_slow_counter(elapsed_us, slow_threshold_us, strike_limit)
    }

    /// Record a miss (pattern was evaluated, no match). Misses still
    /// count toward the slow-match circuit so persistently-slow-but-never-
    /// matching patterns are also disabled.
    pub fn record_miss(&self, elapsed_us: u64, slow_threshold_us: u64, strike_limit: u32) -> bool {
        self.miss_count.fetch_add(1, Ordering::Relaxed);
        self.total_match_us.fetch_add(elapsed_us, Ordering::Relaxed);
        self.update_slow_counter(elapsed_us, slow_threshold_us, strike_limit)
    }

    fn update_slow_counter(
        &self,
        elapsed_us: u64,
        slow_threshold_us: u64,
        strike_limit: u32,
    ) -> bool {
        if elapsed_us > slow_threshold_us {
            self.slow_count.fetch_add(1, Ordering::Relaxed);
            let new_strikes = self.strike_count.fetch_add(1, Ordering::Relaxed) + 1;
            if new_strikes >= strike_limit as u64 && !self.disabled.swap(true, Ordering::Relaxed) {
                return true;
            }
        } else {
            // Fast match resets the consecutive counter.
            self.strike_count.store(0, Ordering::Relaxed);
        }
        false
    }

    /// Average match time in milliseconds.
    pub fn avg_match_ms(&self) -> f64 {
        let evals =
            self.match_count.load(Ordering::Relaxed) + self.miss_count.load(Ordering::Relaxed);
        if evals == 0 {
            0.0
        } else {
            (self.total_match_us.load(Ordering::Relaxed) as f64) / (evals as f64) / 1000.0
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::Relaxed)
    }
}

/// A validated pack of compiled patterns.
#[derive(Debug)]
pub struct CompiledPack {
    pub name: String,
    pub description: Option<String>,
    /// Patterns that passed regex compilation.
    pub patterns: Vec<std::sync::Arc<CompiledPattern>>,
    /// Per-pattern load errors: `(pattern_name, error_message)`.
    pub load_errors: Vec<(String, String)>,
}

// ── Match result ────────────────────────────────────────────────────────

/// Runtime result of a successful pattern match.
///
/// Lives only long enough for the dispatcher to route it to the hotspot
/// registry / widget substrate / event bus. Owns the captures so it can
/// cross thread boundaries safely.
#[derive(Debug, Clone)]
pub struct PatternMatch {
    /// The originating pattern (pack-qualified id).
    pub pack_name: String,
    pub pattern_name: String,
    /// Which scope produced the match.
    pub scope: PatternScope,
    /// Optional pane id the match was produced for (None for synthetic
    /// tests / global dispatch).
    pub pane_id: Option<u64>,
    /// Byte range within the matched-against string.
    pub byte_start: usize,
    pub byte_end: usize,
    /// The full matched substring.
    pub matched_text: String,
    /// Named captures, expanded with empty strings for unparticipating
    /// groups.
    pub captures: HashMap<String, String>,
    /// Resolved action ready to dispatch.
    pub action: ResolvedAction,
}

/// Action with all `{capture}` references expanded.
#[derive(Debug, Clone)]
pub enum ResolvedAction {
    Hotspot(ResolvedHotspot),
    Widget(ResolvedWidget),
    EmitEvent(ResolvedEmitEvent),
}

impl ResolvedAction {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Hotspot(_) => "hotspot",
            Self::Widget(_) => "widget",
            Self::EmitEvent(_) => "emit_event",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedHotspot {
    pub on_click: HotspotOnClick,
    pub target: Option<String>,
    pub label: Option<String>,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedWidget {
    pub kind: WidgetKind,
    pub anchor: WidgetAnchor,
    pub label: Option<String>,
    /// Parsed numeric value (None if `value_template` was absent or could
    /// not be parsed as f64).
    pub value: Option<f64>,
    pub max: Option<f64>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Resolved color for this match: picks the entry whose key matches a
    /// capture value (including the special `"default"` key). `None` when
    /// no color table was configured or no entry matched.
    pub color: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedEmitEvent {
    /// Merged key/value body. Always contains `matched_text`; additional
    /// entries from `extra` have `{capture}` refs expanded.
    pub extra: HashMap<String, String>,
}

// ── Utilities ───────────────────────────────────────────────────────────

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Expand `{capture_name}` placeholders in `template` using `captures`.
///
/// Missing captures expand to `""` per SPEC §2.4. Literal braces are
/// written as `{{` and `}}`.
pub fn expand_template(template: &str, captures: &HashMap<String, String>) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'{' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                out.push('{');
                i += 2;
                continue;
            }
            // Find closing `}`.
            if let Some(end) = bytes[i + 1..].iter().position(|&b| b == b'}') {
                let name = &template[i + 1..i + 1 + end];
                if !name.is_empty() {
                    if let Some(val) = captures.get(name) {
                        out.push_str(val);
                    }
                    // Missing → empty string (per SPEC §2.4).
                    i += 1 + end + 1;
                    continue;
                }
            }
            // Malformed `{...` — emit literal.
            out.push('{');
            i += 1;
        } else if c == b'}' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'}' {
                out.push('}');
                i += 2;
                continue;
            }
            out.push('}');
            i += 1;
        } else {
            // UTF-8 safe: push a whole char starting at byte i.
            let ch = template[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_template_basic() {
        let mut caps = HashMap::new();
        caps.insert("file".to_string(), "src/lib.rs".to_string());
        caps.insert("line".to_string(), "42".to_string());
        assert_eq!(
            expand_template("Open {file}:{line}", &caps),
            "Open src/lib.rs:42"
        );
    }

    #[test]
    fn expand_template_missing_capture_is_empty() {
        let caps = HashMap::new();
        assert_eq!(expand_template("X{missing}Y", &caps), "XY");
    }

    #[test]
    fn expand_template_literal_braces() {
        let caps = HashMap::new();
        assert_eq!(expand_template("{{foo}}", &caps), "{foo}");
    }

    #[test]
    fn expand_template_unicode() {
        let mut caps = HashMap::new();
        caps.insert("name".to_string(), "π".to_string());
        assert_eq!(expand_template("∑{name}²", &caps), "∑π²");
    }

    #[test]
    fn applies_to_global_matches_everywhere() {
        let at = AppliesTo::Global;
        assert!(at.matches(None, None));
        assert!(at.matches(Some("claude"), Some("cargo build")));
    }

    #[test]
    fn applies_to_harness_requires_match() {
        let at = AppliesTo::Harness("claude".into());
        assert!(at.matches(Some("claude"), None));
        assert!(!at.matches(Some("codex"), None));
        assert!(!at.matches(None, None));
    }

    #[test]
    fn applies_to_command_prefix_match() {
        let at = AppliesTo::Command("cargo".into());
        // Literal `str::starts_with` semantics per SPEC §6.3: "prefix
        // match on the command name". The command string is whatever the
        // OSC 633 command tracker surfaces — typically the full argv as a
        // single string — and the prefix is compared byte-for-byte.
        assert!(at.matches(None, Some("cargo build")));
        assert!(at.matches(None, Some("cargo")));
        assert!(at.matches(None, Some("cargo-watch")));
        // A true literal prefix is greedy: `cargo` also prefixes `cargotest`.
        // Users who want word-boundary matching should add a trailing
        // space or switch to a finalized_line pattern with an anchor.
        assert!(at.matches(None, Some("cargotest")));
        assert!(!at.matches(None, Some("make")));
        assert!(!at.matches(None, None));
    }

    #[test]
    fn slow_match_disables_after_three_strikes() {
        let pat = CompiledPattern {
            pack_name: "t".into(),
            name: "p".into(),
            description: None,
            regex: Regex::new("x").unwrap(),
            scope: PatternScope::FinalizedLine,
            applies_to: AppliesTo::Global,
            action: PatternAction::EmitEvent(EmitEventAction::default()),
            match_count: AtomicU64::new(0),
            miss_count: AtomicU64::new(0),
            total_match_us: AtomicU64::new(0),
            slow_count: AtomicU64::new(0),
            strike_count: AtomicU64::new(0),
            last_match_ts_ms: AtomicU64::new(0),
            disabled: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(!pat.record_success(2000, 1000, 3));
        assert!(!pat.record_success(2000, 1000, 3));
        // Third strike triggers disable.
        assert!(pat.record_success(2000, 1000, 3));
        assert!(pat.is_disabled());
    }

    #[test]
    fn fast_match_resets_strike_counter() {
        let pat = CompiledPattern {
            pack_name: "t".into(),
            name: "p".into(),
            description: None,
            regex: Regex::new("x").unwrap(),
            scope: PatternScope::FinalizedLine,
            applies_to: AppliesTo::Global,
            action: PatternAction::EmitEvent(EmitEventAction::default()),
            match_count: AtomicU64::new(0),
            miss_count: AtomicU64::new(0),
            total_match_us: AtomicU64::new(0),
            slow_count: AtomicU64::new(0),
            strike_count: AtomicU64::new(0),
            last_match_ts_ms: AtomicU64::new(0),
            disabled: std::sync::atomic::AtomicBool::new(false),
        };
        pat.record_success(2000, 1000, 3);
        pat.record_success(2000, 1000, 3);
        pat.record_success(100, 1000, 3);
        assert!(!pat.is_disabled());
        // Must start over after the reset.
        pat.record_success(2000, 1000, 3);
        pat.record_success(2000, 1000, 3);
        assert!(!pat.is_disabled());
    }
}
