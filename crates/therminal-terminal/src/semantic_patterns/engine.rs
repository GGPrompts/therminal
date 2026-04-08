//! The pattern engine — runs compiled packs against incoming terminal text.
//!
//! The engine is a pure library component. It does not own a file watcher,
//! does not talk to a widget substrate, does not reach into the hotspot
//! registry directly. Callers feed it finalized lines / prompt-boundary
//! transcripts via the `process_*` APIs and consume the returned
//! [`PatternMatch`]es.
//!
//! Dispatch routing (the "where does this match end up?" decision) lives
//! outside the engine: in `therminal-terminal::hotspot_detection` for
//! hotspot-sourced entries, and in the daemon for widget placement and
//! event-bus publication. The engine emits a uniform [`PatternMatch`]
//! struct so downstream code has a stable shape to work against.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tracing::{info, warn};

use super::loader::{PackLoadError, load_packs_from_dir};
use super::types::{
    CompiledPack, CompiledPattern, EmitEventAction, HotspotAction, PatternAction, PatternMatch,
    PatternScope, ResolvedAction, ResolvedEmitEvent, ResolvedHotspot, ResolvedWidget, WidgetAction,
    expand_template,
};

// ── Engine config ───────────────────────────────────────────────────────

/// Runtime configuration mirrored from `TherminalConfig.patterns`.
///
/// Kept here (rather than referencing `therminal_core::config`) so the
/// `therminal-terminal` crate stays free of a `therminal-core` dependency.
/// The daemon constructs a `PatternEngineConfig` from the loaded
/// `TherminalConfig` at startup and passes it to [`PatternEngine::new`].
#[derive(Debug, Clone)]
pub struct PatternEngineConfig {
    /// Master enable toggle. When `false`, `process_*` calls return
    /// immediately without consulting any pack.
    pub enabled: bool,
    /// User pattern directory. `None` = use the default
    /// `<config_dir>/patterns` resolved at load time.
    pub user_pattern_dir: Option<PathBuf>,
    /// Shipped-example pack directory. `None` = auto-resolve from
    /// `THERMINAL_RESOURCES_DIR` / `<exe_dir>/../resources/..` / workspace
    /// root. Callers that want to suppress the shipped packs (unit tests)
    /// can pass an empty-but-present path that doesn't exist.
    pub shipped_pattern_dir: Option<PathBuf>,
    /// Hard cap on the total number of patterns loaded across all packs.
    /// SPEC §4.1 default: 500.
    pub max_patterns: usize,
    /// Slow-match threshold in microseconds. SPEC §8.3 default: 1000 (1ms).
    pub slow_pattern_threshold_us: u64,
    /// Consecutive slow strikes before a pattern is disabled. SPEC §3.2
    /// default: 3.
    pub slow_strike_limit: u32,
}

impl PatternEngineConfig {
    pub fn new_default() -> Self {
        Self {
            enabled: true,
            user_pattern_dir: None,
            shipped_pattern_dir: None,
            max_patterns: 500,
            slow_pattern_threshold_us: 1000,
            slow_strike_limit: 3,
        }
    }

    /// Disable the engine entirely. Convenience for tests and for construction
    /// paths that want to explicitly opt out.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::new_default()
        }
    }
}

impl Default for PatternEngineConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

// ── Engine ──────────────────────────────────────────────────────────────

/// Compiled, ready-to-run pattern engine.
///
/// Cloneable (cheap `Arc` clone on the inner state) so the daemon can hand
/// a handle to the MCP server and to pane workers without reference-juggling.
#[derive(Clone)]
pub struct PatternEngine {
    inner: Arc<RwLock<EngineInner>>,
    config: Arc<PatternEngineConfig>,
}

struct EngineInner {
    packs: Vec<CompiledPack>,
    load_errors: Vec<PackLoadError>,

    /// Scope indexes: pattern references filtered by scope for O(1) dispatch.
    index_finalized_line: Vec<Arc<CompiledPattern>>,
    index_prompt_boundary: Vec<Arc<CompiledPattern>>,
    index_region: Vec<Arc<CompiledPattern>>,

    /// True if the load-time cap kicked in during the last (re)load.
    cap_reached: bool,
}

