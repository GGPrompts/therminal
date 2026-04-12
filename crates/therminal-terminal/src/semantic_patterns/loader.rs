//! Load pattern packs from TOML files on disk.
//!
//! Validates against the SPEC in `docs/pattern-matching-spec.md`. Errors
//! are non-fatal per §7: a bad pack is skipped, other packs continue
//! loading; a bad pattern within a pack is skipped, the rest of the pack
//! compiles normally.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use regex::Regex;
use tracing::{debug, warn};

use super::schema::{
    AppliesToToml, EmitEventActionToml, HotspotActionToml, PatternPackToml, PatternToml,
    WidgetActionToml,
};
use super::types::{
    AppliesTo, CompiledPack, CompiledPattern, EmitEventAction, HotspotAction, HotspotOnClick,
    PatternAction, PatternScope, WidgetAction, WidgetAnchor, WidgetKind,
};

// ── Limits from SPEC §4.2 + §7 ──────────────────────────────────────────

/// Maximum length of a single regex source string. SPEC §4.2.
const MAX_REGEX_LEN: usize = 4096;

/// A pack-level load error (file couldn't be parsed or a rule failed
/// validation). Surfaced via `terminal.patterns.stats`.
#[derive(Debug, Clone)]
pub struct PackLoadError {
    pub pack_name: String,
    pub path: PathBuf,
    pub error: String,
}

/// Validate a pack-or-pattern name against the `[a-z0-9_-]+` SPEC regex.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Load a single `.toml` file into a [`CompiledPack`]. Returns `Err` only
/// for fatal I/O or pack-level schema errors; per-pattern errors are
/// collected in `CompiledPack::load_errors`.
pub fn load_pack_from_file(path: &Path) -> Result<CompiledPack, PackLoadError> {
    let text = std::fs::read_to_string(path).map_err(|e| PackLoadError {
        pack_name: pack_name_from_path(path),
        path: path.to_path_buf(),
        error: format!("read error: {e}"),
    })?;
    let mut pack =
        load_pack_from_str(&text, &pack_name_from_path(path)).map_err(|e| PackLoadError {
            pack_name: pack_name_from_path(path),
            path: path.to_path_buf(),
            error: e,
        })?;
    debug!(
        path = %path.display(),
        pack = %pack.name,
        patterns = pack.patterns.len(),
        errors = pack.load_errors.len(),
        "loaded pattern pack"
    );
    // Sort deterministically so tests are stable and stats output is ordered.
    pack.patterns.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(pack)
}

/// Load a pack from raw TOML text. Used by both `load_pack_from_file` and
/// unit tests. `fallback_pack_name` is used when the TOML omits `pack_name`.
pub fn load_pack_from_str(text: &str, fallback_pack_name: &str) -> Result<CompiledPack, String> {
    let raw: PatternPackToml =
        toml::from_str(text).map_err(|e| format!("toml parse error: {e}"))?;

    let pack_name = match raw.pack_name {
        Some(name) => {
            if !is_valid_name(&name) {
                return Err(format!(
                    "invalid pack_name {name:?}: must match [a-z0-9_-]+"
                ));
            }
            name
        }
        None => {
            // Fallback names come from filenames via `pack_name_from_path`
            // which sanitizes to `[a-z0-9_-]+`, but defend against callers
            // that hand us a raw fallback (unit tests, future entry points).
            // A fallback that fails validation is a caller bug — fail loud
            // so shipped-vs-user shadowing stays deterministic.
            if !is_valid_name(fallback_pack_name) {
                return Err(format!(
                    "invalid fallback pack_name {fallback_pack_name:?}: must match [a-z0-9_-]+"
                ));
            }
            fallback_pack_name.to_string()
        }
    };

    let mut patterns: Vec<Arc<CompiledPattern>> = Vec::with_capacity(raw.patterns.len());
    let mut load_errors: Vec<(String, String)> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    for pat in raw.patterns {
        let name = pat.name.clone();
        match compile_one(&pack_name, pat, &mut seen_names) {
            Ok(compiled) => patterns.push(Arc::new(compiled)),
            Err(e) => {
                warn!(
                    pack = %pack_name,
                    pattern = %name,
                    error = %e,
                    "pattern-pack: skipping pattern"
                );
                load_errors.push((name, e));
            }
        }
    }

    Ok(CompiledPack {
        name: pack_name,
        description: raw.pack_description,
        patterns,
        load_errors,
    })
}

