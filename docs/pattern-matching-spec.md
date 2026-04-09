# Semantic Pattern Matching Engine — Specification

**Prerequisite context:** The integration taxonomy in the root `CLAUDE.md`
defines three surfaces — core capabilities, harness crates, and pattern packs.
This document specifies the pattern-pack surface: the config-driven regex +
action engine that lets users and AI agents extend terminal behavior without
writing Rust.

**Status:** Shipped. Engine implementation landed in `tn-yrjd` (2026-04-08); PTY line-finalization and OSC 133 C dispatch wiring landed in `tn-86us` (2026-04-09). The `emit_event` action is fully live. The `hotspot` action produces events today and a follow-up (`tn-f9cl`) wires the per-pane hotspot sink. The `widget` action publishes events; widget placement onto the tn-npd substrate is tracked as a separate follow-up.

---

## 1. What is a pattern pack?

A pattern pack is a TOML file containing one or more pattern rules. Each rule
pairs a regular expression with an action. When a rule's regex matches terminal
text in the declared scope, the engine executes the action: registering a
clickable hotspot, rendering a widget, or publishing an event.

Pattern packs are:

- **Config-driven.** No Rust, no build step, no install process beyond copying
  a file.
- **Shareable.** A pack is a single TOML file; distributing it is copy-paste.
- **AI-authorable.** The authoring guide (`docs/pattern-packs-authoring.md`)
  is the only context an AI needs to write a correct pack.
- **Composable with the rest of therminal.** Pattern-sourced hotspots enter the
  same hotspot registry as file/URL/error hotspots. Pattern-sourced events flow
  through the same event bus (`docs/event-bus-spec.md`) as harness events.

---

## 2. Config schema

### 2.1 File layout

| Location | Purpose |
|---|---|
| `~/.config/therminal/patterns/*.toml` | User patterns; loaded at startup and on hot-reload |
| `plugins/examples/*.toml` | Shipped example packs (in-repo, read-only for the engine) |
| Harness-bundled | Harness crates may register packs programmatically when their harness is active |

All `*.toml` files in the user patterns directory are loaded. Subdirectories
are not traversed. Files that fail to parse or fail schema validation are
reported as load errors (see §7) and skipped — they do not prevent other packs
from loading.

Hot-reload is handled by the existing `notify` file watcher that also reloads
`therminal.toml`. Pattern packs are recompiled whenever their file changes.

### 2.2 Top-level pack fields

A pattern pack file may optionally declare pack-level metadata before its
`[[pattern]]` entries:

```toml
pack_name = "cargo-errors"          # optional; overrides filename as pack identifier
pack_description = "Rust compiler error hotspots and widgets"  # optional
```

If `pack_name` is omitted, the engine uses the filename stem (e.g.,
`cargo-errors.toml` → `cargo-errors`). Pack names must match `[a-z0-9_-]+`
and be unique across all loaded packs. Duplicate pack names at load time are a
load error; the later-sorted file wins and the first is skipped with a warning.

### 2.3 Per-pattern fields

Each `[[pattern]]` entry has the following fields:

| Field | Required | Type | Description |
|---|---|---|---|
| `name` | yes | string | Unique identifier within the pack. Used as the `kind` field when this pattern emits an event. Must match `[a-z0-9_-]+`. |
| `description` | no | string | Human-readable explanation. Shown in `terminal.patterns.stats` output. |
| `match` | yes | string | Regular expression. See §3 for the regex flavor. |
| `scope` | yes | enum | When and against what text the match runs. See §4. |
| `action` | yes | enum | What happens on a match: `hotspot`, `widget`, or `emit_event`. See §5. |
| `applies_to` | no | string or table | Restricts which panes the pattern runs on. See §6. |

Action-specific configuration lives in a sub-table named after the action:
`[pattern.hotspot]`, `[pattern.widget]`, or `[pattern.emit_event]`.

**One pattern, one action.** A rule emits exactly one action. If you need two
actions for the same match (e.g., both a hotspot and an event), write two
patterns with the same `match` value. The engine evaluates all patterns
independently.

### 2.4 Capture reference syntax

Named captures from the `match` regex are referenced in action field values
using `{capture_name}` syntax. If a capture is optional in the regex (it may
not participate in a given match), a missing capture expands to an empty string.

Example: `match = '''error\[(?P<code>E\d+)\]'''` → reference as `{code}`.