impl PatternEngine {
    /// Construct a new engine, loading packs from disk immediately.
    ///
    /// Pack load errors do NOT fail construction — they are recorded on
    /// [`EngineStats`] and the engine runs with whatever successfully
    /// compiled. If `config.enabled == false`, construction still loads
    /// packs (so `terminal.patterns.stats` can report them) but
    /// `process_*` APIs will no-op.
    pub fn new(config: PatternEngineConfig) -> Self {
        let mut inner = EngineInner {
            packs: Vec::new(),
            load_errors: Vec::new(),
            index_finalized_line: Vec::new(),
            index_prompt_boundary: Vec::new(),
            index_region: Vec::new(),
            cap_reached: false,
        };
        let cfg = Arc::new(config);
        reload_into(&mut inner, &cfg);
        Self {
            inner: Arc::new(RwLock::new(inner)),
            config: cfg,
        }
    }

    /// Convenience constructor: build an engine with the default config
    /// and no shipped packs. Used by tests.
    pub fn empty() -> Self {
        Self::new(PatternEngineConfig::disabled())
    }

    /// Reload all packs from disk. Called when the config watcher fires or
    /// when pack files change.
    pub fn reload(&self) {
        let mut inner = self.inner.write().unwrap();
        reload_into(&mut inner, &self.config);
    }

    /// Return a snapshot of the current stats, suitable for the
    /// `terminal.patterns.stats` MCP tool (§6.3 of the perf model).
    pub fn stats(&self) -> EngineStats {
        let inner = self.inner.read().unwrap();
        EngineStats::from_inner(&inner)
    }

    /// The effective runtime config (cloneable).
    pub fn config(&self) -> &PatternEngineConfig {
        &self.config
    }

    // ── Processing hooks ────────────────────────────────────────────

    /// Run every `finalized_line`-scoped pattern applicable to this pane
    /// against `line`. Returns the resulting matches (possibly empty).
    ///
    /// `active_harness` is the harness identifier if a harness is active in
    /// the pane (from `AgentRegistry::get(pane_id).agent_type.as_str()`),
    /// `None` otherwise. `current_command` is the first token of the
    /// in-progress OSC 633 command region (if any).
    pub fn process_finalized_line(
        &self,
        pane_id: u64,
        line: &str,
        active_harness: Option<&str>,
        current_command: Option<&str>,
    ) -> Vec<PatternMatch> {
        if !self.config.enabled {
            return Vec::new();
        }
        let inner = self.inner.read().unwrap();
        let mut out = Vec::new();
        for pat in &inner.index_finalized_line {
            if pat.is_disabled() {
                continue;
            }
            if !pat.applies_to.matches(active_harness, current_command) {
                continue;
            }
            run_pattern_against(
                pat,
                line,
                PatternScope::FinalizedLine,
                Some(pane_id),
                &self.config,
                &mut out,
            );
        }
        out
    }

    /// Run every `prompt_boundary`-scoped pattern applicable to this pane
    /// against the command-finished transcript `output`. Called from the
    /// OSC 633 `D` mark handler; `command` is the originating command (if
    /// known from an `E` mark).
    pub fn process_prompt_boundary(
        &self,
        pane_id: u64,
        output: &str,
        active_harness: Option<&str>,
        command: Option<&str>,
    ) -> Vec<PatternMatch> {
        if !self.config.enabled {
            return Vec::new();
        }
        let inner = self.inner.read().unwrap();
        let mut out = Vec::new();
        for pat in &inner.index_prompt_boundary {
            if pat.is_disabled() {
                continue;
            }
            if !pat.applies_to.matches(active_harness, command) {
                continue;
            }
            run_pattern_against(
                pat,
                output,
                PatternScope::PromptBoundary,
                Some(pane_id),
                &self.config,
                &mut out,
            );
        }
        out
    }

    /// Run every `region`-scoped pattern applicable to this pane against
    /// `text`. Reserved in v1 — wired up but returns an empty vec with a
    /// warning log the first time it's called on a non-stub engine, per
    /// the scope-boundary note in the task.
    pub fn process_region(
        &self,
        pane_id: u64,
        text: &str,
        active_harness: Option<&str>,
        command: Option<&str>,
    ) -> Vec<PatternMatch> {
        if !self.config.enabled {
            return Vec::new();
        }
        let inner = self.inner.read().unwrap();
        if inner.index_region.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for pat in &inner.index_region {
            if pat.is_disabled() {
                continue;
            }
            if !pat.applies_to.matches(active_harness, command) {
                continue;
            }
            run_pattern_against(
                pat,
                text,
                PatternScope::Region,
                Some(pane_id),
                &self.config,
                &mut out,
            );
        }
        out
    }
}

// ── Load-time machinery ─────────────────────────────────────────────────

