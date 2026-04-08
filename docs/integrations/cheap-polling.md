# Cheap Pane Polling

When a conductor watches multiple worker panes for "is anything new?", pulling the full
visible grid every tick is wasteful: a freshly spawned shell pane returns ~2 KB of mostly
trailing whitespace. With five workers, that's 10 KB per tick — enough to chew through
Claude Code's prompt cache surprisingly fast.

This page documents the cheap polling primitives added in **tn-sp3n**. They're all
additive — existing callers continue to work — and they layer on top of the existing
`terminal.panes.get_content` tool.

## TL;DR

| You want to know... | Call this | Typical bytes |
|--------------------|-----------|---------------|
| "Did anything change?" | `terminal.panes.get_summary` | ~120 |
| "What just happened?" | `terminal.panes.peek` | ~90 (idle) – 500 (busy) |
| "Show me the screen" | `terminal.panes.get_content` (default trim) | ~180 (idle) – several KB (busy) |
| "Show me the historical fixed-width grid" | `terminal.panes.get_content` with `trim_trailing_whitespace=false` | unchanged from pre-tn-sp3n |

The default behavior of `terminal.panes.get_content` changed in tn-sp3n: trailing
whitespace is now trimmed from every emitted row. On a sparse pane this cuts the wire
payload by 70-90%. The legacy fixed-width grid is still available behind an opt-in flag.

## Default trim

```jsonc
// terminal.panes.get_content { pane_id: 1 }
{
  "pane_id": 1,
  "lines": [
    "user@host:~$ ",     // empty rows are now ""
    "",
    ""
  ],
  "cursor_col": 13,
  "cursor_line": 0,
  "cols": 80,
  "rows": 24,
  "content_hash": "9f3b2a1c4e5d6708"
}
```

The behavior change applies to both the tool and the `terminal://pane/{id}/content` MCP
resource. The resource protocol carries no per-call params, so trimming there is
unconditional. If you need the historical fixed-width 80-column grid for some
visual-diff-against-the-old-shape comparison, use the tool with
`trim_trailing_whitespace=false`.

### `content_hash`

Every `get_content` (and `get_summary` / `peek`) response now includes a hex-encoded
64-bit hash of the visible grid + cursor position. The hash is computed BEFORE any
trim / compact / rows filtering, so a polling client can compare consecutive hashes to
short-circuit the "nothing changed" branch without paying for a re-render:

```python
prev_hash = None
while True:
    summary = mcp.call("terminal.panes.get_summary", {"pane_id": 1})
    if summary["content_hash"] == prev_hash:
        time.sleep(1)
        continue
    prev_hash = summary["content_hash"]
    # something changed — pull a peek or full content
    print(mcp.call("terminal.panes.peek", {"pane_id": 1, "lines": 5}))
```

The hash is **not** cryptographic — it's `std::hash::DefaultHasher` (SipHash-1-3) over
the cell contents and cursor coordinates. It's stable across processes for a given grid
state, but you should treat it as opaque: don't try to reconstruct content from it.

## Optional `get_content` params

| Param | Default | Effect |
|-------|---------|--------|
| `trim_trailing_whitespace` | `true` | Strip trailing whitespace per row. Set `false` for the historical fixed-width grid. |
| `compact` | `false` | Drop fully-whitespace rows entirely. Implies trim. |
| `rows` | unset | Return only the LAST N rows after trim/compact. Useful on tall panes when you only care about the bottom. |

When `rows` truncates the response, the `truncated_rows` field reports how many rows
were dropped off the top, so the client can show "... 12 more rows above" if it wants to.

## `terminal.panes.get_summary`

A ~100-byte snapshot of the bits a conductor actually polls for: cursor position,
content hash, last finished command + exit code, hotspot count, agent name + status,
and a wall-clock timestamp.

```jsonc
// terminal.panes.get_summary { pane_id: 1 }
{
  "pane_id": 1,
  "cursor_col": 13,
  "cursor_line": 0,
  "content_hash": "9f3b2a1c4e5d6708",
  "hotspot_count": 0,
  "timestamp_secs": 1764201645
}
```

If shell integration (OSC 633) is wired up, `last_command` and `last_exit_code` are
populated. If an agent is registered on the pane, `agent_name` and `agent_status`
appear too. All optional fields are skipped from the wire when unknown so the
common case stays small.

This is the right tool for **"is this pane idle, working, or done?"** dashboards —
you can poll dozens of panes per tick at very low cost.

## `terminal.panes.peek`

A bigger sibling of `get_summary`: returns the last N non-empty trimmed lines from
the visible grid plus the same `content_hash` and timestamp. Default `lines=10`,
capped server-side at `lines=50`.

```jsonc
// terminal.panes.peek { pane_id: 1, lines: 5 }
{
  "pane_id": 1,
  "lines": [
    "$ cargo test",
    "running 42 tests",
    "test result: ok. 42 passed; 0 failed; ...",
    "$ "
  ],
  "content_hash": "9f3b2a1c4e5d6708",
  "timestamp_secs": 1764201645
}
```

Use this when `get_summary` says the screen changed and you want a quick look at the
tail of the buffer without paying for a full grid pull.

## Recommended polling shape

```text
            ┌──────────┐
            │  tick    │
            └────┬─────┘
                 │
        get_summary (~100 B)
                 │
        ┌────────┴────────┐
        │  hash changed?  │
        └────────┬────────┘
                 │
        ┌────────┴────────┐
        │ no              │ yes
        │                 │
       sleep        ┌──────┴───────┐
                    │  agent done? │  (status / last_exit_code from summary)
                    └──────┬───────┘
                           │
              ┌────────────┴────────────┐
              │ no  →  peek (5-10 lines) │
              │ yes →  get_content (full grid for the report) │
              └─────────────────────────┘
```

A conductor watching 5 panes with this pattern pays roughly **600 bytes per tick**
(`5 × ~120` for `get_summary`) instead of the **10 KB per tick** the pre-tn-sp3n
default cost. That's a ~94% reduction in steady-state cache churn.
