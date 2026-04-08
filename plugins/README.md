# Pattern Packs

A pattern pack is a TOML file that teaches therminal to recognize specific output
in your terminal and react to it. Each pack contains one or more rules: "when a
line matches this regex, make it clickable", "show a badge with the test result",
"publish an event to my orchestrator". Packs are data files — no code, no
compilation step. Drop a file in `~/.config/therminal/patterns/` and therminal
picks it up immediately, no restart required.

## Installing a pack

```bash
cp my-pack.toml ~/.config/therminal/patterns/
```

Therminal watches that directory and reloads automatically. To check that the
pack loaded correctly:

```bash
therminal semantic patterns stats --pack my-pack-name
```

## Writing a pack

See **`docs/pattern-packs-authoring.md`** for the full schema, all action types,
scoping options, regex constraints, and examples. It is the only document you
need.

Short version: create a `.toml` file with one or more `[[pattern]]` blocks, each
with a `name`, `match` (regex), `scope`, and `action`. Capture groups let you
pull values out of the match and reference them in action fields as `{name}`.

## Sharing a pack

A pattern pack is just a TOML file. Copy it, paste it, send it in a chat
message. No packaging or signing required.

## Shipped examples

The following packs are bundled with therminal and loaded by default. You can
copy any of them as a starting point for your own packs.

### `examples/cargo-errors.toml`

Makes Rust compiler error and warning locations clickable. Clicking a highlighted
`--> src/lib.rs:42:5` line opens the file in your configured editor at the right
line.

### `examples/claude-usage.toml`

Turns Claude Code's `Context: NN%` statusline output into a live gauge widget at
the right edge of the line. Also publishes a context-usage event so orchestrators
can track context consumption over time. Active only when Claude Code is running
in the pane.

### `examples/glossary.toml`

Emits a lookup event whenever a common technical acronym (OSC, PTY, MCP) appears
in terminal output. Demonstrates the minimal `emit_event` pattern: two named
captures, no widget, no click handler. A tooltip overlay or chat-op notifier
subscribed to `therminal://events?source_class=pattern` can surface the
definition however it likes.

### `examples/github-urls.toml`

Makes `github.com/owner/repo#N` issue and PR references clickable, opening them
in the browser. Captures owner, repo, and issue number separately so the label
can display a clean `owner/repo#N` tooltip.

### `examples/test-results.toml`

Badge and event for cargo test, pytest, and jest summary lines. Shows a
pass/fail badge at the right edge of the result line and publishes a structured
event carrying the pass/fail counts and test runner name.

## Planned examples

The following packs are noted for future addition once the relevant spec
sections stabilise:

- **python-tracebacks.toml** — hotspot for `File "path.py", line N` traceback
  lines (pending `region` scope hookup in the region indexer).
