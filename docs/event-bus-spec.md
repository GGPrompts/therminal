# Therminal Unified Event Bus — Specification

**Prerequisite context:** The integration taxonomy in the root `CLAUDE.md`
defines three surfaces — core capabilities, harness crates, and pattern packs.
All three publish events. This document specifies the unified bus they publish
into and the MCP resource subscribers use to consume it.

**Status:** SPEC — no implementation yet. Consumer issue: `tn-xula`.

---

## 1. The `TerminalEvent` record

Every event that flows through the bus has this shape:

```
{
  source_class : "harness" | "pattern" | "core"
  source_id    : string          // harness name, pack name, or core subsystem
  kind         : string          // see event-bus-kinds.md
  pane_id      : u64 | null      // null for session-scoped events
  ts_ms        : u64             // Unix time in milliseconds (wall clock)
  cursor       : u64             // Monotonic bus position (see §2)
  body         : object          // Source-defined payload
}
```

### Field constraints

| Field | Type | Constraint |
|---|---|---|
| `source_class` | enum | One of `harness`, `pattern`, `core` |
| `source_id` | string | Non-empty; lowercase `[a-z0-9_-]+`; e.g. `claude`, `cargo-errors`, `shell-integration` |
| `kind` | string | Non-empty; lowercase with dots for namespacing; e.g. `tool_call`, `claude.thinking_started` |
| `pane_id` | u64 or null | null means the event is not scoped to a single pane |
| `ts_ms` | u64 | Milliseconds since Unix epoch, set by the publisher at emit time |
| `cursor` | u64 | Assigned by the bus, monotonically increasing (see §2) |
| `body` | JSON object | Must be an object (`{}`), not a bare value; see size caps below |

### Body size caps

- **Recommended limit:** 4 KB per event body. Publishers should truncate or summarize
  large captures (e.g., stdout snapshots) before emitting.
- **Hard cap:** 64 KB. The bus rejects events whose serialized body exceeds this limit
  and emits a `core` / `error` event with `kind = "bus.body_too_large"` in its place.
- Reason: pattern packs can match against large terminal captures. Without a size cap,
  a single runaway match can exhaust the ring buffer.

### Trust and redaction

Events are **low-trust by default**. The body may include a top-level `"secret": true`
field to signal that the body contains sensitive content (API keys, tokens, passwords).
When a low-trust subscriber reads such an event, the bus replaces `body` with
`{"redacted": true}`. The `source_class`, `source_id`, `kind`, `pane_id`, `ts_ms`, and
`cursor` fields are always visible to all trust tiers; only `body` is redacted.

