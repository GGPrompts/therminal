# plugins/

This directory contains therminal **pattern packs** — TOML configuration files
that match terminal output and trigger hotspots, widgets, or events.

## Scope (AI agents: read this first)

**You are in a config-only directory.** When editing or creating a pack:

- Work only with `.toml` files in this directory or its subdirectories.
- The only reference document you need is `docs/pattern-packs-authoring.md`.
  Load that file plus the specific pack file you are editing. That is all.
- Do NOT open any Rust source files. Pattern packs are data files consumed by
  therminal as a black box. No knowledge of therminal's internals is needed or
  useful here.
- Do NOT add Rust code here. This directory contains only TOML files.
- Harness-specific integration code (OSC markers, daemon hooks, parsers) belongs
  in `crates/therminal-harness-*/`, not here.

## Directory layout

```
plugins/
└── examples/        # Shipped example packs (loaded by default)
    ├── cargo-errors.toml
    ├── claude-usage.toml
    └── glossary.toml
```

User-installed packs live in `~/.config/therminal/patterns/*.toml`, not here.

## What a pattern pack is

A `.toml` file with one or more `[[pattern]]` blocks. Each pattern says:
"when this regex matches in the terminal, do this action." Three actions are
available: `hotspot` (click-to-open), `widget` (badge / gauge / card), and
`emit_event` (publish to the event bus).

Full schema and examples: `docs/pattern-packs-authoring.md`.

## Writing a new pack

1. Read `docs/pattern-packs-authoring.md` — it is the complete reference.
2. Create a `.toml` file in `plugins/examples/` (for shipped examples) or
   advise the user to drop it in `~/.config/therminal/patterns/` (for personal
   packs).
3. Give the pack a `pack_name` that is `[a-z0-9_-]+` and unique across the
   examples directory.
4. Keep regexes tight. Avoid `.*` without anchors; it fires too broadly.

## Testing a pack locally

Drop the file in `~/.config/therminal/patterns/`. Therminal reloads it
automatically — no restart needed.

Check for load errors and match stats via the CLI:

```bash
therminal semantic patterns stats
therminal semantic patterns stats --pack my-pack-name --json
```

Test a single pattern against a sample line:

```bash
therminal semantic patterns test my-pack-name/pattern-name --input "sample line"
```

If you have access to the MCP server, the `terminal.patterns.stats` tool returns
the same data in structured JSON.

## Smoke test

The test suite picks up every `.toml` file in `plugins/examples/` automatically.
After adding or editing a pack, run:

```bash
cargo test -p therminal-terminal shipped_example_packs_load_cleanly
```

A load error in any pack will fail this test. Fix the error before committing.

## What NOT to do here

- Do not add Rust, Python, or shell scripts.
- Do not reference therminal's internal module names or Rust types — the pack
  format is a public contract that treats therminal as an opaque host.
- Do not add lookahead/lookbehind or backreferences to regexes — the Rust
  `regex` crate does not support them (see authoring guide for details).