fn compile_one(
    pack_name: &str,
    pat: PatternToml,
    seen_names: &mut HashSet<String>,
) -> Result<CompiledPattern, String> {
    // Name validation and uniqueness.
    if !is_valid_name(&pat.name) {
        return Err(format!(
            "invalid pattern name {:?}: must match [a-z0-9_-]+",
            pat.name
        ));
    }
    if !seen_names.insert(pat.name.clone()) {
        return Err(format!("duplicate pattern name {:?} within pack", pat.name));
    }

    // Regex length + compile.
    if pat.match_regex.len() > MAX_REGEX_LEN {
        return Err(format!(
            "regex too long ({} > {MAX_REGEX_LEN} chars)",
            pat.match_regex.len()
        ));
    }
    let regex = Regex::new(&pat.match_regex).map_err(|e| format!("regex compile error: {e}"))?;

    // Scope + action.
    let scope =
        PatternScope::parse(&pat.scope).ok_or_else(|| format!("unknown scope {:?}", pat.scope))?;
    let action = compile_action(&pat)?;
    let applies_to = compile_applies_to(pat.applies_to)?;

    Ok(CompiledPattern {
        pack_name: pack_name.to_string(),
        name: pat.name,
        description: pat.description,
        regex,
        scope,
        applies_to,
        action,
        match_count: AtomicU64::new(0),
        miss_count: AtomicU64::new(0),
        total_match_us: AtomicU64::new(0),
        slow_count: AtomicU64::new(0),
        strike_count: AtomicU64::new(0),
        last_match_ts_ms: AtomicU64::new(0),
        disabled: AtomicBool::new(false),
    })
}

fn compile_applies_to(raw: Option<AppliesToToml>) -> Result<AppliesTo, String> {
    match raw {
        None => Ok(AppliesTo::Global),
        Some(AppliesToToml::Harness(h)) => {
            if h.is_empty() {
                return Err("applies_to cannot be empty string".into());
            }
            Ok(AppliesTo::Harness(h))
        }
        Some(AppliesToToml::Table(t)) => {
            let harness = t.harness.filter(|s| !s.is_empty());
            let command = t.command.filter(|s| !s.is_empty());
            if t.shell.is_some() {
                // Reserved for v2 — noted in SPEC §6.4.
                warn!("applies_to.shell is reserved for v2 and ignored in v1");
            }
            match (harness, command) {
                (None, None) => Err("applies_to table must set harness or command".into()),
                (Some(h), None) => Ok(AppliesTo::Harness(h)),
                (None, Some(c)) => Ok(AppliesTo::Command(c)),
                (Some(h), Some(c)) => Ok(AppliesTo::HarnessAndCommand {
                    harness: h,
                    command: c,
                }),
            }
        }
    }
}

fn compile_action(pat: &PatternToml) -> Result<PatternAction, String> {
    match pat.action.as_str() {
        "hotspot" => {
            let h = pat
                .hotspot
                .as_ref()
                .ok_or("action = \"hotspot\" but no [pattern.hotspot] sub-table")?;
            Ok(PatternAction::Hotspot(compile_hotspot(h)?))
        }
        "widget" => {
            let w = pat
                .widget
                .as_ref()
                .ok_or("action = \"widget\" but no [pattern.widget] sub-table")?;
            Ok(PatternAction::Widget(compile_widget(w)?))
        }
        "emit_event" => {
            let e = pat.emit_event.clone().unwrap_or_default();
            Ok(PatternAction::EmitEvent(compile_emit_event(e)))
        }
        other => Err(format!("unknown action {other:?}")),
    }
}

