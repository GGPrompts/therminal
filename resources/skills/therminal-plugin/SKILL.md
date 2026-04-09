---
name: therminal-plugin
description: >
  Use when the user wants to customize therminal terminal output — highlight
  patterns, add clickable hotspots, render widgets, or emit events for
  orchestrators. Writes pattern pack TOML files.
---

# therminal-plugin skill

You are authoring a **therminal pattern pack** — a TOML file that makes
terminal output interactive without writing any Rust or rebuilding therminal.

## Quick start

1. Decide what terminal text you want to react to (a compiler error line, a
   test summary, a status word, a URL, etc.).
2. Write a `[[pattern]]` block with a `match` regex, a `scope`, and an
   `action`.
3. Drop the finished `.toml` file in `~/.config/therminal/patterns/`.
   Therminal hot-reloads it immediately — no restart.

Minimal working pack:

```toml
[[pattern]]
name = "done-marker"
match = '^\s*DONE\s*$'
scope = "finalized_line"
action = "emit_event"
```

## Reference documents in this skill

- **schema.md** — complete field-by-field reference for every `[[pattern]]`
  option, every action type, all widget kinds, and regex constraints.
- **cookbook.md** — 15 task-oriented recipes, each ~15 lines, covering the
  most common use cases.

Read schema.md first if you need to understand a field. Go straight to
cookbook.md if you know roughly what you want to build.

## Trigger phrases

Load this skill when the user says things like:

- "write a pattern pack for..."
- "make `cargo build` errors clickable"
- "add a badge when my tests pass"
- "emit an event when Claude finishes a task"
- "highlight [X] in the terminal"
- "create a therminal plugin for..."
- "add a widget that shows..."

## Constraints to keep in mind

- One action per `[[pattern]]`. For two actions on the same match, write two
  patterns with the same `match` value.
- Regex flavor: Rust `regex` crate. No lookahead/lookbehind, no backreferences.
  Named captures: `(?P<name>...)`.
- `pack_name` and pattern `name` must match `[a-z0-9_-]+`.
- Maximum regex length: 4096 characters.

## Verification

After writing a pack, tell the user to run:

```bash
therminal semantic patterns stats --pack <pack-name>
```

A `status = "error"` line means the pack failed to load. The error message
will pinpoint the problem.
