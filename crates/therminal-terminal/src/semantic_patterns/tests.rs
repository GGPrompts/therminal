//! Engine-level integration and perf tests.

use std::path::PathBuf;
use std::time::Instant;

use super::*;

/// Build an engine rooted at a single user pattern directory. Shipped
/// packs are suppressed (empty path) so tests are hermetic.
fn engine_with_user_dir(dir: PathBuf) -> PatternEngine {
    PatternEngine::new(PatternEngineConfig {
        enabled: true,
        user_pattern_dir: Some(dir),
        shipped_pattern_dir: Some(PathBuf::new()),
        max_patterns: 500,
        slow_pattern_threshold_us: 1000,
        slow_strike_limit: 3,
    })
}

#[test]
fn process_finalized_line_runs_matching_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("rust-errs.toml"),
        r#"
pack_name = "rust-errs"
[[pattern]]
name = "compile-error"
match = 'error\[(?P<code>E\d+)\]: .+ --> (?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "{code} at {file}:{line}:{col}"
kind = "error"
"#,
    )
    .unwrap();

    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    let matches = engine.process_finalized_line(
        42,
        "error[E0432]: unresolved import --> src/lib.rs:1:5",
        None,
        None,
    );
    assert_eq!(matches.len(), 1);
    let m = &matches[0];
    assert_eq!(m.pack_name, "rust-errs");
    assert_eq!(m.pattern_name, "compile-error");
    assert_eq!(m.pane_id, Some(42));
    assert_eq!(m.captures.get("file"), Some(&"src/lib.rs".to_string()));
    assert_eq!(m.captures.get("line"), Some(&"1".to_string()));

    match &m.action {
        ResolvedAction::Hotspot(h) => {
            assert_eq!(h.on_click, HotspotOnClick::OpenEditor);
            assert_eq!(h.target.as_deref(), Some("src/lib.rs"));
            assert_eq!(h.label.as_deref(), Some("E0432 at src/lib.rs:1:5"));
            assert_eq!(h.kind, "error");
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn prompt_boundary_runs_against_full_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("test-badge.toml"),
        r#"
pack_name = "test-badge"
[[pattern]]
name = "cargo-result"
match = 'test result: (?P<status>FAILED|ok)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed'
scope = "prompt_boundary"
action = "widget"

[pattern.widget]
kind = "badge"
label = "{passed} passed / {failed} failed"
color.ok = "green"
color.FAILED = "red"
"#,
    )
    .unwrap();

    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    // `process_finalized_line` must NOT trigger a prompt_boundary pattern.
    let line_matches = engine.process_finalized_line(
        1,
        "test result: ok. 3 passed; 0 failed",
        None,
        Some("cargo"),
    );
    assert!(line_matches.is_empty());

    // `process_prompt_boundary` runs it.
    let transcript = "running 3 tests\nall good\ntest result: ok. 3 passed; 0 failed\n";
    let boundary_matches = engine.process_prompt_boundary(1, transcript, None, Some("cargo"));
    assert_eq!(boundary_matches.len(), 1);
    match &boundary_matches[0].action {
        ResolvedAction::Widget(w) => {
            assert_eq!(w.kind, WidgetKind::Badge);
            assert_eq!(w.label.as_deref(), Some("3 passed / 0 failed"));
            // `status` captured "ok" → picks `color.ok = "green"`.
            assert_eq!(w.color.as_deref(), Some("green"));
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn emit_event_action_expands_extra_templates() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("task.toml"),
        r#"
[[pattern]]
name = "task-complete"
match = '✓ (?P<task>.+) \((?P<duration>\d+\.\d+)s\)'
scope = "finalized_line"
action = "emit_event"

[pattern.emit_event]
extra.status = "done"
extra.summary = "Task {task} took {duration}s"
"#,
    )
    .unwrap();

    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    let matches = engine.process_finalized_line(7, "✓ build service (4.21s)", None, None);
    assert_eq!(matches.len(), 1);
    match &matches[0].action {
        ResolvedAction::EmitEvent(e) => {
            assert_eq!(e.extra.get("status").map(|s| s.as_str()), Some("done"));
            assert_eq!(
                e.extra.get("summary").map(|s| s.as_str()),
                Some("Task build service took 4.21s")
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

#[test]
fn harness_scoping_filters_pattern() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("claude-only.toml"),
        r#"
[[pattern]]
name = "claude-ctx"
match = 'Context: (?P<pct>\d+)%'
scope = "finalized_line"
action = "emit_event"
applies_to = "claude"
"#,
    )
    .unwrap();
    let engine = engine_with_user_dir(tmp.path().to_path_buf());

    // No harness active → no match.
    let none_matches = engine.process_finalized_line(1, "Context: 78%", None, None);
    assert!(none_matches.is_empty());
    // Wrong harness → no match.
    let wrong = engine.process_finalized_line(1, "Context: 78%", Some("codex"), None);
    assert!(wrong.is_empty());
    // Correct harness → matches.
    let ok = engine.process_finalized_line(1, "Context: 78%", Some("claude"), None);
    assert_eq!(ok.len(), 1);
    assert_eq!(ok[0].captures.get("pct"), Some(&"78".to_string()));
}

#[test]
fn command_scoping_filters_by_prefix() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("cargo-only.toml"),
        r#"
[[pattern]]
name = "warn"
match = 'warning'
scope = "finalized_line"
action = "emit_event"

[pattern.applies_to]
command = "cargo"
"#,
    )
    .unwrap();
    let engine = engine_with_user_dir(tmp.path().to_path_buf());

    // No current command → no match.
    assert!(
        engine
            .process_finalized_line(1, "got a warning here", None, None)
            .is_empty()
    );
    // Other command → no match.
    assert!(
        engine
            .process_finalized_line(1, "got a warning here", None, Some("make"))
            .is_empty()
    );
    // Cargo command → match.
    let m = engine.process_finalized_line(1, "got a warning here", None, Some("cargo build"));
    assert_eq!(m.len(), 1);
}

#[test]
fn region_scope_loads_and_runs() {
    // Region scope is reserved in v1 per SPEC §4.3 but the engine must
    // still load region-scoped patterns and run them when
    // `process_region` is called explicitly. This test ensures the wire
    // path is alive so downstream code can feed it from the region
    // indexer when the hook lands.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("r.toml"),
        r#"
[[pattern]]
name = "region-tag"
match = '(?s)TAG\s+(?P<name>\S+)'
scope = "region"
action = "emit_event"
"#,
    )
    .unwrap();
    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    let m = engine.process_region(1, "line one\nTAG foo\nline three", None, None);
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].captures.get("name"), Some(&"foo".to_string()));
}

