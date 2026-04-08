# Integrations

Per-harness setup guides, MCP usage patterns, and known quirks.

Therminal's stable contract is the MCP server. Any harness that speaks MCP can drive it.
See `crates/therminal-daemon/CLAUDE.md` for the full tool and resource reference.

## Guides

- [Streaming Pane Output](streaming-pane-output.md) — Subscribe to live PTY output via
  `terminal://pane/{id}/output`; comparison with conductor-mcp `watch_pane`.
- [WSL2](wsl2.md) — Running therminal under WSL2: quirks, GPU, clipboard.
