//! OSC handler registry — the hook that lets harness crates claim OSC codes
//! and install typed parsers without hardcoding into `therminal-terminal`.
//!
//! See `docs/osc-handler-registry.md` for the normative specification and
//! `docs/osc-code-registry.md` for the canonical table of claimed codes.
//!
//! # Architecture
//!
//! The registry is owned by the daemon and shared across every
//! [`TherminalInterceptor`] as `Arc<OscHandlerRegistry>`. Handlers are
//! installed **once at daemon startup** via
//! [`OscHandlerRegistry::register`] (or the convenience
//! `TherminalInterceptor::register_osc_handler` method that delegates to this
//! type). The registry uses interior mutability (`RwLock`) so the `Arc` can
//! be cloned freely into each pane's interceptor while harness crates mutate
//! the handler map at startup time through a shared reference.
//!
//! # Panic safety
//!
//! Every handler invocation is wrapped in [`std::panic::catch_unwind`]. On the
//! first panic the handler is replaced with a no-op closure and marked
//! `disabled` for the rest of the process lifetime. Subsequent OSC sequences
//! routed to that code return `None` silently without invoking the stored
//! no-op.
//!
//! # Dispatch output
//!
//! The registry's `dispatch` method returns `Option<TaggedHarnessEvent>` —
//! a pairing of the owner string (injected by the dispatcher, never supplied
//! by the handler itself) and the handler's [`HarnessEvent`]. The caller
//! (the interceptor) is responsible for forwarding the tagged event onto
//! whatever sink the daemon has wired up (today: a `std::sync::mpsc` channel
//! on the interceptor; tomorrow: the unified event bus from tn-xula).

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, trace, warn};

/// Upper bound for the reserved-code range, exclusive.
///
/// Codes `0..RESERVED_CODE_MAX` are owned by therminal core and widely-used
/// standard OSCs (OSC 7, 9, 133, 633, 1337, 7777). Harness crates must claim
/// codes `>= RESERVED_CODE_MAX`.
pub const RESERVED_CODE_MAX: u16 = 1024;

/// Boxed closure type for a registered OSC handler.
///
/// Handlers receive the raw VTE `params` slice (the same layout that
/// `SequenceInterceptor::intercept_osc` gets: `params[0]` is the stringified
/// OSC code and `params[1..]` are the semicolon-delimited payload chunks).
/// They return `Some(HarnessEvent)` to forward an event onto the bus, or
/// `None` to drop the sequence silently (malformed payload, dormant harness,
/// etc.).
pub type HarnessOscHandler = Box<dyn Fn(&[&[u8]]) -> Option<HarnessEvent> + Send + Sync + 'static>;

/// An event produced by a harness OSC handler, to be forwarded to the event
/// bus.
///
/// Harness handlers return `Option<HarnessEvent>`. `Some` means "forward this
/// event to the bus". `None` means "drop silently" (e.g. the harness is
/// dormant, or the payload was malformed but non-fatal).
///
/// The dispatcher translates `HarnessEvent` into a `TerminalEvent` (see
/// `docs/event-bus-spec.md`) by injecting `source_class = "harness"` and
/// `source_id` from the registration's `owner` field. Harness handlers do not
/// set `source_id` themselves.
///
/// `body` must be a `serde_json::Value::Object`. The dispatcher will replace
/// a non-object body with an empty object and log a warning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessEvent {
    /// Event kind string. Lowercase with dots for namespacing.
    ///
    /// Use the cross-surface vocabulary from `docs/event-bus-kinds.md` where
    /// applicable (e.g. `"tool_call"`, `"agent_state"`, `"done"`). Source-
    /// specific kinds are allowed with a namespace prefix (e.g.
    /// `"claude.thinking_started"`).
    pub kind: String,

    /// Arbitrary JSON object carrying the harness-specific payload.
    ///
    /// Recommended limit: 4 KB. Hard cap enforced by the event bus: 64 KB.
    /// See `docs/event-bus-spec.md` §1 for body size semantics.
    pub body: serde_json::Value,
}

