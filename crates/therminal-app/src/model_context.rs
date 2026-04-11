//! Model → context-window lookup for the pane-header context gauge (tn-nhbv).
//!
//! The OSC 7777 `AgentReport` sequence carries a free-form `model=` field
//! that cooperative agents (Claude Code, Codex, etc.) set to whatever
//! identifier they use internally. This module maps those identifiers to
//! their total context-window size so the pane header can render a
//! fill-ratio bar (green / yellow / red).
//!
//! This is intentionally a tiny starter table — 5 to 15 entries is enough.
//! **Update as new models ship.** Unknown models return `None` and the
//! gauge gracefully disappears for that pane.
//!
//! Matching is exact-string first, then a longest-prefix fallback so that
//! variants like `claude-opus-4-6[1m]` or `claude-opus-4-6-20251001` both
//! resolve to the base family. Keep entries sorted long-to-short within a
//! family so the prefix scan picks the most specific match first.

/// Gauge color thresholds, shared by renderer + unit tests.
pub const GAUGE_YELLOW_THRESHOLD: f32 = 0.50;
pub const GAUGE_RED_THRESHOLD: f32 = 0.80;

/// Color tier for a given fill ratio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GaugeTier {
    Green,
    Yellow,
    Red,
}

/// Bucket a fill ratio into green/yellow/red. Clamps ratios < 0 to green
/// and ratios > 1 to red so pathological inputs never panic.
pub fn gauge_tier(ratio: f32) -> GaugeTier {
    if !(ratio.is_finite()) || ratio < GAUGE_YELLOW_THRESHOLD {
        GaugeTier::Green
    } else if ratio < GAUGE_RED_THRESHOLD {
        GaugeTier::Yellow
    } else {
        GaugeTier::Red
    }
}

/// Known model → context window (tokens). Prefix-matched, longest-first
/// within each family. Extend as new models ship.
const MODEL_CONTEXT_WINDOWS: &[(&str, u64)] = &[
    // Anthropic — Claude 4.6 (1M context is the default)
    ("claude-opus-4-6", 1_000_000),
    ("claude-sonnet-4-6", 1_000_000),
    ("claude-haiku-4-6", 200_000),
    // Anthropic — Claude 4.5
    ("claude-opus-4-5", 200_000),
    ("claude-sonnet-4-5", 1_000_000),
    ("claude-haiku-4-5", 200_000),
    // Anthropic — Claude 3.7 / 3.5 legacy
    ("claude-3-7-sonnet", 200_000),
    ("claude-3-5-sonnet", 200_000),
    ("claude-3-5-haiku", 200_000),
    // OpenAI
    ("gpt-4o", 128_000),
    ("gpt-4-turbo", 128_000),
    ("o1-mini", 128_000),
    ("o1", 200_000),
];

/// Look up the context window (in tokens) for a model identifier.
///
/// Returns `None` for unknown models. Matching rules:
/// 1. Case-insensitive.
/// 2. Exact match wins.
/// 3. Otherwise, the longest table entry that is a prefix of the input
///    wins. This handles variants like `claude-opus-4-6[1m]` and date
///    suffixes like `claude-opus-4-6-20251001`.
pub fn context_window_for_model(model: &str) -> Option<u64> {
    let needle = model.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }

    // Exact match first.
    for (name, window) in MODEL_CONTEXT_WINDOWS {
        if *name == needle.as_str() {
            return Some(*window);
        }
    }

    // Longest-prefix fallback.
    let mut best: Option<(&str, u64)> = None;
    for (name, window) in MODEL_CONTEXT_WINDOWS {
        if needle.starts_with(name) && best.map(|(n, _)| name.len() > n.len()).unwrap_or(true) {
            best = Some((name, *window));
        }
    }
    best.map(|(_, w)| w)
}

/// Compute the fill ratio for a gauge, or `None` when either input is
/// missing or the window is zero. The returned ratio is **not** clamped
/// so callers can distinguish "over quota" from "near quota" if desired.
pub fn fill_ratio(tokens: u64, window: u64) -> Option<f32> {
    if window == 0 {
        return None;
    }
    Some(tokens as f32 / window as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_anthropic_opus_4_6_is_1m() {
        assert_eq!(context_window_for_model("claude-opus-4-6"), Some(1_000_000));
    }

    #[test]
    fn anthropic_opus_4_6_1m_variant_resolves() {
        // Exact model ID form emitted by some harnesses.
        assert_eq!(
            context_window_for_model("claude-opus-4-6[1m]"),
            Some(1_000_000)
        );
    }

    #[test]
    fn anthropic_date_suffix_resolves_to_family() {
        assert_eq!(
            context_window_for_model("claude-sonnet-4-6-20260101"),
            Some(1_000_000)
        );
    }

    #[test]
    fn anthropic_opus_4_5_is_200k() {
        assert_eq!(context_window_for_model("claude-opus-4-5"), Some(200_000));
    }

    #[test]
    fn openai_gpt_4o_is_128k() {
        assert_eq!(context_window_for_model("gpt-4o"), Some(128_000));
    }

    #[test]
    fn openai_gpt_4o_date_variant_resolves() {
        assert_eq!(context_window_for_model("gpt-4o-2024-05-13"), Some(128_000));
    }

    #[test]
    fn openai_o1_mini_beats_o1_prefix() {
        // Longest-prefix match: "o1-mini" (128k) must beat "o1" (200k).
        assert_eq!(context_window_for_model("o1-mini"), Some(128_000));
        assert_eq!(context_window_for_model("o1-mini-20241009"), Some(128_000));
    }

    #[test]
    fn unknown_model_returns_none() {
        assert_eq!(context_window_for_model("llama-3-1234"), None);
        assert_eq!(context_window_for_model("mistral-large"), None);
        assert_eq!(context_window_for_model(""), None);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(context_window_for_model("Claude-Opus-4-6"), Some(1_000_000));
        assert_eq!(context_window_for_model("GPT-4O"), Some(128_000));
    }

    #[test]
    fn gauge_tier_thresholds() {
        assert_eq!(gauge_tier(0.0), GaugeTier::Green);
        assert_eq!(gauge_tier(0.49), GaugeTier::Green);
        assert_eq!(gauge_tier(0.50), GaugeTier::Yellow);
        assert_eq!(gauge_tier(0.79), GaugeTier::Yellow);
        assert_eq!(gauge_tier(0.80), GaugeTier::Red);
        assert_eq!(gauge_tier(1.0), GaugeTier::Red);
        assert_eq!(gauge_tier(2.5), GaugeTier::Red);
    }

    #[test]
    fn gauge_tier_handles_non_finite() {
        // NaN → Green (graceful no-op tier choice).
        assert_eq!(gauge_tier(f32::NAN), GaugeTier::Green);
    }

    #[test]
    fn fill_ratio_zero_window_is_none() {
        assert_eq!(fill_ratio(1000, 0), None);
    }

    #[test]
    fn fill_ratio_half_full() {
        let r = fill_ratio(500_000, 1_000_000).unwrap();
        assert!((r - 0.5).abs() < 1e-6);
    }
}
