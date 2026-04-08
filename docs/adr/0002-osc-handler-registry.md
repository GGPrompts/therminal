# ADR 0002 — OSC Handler Registry

**Status:** Accepted
**Date:** 2026-04-08
**Spec:** `docs/osc-handler-registry.md`
**Code registry:** `docs/osc-code-registry.md`
**Impl issue:** `tn-hkpz`

---

## Context

Therminal's integration taxonomy (root `CLAUDE.md`) defines harness crates as
first-class support for specific AI coding tools. Each harness crate owns the
private OSC marker grammar its tool emits. The first harness crate,
`therminal-harness-claude` (tn-y92y), needs to parse Claude Code's OSC markers
without those markers being hardcoded into `therminal-terminal`.

Core needs a hook that lets harness crates register typed handlers for OSC
codes at daemon startup, while keeping the interceptor's dispatch loop free of
harness-specific logic.

The key design questions were: how handlers are registered, what they return,
how errors are surfaced, and how dormant harnesses are handled.

---

## Decision

A `register_osc_handler(osc_code, owner, handler)` method on
`TherminalInterceptor`. Handlers return `Option<HarnessEvent>`. Registration
is at daemon startup, one call per code, with hard errors on collision.

Full spec: `docs/osc-handler-registry.md`.

---

## Alternatives rejected

### One global marker grammar (old tn-xy4a scope)

Add all harness markers to a single central grammar inside
`therminal-terminal`, with one parser that handles all known harnesses.

**Rejected because:** a central grammar is a compatibility ratchet. Adding
support for a new harness requires editing core code and re-releasing
therminal. Each harness's grammar also evolves on the harness's release
cadence, not therminal's. The extraction goal (harness crates as separate
crates with their own CLAUDE.md and release notes) is impossible with a
monolithic grammar. The registry gives each harness crate full ownership of
its parser while core remains unaware of the grammar details.

### Static compile-time registration via `const` arrays

Declare a `static HANDLERS: &[OscHandlerEntry]` in each harness crate and
aggregate them in a central manifest crate that therminal-terminal links.

**Rejected because:** this requires a central manifest crate that every harness
crate must register with. Adding a new harness means editing that central
manifest, which recreates the coupling the extraction goal is trying to avoid.
It also prevents the daemon from conditionally activating harnesses based on
runtime configuration without `#[cfg]` complexity. Dynamic registration at
startup is a single additional line per harness crate and imposes no extra
linkage requirements.

### Handler returns raw `Vec<u8>`

Handlers return the raw OSC payload bytes for the dispatcher to parse.

**Rejected because:** this pushes the parsing cost and the failure modes to the
dispatch loop. The dispatcher cannot return a useful typed event without
knowing each harness's schema. It also makes backpressure harder: if parsing
fails, the dispatcher cannot distinguish "valid harness event with no bus
subscriber" from "malformed payload from unknown source". Returning
`Option<HarnessEvent>` pins parsing responsibility to the handler, where the
harness-crate author can reason about it, and gives the dispatcher a clean
typed value to translate to `TerminalEvent`.

### Dispatcher pre-filters by process-map activity

Before calling a registered handler, the dispatcher checks whether the
owning harness's process is present in the pane's process map. If not, it
skips the call entirely.

**Rejected as premature optimization.** OSC marker emission rates are low
(one sequence per shell prompt, per command, or per agent state transition).
The cost of a hashmap lookup plus a fast `None` return from the handler is
negligible. Dispatcher-side pre-filtering would require the registry to store
a reference to the process map, adding coupling and complexity that is not
justified by any measurable overhead. Handlers can short-circuit themselves
by checking harness-local state (§2.3 of the spec), which achieves the same
result with zero coupling to the process map.

### Dynamic registration and unregistration at runtime

Allow harness crates to register and unregister handlers while the daemon is
running, e.g. in response to config changes or harness process lifecycle
events.

**Rejected for v1.** No use case requires it. Dynamic deregistration raises
questions about in-flight events (do they complete with the old handler or get
dropped?), handler reference lifetime, and thread safety. Startup-only
registration sidesteps all of these: the window is well-defined, ordering is
deterministic, and the only error mode is a duplicate claim which fails the
daemon immediately.

---

## Consequences

- Harness crates are fully independent of core's dispatch logic. Core does not
  need to be modified to support a new harness.
- Duplicate claims fail the daemon at startup with a clear error message,
  preventing silent event loss.
- The `docs/osc-code-registry.md` table is the social contract for avoiding
  races between harness PRs. The runtime check is a safety net, not a
  substitute.
- The `HarnessEvent → TerminalEvent` translation in the dispatcher means
  harness crates do not need to depend on the full `TerminalEvent` type or the
  event bus API. The dependency is one-way: dispatcher depends on
  `HarnessEvent`; harness crates do not depend on the bus.
