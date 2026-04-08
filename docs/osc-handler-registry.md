# OSC Handler Registry — Specification

**Status:** SPEC — no implementation yet. Consumer issue: `tn-hkpz`.
**Concurrent specs:** `docs/event-bus-spec.md` (event bus shape, tn-bu1s),
harness extraction `tn-y92y`.

---

## Overview

Therminal's integration taxonomy (root `CLAUDE.md`, "Integration Taxonomy")
defines three event surfaces: core capabilities, harness crates, and pattern
packs. Harness crates own the private OSC marker grammars their tool emits.
Core must not hardcode those grammars; instead it provides a registration hook
that harness crates call at daemon startup to claim OSC codes and install
typed handlers.

This document is the normative specification for that hook: the registration
API, handler contract, error modes, dispatch semantics, and lifetime model.

The canonical table of claimed codes lives in `docs/osc-code-registry.md`.

---

## 1. Registration API

### 1.1 Method signature

```rust
impl TherminalInterceptor {
    pub fn register_osc_handler(
        &mut self,
        osc_code: u16,
        owner: &'static str,
        handler: Box<dyn Fn(&[&[u8]]) -> Option<HarnessEvent> + Send + Sync>,
    ) -> Result<(), OscRegistrationError>;
}
```

**Parameters:**

| Parameter | Type | Description |
|---|---|---|
| `osc_code` | `u16` | Numeric OSC final code this handler claims (e.g. `1341`). |
| `owner` | `&'static str` | Stable identifier for the registering crate. Lowercase `[a-z0-9_-]+`. Used in collision errors and as `source_id` for bus events produced by this handler. Must match the crate name without the `therminal-harness-` prefix (e.g. `"claude"`, `"codex"`). |
| `handler` | `Box<dyn Fn(…)>` | Called when a matching OSC arrives. Receives the raw VTE params slice (same layout as `intercept_osc`). Returns `Some(HarnessEvent)` to forward onto the bus, or `None` to drop silently. |

**Return value:** `Ok(())` on success. `Err(OscRegistrationError)` if the code
is already claimed or falls in the reserved range (see §3).

### 1.2 `HarnessEvent`

The return type of every registered handler. A thin envelope that keeps the
handler contract narrow while allowing arbitrary structured payloads:

```rust
pub struct HarnessEvent {
    pub kind: String,
    pub body: serde_json::Value,
}
```

| Field | Description |
|---|---|
| `kind` | Event kind string. Lowercase with dots for namespacing (e.g. `"tool_call"`, `"claude.thinking_started"`). See `docs/event-bus-kinds.md` for the cross-surface vocabulary. |
| `body` | Arbitrary JSON object. Must be a `serde_json::Value::Object`. Harness-crate–defined payload. |

The dispatcher translates a `HarnessEvent` into a `TerminalEvent` (defined in
`docs/event-bus-spec.md`) by setting:
- `source_class = "harness"`
- `source_id` = the `owner` string from the registration call
- `kind` = `harness_event.kind`
- `body` = `harness_event.body`
- `pane_id`, `ts_ms`, `cursor` filled by the dispatcher

Harness handlers do **not** set `source_id` themselves — the dispatcher injects
it from the registration metadata. This prevents a misbehaving handler from
forging a different `source_id`.

**Rejected alternative:** returning `Vec<u8>` (raw bytes). This pushes parsing
cost to the dispatch loop, makes type-safety impossible at the handler boundary,
and makes backpressure harder to reason about. The `HarnessEvent` wrapper pins
the parsing cost to the handler call site where the crate author can reason
about it directly.

---

## 2. Lifetime and activation

### 2.1 Registration window

Registration happens **once at daemon startup** in a deterministic order. The
daemon's `main.rs` calls each harness crate's public activation function before
the first PTY is opened. Each activation function calls
`register_osc_handler` for each OSC code it needs.

Handlers remain registered **for the life of the process**. There is no dynamic
unregistration in v1.

Rationale: OSC handlers are cheap (a `HashMap` lookup on every unknown OSC). The
complexity of a runtime unregistration API — including the question of what to
do with in-flight events — is not justified by any v1 use case.

### 2.2 Activation function convention

Each harness crate exposes a public function with this signature:

```rust
// In therminal-harness-<name>/src/lib.rs
pub fn activate(interceptor: &mut TherminalInterceptor) {
    interceptor
        .register_osc_handler(CODE, "name", Box::new(handle_osc_CODE))
        .expect("OSC code claimed by another handler — check osc-code-registry.md");
}
```

Using `.expect()` at startup is intentional: a duplicate registration is a
programming mistake that must be caught at startup, not silently papered over.

### 2.3 Dormant harnesses

When a harness's process is not present in any pane's process map, the handler
is still invoked if an OSC with the claimed code arrives (another process could
theoretically emit it). Handlers **should** short-circuit by returning `None`
when their harness is inactive:

```rust
fn handle_osc_1341(params: &[&[u8]]) -> Option<HarnessEvent> {
    // Check harness-local state. If no active sessions, return None immediately.
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    // ... parse params ...
}
```

