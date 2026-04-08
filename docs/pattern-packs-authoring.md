# Pattern Packs — Authoring Guide

This guide is the only document you need to write a therminal pattern pack.
It covers the full TOML schema, every action type, how to scope patterns to
specific contexts, and how to avoid common pitfalls.

No knowledge of therminal's internals is required. Treat therminal as a black
box that reads your TOML file and reacts to matching terminal output.

---

## What is a pattern pack?

A pattern pack is a TOML file. It describes rules: "when terminal output
matches this regex, do this". Each rule is a `[[pattern]]` block.

Therminal reads packs from `~/.config/therminal/patterns/*.toml`. Drop a file
there and therminal picks it up automatically — no restart, no install command.

A pack can contain one pattern or fifty. Each pattern is independent.

---

## Minimal example

```toml
[[pattern]]
name = "done-marker"
match = '''^\s*DONE\s*$'''
scope = "finalized_line"
action = "emit_event"
```

This is a complete, valid pattern pack. When any line in any terminal pane
contains only "DONE" (with optional surrounding whitespace), therminal emits
an event that orchestrators can subscribe to.

---

## The `[[pattern]]` block

Every pattern has four required fields:

| Field | What it does |
|---|---|
| `name` | Unique identifier within this file. Used as the event `kind`. Lowercase letters, digits, hyphens only: `[a-z0-9-]+`. |
| `match` | Regular expression. See [Regex flavor](#regex-flavor). |
| `scope` | When to run the match. See [Scopes](#scopes). |
| `action` | What to do on a match: `hotspot`, `widget`, or `emit_event`. |

Optional fields on `[[pattern]]`:

| Field | What it does |
|---|---|
| `description` | Human-readable note. Shown in diagnostic output. |
| `applies_to` | Restricts which panes the pattern runs on. See [Scoping](#scoping). |

Action-specific configuration goes in a sub-table named after the action:
`[pattern.hotspot]`, `[pattern.widget]`, or `[pattern.emit_event]`.

---

## Capture references

Use named captures in your regex with `(?P<name>...)` syntax. Reference them
in action fields as `{name}`.

```toml
[[pattern]]
name = "build-errors"
match = '''error\[(?P<code>E\d+)\]: (?P<msg>.+) --> (?P<file>[^:]+):(?P<line>\d+)'''
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "Jump to {file}:{line} ({code})"
```

If a capture is optional in your regex (it may not always participate in a
match), a missing capture expands to an empty string.

To include a literal `{` or `}` in a field value, write `{{` or `}}`.

---

## Scopes

The `scope` field controls when therminal runs your regex and what text it
matches against.

### `finalized_line`

Runs once per line, as each line is committed to the terminal grid. Matches
against a single line of plain text (escape sequences stripped).

- Works in every pane, no shell integration required.
- Can only see one line at a time.
- Best for: error markers, status lines, any pattern that fits on one line.

```toml
scope = "finalized_line"
```

### `prompt_boundary`

Runs once per command, triggered when the command finishes. Matches against
the full output of that command as a multi-line string.

- Requires shell integration (therminal's built-in integration scripts for
  bash, zsh, fish, and PowerShell handle this automatically).
- Fires after the command exits, not while it is running.
- The `.` character in your regex does not cross newlines by default. Add
  `(?s)` at the start of your regex to enable dot-matches-newline mode.
- Best for: build summaries, test results, linter totals — anything that
  summarizes an entire command run.

```toml
scope = "prompt_boundary"
```

### `region`

Runs once per semantic region boundary (shell-integration-delimited sections
of output). Multi-line aware. Otherwise similar to `prompt_boundary`.

- Requires shell integration.
- Use when you need multi-line matching within a structured block that does
  not correspond to a full command.

```toml
scope = "region"
```

---

## Actions

### `hotspot`

Makes matching text clickable. When the user clicks the highlighted region,
therminal executes the click handler.

```toml
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"   # required
target   = "{file}"        # required for open_editor and open_url
label    = "Open {file}"   # optional tooltip
kind     = "error"         # optional filter hint (default: "pattern")
```

**`on_click` values:**

| Value | What happens |
|---|---|
| `open_editor` | Opens `target` in the user's configured editor (same chain as clicking a file path hotspot). |
| `open_url` | Opens `target` in the default browser. |
| `emit_event` | Publishes a pattern event when the user clicks (instead of on match). |

`target` supports `{capture}` references.

Full example — clickable Rust compiler error:

```toml
[[pattern]]
name = "rust-error"
description = "Click to jump to Rust compiler error location"
match = '''error\[(?P<code>E\d+)\]:.+ --> (?P<file>[^:]+):(?P<line>\d+)'''
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "{code} at {file}:{line}"
kind = "error"
```

### `widget`

Renders a small visual widget near the matched text.

```toml
action = "widget"

[pattern.widget]
kind   = "badge"         # required: badge, gauge, sparkline, or card
anchor = "line_right"    # optional: inline, line_right, overlay (default: line_right)
label  = "{status}"      # text label (required for badge, optional for card)
```

**Widget kinds:**

**`badge`** — colored pill with a text label. Good for pass/fail, counts,
status words.

```toml
[pattern.widget]
kind  = "badge"
label = "{passed} passed / {failed} failed"
color.ok     = "green"
color.FAILED = "red"
```

The `color` table maps capture values to colors. The key is the capture value
(e.g., the literal string `"ok"` or `"FAILED"`); the value is a CSS color name
or hex string. An unmatched value gets the default theme color.

**`gauge`** — horizontal progress bar. Requires a numeric `value`.

```toml
[pattern.widget]
kind  = "gauge"
value = "{pct}"   # parsed as a number
max   = "100"     # optional, default 100
color.default = "#4a9eff"
```

**`sparkline`** — small inline bar chart that accumulates successive numeric
values over time. Use for metrics that change repeatedly in a session.

```toml
[pattern.widget]
kind  = "sparkline"
value = "{latency_ms}"
```

**`card`** — floating tooltip card with a title and body. Shown on hover or
triggered by `prompt_boundary`.

```toml
[pattern.widget]
kind  = "card"
title = "Test result"
body  = "{passed} passed, {failed} failed\nDuration: {duration}s"
```

**Anchor options:**

| Value | Placement |
|---|---|
| `inline` | At the position of the match in the grid |
| `line_right` | At the end of the matched line (default) |
| `overlay` | Floating near the match, above grid content |

### `emit_event`

Publishes a structured event that orchestrators, scripts, and other MCP
subscribers can listen for. Events are lightweight — the main use case is
signalling external systems.

```toml
action = "emit_event"

[pattern.emit_event]
extra.status  = "done"                          # optional static fields
extra.summary = "Finished in {duration}s"       # supports {capture} refs
```

The emitted event automatically includes all named captures from the match in
a `captures` field. You do not need to list captures again in `extra`.

Subscribe to pattern events from outside therminal:

```
therminal://events?source_class=pattern
therminal://events?source_class=pattern&source_id=my-pack-name
```

The `kind` field of the event equals the pattern's `name`.

Full example:

```toml
[[pattern]]
name = "task-complete"
match = '''✓\s+(?P<task>.+)\s+\((?P<duration>\d+\.\d+)s\)'''
scope = "finalized_line"
action = "emit_event"

[pattern.emit_event]
extra.status = "done"
```

---

## Scoping: limiting which panes a pattern runs on

By default a pattern runs on every pane. The `applies_to` field restricts this.

### Global (default)

No `applies_to` field. Pattern runs everywhere.

### Harness-scoped

Runs only on panes where the specified AI coding agent is active:

```toml
applies_to = "claude"   # or: "codex", "copilot", "opencode"
```

### Command-scoped

Runs only within the output of a specific command:

```toml
[pattern.applies_to]
command = "cargo"
```

Therminal uses shell integration to detect which command produced each block
of output. A command-scoped pattern is automatically inactive in panes without
shell integration.

You can combine scoping. This pattern runs only on panes running Claude, only
during `cargo` invocations:

```toml
applies_to = "claude"

# and separately:
[pattern.applies_to]
command = "cargo"
```

---

## Regex flavor

Patterns use the Rust `regex` crate. Key constraints:

- **No lookahead or lookbehind** (`(?=...)`, `(?!...)`, etc.)
- **No backreferences** (`\1`, `\k<name>`)
- **No possessive quantifiers**

These features are absent by design: the `regex` crate guarantees that match
time is always bounded regardless of input, which matters for a terminal that
matches against every line of output.

Everything else you expect works: character classes, alternation, quantifiers,
anchors, flags. Full syntax reference:
<https://docs.rs/regex/latest/regex/#syntax>

**Named captures:** `(?P<name>...)` — required to use `{capture}` references
in action fields.

**Flags you can use inline:**

- `(?i)` — case-insensitive
- `(?m)` — multiline (^ and $ match line boundaries)
- `(?s)` — dot matches newline (useful in `prompt_boundary` patterns)
- `(?x)` — allow whitespace and `#` comments in pattern (verbose mode)

**Common pitfall:** greedy `.*` in the middle of a pattern can match much more
than intended. Prefer specific character classes or lazy quantifiers (`.*?`).

---

## Pack-level metadata (optional)

You can add metadata at the top of your file (before any `[[pattern]]`):

```toml
pack_name = "my-pack"
pack_description = "What this pack does"

[[pattern]]
# ...
```

`pack_name` defaults to the filename stem if omitted. It identifies the pack
in events (`source_id`) and diagnostic output. Must match `[a-z0-9_-]+`.

---

## Testing your pack locally

After dropping your pack into `~/.config/therminal/patterns/`, therminal
reloads it automatically (no restart needed).

Check load errors and match statistics:

```bash
therminal semantic patterns stats
therminal semantic patterns stats --pack my-pack-name --json
```

If your pack has a syntax error, the stats output will show `status = "error"`
with the error message.

To test a specific pattern against sample text without running the full
terminal:

```bash
therminal semantic patterns test my-pack-name/pattern-name --input "sample line"
```

(This command is added by the implementation tracked in `tn-yrjd`.)

---

## Good patterns vs bad patterns

### Good: tight regex with clear anchors

```toml
# Matches "test result: ok. 5 passed; 0 failed" — specific enough
match = '''test result: (?P<status>FAILED|ok)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed'''
```

### Bad: overly broad regex that fires everywhere

```toml
# Matches any line containing "error" — too many false positives
match = '''.*error.*'''
```

### Good: narrow scope that only fires when relevant

```toml
# Only fires after cargo commands complete — not on every line in every pane
scope = "prompt_boundary"

[pattern.applies_to]
command = "cargo"
```

### Bad: global finalized_line for a pattern that only makes sense in one context

```toml
# Fires on every line in every pane, wasting CPU
scope = "finalized_line"
# (no applies_to)
match = '''cargo test result'''
```

Better:

```toml
scope = "prompt_boundary"
[pattern.applies_to]
command = "cargo"
```

### Good: two patterns for two actions

If you want both a hotspot and an event for the same match, write two patterns:

```toml
[[pattern]]
name = "rust-error-hotspot"
match = '''error\[(?P<code>E\d+)\].+ --> (?P<file>[^:]+):(?P<line>\d+)'''
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"

[[pattern]]
name = "rust-error-event"
match = '''error\[(?P<code>E\d+)\].+ --> (?P<file>[^:]+):(?P<line>\d+)'''
scope = "finalized_line"
action = "emit_event"
```

### Bad: trying to do too much in one pattern

Therminal does not support multiple actions per pattern. One rule, one action.

---

## Full multi-pattern example: `cargo-errors.toml`

```toml
pack_name = "cargo-errors"
pack_description = "Clickable hotspots and result badges for cargo builds and tests"

# Clickable link for Rust compiler errors
[[pattern]]
name = "compile-error"
description = "Jump to Rust compiler error location"
match = '''error\[(?P<code>E\d+)\]: (?P<msg>[^\n]+?) --> (?P<file>[^:\s]+):(?P<line>\d+):(?P<col>\d+)'''
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "{code}: {msg} ({file}:{line})"
kind = "error"

# Badge on test summary line
[[pattern]]
name = "test-result-badge"
description = "Pass/fail badge for cargo test summary"
match = '''test result: (?P<status>FAILED|ok)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed'''
scope = "prompt_boundary"
action = "widget"
applies_to = { command = "cargo" }

[pattern.widget]
kind  = "badge"
anchor = "line_right"
label = "{passed} passed / {failed} failed"
color.ok     = "green"
color.FAILED = "red"

# Event for orchestrators watching build results
[[pattern]]
name = "build-finished"
description = "Emit event when a cargo build completes"
match = '''(?P<outcome>error|warning)(\[.+\])?:'''
scope = "prompt_boundary"
action = "emit_event"
applies_to = { command = "cargo" }

[pattern.emit_event]
extra.tool = "cargo"
```

---

## Reference: all fields at a glance

### `[[pattern]]` fields

| Field | Required | Type | Notes |
|---|---|---|---|
| `name` | yes | string | `[a-z0-9-]+`, unique within pack |
| `match` | yes | string | Rust `regex` syntax |
| `scope` | yes | enum | `finalized_line`, `prompt_boundary`, `region` |
| `action` | yes | enum | `hotspot`, `widget`, `emit_event` |
| `description` | no | string | Human-readable note |
| `applies_to` | no | string or table | Harness name string, or `{ command = "..." }` table |

### `[pattern.hotspot]` fields

| Field | Required | Type | Notes |
|---|---|---|---|
| `on_click` | yes | enum | `open_editor`, `open_url`, `emit_event` |
| `target` | cond. | string | Required for `open_editor` and `open_url`. Supports `{capture}`. |
| `label` | no | string | Tooltip text. Supports `{capture}`. |
| `kind` | no | string | Filter hint. Default: `"pattern"`. |

### `[pattern.widget]` fields

| Field | Required | Type | Notes |
|---|---|---|---|
| `kind` | yes | enum | `badge`, `gauge`, `sparkline`, `card` |
| `anchor` | no | enum | `inline`, `line_right`, `overlay`. Default: `line_right`. |
| `label` | cond. | string | Required for `badge`. Supports `{capture}`. |
| `value` | cond. | string | Required for `gauge`, `sparkline`. Parsed as f64. Supports `{capture}`. |
| `max` | no | string | Max for `gauge`. Default: `"100"`. |
| `color` | no | table | Map of capture value → CSS color. |
| `title` | no | string | Card title. `card` kind only. Supports `{capture}`. |
| `body` | no | string | Card body. `card` kind only. Supports `{capture}`. |

### `[pattern.emit_event]` fields

| Field | Required | Type | Notes |
|---|---|---|---|
| `extra` | no | table | Static key/value pairs merged into `body`. Values support `{capture}`. |
