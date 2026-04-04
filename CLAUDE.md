# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

Early planning phase. Core technology exists in [thermal-desktop](../thermal-desktop/) and will be extracted and made cross-platform.

## Architecture

Cargo workspace with crates extracted from thermal-desktop:

```
crates/
├── therminal-protocol/    # Wire types, MCP schema, semantic events
├── therminal-terminal/    # PTY management, OSC 633, state inference engine
├── therminal-core/        # Color palette, wgpu context, text renderer
├── therminal-runtime/     # Cross-platform IPC (interprocess), locks, paths
├── therminal-daemon/      # Session manager, event bus, multiplexer, MCP server
└── therminal-app/         # winit window, grid renderer, overlays, tiling
```

### Core Stack
- **GPU rendering**: wgpu + glyphon + cosmic-text
- **Terminal emulation**: alacritty_terminal (vendored)
- **Windowing**: winit (cross-platform)
- **IPC**: interprocess crate (Unix sockets / named pipes)
- **Wire protocol**: MessagePack framing
- **Language**: Rust

## Key Docs

| Doc | Purpose |
|-----|---------|
| `PLAN.md` | Full project plan, competitive analysis, architecture |
| `CLAUDE.md` | This file — development instructions |

## Scope Boundary: Terminal vs Integration

The core architectural decision: **"Does this need bytes-in-flight, or can it work from stored state?"**

If it needs the live PTY stream or GPU surface, it belongs in the terminal. If it works from stored/structured data, it's an external tool that connects via MCP, CLI, or daemon IPC.

### In the terminal (needs live PTY stream or GPU surface)
- Terminal emulation and rendering
- PTY parsing, OSC sequence interception, agent state detection
- Semantic region tagging as bytes flow through
- Daemon, session persistence, multiplexing
- MCP server exposing live pane state
- GPU overlays (agent status, thinking indicators, tool call cards)
- Hotspot detection and rendering (clickable file paths, URLs, errors)
- Geometry-aware tiling and auto-tiling for swarms

### Outside the terminal (consumes structured data via MCP or files)
- Session history viewer/search (TUI or web UI reading SQLite/JSONL)
- Agent coordination dashboard (logic separate, could render in WebView pane)
- Trust tier configuration UI
- Session analytics (token usage, cost tracking)
- Any tool that operates on stored state after the fact

### Gray zone (terminal hosts it but doesn't implement the logic)
- Semantic scrollback navigation: tagging is in-terminal, but rich search/filter UI may be better as a TUI connecting via MCP
- JSONL session viewer: hosted as a WebView pane, but the viewer itself is separate
- Agent-to-agent messaging: daemon provides the bus, protocol/tooling is separate

**Therminal is the platform, not the monolith.** It stays fast and focused on its privileged position — the live PTY stream and GPU surface — while the ecosystem grows around the MCP interface.

## Task Tracking

Issue tracking via beads (prefix: `tn`).

## Related Projects

- **thermal-desktop** — Linux Hyprland shell, will consume therminal as a dependency
- **TabzChrome** — Browser MCP tools, paired with therminal for complete agent workspace


<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->
