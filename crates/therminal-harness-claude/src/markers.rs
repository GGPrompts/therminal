//! Claude Code OSC marker handler.
//!
//! This module is the Claude harness's entry into the shared OSC handler
//! registry (`docs/osc-handler-registry.md`). It claims **OSC 1341** at
//! daemon startup and parses a minimal `key=value;…` marker grammar into
//! [`HarnessEvent`]s for the unified event bus.
//!
//! # Why a dedicated crate module?
//!
//! The Claude harness has historically relied on the JSONL tailer
//! (`~/.claude/projects/`) and the hook state files (`/tmp/claude-code-state/`)
//! to infer what a Claude Code session is doing. Both sources are
//! *cooperative file reads*: they only work if the Claude Code CLI writes
//! the files, and they introduce a polling delay of ~150 ms.
//!
//! OSC markers are additive. When Claude Code starts emitting them inline
//! with PTY output, the daemon picks them up synchronously on the PTY
//! reader thread, no poller required. This gives us sub-millisecond
//! latency for state changes and survives environments where the hook
//! scripts cannot write to `/tmp` (e.g. read-only sandboxes).
//!
//! The JSONL tailer and state poller remain the authoritative source for
//! historical data and capacity metrics — see `CLAUDE.md` for the full
//! architecture. OSC markers are **live signal**, not a replacement for
//! stored state.
//!
//! # Grammar (v0)
//!
//! ```text
//! ESC ] 1341 ; key=value [ ; key=value ]* ST
//! ```
//!
//! Recognised keys:
//!
//! | Key      | Value               | Meaning                             |
//! |----------|---------------------|-------------------------------------|
//! | `state`  | idle / processing / tool_use / awaiting_input | Claude session status |
//! | `tool`   | string              | Tool name (if `state=tool_use`)     |
//! | `session_id` | string          | Claude session UUID                 |
//!
//! Unknown keys are preserved as-is in the emitted event body (for
//! forward-compatibility) but do not change the event `kind`.
//!
//! The event `kind` is always `claude.state` for v0. Future grammar
//! extensions may add additional kinds (`claude.tool_call`,
//! `claude.thinking_started`, …). The `body` carries `{ "state": …,
//! "tool": …, "session_id": …, "extra": { … } }`.
//!
//! # Registration
//!
//! The daemon calls [`activate`] once at startup on the shared
//! [`OscHandlerRegistry`]. Registration is idempotent on the registry
//! level (a duplicate claim fails at startup) but [`activate`] itself is
//! not — call it exactly once per daemon process.
//!
//! [`HarnessEvent`]: therminal_terminal::HarnessEvent
//! [`OscHandlerRegistry`]: therminal_terminal::OscHandlerRegistry

use serde_json::{Map, Value};

use therminal_terminal::osc_registry::{
    HarnessEvent, HarnessOscHandler, OscHandlerRegistry, OscRegistrationError,
};

/// OSC code claimed by the Claude harness for inline marker emission.
///
/// Also listed in `docs/osc-code-registry.md`. Do not reuse this code for
/// any other purpose — the registry enforces exclusive ownership at
/// daemon startup.
pub const CLAUDE_OSC_CODE: u16 = 1341;

/// Owner identifier for Claude-emitted events.
///
/// Used as the `source_id` in [`TaggedHarnessEvent`]s produced by this
/// handler, and as the second argument to `register_osc_handler`. Must
/// match the `source_id` that downstream event-bus consumers filter on.
///
/// [`TaggedHarnessEvent`]: therminal_terminal::TaggedHarnessEvent
pub const CLAUDE_OWNER: &str = "claude";

/// Event `kind` string emitted for OSC 1341 state markers.
///
/// Namespaced under `claude.` following the cross-surface vocabulary from
/// `docs/event-bus-kinds.md`. v0 only emits this single kind; follow-up
/// grammar extensions will add `claude.tool_call`, `claude.thinking_*`, etc.
pub const CLAUDE_STATE_KIND: &str = "claude.state";

/// Claim OSC 1341 on the shared registry and install the Claude marker
/// handler.
///
/// Returns `Err` only if OSC 1341 is already claimed (programming mistake:
/// two `activate` calls, or another harness crate racing for the same
/// code). Callers should `.expect()` the result at daemon startup so a
/// conflict fails fast rather than silently losing events.
///
/// The handler is dormant by design: it parses any incoming OSC 1341
/// sequence but produces no side effects beyond emitting a
/// [`HarnessEvent`] onto the dispatcher. If Claude Code is not running,
/// no OSC 1341 sequences arrive and the handler is never called.
pub fn activate(registry: &OscHandlerRegistry) -> Result<(), OscRegistrationError> {
    registry.register(CLAUDE_OSC_CODE, CLAUDE_OWNER, build_handler())
}

/// Build the OSC 1341 handler closure in isolation from registry wiring.
///
/// Factored out so the unit tests below can exercise the parser without
/// standing up a full registry.
pub fn build_handler() -> HarnessOscHandler {
    Box::new(|params: &[&[u8]]| parse_osc_1341(params))
}

