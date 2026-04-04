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

## Task Tracking

Issue tracking via beads (prefix: `therm` — shared with thermal-desktop during extraction).

## Related Projects

- **thermal-desktop** — Linux Hyprland shell, will consume therminal as a dependency
- **TabzChrome** — Browser MCP tools, paired with therminal for complete agent workspace