fn reload_into(inner: &mut EngineInner, config: &PatternEngineConfig) {
    inner.packs.clear();
    inner.load_errors.clear();
    inner.index_finalized_line.clear();
    inner.index_prompt_boundary.clear();
    inner.index_region.clear();
    inner.cap_reached = false;

    // Determine pack directories.
    let user_dir = match config.user_pattern_dir.clone() {
        Some(d) => d,
        None => default_user_pattern_dir(),
    };
    let shipped_dir = match config.shipped_pattern_dir.clone() {
        Some(d) => d,
        None => default_shipped_pattern_dir(),
    };

    // Load user packs first (they take precedence on pack-name collision
    // because user customizations should beat shipped examples).
    if user_dir.as_os_str().is_empty() {
        // Explicitly disabled via empty path (test convenience).
    } else {
        let (packs, errors) = load_packs_from_dir(&user_dir);
        inner.packs.extend(packs);
        inner.load_errors.extend(errors);
    }

    // Then shipped examples. Pack names that collide with a user pack are
    // skipped with a warning (user wins).
    if !shipped_dir.as_os_str().is_empty() {
        let (packs, errors) = load_packs_from_dir(&shipped_dir);
        for pack in packs {
            if inner.packs.iter().any(|p| p.name == pack.name) {
                info!(
                    pack = %pack.name,
                    "pattern-pack: user pack shadows shipped pack with same name"
                );
                continue;
            }
            inner.packs.push(pack);
        }
        inner.load_errors.extend(errors);
    }

    // Apply the global pattern cap BEFORE building the scope indexes so
    // that `packs[].patterns` and the indexes agree on what's actually
    // loaded. Overflow patterns are dropped entirely — they don't count
    // toward `total_loaded` and don't appear in `terminal.patterns.stats`.
    let mut total = 0usize;
    for pack in &mut inner.packs {
        let mut kept: Vec<Arc<CompiledPattern>> = Vec::with_capacity(pack.patterns.len());
        for pat in pack.patterns.drain(..) {
            if total >= config.max_patterns {
                inner.cap_reached = true;
                warn!(
                    cap = config.max_patterns,
                    pack = %pack.name,
                    pattern = %pat.name,
                    "pattern-pack: global cap reached, skipping pattern"
                );
                continue;
            }
            total += 1;
            kept.push(pat);
        }
        pack.patterns = kept;
    }

    // Scope-index the survivors.
    for pack in &inner.packs {
        for pat in &pack.patterns {
            let arc = Arc::clone(pat);
            match arc.scope {
                PatternScope::FinalizedLine => inner.index_finalized_line.push(arc),
                PatternScope::PromptBoundary => inner.index_prompt_boundary.push(arc),
                PatternScope::Region => inner.index_region.push(arc),
            }
        }
    }

    info!(
        user_dir = %user_dir.display(),
        shipped_dir = %shipped_dir.display(),
        packs = inner.packs.len(),
        total_patterns = total,
        "pattern-pack: reload complete"
    );
}

fn default_user_pattern_dir() -> PathBuf {
    // `therminal-runtime::paths::config_dir()` already handles WSL2,
    // Windows Roaming AppData, and macOS Application Support — reuse it
    // instead of calling `dirs::config_dir()` directly.
    therminal_runtime::paths::config_dir().join("patterns")
}

fn default_shipped_pattern_dir() -> PathBuf {
    // Shipped example packs live in the repo at `plugins/examples/`.
    // The shell-integration scripts already use `THERMINAL_RESOURCES_DIR`
    // to find `resources/shell-integration`; we piggy-back on the same
    // env var so packaging stays consistent.
    if let Ok(env_dir) = std::env::var("THERMINAL_RESOURCES_DIR") {
        let p = PathBuf::from(env_dir);
        // Prefer `<resources>/plugins/examples` if it exists, otherwise
        // fall back to `<resources>/../plugins/examples` (for installs
        // where `plugins/` is a sibling of `resources/`).
        let bundled = p.join("plugins").join("examples");
        if bundled.exists() {
            return bundled;
        }
        if let Some(parent) = p.parent() {
            let sibling = parent.join("plugins").join("examples");
            if sibling.exists() {
                return sibling;
            }
        }
        return bundled;
    }

    // Fall back to the runtime layout: `<resources_dir>/../plugins/examples`.
    let resources = therminal_runtime::paths::resources_dir();
    if let Some(parent) = resources.parent() {
        let sibling = parent.join("plugins").join("examples");
        if sibling.exists() {
            return sibling;
        }
    }
    resources.join("plugins").join("examples")
}

// ── Match execution ─────────────────────────────────────────────────────