/// Parse a VTE `params` slice for an OSC 1341 sequence.
///
/// `params[0]` is the decimal OSC code (always `b"1341"` on dispatch;
/// verified here defensively so future changes cannot accidentally route
/// a different code through this parser). `params[1..]` are the
/// semicolon-delimited `key=value` chunks.
///
/// Returns `None` on any of:
///
/// - `params[0]` is not `"1341"` (defense in depth),
/// - no `key=value` chunks were supplied,
/// - every chunk is malformed (no `=` separator).
///
/// Individual malformed chunks are skipped silently: a marker with one
/// good chunk and two garbage chunks still emits an event covering the
/// good chunk.
fn parse_osc_1341(params: &[&[u8]]) -> Option<HarnessEvent> {
    let code = params.first()?;
    if *code != b"1341" {
        return None;
    }
    if params.len() < 2 {
        return None;
    }

    let mut state: Option<String> = None;
    let mut tool: Option<String> = None;
    let mut session_id: Option<String> = None;
    let mut extra: Map<String, Value> = Map::new();

    for chunk in &params[1..] {
        let Ok(s) = std::str::from_utf8(chunk) else {
            continue;
        };
        let Some((key, value)) = s.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            continue;
        }

        match key {
            "state" => state = Some(value.to_string()),
            "tool" => tool = Some(value.to_string()),
            "session_id" => session_id = Some(value.to_string()),
            _ => {
                extra.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
    }

    // If the marker carried nothing we recognised and no extras either,
    // there is nothing worth forwarding onto the bus.
    if state.is_none() && tool.is_none() && session_id.is_none() && extra.is_empty() {
        return None;
    }

    let mut body = Map::new();
    if let Some(s) = state {
        body.insert("state".to_string(), Value::String(s));
    }
    if let Some(t) = tool {
        body.insert("tool".to_string(), Value::String(t));
    }
    if let Some(sid) = session_id {
        body.insert("session_id".to_string(), Value::String(sid));
    }
    if !extra.is_empty() {
        body.insert("extra".to_string(), Value::Object(extra));
    }

    Some(HarnessEvent {
        kind: CLAUDE_STATE_KIND.to_string(),
        body: Value::Object(body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_state_and_tool() {
        let params: &[&[u8]] = &[b"1341", b"state=tool_use", b"tool=Edit"];
        let event = parse_osc_1341(params).expect("event");
        assert_eq!(event.kind, CLAUDE_STATE_KIND);
        assert_eq!(
            event.body,
            serde_json::json!({
                "state": "tool_use",
                "tool": "Edit",
            })
        );
    }

    #[test]
    fn parses_state_only() {
        let params: &[&[u8]] = &[b"1341", b"state=idle"];
        let event = parse_osc_1341(params).expect("event");
        assert_eq!(event.body, serde_json::json!({ "state": "idle" }));
    }

    #[test]
    fn unknown_keys_move_to_extra() {
        let params: &[&[u8]] = &[b"1341", b"state=thinking", b"custom=foo", b"another=bar"];
        let event = parse_osc_1341(params).expect("event");
        assert_eq!(
            event.body,
            serde_json::json!({
                "state": "thinking",
                "extra": { "custom": "foo", "another": "bar" },
            })
        );
    }

    #[test]
    fn malformed_chunks_are_skipped() {
        let params: &[&[u8]] = &[b"1341", b"garbage", b"state=idle", b""];
        let event = parse_osc_1341(params).expect("event");
        assert_eq!(event.body, serde_json::json!({ "state": "idle" }));
    }

    #[test]
    fn empty_marker_returns_none() {
        assert!(parse_osc_1341(&[b"1341"]).is_none());
        assert!(parse_osc_1341(&[b"1341", b"garbage"]).is_none());
        assert!(parse_osc_1341(&[b"1341", b"=novalue"]).is_none());
    }

    #[test]
    fn wrong_code_returns_none() {
        // Defensive: if a future routing bug hands the wrong code to
        // this parser it should return None rather than mis-reporting.
        assert!(parse_osc_1341(&[b"1342", b"state=idle"]).is_none());
    }

    #[test]
    fn activate_claims_1341() {
        let registry = OscHandlerRegistry::new();
        activate(&registry).expect("first activation");
        assert_eq!(registry.owner_of(CLAUDE_OSC_CODE), Some(CLAUDE_OWNER));
        // Second activation fails with DuplicateCode.
        let err = activate(&registry).unwrap_err();
        assert!(matches!(err, OscRegistrationError::DuplicateCode { .. }));
    }

    #[test]
    fn registered_handler_dispatches_end_to_end() {
        let registry = OscHandlerRegistry::new();
        activate(&registry).expect("activate");
        let tagged = registry
            .dispatch(&[b"1341", b"state=tool_use", b"tool=Bash"])
            .expect("dispatch");
        assert_eq!(tagged.source_id, CLAUDE_OWNER);
        assert_eq!(tagged.event.kind, CLAUDE_STATE_KIND);
        assert_eq!(
            tagged.event.body,
            serde_json::json!({
                "state": "tool_use",
                "tool": "Bash",
            })
        );
    }

    #[test]
    fn state_with_session_id() {
        let params: &[&[u8]] = &[b"1341", b"state=processing", b"session_id=abc-123-def"];
        let event = parse_osc_1341(params).expect("event");
        assert_eq!(
            event.body,
            serde_json::json!({
                "state": "processing",
                "session_id": "abc-123-def",
            })
        );
    }
}