Dispatcher-side pre-filtering by process-map activity is **explicitly deferred**
(see §5 and ADR `docs/adr/0002-osc-handler-registry.md`). OSC marker rates are
low enough that the cost of a hashmap lookup plus a cheap early-return is not
worth optimizing.

---

## 3. Error handling

### 3.1 `OscRegistrationError`

```rust
pub enum OscRegistrationError {
    DuplicateCode {
        code: u16,
        existing_owner: &'static str,
        new_owner: &'static str,
    },
    ReservedCode { code: u16 },
}
```

**`DuplicateCode`:** Two harness crates attempted to claim the same OSC code.
This is a programming mistake. The daemon **fails to launch** with a clear log
line at `error` level:

```
[ERROR] OSC handler registry: code 1341 claimed by both "claude" and "codex" — \
        check docs/osc-code-registry.md and resolve the conflict before restarting.
```

No silent fallback. The early failure prevents subtle bugs where one harness's
sequences silently disappear.

**`ReservedCode`:** Harness tried to claim a code in the `0–1023` range, which
is reserved for therminal core (OSC 7, 9, 133, 633, 1337, 7777) and
widely-used standard OSCs. Return `Err(OscRegistrationError::ReservedCode)`
immediately. Harness crates must use codes `≥ 1024`.

### 3.2 Malformed OSC payload

If the handler cannot parse the params (wrong number of params, bad UTF-8,
unexpected format), it returns `None`. The dispatcher logs at `debug` and
drops the event. No crash. No user-visible noise.

Example:

```rust
fn handle_osc_1341(params: &[&[u8]]) -> Option<HarnessEvent> {
    if params.len() < 2 {
        return None;  // logged at debug by dispatcher
    }
    let payload = std::str::from_utf8(params[1]).ok()?;
    // ...
}
```

### 3.3 Panics in handlers

Handler panics are caught by the dispatcher via `std::panic::catch_unwind`.
On the first panic:

1. Log at `warn` with the owner name and a truncated panic message.
2. Replace the stored handler with a no-op closure (`|_| None`).
3. Continue normal operation.

The handler cannot panic a second time because it has been replaced with a
no-op. The no-op replacement persists for the life of the process; there is
no automatic recovery. The harness crate must be fixed and the daemon restarted
to restore handler functionality.

---

## 4. Dispatch semantics

The dispatch path inside `TherminalInterceptor::intercept_osc`:

```
params[0] matches a native code?
  yes → handle natively (OSC 7, 133, 633, etc.)
  no  → registry lookup by code
        found?
          yes → call handler under catch_unwind
                returned Some(event)?
                  yes → forward to event bus dispatcher
                  no  → drop silently
          no  → fall through; log at trace ("unhandled OSC <code>")
```

Key properties:

- **No allocation in the hot path for unknown codes.** The registry is a
  `HashMap<u16, …>` and lookup is O(1).
- **Native handlers take precedence.** The registry is only consulted after the
  match arm for native codes fails. This guarantees that a harness crate cannot
  shadow core OSC handling regardless of registration order.
- **`catch_unwind` on every registered call.** This is the only place panics
  are caught; the no-op replacement (§3.3) ensures the overhead exists only on
  the first panic.
- **Bus forwarding is best-effort.** If the event bus ring buffer is full (see
  `docs/event-bus-spec.md` §5), events are dropped at the bus boundary, not at
  the dispatch point. The dispatcher does not back-pressure the PTY reader.

---

## 5. Deferred work

The following items are explicitly out of scope for v1:

| Item | Rationale |
|---|---|
| Dispatcher pre-filters by process-map activity | OSC rates are low; let handlers self-short-circuit (§2.3). Pre-filtering adds complexity without measurable benefit. |
| Dynamic registration / unregistration | No v1 use case. Registration window is startup-only. |
| Multiple handlers per code | No use case; codes are owned exclusively. |
| Code ranges (claim 1340–1349) | No use case yet. Single-code registration is sufficient. |
| Handler hot-reload on config change | Out of scope; handlers are compiled into harness crates. |

---

## 6. Interactions with other systems

### 6.1 Event bus

`HarnessEvent` is an intermediate type. The dispatcher translates it to a
`TerminalEvent` before writing to the bus. See `docs/event-bus-spec.md` for
the full `TerminalEvent` shape and the ring buffer semantics.

### 6.2 Trust tier enforcement

The dispatcher fills `source_id` from the registration's `owner` field. Trust
tier enforcement in the MCP server applies to `TerminalEvent.source_id` as
defined in the daemon trust spec. Harness crates do not interact with trust
tiers directly.

### 6.3 `therminal-harness-claude` (tn-y92y)

The Claude harness is the first real consumer of this registry. It is being
extracted concurrently. The wiring — calling `activate()` from the daemon's
`main.rs` and storing the registry inside `TherminalInterceptor` — is the
responsibility of issue `tn-hkpz`, which builds on this spec.

---

## 7. Non-goals

- This spec does not define the shape of any harness's OSC grammar. Each
  harness crate owns its own grammar; this registry is the hook, not the
  grammar.
- This spec does not define the event kind vocabulary. See
  `docs/event-bus-kinds.md`.
- This spec does not define the `TerminalEvent` wire format. See
  `docs/event-bus-spec.md`.