/// A [`HarnessEvent`] paired with the owner string from its registration.
///
/// The registry injects `source_id` from the registration's `owner` field
/// so a misbehaving handler cannot forge a different source. Downstream
/// event-bus plumbing will translate this into a `TerminalEvent` with
/// `source_class = "harness"` and `source_id = owner`.
///
/// `pane_id` is `None` when the event originates from the registry dispatch
/// (the registry has no pane context). The interceptor stamps the pane_id
/// before forwarding to the drain thread so the daemon can route marker
/// events into the correct `PaneCapacityCache` entry without PID resolution.
#[derive(Debug, Clone)]
pub struct TaggedHarnessEvent {
    /// Stable identifier for the harness crate that produced this event.
    ///
    /// Guaranteed to match the `owner` string supplied at registration time.
    pub source_id: &'static str,
    /// The event returned by the handler.
    pub event: HarnessEvent,
    /// Pane that produced this event. `None` when emitted by the registry
    /// dispatch (no pane context); stamped by the interceptor before
    /// forwarding to the daemon-side drain thread.
    pub pane_id: Option<u64>,
}

/// Errors returned by [`OscHandlerRegistry::register`] (and the forwarding
/// `TherminalInterceptor::register_osc_handler` method).
///
/// Both variants represent programming mistakes that must be caught at daemon
/// startup, not silently handled. The caller (harness crate activation
/// function) should `.expect()` the result so the daemon fails fast with a
/// clear message.
///
/// See `docs/osc-handler-registry.md` §3.1 for full error semantics.
#[derive(Debug, Error)]
pub enum OscRegistrationError {
    /// Two harness crates attempted to claim the same OSC code.
    ///
    /// Resolve by consulting `docs/osc-code-registry.md` and choosing a
    /// different code. The daemon will not start while this conflict exists.
    #[error(
        "OSC code {code} is already claimed by \"{existing_owner}\"; \
         \"{new_owner}\" cannot also claim it — \
         see docs/osc-code-registry.md to resolve the conflict"
    )]
    DuplicateCode {
        /// The contested OSC code.
        code: u16,
        /// The `owner` string from the first (winning) registration.
        existing_owner: &'static str,
        /// The `owner` string from the second (rejected) registration.
        new_owner: &'static str,
    },

    /// Harness crate attempted to claim a code in the reserved range `0–1023`.
    ///
    /// Codes in this range are reserved for therminal core (OSC 7, 9, 133,
    /// 633, 1337, 7777) and widely-used standard OSCs. Harness crates must
    /// use codes `≥ 1024`.
    #[error(
        "OSC code {code} is in the reserved range 0–1023 (therminal core + \
         standard OSCs); harness crates must claim codes ≥ 1024"
    )]
    ReservedCode {
        /// The rejected OSC code.
        code: u16,
    },

    /// Harness crate attempted to register using an invalid owner string.
    ///
    /// `owner` must be a lowercase `[a-z0-9_-]+` identifier matching the
    /// crate name without the `therminal-harness-` prefix. Empty strings,
    /// uppercase characters, whitespace, and punctuation are all rejected.
    #[error(
        "OSC owner identifier {owner:?} is invalid: \
         must be a non-empty lowercase [a-z0-9_-]+ string"
    )]
    InvalidOwner {
        /// The rejected owner string.
        owner: &'static str,
    },
}

/// Internal entry stored per claimed OSC code.
///
/// The handler is wrapped in a `RwLock` so it can be swapped out for a no-op
/// closure on first panic without invalidating outstanding references into
/// the map. `disabled` is an independent flag so introspection can tell the
/// difference between "live" and "panicked and replaced".
struct RegistryEntry {
    owner: &'static str,
    handler: RwLock<HarnessOscHandler>,
    disabled: RwLock<bool>,
}