Literal braces are written as `{{` and `}}`.

### 2.5 Full worked example

```toml
pack_name = "cargo-test-badges"
pack_description = "Red/green badges for cargo test output"

[[pattern]]
name = "cargo-test-result"
description = "Render a red/green badge for cargo test summaries"
match = '''test result: (?P<status>FAILED|ok)\. (?P<passed>\d+) passed; (?P<failed>\d+) failed'''
scope = "prompt_boundary"
action = "widget"

[pattern.widget]
kind = "badge"
label = "{passed} passed / {failed} failed"
color.ok = "green"
color.FAILED = "red"
```

---

## 3. Regex flavor

Pattern packs use the **Rust `regex` crate** (version in `Cargo.toml`).

**Key constraints:**

- No lookahead or lookbehind assertions.
- No backreferences.
- No possessive quantifiers.

The `regex` crate provides a **bounded-time guarantee**: match time is
O(input_length × pattern_complexity) and cannot be made to regress to
exponential by crafted input. This guarantee is load-bearing for the
performance model (§8).

Named captures use the `(?P<name>...)` syntax.

Full syntax reference: <https://docs.rs/regex/latest/regex/#syntax>

The engine does **not** expose a `fancy-regex` mode. Any pattern that requires
lookaround or backreferences belongs in a harness crate, not a pattern pack.
See ADR `docs/adr/0003-semantic-pattern-matching.md` for the rationale.

---

## 4. Match scope

Scope declares when the engine runs the match and what text it matches against.

### 4.1 `finalized_line`

Triggers each time a complete line is committed to the terminal grid (line
wrap, newline character, or scroll-off). The match runs against the full
content of that single line as a plain string (ANSI escape sequences stripped).

- **Cost:** cheapest scope. One regex match per line per pattern with this
  scope.
- **Coverage:** any text that appears in the terminal, regardless of whether
  shell integration is active.
- **Limitation:** cannot match across line boundaries.

Use for patterns that can identify their target from a single line: error
markers, status lines, progress indicators, single-line summaries.

### 4.2 `prompt_boundary`

Triggers on OSC 133 mark `D` (CommandFinished). The match runs against the
full transcript of the preceding command's output, as captured by the OSC 633
command tracker. The transcript is a multi-line string; `.` in the regex does
not match newlines by default (use `(?s)` inline flag to enable).

**Requires shell integration.** Panes without active OSC 133/633 marks receive
no `prompt_boundary` triggers.

- **Cost:** one match per command completion. Not triggered on every line.
- **Coverage:** the entire output of one command invocation.
- **Limitation:** fires only after the command exits, not during streaming output.

Use for patterns that summarize command results: test run outcomes, build
summaries, linter totals.

### 4.3 `region`

Triggers on OSC 133 region boundaries (PromptStart `A` ... PromptEnd `B`
delimiters from the semantic region indexer). The match runs against the text
within the delimited region. Multi-line aware.

**Requires shell integration.**

- **Cost:** one match per region boundary per pattern. Deferred if the region
  is large and the match is slow (see §8.3).
- **Coverage:** structured regions as reported by the shell integration script.

Ship only when the region indexer's boundary events are cheap to tap. If the
implementation cost proves nontrivial, defer to v2. The SPEC reserves this
scope name.

### 4.4 `live_stream` (deferred — v2)

Explicitly deferred. Matching against raw PTY bytes before the grid commit
introduces latency on every byte, risks partial matches on split writes, and
complicates the performance model. Not in v1.

---

## 5. Action vocabulary

### 5.1 `hotspot`

Registers a clickable region in the existing hotspot registry
(`crates/therminal-terminal/src/hotspot_detection.rs`). Pattern-sourced
hotspots are first-class: they appear in `terminal.semantic.hotspots` output,
get pointer cursor affordance, and can be clicked just like file-path or
URL hotspots.

Sub-table: `[pattern.hotspot]`

| Field | Required | Type | Description |
|---|---|---|---|
| `on_click` | yes | string (enum) | Click handler: `open_editor`, `open_url`, `emit_event`, or `run_command` (v2 only — see §5.4). |
| `target` | cond. | string | For `open_editor`: the path to open (supports `{capture}` refs). For `open_url`: the URL. |
| `label` | no | string | Tooltip text shown on hover. Supports `{capture}` refs. |
| `kind` | no | string | Hotspot kind hint for filtering in `semantic hotspots` CLI. Defaults to `pattern`. |