#[test]
fn disabled_engine_returns_empty() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("x.toml"),
        r#"
[[pattern]]
name = "always"
match = "."
scope = "finalized_line"
action = "emit_event"
"#,
    )
    .unwrap();
    let engine = PatternEngine::new(PatternEngineConfig {
        enabled: false,
        user_pattern_dir: Some(tmp.path().to_path_buf()),
        shipped_pattern_dir: Some(PathBuf::new()),
        ..PatternEngineConfig::new_default()
    });
    // Disabled: nothing matches, even though the pack loaded.
    let m = engine.process_finalized_line(1, "any text", None, None);
    assert!(m.is_empty());
    // But stats still report the pack.
    let stats = engine.stats();
    assert_eq!(stats.packs.len(), 1);
    assert_eq!(stats.packs[0].pattern_count, 1);
}

#[test]
fn empty_stats_with_no_packs() {
    let engine = PatternEngine::new(PatternEngineConfig {
        enabled: true,
        user_pattern_dir: Some(PathBuf::new()),
        shipped_pattern_dir: Some(PathBuf::new()),
        ..PatternEngineConfig::new_default()
    });
    let stats = engine.stats();
    assert_eq!(stats.global.total_loaded, 0);
    assert!(stats.packs.is_empty());
}

#[test]
fn max_patterns_cap_is_enforced() {
    let tmp = tempfile::tempdir().unwrap();
    // 5 patterns in one pack, cap at 3.
    let mut s = String::new();
    s.push_str("pack_name = \"big\"\n");
    for i in 0..5 {
        s.push_str(&format!(
            r#"
[[pattern]]
name = "p{i}"
match = "{i}"
scope = "finalized_line"
action = "emit_event"
"#
        ));
    }
    std::fs::write(tmp.path().join("big.toml"), s).unwrap();
    let engine = PatternEngine::new(PatternEngineConfig {
        enabled: true,
        user_pattern_dir: Some(tmp.path().to_path_buf()),
        shipped_pattern_dir: Some(PathBuf::new()),
        max_patterns: 3,
        slow_pattern_threshold_us: 1000,
        slow_strike_limit: 3,
    });
    let stats = engine.stats();
    assert_eq!(stats.global.total_loaded, 3);
    assert!(stats.global.cap_reached);
}

