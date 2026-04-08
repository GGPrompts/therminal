//! End-to-end integration test for the Claude OSC marker handler.
//!
//! Proves the full chain works:
//!
//! 1. Build a shared [`OscHandlerRegistry`].
//! 2. Run the Claude harness's [`activate_markers`] on it (as the daemon
//!    does in `ensure.rs`).
//! 3. Construct a [`TherminalInterceptor`] wired to the same registry +
//!    a harness-event sink channel.
//! 4. Feed a synthetic OSC 1341 sequence through the interceptor via its
//!    [`dispatch_osc`] convenience wrapper.
//! 5. Assert a `TaggedHarnessEvent` arrives on the sink with the correct
//!    owner, kind, and body shape.
//!
//! This mirrors the production wiring without standing up the daemon,
//! PTYs, or the event bus — just the `TherminalInterceptor` ↔ registry
//! ↔ harness handler path.
//!
//! [`dispatch_osc`]: therminal_terminal::interceptor::TherminalInterceptor::dispatch_osc

use std::sync::Arc;
use std::sync::mpsc;

use therminal_harness_claude::{
    CLAUDE_OSC_CODE, CLAUDE_OWNER, CLAUDE_STATE_KIND, activate_markers,
};
use therminal_terminal::OscHandlerRegistry;
use therminal_terminal::interceptor::TherminalInterceptor;

/// Build a slice of `&[u8]` views from an array of byte slices of any
/// lengths. Using this helper sidesteps the "array with a size of N"
/// coercion error that arises when array literals of differing lengths
/// would otherwise need to unify.
fn params<const N: usize>(chunks: [&[u8]; N]) -> [&[u8]; N] {
    chunks
}

#[test]
fn claude_marker_flows_through_interceptor_to_harness_sink() {
    // 1. Build the shared registry and activate Claude's OSC claim.
    let registry = Arc::new(OscHandlerRegistry::new());
    activate_markers(&registry).expect("activate_markers");
    assert_eq!(registry.owner_of(CLAUDE_OSC_CODE), Some(CLAUDE_OWNER));

    // 2. Build an interceptor, install the shared registry, and attach a
    //    harness-event sink so dispatched events land on a channel we can
    //    read from the test thread.
    let (mut interceptor, _intercepted_rx) = TherminalInterceptor::with_defaults();
    interceptor.set_osc_registry(Arc::clone(&registry));
    let (harness_tx, harness_rx) = mpsc::channel();
    interceptor.set_harness_event_sink(harness_tx);

    // 3. Dispatch a synthetic `state=tool_use tool=Edit session_id=abc`
    //    marker through the interceptor. This is the exact entry point
    //    the PTY reader thread uses in production (via the
    //    `SequenceInterceptor` trait impl).
    let full_marker = params([
        &b"1341"[..],
        &b"state=tool_use"[..],
        &b"tool=Edit"[..],
        &b"session_id=abc-123"[..],
    ]);
    let consumed = interceptor.dispatch_osc(&full_marker, true);
    assert!(consumed, "interceptor should consume the OSC 1341 sequence");

    // 4. Assert the harness event arrived on the sink with the right
    //    shape.
    let tagged = harness_rx.try_recv().expect("harness event emitted");
    assert_eq!(tagged.source_id, CLAUDE_OWNER);
    assert_eq!(tagged.event.kind, CLAUDE_STATE_KIND);
    assert_eq!(
        tagged.event.body,
        serde_json::json!({
            "state": "tool_use",
            "tool": "Edit",
            "session_id": "abc-123",
        })
    );

    // 5. A second identical marker should produce a second event — the
    //    handler is reusable, not single-shot.
    let consumed = interceptor.dispatch_osc(&full_marker, true);
    assert!(consumed);
    let tagged2 = harness_rx.try_recv().expect("second event");
    assert_eq!(tagged2.source_id, CLAUDE_OWNER);
}

#[test]
fn claude_marker_for_unknown_state_is_still_forwarded() {
    let registry = Arc::new(OscHandlerRegistry::new());
    activate_markers(&registry).expect("activate_markers");

    let (mut interceptor, _) = TherminalInterceptor::with_defaults();
    interceptor.set_osc_registry(Arc::clone(&registry));
    let (harness_tx, harness_rx) = mpsc::channel();
    interceptor.set_harness_event_sink(harness_tx);

    // Unknown state values are forwarded as-is — the harness layer is
    // intentionally permissive so forward-compatible state names do not
    // silently drop.
    let marker = params([&b"1341"[..], &b"state=some_future_state"[..]]);
    assert!(interceptor.dispatch_osc(&marker, true));

    let tagged = harness_rx.try_recv().expect("event");
    assert_eq!(
        tagged.event.body,
        serde_json::json!({ "state": "some_future_state" })
    );
}

#[test]
fn two_harness_codes_dispatch_independently_through_one_interceptor() {
    // Prove the registry supports multiple concurrent claims by adding
    // a synthetic "codex" handler alongside the real Claude claim.
    let registry = Arc::new(OscHandlerRegistry::new());
    activate_markers(&registry).expect("activate claude");
    registry
        .register(
            1342,
            "codex",
            Box::new(|params| {
                let payload = params
                    .get(1)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("")
                    .to_string();
                Some(therminal_terminal::HarnessEvent {
                    kind: "codex.state".to_string(),
                    body: serde_json::json!({ "payload": payload }),
                })
            }),
        )
        .expect("register codex");

    let (mut interceptor, _) = TherminalInterceptor::with_defaults();
    interceptor.set_osc_registry(Arc::clone(&registry));
    let (harness_tx, harness_rx) = mpsc::channel();
    interceptor.set_harness_event_sink(harness_tx);

    let claude_marker = params([&b"1341"[..], &b"state=idle"[..]]);
    let codex_marker = params([&b"1342"[..], &b"state=busy"[..]]);
    assert!(interceptor.dispatch_osc(&claude_marker, true));
    assert!(interceptor.dispatch_osc(&codex_marker, true));

    let first = harness_rx.try_recv().expect("first event");
    let second = harness_rx.try_recv().expect("second event");
    assert_eq!(first.source_id, "claude");
    assert_eq!(first.event.kind, "claude.state");
    assert_eq!(second.source_id, "codex");
    assert_eq!(second.event.kind, "codex.state");
    assert_eq!(
        second.event.body,
        serde_json::json!({ "payload": "state=busy" })
    );
}
