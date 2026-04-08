# ADR 0004 — Unified Event Bus Shape

**Status:** Accepted  
**Date:** 2026-04-08  
**Spec:** `docs/event-bus-spec.md`  
**Impl issue:** `tn-xula`

---

## Context

Therminal's integration taxonomy (root `CLAUDE.md`) defines three event sources:
core capabilities, harness crates, and pattern packs. Each source needs to publish
typed events that orchestrators can observe via MCP. Several design choices had to
be made before any implementation lands so that consumers written today do not
break when the implementation arrives.

The key questions were: one bus or many; what shape; one cursor or many; how much
to retain; and what the size bounds are.

---

## Decision

A single unified bus with one `TerminalEvent` record, a single global monotonic
cursor, a bounded ring buffer, and a single MCP resource URI
(`therminal://events`) with a filter query string.

Full spec: `docs/event-bus-spec.md`. Kind vocabulary: `docs/event-bus-kinds.md`.

---

## Alternatives rejected

### Per-source resource URIs

`therminal://claude/events`, `therminal://patterns/events`,
`therminal://core/events` — one resource per surface class.

**Rejected because:** adversarial-review use cases (watching all harnesses
simultaneously to detect coordinated tool calls) need to subscribe once and
receive a single ordered stream. Per-source URIs force orchestrators to manage
N subscriptions, merge N streams, and reconstruct total ordering from N
independent cursors. The unified bus gives that ordering for free. The existing
`therminal://claude/events` resource is preserved as a backward-compat shim
routed to `therminal://events?source_class=harness&source_id=claude`.

### JSONL-only format (no typed record)

Emit raw JSONL with no enforced schema; let each source define its own shape.

**Rejected because:** orchestrators want schema discovery. An MCP resource with
a typed `TerminalEvent` record lets Claude Code and other consumers introspect
the schema via `list_resources` / `describe_resource` without reading docs.
A fully free-form JSONL stream would require consumers to handle unknown shapes
defensively on every event. The `body` field preserves source-specific
extensibility while the envelope fields remain typed.

### Per-source cursor counters

Each source (or each source class) maintains its own cursor starting at 0.

**Rejected because:** resumption semantics become tangled. An orchestrator
subscribed to both `source_class=harness` and `source_class=pattern` would hold
two independent cursors and need to merge streams to reconstruct ordering. A
single global cursor means any filter combination produces a single cursor the
subscriber can resume from, and the interleaving of events from different sources
reflects wall-clock arrival order.

### Unbounded retention

Keep all events since daemon start in an append-only log (memory or disk).

**Rejected because:** pattern packs that match against large terminal captures
(e.g., a cargo build with thousands of lines of output) can emit large event
bodies in bursts. Without a size cap, a pathological pattern pack can exhaust
available memory. The 10,000-event ring buffer is large enough for typical
orchestrator polling intervals (seconds to minutes) while bounding worst-case
memory use. The 4 KB recommended / 64 KB hard body size cap further bounds
per-event memory. Neither the ring buffer capacity nor the body cap prevents
sources from emitting structural summary events that reference external storage
for large payloads.

### Persist events across daemon restarts

Write the ring buffer to disk and replay on restart, so subscribers can resume
across daemon restarts.

**Rejected for v1** because: events often reference pane IDs and session state
that no longer exists after a restart. Replaying stale events with invalid
references would confuse subscribers more than a clean slate. Cursor values reset
to 1 on restart; the spec documents this so consumers can detect the reset by
comparing the new daemon's cursor sequence against their cached cursor.

---

## Size cap rationale

| Limit | Value | Reason |
|---|---|---|
| Recommended body | 4 KB | Fits a short tool result, a stack trace, or a progress message; sources should summarize beyond this |
| Hard body cap | 64 KB | Prevents a single event from dominating the ring buffer; 10,000 × 64 KB = 640 MB worst-case, acceptable for a terminal emulator |
| Ring buffer | 10,000 events | ~1–4 MB typical (assuming 100–400 byte average body); configurable via `[event_bus] ring_buffer_capacity` |

---

## Consequences

- All three integration surfaces share one ordered stream. Orchestrators write
  one subscription and filter with query parameters.
- The backward-compat shim for `therminal://claude/events` requires no publisher
  changes — it is purely a URI alias in the MCP resource handler.
- Consumers that cache cursors must handle cursor reset on daemon restart
  (treat stale cursor as `since=0`).
- Pattern packs that emit large bodies must truncate or summarize to stay under
  the 4 KB recommendation. The hard 64 KB cap is enforced by the bus; violations
  are replaced with a `bus.body_too_large` error event so the subscriber sees
  the gap rather than silently losing the event.