#[test]
fn stats_record_successful_match() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("p.toml"),
        r#"
[[pattern]]
name = "digit"
match = '\d+'
scope = "finalized_line"
action = "emit_event"
"#,
    )
    .unwrap();
    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    engine.process_finalized_line(1, "abc 123 def", None, None);
    engine.process_finalized_line(1, "no digits here", None, None);
    let stats = engine.stats();
    let pat = stats
        .packs
        .iter()
        .flat_map(|p| p.patterns.iter())
        .find(|p| p.name == "digit")
        .unwrap();
    assert_eq!(pat.match_count, 1);
    assert_eq!(pat.miss_count, 1);
    assert!(pat.last_match_ts_ms.is_some());
}

#[test]
fn reload_picks_up_new_files() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    assert_eq!(engine.stats().global.total_loaded, 0);

    // Add a pack, reload.
    std::fs::write(
        tmp.path().join("late.toml"),
        r#"
[[pattern]]
name = "x"
match = "x"
scope = "finalized_line"
action = "emit_event"
"#,
    )
    .unwrap();
    engine.reload();
    assert_eq!(engine.stats().global.total_loaded, 1);
}

#[test]
fn widget_color_default_when_no_capture_matches() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("gauge.toml"),
        r##"
[[pattern]]
name = "ctx"
match = 'Context: (?P<pct>\d+)%'
scope = "finalized_line"
action = "widget"

[pattern.widget]
kind = "gauge"
value = "{pct}"
max = "100"
color.default = "#4a9eff"
"##,
    )
    .unwrap();
    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    let m = engine.process_finalized_line(1, "Context: 55%", None, None);
    assert_eq!(m.len(), 1);
    match &m[0].action {
        ResolvedAction::Widget(w) => {
            assert_eq!(w.value, Some(55.0));
            assert_eq!(w.max, Some(100.0));
            assert_eq!(w.color.as_deref(), Some("#4a9eff"));
        }
        _ => panic!(),
    }
}

// ── Shipped pack smoke test ──────────────────────────────────────────────

/// Resolve the in-repo `plugins/examples/` directory for tests.
///
/// `CARGO_MANIFEST_DIR` points at this crate's directory at build time;
/// walking up two levels lands at the workspace root, where `plugins/`
/// lives. This keeps the test hermetic — it never consults
/// `THERMINAL_RESOURCES_DIR`.
fn repo_plugins_examples_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("plugins")
        .join("examples")
}

