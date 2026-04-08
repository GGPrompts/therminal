//! Types for the OSC handler registry.
//!
//! The registry itself (the `HashMap`, the `register_osc_handler` method, and
//! the dispatch integration) is implemented in issue `tn-hkpz`. This module
//! provides only the types that are shared between `TherminalInterceptor` and
//! harness crates.
//!
//! See `docs/osc-handler-registry.md` for the full specification.

use serde::{Deserialize, Serialize};
use thiserror::Error;

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

/// Errors returned by `TherminalInterceptor::register_osc_handler`.
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
}