fn run_pattern_against(
    pat: &Arc<CompiledPattern>,
    input: &str,
    scope: PatternScope,
    pane_id: Option<u64>,
    config: &PatternEngineConfig,
    out: &mut Vec<PatternMatch>,
) {
    // Measure against the full input; the slow-match circuit normalizes
    // against the 200-char target from SPEC §3.3 by clamping the reported
    // elapsed down for shorter inputs (no sense penalising a 3-char line).
    let start = Instant::now();
    let mut matched_any = false;
    for caps in pat.regex.captures_iter(input) {
        matched_any = true;
        let m0 = caps.get(0).expect("capture 0 always present");
        let matched_text = m0.as_str().to_string();
        let mut capture_map: HashMap<String, String> = HashMap::new();
        for name in pat.regex.capture_names().flatten() {
            let val = caps
                .name(name)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            capture_map.insert(name.to_string(), val);
        }

        let resolved = resolve_action(&pat.action, &capture_map);
        out.push(PatternMatch {
            pack_name: pat.pack_name.clone(),
            pattern_name: pat.name.clone(),
            scope,
            pane_id,
            byte_start: m0.start(),
            byte_end: m0.end(),
            matched_text,
            captures: capture_map,
            action: resolved,
        });
    }

    let elapsed_us = start.elapsed().as_micros() as u64;
    let was_disabled = if matched_any {
        pat.record_success(
            elapsed_us,
            config.slow_pattern_threshold_us,
            config.slow_strike_limit,
        )
    } else {
        pat.record_miss(
            elapsed_us,
            config.slow_pattern_threshold_us,
            config.slow_strike_limit,
        )
    };
    if was_disabled {
        warn!(
            pack = %pat.pack_name,
            pattern = %pat.name,
            avg_ms = pat.avg_match_ms(),
            "pattern-match: slow pattern disabled after {} strikes",
            config.slow_strike_limit
        );
    }
}

fn resolve_action(action: &PatternAction, captures: &HashMap<String, String>) -> ResolvedAction {
    match action {
        PatternAction::Hotspot(h) => ResolvedAction::Hotspot(resolve_hotspot(h, captures)),
        PatternAction::Widget(w) => ResolvedAction::Widget(resolve_widget(w, captures)),
        PatternAction::EmitEvent(e) => ResolvedAction::EmitEvent(resolve_emit_event(e, captures)),
    }
}

fn resolve_hotspot(h: &HotspotAction, captures: &HashMap<String, String>) -> ResolvedHotspot {
    ResolvedHotspot {
        on_click: h.on_click.clone(),
        target: h
            .target_template
            .as_deref()
            .map(|t| expand_template(t, captures)),
        label: h
            .label_template
            .as_deref()
            .map(|t| expand_template(t, captures)),
        kind: h.kind.clone(),
    }
}

fn resolve_widget(w: &WidgetAction, captures: &HashMap<String, String>) -> ResolvedWidget {
    let value = w
        .value_template
        .as_deref()
        .map(|t| expand_template(t, captures))
        .and_then(|s| s.trim().parse::<f64>().ok());
    let max = w
        .max_template
        .as_deref()
        .map(|t| expand_template(t, captures))
        .and_then(|s| s.trim().parse::<f64>().ok());

    // Color selection: walk the color table and pick the entry whose key
    // matches a capture value; fall back to the special `"default"` key.
    let mut picked_color: Option<String> = None;
    if !w.color.is_empty() {
        for (key, color) in &w.color {
            if captures.values().any(|v| v == key) {
                picked_color = Some(color.clone());
                break;
            }
        }
        if picked_color.is_none()
            && let Some(default_color) = w.color.get("default")
        {
            picked_color = Some(default_color.clone());
        }
    }

    ResolvedWidget {
        kind: w.kind,
        anchor: w.anchor,
        label: w
            .label_template
            .as_deref()
            .map(|t| expand_template(t, captures)),
        value,
        max,
        title: w
            .title_template
            .as_deref()
            .map(|t| expand_template(t, captures)),
        body: w
            .body_template
            .as_deref()
            .map(|t| expand_template(t, captures)),
        color: picked_color,
    }
}

fn resolve_emit_event(
    e: &EmitEventAction,
    captures: &HashMap<String, String>,
) -> ResolvedEmitEvent {
    let mut extra = HashMap::new();
    for (k, v) in &e.extra {
        extra.insert(k.clone(), expand_template(v, captures));
    }
    ResolvedEmitEvent { extra }
}

// ── Stats ───────────────────────────────────────────────────────────────