**`open_editor`** resolves `target` to an absolute path (tilde-expanded, cwd-
relative paths resolved against the pane's cwd) and opens it via the existing
editor fallback chain (`$EDITOR`, `$VISUAL`, etc.) exactly as built-in
file-path hotspots do.

**`open_url`** resolves `target` and opens it via the system URL handler.

**`emit_event`** publishes a `TerminalEvent` with `source_class = "pattern"`,
`source_id = <pack_name>`, `kind = <pattern_name>`, and a `body` containing
the captures map. Identical to the `emit_event` action (§5.3), but triggered
on click rather than on match.

Example:

```toml
[[pattern]]
name = "rust-error-link"
match = '''error\[(?P<code>E\d+)\]: .+ --> (?P<file>[^:]+):(?P<line>\d+)'''
scope = "finalized_line"
action = "hotspot"

[pattern.hotspot]
on_click = "open_editor"
target = "{file}"
label = "Open {file}:{line} ({code})"
kind = "error"
```

### 5.2 `widget`

Renders a small widget at an anchor point relative to the matched text. Widget
rendering uses the tiny-skia overlay substrate from `tn-npd` (shipped). The
pattern engine currently publishes widget-match events to the bus; the sink that
routes matches into the app-side `WidgetManager` is tracked as a follow-up. No
pack changes will be required when that sink lands.

Sub-table: `[pattern.widget]`

| Field | Required | Type | Description |
|---|---|---|---|
| `kind` | yes | string (enum) | Widget kind: `badge`, `gauge`, `sparkline`, or `card`. |
| `anchor` | no | string (enum) | Placement: `inline` (at match position), `line_right` (end of matched line), `overlay` (floating near match). Default: `line_right`. |
| `label` | cond. | string | Text label. Supports `{capture}` refs. Required for `badge`; optional for `card`. |
| `value` | cond. | number or string | Numeric value for `gauge` and `sparkline`. Supports `{capture}` refs (parsed as f64). |
| `max` | no | number or string | Maximum for `gauge`. Default `100`. |
| `color` | no | table | Color mapping. Keys are capture values; values are CSS-style color names or hex strings (e.g., `"#e55"`, `"green"`). Applied to `badge` background and `gauge` fill. |
| `title` | no | string | Card title. Supports `{capture}` refs. `card` kind only. |
| `body` | no | string | Card body text. Supports `{capture}` refs. `card` kind only. |

**Widget kinds:**

- `badge` — colored pill with a text label. Typical uses: pass/fail, status, count.
- `gauge` — horizontal progress bar with optional percentage label. Requires a
  numeric `value` and optional `max`.
- `sparkline` — small inline time-series bar chart. Accumulates successive
  `value` matches over time. Clears when the session resets.
- `card` — floating tooltip card with `title` + `body` text, shown on hover
  or triggered by the `prompt_boundary` scope.

Example (gauge for Claude context usage):

```toml
[[pattern]]
name = "claude-context-gauge"
description = "Context usage gauge from Claude Code output"
match = '''Context: (?P<pct>\d+)%'''
scope = "finalized_line"
action = "widget"

[pattern.widget]
kind = "gauge"
anchor = "line_right"
value = "{pct}"
max = "100"
color.default = "#4a9eff"
```

### 5.3 `emit_event`

Publishes a `TerminalEvent` to the unified event bus
(`docs/event-bus-spec.md`). The event shape is:

```
source_class = "pattern"
source_id    = <pack_name>           # the pack's name field or filename stem
kind         = <pattern.name>        # the pattern rule's name
pane_id      = <current pane>
body         = { captures: { <name>: <value>, ... }, matched_text: "..." }
```

Subscribers filter via `therminal://events?source_class=pattern` or by
`source_id` for a specific pack.

Sub-table: `[pattern.emit_event]`

| Field | Required | Type | Description |
|---|---|---|---|
| `extra` | no | table | Static key/value pairs merged into `body` alongside `captures`. Values are strings. Supports `{capture}` refs in values. |

Example:

```toml
[[pattern]]
name = "task-complete"
description = "Emit an event when a long-running task finishes"
match = '''^\s*✓\s+(?P<task>.+)\s+in\s+(?P<duration>\d+\.\d+)s'''
scope = "finalized_line"
action = "emit_event"

[pattern.emit_event]
extra.status = "done"
extra.summary = "Task {task} completed in {duration}s"
```

### 5.4 Actions not in v1

**`run_command`** (v2) — execute a shell command with capture-expanded
arguments. Deferred because executing arbitrary shell from regex captures
requires trust-tier gating not yet designed. A pattern that needs to run a
command should use `emit_event` and have an external orchestrator act on the
event.

**`modify_pane`** — send keystrokes back into the pane based on a match.
Permanently deferred. The automation risk outweighs the benefit; therminal
already provides MCP tools for controlled pane writes.

---

## 6. Pack scoping (`applies_to`)

By default, every loaded pattern runs on every pane. The `applies_to` field
narrows this.

### 6.1 Global (default)

Omit `applies_to`. The pattern runs on every pane in every session.

### 6.2 Harness-scoped

```toml
applies_to = "claude"
```

Runs only on panes where the process detector has classified the named harness
as active. Uses the existing process map — no new detection work. Valid values
are the harness identifiers used in the process detector and agent registry:
`claude`, `codex`, `copilot`, `opencode`.

### 6.3 Command-scoped

```toml
[pattern.applies_to]
command = "cargo"
```

Runs only within OSC 633 regions where the initiating command matches the
given string (prefix match on the command name). Uses the existing OSC 633
command tracker and region indexer.

Command-scoped patterns are only evaluated for scopes that have command
context (`prompt_boundary`, `region`). A command-scoped pattern with scope
`finalized_line` will be evaluated on finalized lines that fall within a
matching command's region (i.e., between the command's `PreExec` and
`CommandFinished` marks).

