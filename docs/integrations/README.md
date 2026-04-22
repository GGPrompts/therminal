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

Current high-value MCP surfaces include:

- `terminal://pane/{id}/output` for live pane output subscriptions
- `therminal://claude/events` for live Claude Code parent/subagent event streams
- semantic queries and pane summary/peek calls where typed structure matters

Current high-value CLI surfaces include:

- `therminal pane list|peek|send|tag` for low-overhead polling and writes
- `therminal agents list` for conductor-side swarm inspection
- `therminal events --follow` for JSON Lines event streaming into shell tools

## Guides

- [`../cli.md`](../cli.md) — `therminal pane|session|workspace|agents|events|semantic`
  cache-friendly CLI surface (tn-k13n).
- `crates/therminal-daemon/CLAUDE.md` — daemon MCP tool and resource reference.
- [Cheap Polling](cheap-polling.md) — Use `terminal.panes.get_summary`, `terminal.panes.peek`,
  and `content_hash` to poll many panes without burning conductor cache (tn-sp3n).
- [Claude Code Session Titles](claude-code-session-titles.md) — Use `UserPromptSubmit`
  hooks to set workspace tab labels from Claude Code session titles (tn-lxq9).
- [Streaming Pane Output](streaming-pane-output.md) — Subscribe to live PTY output via
  `terminal://pane/{id}/output`; comparison with conductor-mcp `watch_pane`.
- [WSL2](wsl2.md) — Running therminal under WSL2: quirks, GPU, clipboard.
- [Kitty Graphics Clients](kitty-graphics-clients.md) — How image-preview tools
  auto-detect therminal via `KITTY_WINDOW_ID` + the feature-query APC (tn-xnsv).
