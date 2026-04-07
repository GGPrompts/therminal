//! Output cadence analysis for distinguishing human vs agent output.

use std::collections::VecDeque;
use std::time::Instant;

/// Statistics for a single chunk of PTY output, used for cadence analysis.
#[derive(Debug, Clone)]
pub struct ByteChunkStats {
    /// When this chunk was received.
    pub timestamp: Instant,
    /// Number of bytes in the chunk.
    pub byte_count: usize,
    /// Whether the chunk contained backspace (0x08) or DEL (0x7F).
    pub has_backspace: bool,
    /// Whether the chunk contained CSI sequences for cursor movement.
    pub has_cursor_control: bool,
    /// Number of visible (non-control) characters after ANSI stripping.
    pub visible_chars: usize,
}

/// Classification of the output stream cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputCadence {
    /// Human typing: small chunks, moderate intervals, backspaces present.
    Human,
    /// Agent output: large chunks, rapid intervals, no backspaces, sustained.
    Agent,
    /// Burst output (e.g., `cat` a file, compiler output): large but not sustained.
    Burst,
    /// Not enough data to classify.
    Unknown,
}

/// Maximum number of chunk stats to keep in the sliding window.
pub(crate) const MAX_CHUNK_STATS: usize = 20;

/// Minimum number of chunks required before classification (below this -> Unknown).
pub(crate) const MIN_CHUNKS_FOR_CLASSIFICATION: usize = 3;

/// Minimum sustained duration (in seconds) to distinguish Agent from Burst.
pub(crate) const AGENT_SUSTAINED_SECS: f64 = 2.0;

/// Classify the output stream cadence from the given chunk stats window.
pub(crate) fn classify_output_cadence(stats: &VecDeque<ByteChunkStats>) -> OutputCadence {
    if stats.len() < MIN_CHUNKS_FOR_CLASSIFICATION {
        return OutputCadence::Unknown;
    }

    // Compute averages over the window.
    let total_visible: usize = stats.iter().map(|s| s.visible_chars).sum();
    let avg_visible = total_visible as f64 / stats.len() as f64;

    let backspace_count = stats.iter().filter(|s| s.has_backspace).count();
    let backspace_ratio = backspace_count as f64 / stats.len() as f64;

    // Compute average inter-chunk interval (in milliseconds).
    let intervals: Vec<f64> = stats
        .iter()
        .zip(stats.iter().skip(1))
        .map(|(a, b)| b.timestamp.duration_since(a.timestamp).as_secs_f64() * 1000.0)
        .collect();
    let avg_interval_ms = if intervals.is_empty() {
        0.0
    } else {
        intervals.iter().sum::<f64>() / intervals.len() as f64
    };

    // Compute total window duration.
    let window_duration_secs = if let (Some(front), Some(back)) = (stats.front(), stats.back()) {
        back.timestamp.duration_since(front.timestamp).as_secs_f64()
    } else {
        0.0
    };

    // Classification logic:
    //
    // Human: small chunks, moderate intervals, backspaces common.
    if avg_visible < 5.0 && avg_interval_ms > 30.0 && backspace_ratio > 0.01 {
        return OutputCadence::Human;
    }

    // Also classify as Human if chunks are tiny and intervals are human-speed,
    // even without backspaces (careful typer).
    if avg_visible < 5.0 && avg_interval_ms > 50.0 {
        return OutputCadence::Human;
    }

    // Burst: very large chunks but not sustained (short window or few chunks).
    let max_visible = stats.iter().map(|s| s.visible_chars).max().unwrap_or(0);
    if max_visible > 1000 && (stats.len() <= 3 || window_duration_secs < AGENT_SUSTAINED_SECS) {
        return OutputCadence::Burst;
    }

    // Agent: large chunks, rapid intervals, no backspaces, sustained.
    if avg_visible > 50.0
        && avg_interval_ms < 10.0
        && backspace_ratio < 0.01
        && window_duration_secs >= AGENT_SUSTAINED_SECS
    {
        return OutputCadence::Agent;
    }

    // Agent (relaxed): sustained large output even with moderate intervals.
    // With a 20-chunk window, reaching 2s requires ~105ms+ intervals.
    if avg_visible > 30.0
        && avg_interval_ms < 200.0
        && backspace_ratio < 0.01
        && window_duration_secs >= AGENT_SUSTAINED_SECS
    {
        return OutputCadence::Agent;
    }

    // Burst: large output, short lived.
    if avg_visible > 50.0 && window_duration_secs < AGENT_SUSTAINED_SECS {
        return OutputCadence::Burst;
    }

    OutputCadence::Unknown
}

/// Check if recent output looks like a spinner pattern.
///
/// Spinners are characterized by cursor-control-heavy output with low
/// visible text content -- the terminal is being rewritten in place.
pub(crate) fn is_spinner_pattern(stats: &VecDeque<ByteChunkStats>) -> bool {
    if stats.len() < MIN_CHUNKS_FOR_CLASSIFICATION {
        return false;
    }

    // Look at the most recent chunks (up to 10).
    let recent: Vec<&ByteChunkStats> = stats.iter().rev().take(10).collect();

    let cursor_control_count = recent.iter().filter(|s| s.has_cursor_control).count();
    let cursor_control_ratio = cursor_control_count as f64 / recent.len() as f64;

    let avg_visible =
        recent.iter().map(|s| s.visible_chars).sum::<usize>() as f64 / recent.len() as f64;

    // Spinner: high ratio of cursor control with low visible text per chunk.
    // Typical spinner: overwrites a line with CSI sequences, writes 1-5 chars.
    cursor_control_ratio > 0.5 && avg_visible < 20.0
}

/// Check if recent output is a sustained high-throughput stream.
///
/// Streaming is characterized by >500 visible chars/sec sustained over
/// at least 2 seconds with no backspaces -- e.g., an agent writing a
/// long code block or explanation.
pub(crate) fn is_streaming_cadence(stats: &VecDeque<ByteChunkStats>) -> bool {
    if stats.len() < MIN_CHUNKS_FOR_CLASSIFICATION {
        return false;
    }

    let window_duration_secs = if let (Some(front), Some(back)) = (stats.front(), stats.back()) {
        back.timestamp.duration_since(front.timestamp).as_secs_f64()
    } else {
        return false;
    };

    if window_duration_secs < AGENT_SUSTAINED_SECS {
        return false;
    }

    let total_visible: usize = stats.iter().map(|s| s.visible_chars).sum();
    let chars_per_sec = total_visible as f64 / window_duration_secs;

    let has_backspace = stats.iter().any(|s| s.has_backspace);

    chars_per_sec > 500.0 && !has_backspace
}
