# Pattern Pack Cookbook

15 task-oriented recipes. Each produces a valid pack you can drop straight
into `~/.config/therminal/patterns/`. See schema.md for field details.

---

## 1. Clickable build-error hotspot (Rust / cargo)

Make `error[E0123]: ...  --> src/lib.rs:42:5` lines click-to-open.

```toml
pack_name = "my-rust-errors"

[[pattern]]
name = "compile-error"
match = 'error\[(?P<code>E\d+)\]: (?P<msg>[^\n]+?) --> (?P<file>[^:\s]+):(?P<line>\d+):(?P<col>\d+)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "{code}: {msg} ({file}:{line})"
kind = "error"
```

Requiring `error[Ecode]:` before `-->` avoids false positives in git diffs.

---

## 2. Pass/fail badge for cargo test

Show a green/red badge after `cargo test` finishes.

```toml
pack_name = "cargo-test-badge"

[[pattern]]
name = "test-result"
match = 'test result: (?P<status>FAILED|ok)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed'
scope = "prompt_boundary"
action = "widget"

[pattern.applies_to]
command = "cargo"

[pattern.widget]
kind = "badge"
label = "{passed} passed / {failed} failed"
color.ok = "green"
color.FAILED = "red"
```

`prompt_boundary` fires once per command exit. `applies_to.command = "cargo"` prevents false positives.

---

## 3. Claude context-% gauge

Horizontal gauge for Claude Code's context window usage.

```toml
pack_name = "claude-ctx"

[[pattern]]
name = "context-gauge"
match = 'Context:\s+(?P<pct>\d{1,3})%'
scope = "finalized_line"
action = "widget"
applies_to = "claude"

[pattern.widget]
kind = "gauge"
value = "{pct}"
max = "100"
color.default = "#4a9eff"
```

`applies_to = "claude"` prevents false positives from prose that contains
"Context: 42%". The gauge widget appears at the right edge of the matched line.

---

## 4. Glossary popup card

Show a floating definition card when a technical term appears.

```toml
pack_name = "glossary"

[[pattern]]
name = "oci-term"
match = '\b(?P<term>OCI)\b'
scope = "finalized_line"
action = "widget"

[pattern.widget]
kind = "card"
title = "{term}"
body = "Open Container Initiative — the standard for container image formats"
```

Cards appear on hover. Emit an event instead if you want external consumers
to handle the display (see recipe 5).

---

## 5. Long-task completion event for orchestrators

Signal external systems when a task finishes.

```toml
pack_name = "task-done"

[[pattern]]
name = "task-complete"
match = '^\s*(?P<icon>✓|done|DONE)\s+(?P<task>.+)\s+in\s+(?P<duration>\d+\.\d+)s'
scope = "finalized_line"
action = "emit_event"

[pattern.emit_event]
extra.status = "done"
extra.summary = "{task} finished in {duration}s"
```

Subscribe: `therminal://events?source_class=pattern&source_id=task-done`

---

## 6. Multi-line region match (build summary)

Match across multiple lines of a command's output using `prompt_boundary`.

```toml
pack_name = "build-summary"

[[pattern]]
name = "error-count"
# (?s) makes . match newlines so the pattern spans lines.
match = '(?s)error\[E\d+\].*?(?P<count>\d+) error'
scope = "prompt_boundary"
action = "widget"

[pattern.applies_to]
command = "cargo"

[pattern.widget]
kind = "badge"
label = "{count} errors"
color.default = "red"
```

`prompt_boundary` provides the full command transcript as a single string.
`(?s)` must be at the start of the regex to enable dot-matches-newline.

---

## 7. Harness-scoped pattern (Claude only)

Limit a pattern to panes where Claude Code is the active agent.

```toml
pack_name = "claude-tools"

[[pattern]]
name = "tool-call"
match = 'Running tool:\s+(?P<tool>\S+)'
scope = "finalized_line"
action = "emit_event"
applies_to = "claude"

[pattern.emit_event]
extra.tool = "{tool}"
```