fn compile_hotspot(raw: &HotspotActionToml) -> Result<HotspotAction, String> {
    let on_click = match raw.on_click.as_str() {
        "open_editor" => HotspotOnClick::OpenEditor,
        "open_url" => HotspotOnClick::OpenUrl,
        "emit_event" => HotspotOnClick::EmitEvent,
        "run_command" => {
            return Err("on_click = \"run_command\" is not supported in v1 (SPEC §5.4)".into());
        }
        other => return Err(format!("unknown on_click {other:?}")),
    };

    // target is required for open_editor + open_url.
    match on_click {
        HotspotOnClick::OpenEditor | HotspotOnClick::OpenUrl => {
            if raw.target.as_deref().unwrap_or("").is_empty() {
                return Err(format!(
                    "on_click = {:?} requires target",
                    on_click.as_str()
                ));
            }
        }
        HotspotOnClick::EmitEvent => {}
    }

    Ok(HotspotAction {
        on_click,
        target_template: raw.target.clone(),
        label_template: raw.label.clone(),
        kind: raw.kind.clone().unwrap_or_else(|| "pattern".to_string()),
        highlight: raw.highlight.clone(),
    })
}

fn compile_widget(raw: &WidgetActionToml) -> Result<WidgetAction, String> {
    let kind = match raw.kind.as_str() {
        "badge" => WidgetKind::Badge,
        "gauge" => WidgetKind::Gauge,
        "sparkline" => WidgetKind::Sparkline,
        "card" => WidgetKind::Card,
        other => return Err(format!("unknown widget kind {other:?}")),
    };

    let anchor = match raw.anchor.as_deref() {
        None | Some("line_right") => WidgetAnchor::LineRight,
        Some("inline") => WidgetAnchor::Inline,
        Some("overlay") => WidgetAnchor::Overlay,
        Some(other) => return Err(format!("unknown widget anchor {other:?}")),
    };

    // Per-kind required fields.
    match kind {
        WidgetKind::Badge => {
            if raw.label.is_none() {
                return Err("widget kind = \"badge\" requires label".into());
            }
        }
        WidgetKind::Gauge | WidgetKind::Sparkline => {
            if raw.value.is_none() {
                return Err(format!("widget kind = {:?} requires value", kind.as_str()));
            }
        }
        WidgetKind::Card => {
            // Both `title` and `body` are optional per SPEC §5.2 but the
            // card is pointless without at least one of them.
            if raw.title.is_none() && raw.body.is_none() {
                return Err("widget kind = \"card\" requires title or body".into());
            }
        }
    }

    Ok(WidgetAction {
        kind,
        anchor,
        label_template: raw.label.clone(),
        value_template: raw.value.clone(),
        max_template: raw.max.clone(),
        title_template: raw.title.clone(),
        body_template: raw.body.clone(),
        color: raw.color.clone(),
    })
}

fn compile_emit_event(raw: EmitEventActionToml) -> EmitEventAction {
    EmitEventAction { extra: raw.extra }
}