#[test]
fn shipped_example_packs_load_cleanly() {
    let dir = repo_plugins_examples_dir();
    assert!(
        dir.exists(),
        "plugins/examples/ must exist at {}",
        dir.display()
    );

    let (packs, errors) = loader::load_packs_from_dir(&dir);
    assert!(
        errors.is_empty(),
        "pack load errors in plugins/examples/: {errors:?}"
    );
    assert!(
        packs.len() >= 3,
        "expected ≥3 shipped packs, got {} ({:?})",
        packs.len(),
        packs.iter().map(|p| &p.name).collect::<Vec<_>>()
    );

    // Every pattern in every pack must have compiled.
    for pack in &packs {
        assert!(
            pack.load_errors.is_empty(),
            "pack {} has per-pattern errors: {:?}",
            pack.name,
            pack.load_errors
        );
        assert!(
            !pack.patterns.is_empty(),
            "pack {} has zero patterns",
            pack.name
        );
    }

    // Required packs are present.
    let names: Vec<&str> = packs.iter().map(|p| p.name.as_str()).collect();
    for expected in ["cargo-errors", "claude-usage", "glossary"] {
        assert!(
            names.contains(&expected),
            "missing shipped pack {expected}: have {names:?}"
        );
    }
}

#[test]
fn cargo_errors_pack_matches_compile_error_line() {
    let dir = repo_plugins_examples_dir();
    let engine = PatternEngine::new(PatternEngineConfig {
        enabled: true,
        user_pattern_dir: Some(dir),
        shipped_pattern_dir: Some(PathBuf::new()),
        ..PatternEngineConfig::new_default()
    });
    let m = engine.process_finalized_line(
        1,
        "error[E0432]: unresolved import `foo` --> src/lib.rs:3:5",
        None,
        None,
    );
    assert!(
        m.iter().any(|m| m.pattern_name == "compile-error"),
        "expected compile-error match, got {m:?}"
    );
}

#[test]
fn claude_usage_pack_matches_statusline_with_harness_active() {
    let dir = repo_plugins_examples_dir();
    let engine = PatternEngine::new(PatternEngineConfig {
        enabled: true,
        user_pattern_dir: Some(dir),
        shipped_pattern_dir: Some(PathBuf::new()),
        ..PatternEngineConfig::new_default()
    });
    // Harness inactive: no match.
    let inactive = engine.process_finalized_line(1, "Context: 42%", None, None);
    assert!(inactive.is_empty());

    // Harness active: the gauge widget (42% falls in the green/low band) and
    // the emit_event rule fire. The pack has three colour-banded gauge
    // patterns; accept any of the three names so the test is not brittle if
    // thresholds shift.
    let active = engine.process_finalized_line(1, "Context: 42%", Some("claude"), None);
    let names: Vec<&str> = active.iter().map(|m| m.pattern_name.as_str()).collect();
    let has_gauge = names.iter().any(|n| n.starts_with("context-gauge"));
    assert!(has_gauge, "expected a context-gauge-* match, got {names:?}");
    assert!(names.contains(&"context-event"), "got {names:?}");
}

#[test]
fn glossary_pack_emits_event_for_acronym() {
    let dir = repo_plugins_examples_dir();
    let engine = PatternEngine::new(PatternEngineConfig {
        enabled: true,
        user_pattern_dir: Some(dir),
        shipped_pattern_dir: Some(PathBuf::new()),
        ..PatternEngineConfig::new_default()
    });
    let m = engine.process_finalized_line(1, "the MCP tool returned success", None, None);
    let mcp_match = m
        .iter()
        .find(|m| m.pattern_name == "mcp")
        .expect("glossary.mcp should match");
    match &mcp_match.action {
        ResolvedAction::EmitEvent(e) => {
            assert_eq!(e.extra.get("term").map(|s| s.as_str()), Some("MCP"));
            assert!(
                e.extra
                    .get("definition")
                    .map(|s| s.contains("Model Context Protocol"))
                    .unwrap_or(false)
            );
        }
        other => panic!("unexpected action: {other:?}"),
    }
}

// ── Skill example packs smoke test ──────────────────────────────────────

/// Resolve the in-repo `resources/skills/therminal-plugin/examples/` directory.
fn skill_plugin_examples_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .join("resources")
        .join("skills")
        .join("therminal-plugin")
        .join("examples")
}