Valid harness identifiers: `claude`, `codex`, `copilot`, `opencode`.
The pattern is a no-op in panes where no matching harness is detected.

---

## 8. Command-scoped pattern (cargo only)

Run a `finalized_line` pattern only within cargo command output.

```toml
pack_name = "cargo-warnings"

[[pattern]]
name = "unused-import"
match = 'warning: unused import'
scope = "finalized_line"
action = "emit_event"

[pattern.applies_to]
command = "cargo"

[pattern.emit_event]
extra.kind = "unused-import"
```

Command scoping uses a prefix match: `"cargo"` matches `cargo build`,
`cargo test`, `cargo clippy`, etc. Requires shell integration.

---

## 9. Clickable URL hotspot

Open a specific URL pattern in the browser on click.

```toml
pack_name = "custom-urls"

[[pattern]]
name = "dashboard-link"
match = 'https://dashboard\.example\.com/(?P<path>[^\s"]+)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_url"
target = "https://dashboard.example.com/{path}"
label = "Open dashboard: {path}"
kind = "url"
```

Anchor to a specific domain to avoid firing on every URL. Therminal already
highlights plain `https://` URLs; use this for custom labels or transforms.

---

## 10. Sparkline for a repeating metric

Track latency or a counter over time with a tiny inline bar chart.

```toml
pack_name = "latency-spark"

[[pattern]]
name = "response-time"
match = 'response_time=(?P<ms>\d+)ms'
scope = "finalized_line"
action = "widget"

[pattern.widget]
kind = "sparkline"
value = "{ms}"
```

Each successive match appends a bar. The sparkline resets when the session
resets. Good for log tails and health-check output.

---

## 11. Regex flavor caveat — no lookaround

No lookahead/lookbehind or backreferences. Rewrite using captures or literals.

Wrong (fails to load): `match = 'error(?=\[)'`

Correct: `match = 'error\['`

---

## 12. Two actions on the same match

Write two patterns with identical `match`. The engine evaluates all patterns
independently; both fire on the same line.

```toml
pack_name = "rust-err-dual"

[[pattern]]
name = "err-hotspot"
match = 'error\[(?P<code>E\d+)\]: .+ --> (?P<file>[^:]+):(?P<line>\d+)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "{code} at {file}:{line}"

[[pattern]]
name = "err-event"
match = 'error\[(?P<code>E\d+)\]: .+ --> (?P<file>[^:]+):(?P<line>\d+)'
scope = "finalized_line"
action = "emit_event"

[pattern.emit_event]
extra.code = "{code}"
```

---

## 13. Click-to-emit on hotspot click

Publish an event only when the user clicks, not on every match.

```toml
pack_name = "click-events"

[[pattern]]
name = "log-link"
match = 'See log:\s+(?P<path>[^\s]+\.log)'
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "emit_event"
label = "Report this log to orchestrator"
```

No `target` is needed for `on_click = "emit_event"`. The event body includes
all named captures from the match.

---

## 14. Case-insensitive match with inline flag

```toml
pack_name = "status-words"

[[pattern]]
name = "success-badge"
match = '(?i)\b(?P<word>success|succeeded|complete|ok)\b'
scope = "finalized_line"
action = "widget"

[pattern.widget]
kind = "badge"
label = "{word}"
color.default = "green"
```

`(?i)` makes the entire regex case-insensitive. Place it at the very start
of the pattern string.

---

## 15. Updating a pack without restarting therminal

Therminal watches `~/.config/therminal/patterns/` and reloads on save.
Check for load errors immediately after editing:

```bash
therminal semantic patterns stats --pack my-pack-name
therminal semantic patterns test my-pack-name/pattern-name --input "sample text"
```

`status = "error"` shows exactly which field failed. Fix and save — done.