/// Derive the fallback pack name from a file path.
///
/// Takes the file stem, lowercases it, and replaces any character outside
/// `[a-z0-9_-]` with `-` (then collapses consecutive `-` and trims leading/
/// trailing `-`) so the result is guaranteed to satisfy
/// [`is_valid_name`]. A stem that normalizes to the empty string falls back
/// to `"unknown"` so the loader always has a usable identifier.
///
/// This is the single normalization point for filename-derived pack names;
/// [`load_pack_from_str`] re-validates the result so a future caller that
/// bypasses this helper cannot sneak an invalid name through.
pub fn pack_name_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let lowered = stem.to_ascii_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_dash = false;
    for c in lowered.chars() {
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if keep {
            // Collapse runs of '-' to a single one.
            if c == '-' {
                if prev_dash {
                    continue;
                }
                prev_dash = true;
            } else {
                prev_dash = false;
            }
            out.push(c);
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }

    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Load every `*.toml` file from a directory (non-recursive).
///
/// Returns the successfully-loaded packs and a list of load errors. Missing
/// directories are reported as an empty result, not an error — a user who
/// has never created `~/.config/therminal/patterns/` should still get a
/// working engine.
pub fn load_packs_from_dir(dir: &Path) -> (Vec<CompiledPack>, Vec<PackLoadError>) {
    let mut packs: Vec<CompiledPack> = Vec::new();
    let mut errors: Vec<PackLoadError> = Vec::new();

    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(dir = %dir.display(), "pattern directory does not exist");
            return (packs, errors);
        }
        Err(e) => {
            errors.push(PackLoadError {
                pack_name: String::new(),
                path: dir.to_path_buf(),
                error: format!("readdir error: {e}"),
            });
            return (packs, errors);
        }
    };

    // Collect first, then sort by filename for deterministic order (the
    // SPEC §4.1 cap enforcement relies on alphabetical order).
    let mut entries: Vec<PathBuf> = read
        .filter_map(|res| res.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    entries.sort();

    let mut seen_pack_names: HashSet<String> = HashSet::new();

    for entry in entries {
        match load_pack_from_file(&entry) {
            Ok(pack) => {
                if !seen_pack_names.insert(pack.name.clone()) {
                    warn!(
                        pack = %pack.name,
                        path = %entry.display(),
                        "pattern-pack: duplicate pack_name, keeping first"
                    );
                    errors.push(PackLoadError {
                        pack_name: pack.name,
                        path: entry,
                        error: "duplicate pack_name".into(),
                    });
                    continue;
                }
                packs.push(pack);
            }
            Err(e) => errors.push(e),
        }
    }

    (packs, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn loads_minimal_pack() {
        let toml = r#"
[[pattern]]
name = "done-marker"
match = '^\s*DONE\s*$'
scope = "finalized_line"
action = "emit_event"
"#;
        let pack = load_pack_from_str(toml, "test").expect("pack loads");
        assert_eq!(pack.name, "test");
        assert_eq!(pack.patterns.len(), 1);
        assert_eq!(pack.load_errors.len(), 0);
        assert_eq!(pack.patterns[0].name, "done-marker");
    }

    #[test]
    fn rejects_invalid_name() {
        let toml = r#"
[[pattern]]
name = "Invalid Name"
match = "x"
scope = "finalized_line"
action = "emit_event"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack shell loads");
        assert_eq!(pack.patterns.len(), 0);
        assert_eq!(pack.load_errors.len(), 1);
    }

    #[test]
    fn rejects_bad_regex() {
        let toml = r#"
[[pattern]]
name = "bad"
match = "["
scope = "finalized_line"
action = "emit_event"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack shell loads");
        assert_eq!(pack.patterns.len(), 0);
        assert_eq!(pack.load_errors.len(), 1);
        assert!(pack.load_errors[0].1.contains("regex compile"));
    }

    #[test]
    fn duplicate_pattern_name_is_an_error_on_second() {
        let toml = r#"
[[pattern]]
name = "dup"
match = "x"
scope = "finalized_line"
action = "emit_event"

[[pattern]]
name = "dup"
match = "y"
scope = "finalized_line"
action = "emit_event"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack shell loads");
        assert_eq!(pack.patterns.len(), 1);
        assert_eq!(pack.load_errors.len(), 1);
    }

    #[test]
    fn hotspot_requires_subtable() {
        let toml = r#"
[[pattern]]
name = "no-subtable"
match = "x"
scope = "finalized_line"
action = "hotspot"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack shell loads");
        assert_eq!(pack.load_errors.len(), 1);
    }

    #[test]
    fn hotspot_open_editor_requires_target() {
        let toml = r#"
[[pattern]]
name = "no-target"
match = "x"
scope = "finalized_line"
action = "hotspot"
[pattern.hotspot]
on_click = "open_editor"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack shell loads");
        assert_eq!(pack.load_errors.len(), 1);
    }

    #[test]
    fn parses_full_hotspot_pattern() {
        let toml = r#"
[[pattern]]
name = "rust-err"
match = 'error\[(?P<code>E\d+)\]: .+ --> (?P<file>[^:]+):(?P<line>\d+)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "{code} at {file}:{line}"
kind = "error"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack loads");
        assert_eq!(pack.patterns.len(), 1);
        match &pack.patterns[0].action {
            PatternAction::Hotspot(h) => {
                assert_eq!(h.on_click, HotspotOnClick::OpenEditor);
                assert_eq!(h.target_template.as_deref(), Some("{file}"));
                assert_eq!(h.kind, "error");
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn parses_widget_gauge() {
        let toml = r##"
[[pattern]]
name = "ctx-gauge"
match = 'Context: (?P<pct>\d+)%'
scope = "finalized_line"
action = "widget"

[pattern.widget]
kind = "gauge"
value = "{pct}"
max = "100"
color.default = "#4a9eff"
"##;
        let pack = load_pack_from_str(toml, "t").expect("pack loads");
        match &pack.patterns[0].action {
            PatternAction::Widget(w) => {
                assert_eq!(w.kind, WidgetKind::Gauge);
                assert_eq!(w.anchor, WidgetAnchor::LineRight);
                assert_eq!(w.value_template.as_deref(), Some("{pct}"));
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }

    #[test]
    fn applies_to_command_table() {
        let toml = r#"
[[pattern]]
name = "in-cargo"
match = "warning"
scope = "finalized_line"
action = "emit_event"
[pattern.applies_to]
command = "cargo"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack loads");
        match &pack.patterns[0].applies_to {
            AppliesTo::Command(c) => assert_eq!(c, "cargo"),
            other => panic!("unexpected applies_to: {other:?}"),
        }
    }

    #[test]
    fn applies_to_harness_string() {
        let toml = r#"
[[pattern]]
name = "claude-only"
match = "x"
scope = "finalized_line"
action = "emit_event"
applies_to = "claude"
"#;
        let pack = load_pack_from_str(toml, "t").expect("pack loads");
        match &pack.patterns[0].applies_to {
            AppliesTo::Harness(h) => assert_eq!(h, "claude"),
            other => panic!("unexpected applies_to: {other:?}"),
        }
    }

    #[test]
    fn load_packs_from_nonexistent_dir_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("definitely-not-here");
        let (packs, errors) = load_packs_from_dir(&missing);
        assert!(packs.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn load_packs_from_dir_collects_files() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("aaa.toml");
        let b = tmp.path().join("bbb.toml");
        let c = tmp.path().join("ignored.txt");
        let minimal = r#"
[[pattern]]
name = "x"
match = "x"
scope = "finalized_line"
action = "emit_event"
"#;
        std::fs::write(&a, minimal).unwrap();
        std::fs::write(&b, minimal).unwrap();
        std::fs::write(&c, "not-a-pack").unwrap();
        let (packs, _) = load_packs_from_dir(tmp.path());
        assert_eq!(packs.len(), 2);
        let names: Vec<_> = packs.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["aaa".to_string(), "bbb".to_string()]);
    }

    #[test]
    fn duplicate_pack_name_across_files_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        // Both files declare the same `pack_name`; alphabetical order means
        // `a.toml` wins and `z.toml` is reported as an error.
        let body = r#"
pack_name = "shared"
[[pattern]]
name = "x"
match = "x"
scope = "finalized_line"
action = "emit_event"
"#;
        std::fs::write(tmp.path().join("a.toml"), body).unwrap();
        std::fs::write(tmp.path().join("z.toml"), body).unwrap();
        let (packs, errors) = load_packs_from_dir(tmp.path());
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].name, "shared");
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn regex_length_cap_is_enforced() {
        // 4096-byte hard cap: build a regex literal longer than that.
        let long_regex = "a".repeat(4097);
        let toml = format!(
            r#"
[[pattern]]
name = "toolong"
match = {long_regex:?}
scope = "finalized_line"
action = "emit_event"
"#,
        );
        let pack = load_pack_from_str(&toml, "t").expect("pack shell loads");
        assert_eq!(pack.load_errors.len(), 1);
        assert!(pack.load_errors[0].1.contains("regex too long"));
    }

    #[test]
    fn load_pack_from_file_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("demo.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"
pack_name = "demo"
[[pattern]]
name = "one"
match = "foo"
scope = "finalized_line"
action = "emit_event"
"#
        )
        .unwrap();
        let pack = load_pack_from_file(&path).unwrap();
        assert_eq!(pack.name, "demo");
        assert_eq!(pack.patterns.len(), 1);
    }

    // ── tn-gln6 #3: filename-derived fallback pack names ──────────────────

    #[test]
    fn pack_name_from_path_lowercases_stem() {
        assert_eq!(
            pack_name_from_path(Path::new("/tmp/Cargo-Errors.toml")),
            "cargo-errors"
        );
    }

    #[test]
    fn pack_name_from_path_replaces_invalid_chars_with_dash() {
        assert_eq!(
            pack_name_from_path(Path::new("/tmp/my.pack.name.toml")),
            "my-pack-name"
        );
        assert_eq!(
            pack_name_from_path(Path::new("/tmp/some pack.toml")),
            "some-pack"
        );
    }

    #[test]
    fn pack_name_from_path_collapses_and_trims_dashes() {
        // "--foo..bar--" → "foo-bar"
        assert_eq!(
            pack_name_from_path(Path::new("/tmp/--foo..bar--.toml")),
            "foo-bar"
        );
    }

    #[test]
    fn pack_name_from_path_unknown_on_all_invalid() {
        // All-invalid stems normalize to empty and fall back to "unknown".
        assert_eq!(pack_name_from_path(Path::new("/tmp/###.toml")), "unknown");
        assert_eq!(pack_name_from_path(Path::new("/tmp/!!!.toml")), "unknown");
    }

    #[test]
    fn fallback_name_always_passes_is_valid_name() {
        // Every normalized result must round-trip through is_valid_name.
        for raw in [
            "Simple.toml",
            "With Spaces.toml",
            "UPPER_CASE.toml",
            "my.pack.name.toml",
            "weird#chars!.toml",
            "123-numeric.toml",
        ] {
            let name = pack_name_from_path(Path::new(raw));
            assert!(
                is_valid_name(&name),
                "normalized name {name:?} from {raw:?} failed is_valid_name"
            );
        }
    }

    #[test]
    fn load_pack_rejects_unnormalized_fallback_name() {
        // If a caller hands load_pack_from_str a raw (unnormalized)
        // fallback with invalid chars, it must fail hard — not silently
        // load a pack with an invalid id that would break collision logic.
        let toml = "[[patterns]]\nname = \"x\"\nmatch = \"y\"\nscope = \"finalized_line\"\naction = \"emit_event\"\n";
        let err = load_pack_from_str(toml, "Not Normalized").unwrap_err();
        assert!(
            err.contains("invalid fallback pack_name"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn load_pack_accepts_normalized_fallback_name() {
        let toml = "[[patterns]]\nname = \"x\"\nmatch = \"y\"\nscope = \"finalized_line\"\naction = \"emit_event\"\n";
        let pack = load_pack_from_str(toml, "cargo-errors").expect("pack loads");
        assert_eq!(pack.name, "cargo-errors");
    }

    #[test]
    fn load_pack_from_file_normalizes_mixed_case_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Cargo-Errors.toml");
        std::fs::write(
            &path,
            r#"
[[patterns]]
name = "err"
match = "error"
scope = "finalized_line"
action = "emit_event"
"#,
        )
        .unwrap();

        let pack = load_pack_from_file(&path).expect("pack loads");
        assert_eq!(
            pack.name, "cargo-errors",
            "mixed-case filename stem must normalize to lowercase"
        );
    }
}