### 6.4 Shell-scoped (v2)

Deferred. `applies_to.shell = "bash"` is reserved but not evaluated in v1.

---

## 7. Load-time validation

The engine validates each pack at load time. Errors are non-fatal: a bad pack
is skipped; other packs continue loading.

**Schema validation errors (pack is skipped):**

- Missing required fields (`name`, `match`, `scope`, `action`)
- Unknown `scope` or `action` value
- `name` values that do not match `[a-z0-9_-]+`
- Duplicate `name` within a pack
- Duplicate `pack_name` across loaded packs (later-sorted file wins; earlier
  file is skipped with a warning)
- Action sub-table missing required fields (e.g., `hotspot` without `on_click`)
- Invalid `applies_to` value

**Regex compilation errors (pattern is skipped, rest of pack loads):**

- Regex syntax errors are reported per-pattern. The remaining patterns in the
  pack are compiled and loaded normally.

**Load errors are surfaced via:**

- `terminal.patterns.stats` MCP tool (§8.4)
- `therminal semantic patterns stats` CLI command (§8.4)
- Daemon log at `WARN` level: `pattern-pack: <pack>: <error>`

---

## 8. Performance model

See `docs/pattern-performance-model.md` for the full cost model, budget
numbers, and metrics spec. Summary:

- Regex objects are compiled once at pack load time and reused across all
  matches.
- Per-line cost is O(patterns\_scoped\_to\_this\_pane), not O(total\_patterns).
  Scoping (§6) is applied before matching.
- A pattern that takes > 1ms to match on a 200-character line is logged as
  slow and disabled after 3 consecutive slow matches.
- Global load-time cap: 500 patterns across all packs.
- Metrics exposed via `terminal.patterns.stats` (MCP) and
  `therminal semantic patterns stats` (CLI).

---

## 9. Relation to existing infrastructure

| Therminal component | How pattern packs connect |
|---|---|
| `hotspot_detection.rs` | `hotspot` action registers via the same `TextHotspot` pipeline |
| OSC 633 command tracker (`osc633.rs`) | `prompt_boundary` and `region` scopes consume boundary events from this tracker |
| Semantic region indexer (`region_index.rs`) | `region` scope consumes indexed regions |
| Process detector + agent registry (`process_detector.rs`, `agent_registry.rs`) | Harness-scoped `applies_to` queries this registry |
| Event bus (`docs/event-bus-spec.md`) | `emit_event` action publishes here; `source_class = "pattern"` |
| Overlay widget substrate (`tn-npd`) | `widget` action consumes this when available |
| `therminal.toml` hot-reload (`notify` watcher) | Pattern packs reload alongside main config |

Pattern packs do not interact with the PTY byte stream directly. They operate
on derived data: finalized grid lines, OSC 633 transcripts, semantic region
text. The live PTY stream remains the exclusive domain of core capabilities
and harness crates.
