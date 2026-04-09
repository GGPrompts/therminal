# Pattern Pack Schema Reference

A pattern pack is a `.toml` file with optional pack metadata at the top and
one or more `[[pattern]]` blocks.

---

## Pack-level metadata (optional)

```toml
pack_name = "my-pack"           # [a-z0-9_-]+; defaults to filename stem
pack_description = "What this pack does"
```

Drop the file in `~/.config/therminal/patterns/`. Therminal hot-reloads it;
no restart needed.

---

## `[[pattern]]` — required fields

| Field   | Type   | Constraints | Description |
|---------|--------|-------------|-------------|
| `name`  | string | `[a-z0-9-]+`, unique within pack | Used as event `kind`. |
| `match` | string | Rust `regex` syntax, ≤ 4096 chars | The regex to run. |
| `scope` | enum   | see Scopes | When to run the match. |
| `action`| enum   | `hotspot`, `widget`, `emit_event` | What to do on match. |

### Optional pattern fields

| Field        | Type            | Description |
|--------------|-----------------|-------------|
| `description`| string          | Human note shown in stats output. |
| `applies_to` | string or table | Restrict which panes the pattern runs on. |

---

## Scopes

### `finalized_line`
Runs once per line as each line commits to the terminal grid. Matches one
plain-text line (ANSI stripped). No shell integration required.
Best for: error markers, status lines, single-line patterns.

### `prompt_boundary`
Runs once per command, after it exits. Matches the full multi-line output
transcript. Requires shell integration. `.` does not cross newlines by
default; add `(?s)` to enable dot-matches-newline.
Best for: test summaries, build totals, linter counts.

### `region`
Runs on shell-integration-delimited regions. Multi-line aware. Requires
shell integration.

---

## Actions

### `hotspot` — clickable region

```toml
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"   # required: open_editor | open_url | emit_event
target   = "{file}"        # required for open_editor and open_url
label    = "Open {file}"   # optional tooltip (supports {capture})
kind     = "error"         # optional filter hint (default: "pattern")
```

`on_click` values:
- `open_editor` — opens `target` in the user's configured editor.
- `open_url` — opens `target` in the default browser.
- `emit_event` — publishes an event when the user clicks (not on match).

### `widget` — visual overlay

```toml
action = "widget"

[pattern.widget]
kind   = "badge"       # required: badge | gauge | sparkline | card
anchor = "line_right"  # optional: inline | line_right | overlay (default: line_right)
```

**badge** — colored pill with text label. `label` is required.

```toml
[pattern.widget]
kind  = "badge"
label = "{passed} passed / {failed} failed"
color.ok     = "green"
color.FAILED = "red"
```

The `color` table maps a capture value (the literal matched string) to a CSS
color name or hex string. An unmatched value uses the theme default.

**gauge** — horizontal progress bar. `value` is required.

```toml
[pattern.widget]
kind  = "gauge"
value = "{pct}"    # parsed as f64
max   = "100"      # optional, default 100
color.default = "#4a9eff"
```

**sparkline** — inline time-series bar chart. `value` is required.

```toml
[pattern.widget]
kind  = "sparkline"
value = "{latency_ms}"
```

**card** — floating tooltip. Requires `title` or `body` (or both).

```toml
[pattern.widget]
kind  = "card"
title = "Result"
body  = "{passed} passed, {failed} failed\nDuration: {duration}s"
```

Anchor values:
- `inline` — at the match position in the grid.
- `line_right` — end of the matched line (default).
- `overlay` — floating near the match.

### `emit_event` — publish to event bus

```toml
action = "emit_event"

[pattern.emit_event]
extra.key = "value"          # static string
extra.msg = "Done: {task}"   # supports {capture} refs
```

All named captures are automatically included in the event's `captures` field.
`extra` adds additional static or template-expanded key/value pairs.

Subscribe from outside therminal:
```
therminal://events?source_class=pattern
therminal://events?source_class=pattern&source_id=my-pack
```

---

## `applies_to` — scope to specific panes

Omit for global (runs on every pane).

**Harness-scoped** — only on panes where the named agent is active:
```toml
applies_to = "claude"   # or: codex | copilot | opencode
```

**Command-scoped** — only within output of a specific command (prefix match,
requires shell integration):
```toml
[pattern.applies_to]
command = "cargo"
```

**Combined** — set both `applies_to = "claude"` at pattern level and a
`[pattern.applies_to]` table with `command`.

---

## Capture references

Named captures: `(?P<name>...)`. Reference in action fields as `{name}`.
Optional captures that did not participate expand to empty string.
Literal `{` and `}` are written as `{{` and `}}`.

---

## Regex flavor (Rust `regex` crate)

Supported: character classes, alternation, quantifiers, anchors, all flags.

**Not supported:** lookahead/lookbehind, backreferences, possessive quantifiers.

Inline flags: `(?i)` case-insensitive, `(?m)` multiline, `(?s)` dot-matches-newline, `(?x)` verbose.

Common pitfall: `.*` in the middle of a pattern matches far more than intended.
Prefer specific character classes or lazy `.*?`.

---

## Validation rules

- `pack_name` and pattern `name`: `[a-z0-9_-]+`, unique within their scope.
- Pattern names must be unique within a pack.
- Pack names must be unique across all loaded packs.
- One action per pattern. For two actions on the same match, write two patterns.
- `hotspot` without `[pattern.hotspot]` is a load error.
- `widget` without `[pattern.widget]` is a load error.
- `badge` without `label`, `gauge`/`sparkline` without `value`, `card` without
  `title` or `body` are load errors.

Check for errors after dropping a pack:
```bash
therminal semantic patterns stats
therminal semantic patterns stats --pack my-pack --json
```