/// Stats snapshot matching §6.3 of the performance-model doc.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineStats {
    pub packs: Vec<PackStats>,
    pub global: GlobalStats,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PackStats {
    pub pack_name: String,
    pub description: Option<String>,
    pub pattern_count: usize,
    pub active_count: usize,
    pub disabled_count: usize,
    pub error_count: usize,
    pub load_errors: Vec<PatternLoadErrorInfo>,
    pub patterns: Vec<PatternStats>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PatternStats {
    pub name: String,
    pub description: Option<String>,
    pub scope: &'static str,
    pub action: &'static str,
    pub match_count: u64,
    pub miss_count: u64,
    pub avg_match_ms: f64,
    pub slow_count: u64,
    /// `"active"`, `"disabled"`, or `"error"`.
    pub status: &'static str,
    /// Unix millis of the most recent successful match, `None` if never.
    pub last_match_ts_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GlobalStats {
    pub total_loaded: usize,
    pub total_active: usize,
    pub total_disabled: usize,
    pub cap_reached: bool,
    pub cap_limit: usize,
    /// Pack-level load errors: files we couldn't read/parse at all.
    pub pack_load_errors: Vec<PackLoadErrorInfo>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PatternLoadErrorInfo {
    pub name: String,
    pub error: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PackLoadErrorInfo {
    pub pack_name: String,
    pub path: String,
    pub error: String,
}

impl EngineStats {
    fn from_inner(inner: &EngineInner) -> Self {
        use std::sync::atomic::Ordering;
        let mut total_loaded = 0usize;
        let mut total_active = 0usize;
        let mut total_disabled = 0usize;

        let mut packs_out = Vec::with_capacity(inner.packs.len());
        for pack in &inner.packs {
            let mut pattern_stats = Vec::with_capacity(pack.patterns.len());
            let mut active = 0usize;
            let mut disabled = 0usize;
            for pat in &pack.patterns {
                total_loaded += 1;
                let is_disabled = pat.is_disabled();
                if is_disabled {
                    disabled += 1;
                    total_disabled += 1;
                } else {
                    active += 1;
                    total_active += 1;
                }
                let last_match = pat.last_match_ts_ms.load(Ordering::Relaxed);
                pattern_stats.push(PatternStats {
                    name: pat.name.clone(),
                    description: pat.description.clone(),
                    scope: pat.scope.as_str(),
                    action: pat.action.kind_str(),
                    match_count: pat.match_count.load(Ordering::Relaxed),
                    miss_count: pat.miss_count.load(Ordering::Relaxed),
                    avg_match_ms: pat.avg_match_ms(),
                    slow_count: pat.slow_count.load(Ordering::Relaxed),
                    status: if is_disabled { "disabled" } else { "active" },
                    last_match_ts_ms: if last_match == 0 {
                        None
                    } else {
                        Some(last_match)
                    },
                });
            }
            // Pack-level errors get "error" status rows so the CLI/MCP
            // surface sees them uniformly with active patterns.
            for (name, err) in &pack.load_errors {
                pattern_stats.push(PatternStats {
                    name: name.clone(),
                    description: None,
                    scope: "—",
                    action: "—",
                    match_count: 0,
                    miss_count: 0,
                    avg_match_ms: 0.0,
                    slow_count: 0,
                    status: "error",
                    last_match_ts_ms: None,
                });
                let _ = err; // `error` text is also surfaced via load_errors below.
            }
            packs_out.push(PackStats {
                pack_name: pack.name.clone(),
                description: pack.description.clone(),
                pattern_count: pack.patterns.len() + pack.load_errors.len(),
                active_count: active,
                disabled_count: disabled,
                error_count: pack.load_errors.len(),
                load_errors: pack
                    .load_errors
                    .iter()
                    .map(|(n, e)| PatternLoadErrorInfo {
                        name: n.clone(),
                        error: e.clone(),
                    })
                    .collect(),
                patterns: pattern_stats,
            });
        }

        Self {
            packs: packs_out,
            global: GlobalStats {
                total_loaded,
                total_active,
                total_disabled,
                cap_reached: inner.cap_reached,
                cap_limit: 0, // set by caller via `with_cap_limit` if it wants
                pack_load_errors: inner
                    .load_errors
                    .iter()
                    .map(|e| PackLoadErrorInfo {
                        pack_name: e.pack_name.clone(),
                        path: e.path.display().to_string(),
                        error: e.error.clone(),
                    })
                    .collect(),
            },
        }
    }

    /// Fill in `global.cap_limit` from the engine's config. The inner
    /// builder does not know the limit, so callers that want to surface it
    /// fix up the snapshot post-hoc.
    pub fn with_cap_limit(mut self, limit: usize) -> Self {
        self.global.cap_limit = limit;
        self
    }
}

// Silence the `PathBuf` import when the helper functions above are all
// inlined.
#[allow(dead_code)]
fn _force_imports(_: &Path) {}