High-trust subscribers (those that pass the daemon's trust enforcement check) receive
the unredacted body.

---

## 2. Cursor semantics

The cursor is a **single global monotonic `u64`** assigned by the bus, not by publishers.

- The first event ever assigned gets `cursor = 1`. Cursor `0` is reserved as a
  sentinel meaning "start of bus" (see `since=0` in §4).
- The cursor increments by one for every event accepted into the ring buffer.
- Cursors are **not** stable across daemon restarts. After a restart, the cursor
  resets to 1. Subscribers that cache a cursor from a previous daemon session must
  treat `since=<stale_cursor>` as a best-effort hint; if the daemon's ring buffer
  does not contain that cursor, it returns events from the oldest available position.
- Cursor values are opaque to subscribers. Do not interpret the gap between two
  cursors as a count of missed events — the ring buffer may have evicted events
  between them if the subscriber is lagging (see §5).

### Pagination and resumption

- A **one-shot read** that reaches the end of the ring buffer returns a
  `next_cursor` field in the response. Pass `since=<next_cursor>` on the next
  call to resume without overlap or gaps (within retention window).
- A **live subscription** implicitly resumes from the most recently delivered
  cursor each time the subscriber reads after a notification.

---

## 3. Ring buffer retention

The bus maintains an in-memory ring buffer.

```toml
[event_bus]
ring_buffer_capacity = 10000   # default; number of events, not bytes
```

- Default capacity: **10,000 events**.
- When the ring is full, the oldest event is evicted to make room.
- The ring is **not** persisted to disk. Events do not survive daemon restarts.
- The config key lives in `therminal.toml` under `[event_bus]`. Hot-reload is
  supported; a capacity change takes effect on the next daemon restart (live
  resize of the ring is not guaranteed).

---

## 4. MCP resource URI grammar

The unified bus is exposed as a single MCP resource with an optional query string:

```
therminal://events
therminal://events?<filter-params>
```

All filter parameters are **optional** and combine via **AND** — an event must
satisfy every supplied filter to be included.

### Filter parameters

| Parameter | Type | Description |
|---|---|---|
| `source_class` | `harness` \| `pattern` \| `core` | Exact match on `source_class` |
| `source_id` | string | Exact match on `source_id` |
| `kinds` | glob list | Comma-separated list of kind patterns; supports `*` wildcard (see §4.1) |
| `panes` | id list | Comma-separated list of pane IDs (decimal u64); events with `pane_id = null` are excluded when this filter is present |
| `since` | u64 | Return only events with `cursor > since`; `since=0` means return from oldest available |

### 4.1 `kinds` glob matching

Each element in the `kinds` list is matched against the event's `kind` field
using simple glob rules:

- `*` matches any sequence of characters, including dots.
- `,` separates patterns; an event is included if it matches **any** pattern in the list.
- Examples:
  - `kinds=tool_call,agent_state` — exact match on two kinds
  - `kinds=claude.*` — all source-specific Claude kinds
  - `kinds=*` — all kinds (equivalent to omitting the parameter)
  - `kinds=error,*.error` — any bare `error` kind plus any dot-namespaced error kind

### URI examples

```
# All events from the last 100 (resume from cursor 500)
therminal://events?since=500

# Only harness tool_call events
therminal://events?source_class=harness&kinds=tool_call

# All events from a specific harness on two panes
therminal://events?source_id=claude&panes=7,12

# Pattern pack errors from any source
therminal://events?source_class=pattern&kinds=error

# Everything from the Claude harness with cursor resumption
therminal://events?source_id=claude&since=1024
```

---

## 5. Subscription semantics

### One-shot replay

A `resources/read` call on `therminal://events?<filters>` returns a JSON array
of `TerminalEvent` objects matching the filters, up to a server-defined page size
(default 500 events per page). The response body includes:

```json
{
  "events": [ ... ],
  "next_cursor": 1234,
  "has_more": false
}
```

Pass `since=<next_cursor>` to fetch the next page. When `has_more` is `false`,
the subscriber has consumed all available matching events up to the current tip.

### Live subscription

A `resources/subscribe` call on `therminal://events?<filters>` registers the
subscriber for push notifications. Each time a new event arrives that matches
the filters, the daemon emits a `notifications/resources/updated` notification.
The subscriber then calls `resources/read` on the same URI (with `since=<last_cursor>`)
to fetch the new events.

Subscriptions survive pane creation and destruction. Filters referencing
specific `panes` are evaluated against events as they arrive — if a matching
pane is later destroyed, no further events will satisfy the filter naturally.

### Backpressure

If a live subscriber falls more than **1,000 events** behind the bus tip
(i.e., its last acknowledged cursor is more than 1,000 positions behind the
ring tip after filtering), the daemon:

1. Drops the subscriber's subscription silently.
2. Emits a `WARN`-level daemon log: `event-bus: dropped slow subscriber <id>, lag=<N>`.

The subscriber will not receive a notification about being dropped. The next
`resources/read` call will succeed (returning events from the oldest available
position matching `since`), but the gap in delivery is permanent. Callers that
need guaranteed delivery should poll with `since=<cursor>` rather than relying
on push notifications.

---

## 6. Backward-compatibility shim

The existing MCP resource `therminal://claude/events` predates the unified bus.
It must remain functional after the bus lands.

**Shim behavior:** `therminal://claude/events` is re-routed internally to:

```
therminal://events?source_class=harness&source_id=claude
```

All existing subscribers and body shapes are preserved. The shim is implemented
in the MCP resource handler without any changes to publishers.

---

## 7. Config reference

```toml
[event_bus]
# Ring buffer capacity in number of events.
# Default: 10000
ring_buffer_capacity = 10000
```

No other `[event_bus]` keys are defined in this spec. The implementation issue
(`tn-xula`) may extend this section.