/// Shared OSC handler registry.
///
/// Constructed once per daemon process and shared into every
/// [`TherminalInterceptor`] as `Arc<OscHandlerRegistry>`. Registration happens
/// at daemon startup through each harness crate's `activate(…)` function
/// before the first PTY is opened. After startup the registry is effectively
/// read-only; no v1 API lets handlers be removed or re-registered.
///
/// See `docs/osc-handler-registry.md` for the full specification.
pub struct OscHandlerRegistry {
    /// `code -> entry`. Wrapped in a `RwLock` so `register` can take the
    /// write lock at startup and `dispatch` the read lock on the hot path.
    entries: RwLock<HashMap<u16, RegistryEntry>>,
}

impl Default for OscHandlerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OscHandlerRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Claim an OSC code and install a typed handler.
    ///
    /// Returns [`OscRegistrationError::DuplicateCode`] if `code` is already
    /// claimed, [`OscRegistrationError::ReservedCode`] if `code` is in the
    /// reserved range `0..1024`, and [`OscRegistrationError::InvalidOwner`]
    /// if `owner` is not a non-empty lowercase `[a-z0-9_-]+` identifier.
    ///
    /// The spec requires harness crates to call this exactly once per code
    /// at daemon startup; the returned `Result` should be `.expect()`-ed so
    /// the daemon fails fast on conflicts.
    pub fn register(
        &self,
        code: u16,
        owner: &'static str,
        handler: HarnessOscHandler,
    ) -> Result<(), OscRegistrationError> {
        if code < RESERVED_CODE_MAX {
            return Err(OscRegistrationError::ReservedCode { code });
        }
        if !is_valid_owner(owner) {
            return Err(OscRegistrationError::InvalidOwner { owner });
        }

        let mut map = self.entries.write().expect("OSC registry poisoned");
        if let Some(existing) = map.get(&code) {
            return Err(OscRegistrationError::DuplicateCode {
                code,
                existing_owner: existing.owner,
                new_owner: owner,
            });
        }

        map.insert(
            code,
            RegistryEntry {
                owner,
                handler: RwLock::new(handler),
                disabled: RwLock::new(false),
            },
        );
        debug!(code, owner, "OSC handler registered");
        Ok(())
    }

    /// Look up the owner of a claimed code, if any.
    ///
    /// Returns `None` if no handler is registered for `code`. Exposed so
    /// daemon-side introspection (diagnostics, `terminal.agents.*` tooling)
    /// can report which harness owns a given code without calling the
    /// handler itself.
    pub fn owner_of(&self, code: u16) -> Option<&'static str> {
        let map = self.entries.read().expect("OSC registry poisoned");
        map.get(&code).map(|entry| entry.owner)
    }

    /// Return true if the registry has a handler registered for `code`
    /// (whether or not it is currently disabled).
    pub fn contains(&self, code: u16) -> bool {
        let map = self.entries.read().expect("OSC registry poisoned");
        map.contains_key(&code)
    }

    /// Return true if the handler for `code` has been disabled by a panic
    /// and is no longer being invoked.
    pub fn is_disabled(&self, code: u16) -> bool {
        let map = self.entries.read().expect("OSC registry poisoned");
        map.get(&code)
            .is_some_and(|entry| *entry.disabled.read().expect("OSC registry poisoned"))
    }

    /// Dispatch an OSC sequence through the registered handler, if any.
    ///
    /// Returns `Some(TaggedHarnessEvent)` if a handler was registered for
    /// the code in `params[0]` and produced an event. Returns `None` if:
    ///
    /// - no handler is registered for this code,
    /// - the registered handler returned `None`,
    /// - the handler has been disabled by an earlier panic, or
    /// - the handler panicked on this invocation (which also disables it for
    ///   the rest of the process lifetime).
    ///
    /// Handler invocations are wrapped in [`catch_unwind`]; on the first
    /// panic the stored handler is replaced with a no-op and logged at
    /// `warn` level.
    pub fn dispatch(&self, params: &[&[u8]]) -> Option<TaggedHarnessEvent> {
        let code = osc_code_from_params(params)?;

        // Phase 1: read-locked lookup + invocation. We hold only the
        // top-level read lock across the handler call so multiple panes can
        // dispatch in parallel. `handler` is itself behind a per-entry
        // `RwLock` so we take its read lock for the closure call — a panic
        // inside the closure unwinds through `catch_unwind`, not the lock,
        // so the per-entry RwLock is never poisoned by handler bugs.
        let map = self.entries.read().expect("OSC registry poisoned");
        let entry = map.get(&code)?;

        // Short-circuit disabled entries without allocating, logging, or
        // taking the per-entry handler lock.
        if *entry.disabled.read().expect("OSC registry poisoned") {
            trace!(code, owner = entry.owner, "OSC handler disabled; skipping");
            return None;
        }

        let owner = entry.owner;
        let result = {
            let handler = entry.handler.read().expect("OSC registry poisoned");
            catch_unwind(AssertUnwindSafe(|| handler(params)))
        };

        match result {
            Ok(Some(event)) => Some(TaggedHarnessEvent {
                source_id: owner,
                event,
                pane_id: None,
            }),
            Ok(None) => {
                trace!(code, owner, "OSC handler returned None");
                None
            }
            Err(panic_payload) => {
                let panic_msg = panic_message(&*panic_payload);
                warn!(
                    code,
                    owner,
                    panic = %panic_msg,
                    "OSC handler panicked; replacing with no-op and marking disabled"
                );
                // Replace the handler with a no-op and flip the disabled
                // flag. We intentionally hold only the per-entry write locks
                // here; the top-level `entries` read lock is already held
                // for this call.
                *entry.handler.write().expect("OSC registry poisoned") = Box::new(|_| None);
                *entry.disabled.write().expect("OSC registry poisoned") = true;
                None
            }
        }
    }
}

