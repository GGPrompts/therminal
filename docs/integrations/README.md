# Integrations

Per-harness setup guides, MCP usage patterns, and known quirks.

Therminal exposes two stable contracts that talk to the same daemon:

1. **MCP server** — for typed subscriptions, resource URIs, and any
   structured query whose shape feeds back into a tool-use loop. See
   `crates/therminal-daemon/CLAUDE.md` for the full tool and resource
   reference.
2. **`therminal` CLI** — for cache-friendly writes, commands, fire-and-forget
   ops, tiny peeks, and anything called N times in a row (tn-k13n). Same
   daemon, dramatically smaller per-call cost for Claude Code and other
   prompt-cache-sensitive consumers. See [`../cli.md`](../cli.md).

The principle: pick MCP when the structured shape materially matters; pick
the CLI for everything else. Both are thin wrappers over
`therminal-daemon-client`, so the daemon never knows which one called it.

## Guides

- [`../cli.md`](../cli.md) — `therminal pane|session|workspace|agents|events|semantic`
  cache-friendly CLI surface (tn-k13n).
- [Cheap Polling](cheap-polling.md) — Use `terminal.panes.get_summary`, `terminal.panes.peek`,
  and `content_hash` to poll many panes without burning conductor cache (tn-sp3n).
- [Streaming Pane Output](streaming-pane-output.md) — Subscribe to live PTY output via
  `terminal://pane/{id}/output`; comparison with conductor-mcp `watch_pane`.
- [WSL2](wsl2.md) — Running therminal under WSL2: quirks, GPU, clipboard.
