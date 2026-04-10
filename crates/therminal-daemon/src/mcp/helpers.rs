//! Shared helper functions used across MCP tool handlers and resource handlers.
//!
//! Free functions that don't depend on `TherminalMcpServer` state live here
//! to keep `mod.rs` focused on the server struct, `ServerHandler` impl, and
//! tests.

use rmcp::ErrorData;
use rmcp::model::Content;
use rmcp::service::{RequestContext, RoleServer};
use serde::Serialize;

use crate::session::SessionManager;
use crate::trust::AgentIdentity;

// ── Helper: serialize to JSON content ───────────────────────────────────

pub(crate) fn json_content<T: Serialize>(value: &T) -> Result<Content, ErrorData> {
    Content::json(value)
        .map_err(|e| ErrorData::internal_error(format!("serialization error: {e}"), None))
}

pub(crate) fn parse_args<T: serde::de::DeserializeOwned>(
    args: serde_json::Map<String, serde_json::Value>,
) -> Result<T, ErrorData> {
    serde_json::from_value(serde_json::Value::Object(args))
        .map_err(|e| ErrorData::invalid_params(format!("invalid parameters: {e}"), None))
}

// ── Agent identity extraction ──────────────────────────────────────────

/// Extract the agent identity from the MCP connection context.
///
/// Uses the client's `Implementation.name` from the MCP `initialize`
/// handshake. Falls back to `"unknown"` if the peer info is not available
/// (e.g. before initialization completes).
pub(crate) fn extract_agent_identity(context: &RequestContext<RoleServer>) -> AgentIdentity {
    let name = context
        .peer
        .peer_info()
        .map(|info| info.client_info.name.clone())
        .unwrap_or_else(|| "unknown".to_string());
    AgentIdentity { name }
}

// ── Shared helpers used across tools/resources ──────────────────────────

/// Build a content preview string from a region's metadata (first 200 chars).
pub(crate) fn build_content_preview(region: &therminal_terminal::region_index::Region) -> String {
    // Prefer "command" metadata, then "cwd", then concatenate all metadata values.
    let raw = if let Some(cmd) = region.metadata.get("command") {
        cmd.clone()
    } else if let Some(cwd) = region.metadata.get("cwd") {
        cwd.clone()
    } else if region.metadata.is_empty() {
        String::new()
    } else {
        region
            .metadata
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if raw.len() > 200 {
        format!("{}...", &raw[..197])
    } else {
        raw
    }
}

/// Split a file path like `src/main.rs:42:5` into (`src/main.rs`, `:42:5`).
pub(crate) fn split_file_path_parts(text: &str) -> (&str, &str) {
    if let Some(idx) = text.find(':')
        && text[idx + 1..].starts_with(|c: char| c.is_ascii_digit())
    {
        return (&text[..idx], &text[idx..]);
    }
    (text, "")
}

/// Render the visible grid of a `PaneSnapshot` as plain-text lines.
///
/// When `trim_trailing_whitespace` is true (the default for tn-sp3n),
/// trailing spaces / NBSPs / control whitespace are stripped from every
/// row — empty rows become `""` instead of 80+ spaces. This is the
/// cache-churn fix the conductor relies on.
///
/// When `compact` is true, fully-whitespace rows are dropped entirely
/// (implies trimming).
pub(crate) fn render_grid_lines(
    snap: &crate::session::PaneSnapshot,
    trim_trailing_whitespace: bool,
    compact: bool,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(snap.grid.len());
    for row in &snap.grid {
        let raw: String = row.iter().map(|(ch, _)| ch).collect();
        let trimmed = if trim_trailing_whitespace || compact {
            raw.trim_end().to_string()
        } else {
            raw
        };
        if compact && trimmed.trim().is_empty() {
            continue;
        }
        out.push(trimmed);
    }
    out
}

/// Stable 64-bit hash of a `PaneSnapshot`'s visible grid as hex.
///
/// We use `std::hash::DefaultHasher` (SipHash-1-3 in current libstd) which
/// is hash-DoS resistant and zero-dep. This is purely a polling-cache key,
/// not a cryptographic checksum, so collision resistance only needs to be
/// "good enough that consecutive snapshots that differ in any cell get
/// different hashes."
///
/// The hash includes cursor position so a pane with identical visible
/// glyphs but a moved cursor still produces a different value (a common
/// idle/streaming distinction for spinner-style agents).
pub(crate) fn pane_content_hash(snap: &crate::session::PaneSnapshot) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    snap.cols.hash(&mut hasher);
    snap.rows.hash(&mut hasher);
    snap.cursor_col.hash(&mut hasher);
    snap.cursor_line.hash(&mut hasher);
    for row in &snap.grid {
        for (ch, bold) in row {
            (*ch as u32).hash(&mut hasher);
            bold.hash(&mut hasher);
        }
        // Row separator so [["a"], ["b"]] != [["ab"]].
        0u32.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Wall-clock now in Unix seconds (saturating to 0 on the impossible
/// pre-epoch case).
pub(crate) fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Helpers for pane lookup ─────────────────────────────────────────────

/// Find (session_id, cols, rows) for a pane by ID across all sessions.
pub(crate) fn find_pane_info(mgr: &SessionManager, pane_id: u64) -> Option<(u64, u16, u16)> {
    for (session_id, session) in mgr.iter_sessions() {
        for window in &session.windows {
            if let Some(pane) = window.pane(pane_id) {
                return Some((*session_id, pane.cols(), pane.rows()));
            }
        }
    }
    None
}

/// Find (pane_id, cols, rows) of the first pane in a session.
pub(crate) fn find_first_pane_in_session(
    mgr: &SessionManager,
    session_id: u64,
) -> Option<(u64, u16, u16)> {
    for (sid, session) in mgr.iter_sessions() {
        if *sid == session_id
            && let Some(window) = session.windows.first()
            && let Some(pane) = window.panes.first()
        {
            return Some((pane.id, pane.cols(), pane.rows()));
        }
    }
    None
}