#[test]
fn skill_example_packs_load_cleanly() {
    let dir = skill_plugin_examples_dir();
    assert!(
        dir.exists(),
        "resources/skills/therminal-plugin/examples/ must exist at {}",
        dir.display()
    );

    let (packs, errors) = loader::load_packs_from_dir(&dir);
    assert!(
        errors.is_empty(),
        "pack load errors in skill examples: {errors:?}"
    );
    assert!(
        packs.len() >= 3,
        "expected ≥3 skill example packs, got {} ({:?})",
        packs.len(),
        packs.iter().map(|p| &p.name).collect::<Vec<_>>()
    );

    // Every pattern in every pack must have compiled without error.
    for pack in &packs {
        assert!(
            pack.load_errors.is_empty(),
            "skill example pack {} has per-pattern errors: {:?}",
            pack.name,
            pack.load_errors
        );
        assert!(
            !pack.patterns.is_empty(),
            "skill example pack {} has zero patterns",
            pack.name
        );
    }

    // Required example packs must be present.
    let names: Vec<&str> = packs.iter().map(|p| p.name.as_str()).collect();
    for expected in ["cargo-errors", "test-badges", "glossary"] {
        assert!(
            names.contains(&expected),
            "missing skill example pack {expected}: have {names:?}"
        );
    }
}

// ── Perf test ────────────────────────────────────────────────────────────

/// SPEC perf target: 100 patterns × 1000 lines, bounded total time.
///
/// The task description allots 100ms total as "reasonable". On a modern
/// CPU the compiled regex crate typically runs each pattern in the
/// single-microsecond range on 200-char inputs, so 100 × 1000 = 100,000
/// matches should comfortably fit inside 100ms. If the clippy/CI box is
/// unusually slow we cap at 500ms as a hard failure.
#[test]
fn bounded_time_for_100_patterns_1000_lines() {
    let tmp = tempfile::tempdir().unwrap();
    // 100 similar-but-distinct patterns — each pattern is a lazy digit
    // grab so they all do real work on each line but never take
    // pathological time.
    let mut body = String::new();
    body.push_str("pack_name = \"perf\"\n");
    for i in 0..100 {
        body.push_str(&format!(
            r#"
[[pattern]]
name = "p-{i}"
match = 'event-{i}-(?P<n>\d+)'
scope = "finalized_line"
action = "emit_event"
"#
        ));
    }
    std::fs::write(tmp.path().join("perf.toml"), body).unwrap();
    let engine = engine_with_user_dir(tmp.path().to_path_buf());
    assert_eq!(engine.stats().global.total_loaded, 100);

    // Build 1000 lines. Half match, half don't — enough to exercise both
    // the hit and the miss paths through the metrics counter.
    let mut lines: Vec<String> = Vec::with_capacity(1000);
    for i in 0..1000 {
        if i % 2 == 0 {
            lines.push(format!(
                "[stderr] something happened: event-{}-42 in {} ms",
                i % 100,
                i
            ));
        } else {
            lines.push(format!("[stderr] unrelated log line number {i} here"));
        }
    }

    let start = Instant::now();
    for line in &lines {
        let _ = engine.process_finalized_line(1, line, None, None);
    }
    let elapsed = start.elapsed();

    eprintln!(
        "perf: 100 patterns × 1000 lines = {} matches in {:?} ({:.0} ns/check)",
        engine
            .stats()
            .packs
            .iter()
            .flat_map(|p| p.patterns.iter())
            .map(|p| p.match_count)
            .sum::<u64>(),
        elapsed,
        elapsed.as_nanos() as f64 / (100.0 * 1000.0),
    );

    // Hard cap: 500ms. The engine should comfortably beat this by an
    // order of magnitude on any modern CPU; the budget is loose so the
    // test is not flaky on slow CI boxes.
    assert!(
        elapsed.as_millis() < 500,
        "perf regression: 100 × 1000 took {elapsed:?}, budget 500ms"
    );
}