/// Extract the decimal OSC code from `params[0]`, if it is present and
/// well-formed.
fn osc_code_from_params(params: &[&[u8]]) -> Option<u16> {
    let code_bytes = params.first()?;
    let s = std::str::from_utf8(code_bytes).ok()?;
    s.parse::<u16>().ok()
}

/// Check that `owner` matches the `[a-z0-9_-]+` identifier grammar.
fn is_valid_owner(owner: &str) -> bool {
    if owner.is_empty() {
        return false;
    }
    owner
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Best-effort extraction of a human-readable message from a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

// -- Tests --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_handler(kind: &'static str) -> HarnessOscHandler {
        Box::new(move |params: &[&[u8]]| {
            // Echo the first payload chunk back as a `body.payload` field so
            // tests can distinguish which handler was invoked.
            let payload = params
                .get(1)
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();
            Some(HarnessEvent {
                kind: kind.to_string(),
                body: serde_json::json!({ "payload": payload }),
            })
        })
    }

    #[test]
    fn register_and_dispatch_single_handler() {
        let reg = OscHandlerRegistry::new();
        reg.register(1341, "claude", make_handler("claude.state"))
            .expect("register");

        let params: &[&[u8]] = &[b"1341", b"state=tool_use"];
        let tagged = reg.dispatch(params).expect("dispatch");
        assert_eq!(tagged.source_id, "claude");
        assert_eq!(tagged.event.kind, "claude.state");
        assert_eq!(
            tagged.event.body,
            serde_json::json!({ "payload": "state=tool_use" })
        );
    }

    #[test]
    fn register_and_dispatch_two_handlers_are_independent() {
        let reg = OscHandlerRegistry::new();
        reg.register(1341, "claude", make_handler("claude.state"))
            .expect("register claude");
        reg.register(1342, "codex", make_handler("codex.state"))
            .expect("register codex");

        let a = reg.dispatch(&[b"1341", b"a"]).expect("claude dispatch");
        let b = reg.dispatch(&[b"1342", b"b"]).expect("codex dispatch");

        assert_eq!(a.source_id, "claude");
        assert_eq!(a.event.kind, "claude.state");
        assert_eq!(b.source_id, "codex");
        assert_eq!(b.event.kind, "codex.state");
    }

    #[test]
    fn duplicate_registration_returns_error() {
        let reg = OscHandlerRegistry::new();
        reg.register(1341, "claude", make_handler("claude.state"))
            .expect("first register");
        let err = reg
            .register(1341, "codex", make_handler("codex.state"))
            .unwrap_err();
        match err {
            OscRegistrationError::DuplicateCode {
                code,
                existing_owner,
                new_owner,
            } => {
                assert_eq!(code, 1341);
                assert_eq!(existing_owner, "claude");
                assert_eq!(new_owner, "codex");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn reserved_code_is_rejected() {
        let reg = OscHandlerRegistry::new();
        for &code in &[0u16, 7, 133, 633, 1023] {
            let err = reg.register(code, "claude", make_handler("k")).unwrap_err();
            assert!(matches!(err, OscRegistrationError::ReservedCode { code: c } if c == code));
        }
    }

    #[test]
    fn first_claimable_code_is_1024() {
        let reg = OscHandlerRegistry::new();
        reg.register(1024, "claude", make_handler("k"))
            .expect("1024 should be claimable");
    }

    #[test]
    fn invalid_owner_is_rejected() {
        let reg = OscHandlerRegistry::new();
        for owner in ["", "Claude", "claude ", "claude.code", "a/b"] {
            // SAFETY: intentional &'static str from literal for test case;
            // `Box::leak` not needed because literals are already 'static.
            let err = reg.register(1024, owner, make_handler("k")).unwrap_err();
            assert!(
                matches!(err, OscRegistrationError::InvalidOwner { .. }),
                "expected InvalidOwner for {owner:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn dispatch_unknown_code_returns_none() {
        let reg = OscHandlerRegistry::new();
        assert!(reg.dispatch(&[b"1341"]).is_none());
    }

    #[test]
    fn dispatch_handler_returning_none_is_silent() {
        let reg = OscHandlerRegistry::new();
        reg.register(
            1341,
            "claude",
            Box::new(|_| None), // always returns None
        )
        .expect("register");
        assert!(reg.dispatch(&[b"1341", b"whatever"]).is_none());
    }

    #[test]
    fn panicking_handler_disabled_after_first_panic() {
        let reg = OscHandlerRegistry::new();
        let call_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&call_count);

        reg.register(
            1341,
            "buggy",
            Box::new(move |_params| {
                count_clone.fetch_add(1, Ordering::SeqCst);
                panic!("boom");
            }),
        )
        .expect("register");

        // First dispatch triggers the panic, which is caught. The handler
        // is replaced with a no-op and marked disabled.
        assert!(reg.dispatch(&[b"1341", b"first"]).is_none());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert!(reg.is_disabled(1341));

        // Subsequent dispatches short-circuit without invoking the original
        // (now-replaced) closure. The counter must not advance.
        for _ in 0..5 {
            assert!(reg.dispatch(&[b"1341", b"later"]).is_none());
        }
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn owner_of_and_contains() {
        let reg = OscHandlerRegistry::new();
        assert!(reg.owner_of(1341).is_none());
        assert!(!reg.contains(1341));

        reg.register(1341, "claude", make_handler("k"))
            .expect("register");
        assert_eq!(reg.owner_of(1341), Some("claude"));
        assert!(reg.contains(1341));
    }

    #[test]
    fn osc_code_parse_rejects_non_numeric() {
        assert_eq!(osc_code_from_params(&[b"1341"]), Some(1341));
        assert_eq!(osc_code_from_params(&[b"12"]), Some(12));
        assert_eq!(osc_code_from_params(&[b"abc"]), None);
        assert_eq!(osc_code_from_params(&[b""]), None);
        assert_eq!(osc_code_from_params(&[]), None);
        // u16 overflow (1_000_000 does not fit in u16)
        assert_eq!(osc_code_from_params(&[b"1000000"]), None);
    }
}
