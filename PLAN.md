# Therminal — The AI-Native Terminal

## The Thesis

No terminal emulator today understands AI agents. They all treat the terminal as a dumb text pipe. Warp added AI autocomplete but it's proprietary, closed source, and requires routing through their infrastructure. Therminal is an open-source, cross-platform, GPU-accelerated terminal built from the ground up for the era of AI agents working alongside humans.

## Origin

Therminal is extracted from [thermal-desktop](../thermal-desktop/), a custom Wayland desktop environment with a thermal/FLIR aesthetic. The GPU terminal, session daemon, agent detection, and overlay system were built there and are being decoupled into a standalone cross-platform project.

thermal-desktop continues as the Linux Hyprland shell, consuming therminal as a dependency.

## What Makes It Different

### The Problem
AI agents (Claude Code, Codex, Copilot, local LLMs) run in terminals but terminals don't know they exist. The agent is blind to its environment and the human is blind to what the agent is doing. When you have multiple agents running in parallel, it's chaos.

### The Solution
A terminal that passively understands what's happening inside it and exposes that understanding to both humans and AI through structured, queryable interfaces.

### No Other Terminal Does This

| | Alacritty | Kitty | WezTerm | Ghostty | Warp | Rio | **Therminal** |
|---|---|---|---|---|---|---|---|
| GPU API | OpenGL | OpenGL | GL/Metal/Vulkan | Metal/GL | Metal | wgpu | **wgpu** |
| Windows | Yes | No | Yes | No | No | Yes | **Yes** |
| macOS | Yes | Yes | Yes | Yes | Yes | Yes | **Yes** |
| Linux | Yes | Yes | Yes | Yes | Yes | Yes | **Yes** |
| AI agent awareness | None | None | None | None | Autocomplete | None | **Full** |
| Passive agent detection | No | No | No | No | No | No | **Yes** |
| Multi-agent monitoring | No | No | No | No | No | No | **Swarm** |
| Queryable screen state | No | No | No | No | No | No | **MCP** |
| Geometry-aware layout | No | No | No | No | No | No | **Yes** |
| Built-in mux | No | Yes | Yes | No | No | No | **Daemon** |
| Open source | Yes | Yes | Yes | Yes | **No** | Yes | **Yes** |

---

## Core Features

### 1. Passive Agent Detection
The terminal watches the PTY stream and infers what's happening — no hooks, no plugins, no cooperation from the AI tool required. Works with Claude Code, Codex, local LLMs, anything that runs in a terminal.

Detection layers:
- **OSC 633** shell integration sequences (when available)
- **Pattern matching** on PTY output (agent prompts, tool calls, thinking indicators)
- **Timing heuristics** (output cadence distinguishes streaming code from user typing)
- **Process tree inspection** (what binary is actually running)

Agent states: `idle | processing | tool_use | awaiting_input | streaming | thinking`

### 2. Semantic Screen State
The terminal understands its content as typed regions, not a flat text buffer:

```
ScreenState {
    visible_lines: Vec<Line>,
    semantic_regions: [{
        line_range: (start, end),
        type: Prompt | Command | Output | ToolCall | Thinking | Error,
        content_hash: u64,
    }],
    cursor_position: (row, col),
    agent_state: AgentState,
}
```

AI can query "give me just the errors" or "what was the last tool result" instead of ingesting hundreds of lines of scrollback.

### 3. Queryable via MCP
The terminal exposes its state as MCP tools. Any AI that speaks MCP gets environmental awareness:

```
therminal.panes()                    — list panes with geometry + agent type + state
therminal.pane_state(id)             — semantic screen state for a pane
therminal.query(id, filter)          — filtered content (errors only, last command, etc.)
therminal.geometry()                 — terminal dimensions, pane layout, available space
therminal.subscribe(events)          — push notifications for state changes
therminal.spawn(config)              — create a new session/pane
```

### 4. Geometry-Aware Tiling
The terminal exposes its dimensions and pane layout as structured data:

```
TerminalGeometry {
    cols: u16, rows: u16,
    pixel_width: u16, pixel_height: u16,
    cell_width: f32, cell_height: f32,
    dpi_scale: f32,
    panes: [{ id, session_type, bounds: {x,y,w,h}, focused }],
    available_space: { cols, rows },
}
```

An orchestrator can reason about layout: "I have 320 columns, tile 4 agents side by side" vs "80 columns, use a tab switcher." The terminal becomes a layout-aware collaborator.

### 5. Auto-Tiling Subagent Swarms
When parallel agents spawn, panes appear automatically. When they finish, panes close (with a delay to read results). No manual splitting. The terminal manages layout based on what's actually running.

### 6. Sessions, Not Tabs
Each session has a type (agent, shell, build), a detected state, and a semantic history. Attach/detach from the daemon. Navigate scrollback by command boundaries and tool calls, not line numbers.

### 7. Actionable Hotspots
The terminal detects patterns in PTY output and makes them clickable:
- **File paths** → click to open in editor or split a pane
- **URLs** → click to open in browser (or webview pane)
- **JSONL session paths** → click to open rendered session viewer
- **Error locations (file:line)** → click to jump to source
- **Subagent spawn events** → click to focus/split pane for that agent
- **Issue refs (therm-42)** → click to show details
- **Commands** → click to re-run in a new pane

Exposed to AI via MCP: `therminal.hotspots(pane_id)` returns actionable items on screen.

### 8. Hybrid Panes (Terminal + WebView)
Some content is better as HTML — rendered JSONL tailing, git graphs, Mermaid diagrams, kanban boards, markdown docs. Therminal supports two pane backends:

```rust
enum PaneBackend {
    Terminal(PtySession),     // GPU-rendered, alacritty_terminal
    WebView(WebViewHandle),   // wry, serves from localhost
}
```

Both participate in tiling, geometry, MCP queries, and the semantic event bus. The daemon manages both the same way. WebView panes connect to a Rust backend over localhost — same pattern as existing thermal-desktop HTML dashboards.

### 9. GPU Overlay Widgets
Rendered directly on the terminal surface via wgpu:
- Context gauge (token usage)
- Tool call cards (what the agent is doing)
- Thinking indicator
- Result cards (auto-dismiss)
- Adapts to pane size — narrow panes get compact widgets

---

## The Bigger Picture — Agent Workspace Protocol

Therminal is one half of a complete agent workspace:

- **Therminal** — `workspace.terminal.*` — AI sees what's in every terminal pane
- **TabzChrome** — `workspace.browser.*` — AI sees what's in every browser tab

Together, an agent gets complete environmental awareness:
```
"I have 3 panes visible. Pane 1 is Claude Code at 47% context editing auth.rs.
 Pane 2 has a failing test. Pane 3 is idle. Browser has the API docs open
 on tab 4 and the PR on tab 2. I have 200 columns to work with."
```

No other combination of tools provides this. Two separate projects, one shared MCP vocabulary.

---

## Architecture

```
therminal/
├── crates/
│   ├── therminal-protocol/    # Wire types, MCP schema, semantic events
│   ├── therminal-terminal/    # PTY management, OSC 633, state inference engine
│   ├── therminal-core/        # Color palette, wgpu context, text renderer
│   ├── therminal-runtime/     # Cross-platform IPC (interprocess), locks, paths
│   ├── therminal-daemon/      # Session manager, event bus, multiplexer, MCP server
│   └── therminal-app/         # winit window, grid renderer, overlays, tiling
├── Cargo.toml                 # Workspace root
├── PLAN.md
├── CLAUDE.md
└── README.md
```

### Tech Stack
- **GPU rendering**: wgpu (Vulkan/Metal/DX12) + glyphon + cosmic-text
- **Terminal emulation**: alacritty_terminal (vendored)
- **Windowing**: winit (cross-platform)
- **IPC**: interprocess crate (Unix sockets on Linux/macOS, named pipes on Windows)
- **Audio**: rodio (WASAPI/CoreAudio/ALSA)
- **File watching**: notify
- **Wire protocol**: MessagePack framing
- **Language**: Rust

### Cross-Platform Status

| Component | Linux | macOS | Windows | Notes |
|---|---|---|---|---|
| wgpu rendering | Vulkan | Metal | DX12 | Ready |
| alacritty_terminal | PTY | PTY | ConPTY | Ready |
| winit windowing | Wayland/X11 | Cocoa | Win32 | Ready |
| IPC (interprocess) | Unix socket | Unix socket | Named pipe | Needs abstraction |
| File watching (notify) | inotify | FSEvents | ReadDirectoryChanges | Ready |
| Audio (rodio) | ALSA/Pulse | CoreAudio | WASAPI | Ready |
| Notifications | D-Bus | mac-notification-sys | winrt-notification | Use notify-rust |

### Relationship to thermal-desktop

thermal-desktop becomes a consumer of therminal's core crates, adding:
- Wayland layer-shell surfaces (bar, HUD)
- Hyprland workspace integration
- The full thermal desktop shell experience

therminal is the standalone, cross-platform product. thermal-desktop is the opinionated Linux environment built on top of it.

---

## MVP Scope

The smallest useful release — a terminal that detects AI agents and exposes state via MCP:

1. **Single GPU-rendered terminal window** with winit (cross-platform)
2. **Session daemon** with PTY management and attach/detach
3. **Passive agent detection** from PTY stream (no hooks needed)
4. **Basic split panes** with manual and auto-tiling
5. **MCP server** exposing pane state, geometry, and queryable content
6. **TOML config** (profiles, keybindings, appearance)

Post-MVP:
- Overlay widgets
- Subagent swarm auto-tiling
- JSONL session viewer
- TTS integration
- Trust tier tracking
- Semantic scrollback navigation

---

## Implementation Roadmap

### Phase 0: Basic GPU Terminal Window
A single-window terminal that opens, runs a shell, renders with wgpu, handles input. Cross-platform from day one.

**Port directly from thermal-desktop (zero changes, ~4,700 LOC):**
- therminal-protocol (wire types, pure serde)
- therminal-core: palette.rs, wgpu_ctx.rs, text.rs
- therminal-terminal: input.rs (1,111 LOC), osc633.rs (525 LOC), terminal.rs (63 LOC)
- therminal-app: grid_renderer.rs (1,130 LOC), color_mapping.rs (347 LOC), url_detection.rs (81 LOC)

**Rewrite for cross-platform (~1,500 LOC):**
- PTY via portable-pty (replaces nix::pty)
- Window via winit 0.30 (replaces smithay-client-toolkit)
- Input via winit events → thermal-terminal encode_key()
- Clipboard via arboard crate (replaces wl-copy/wl-paste)
- Runtime paths via dirs crate

### Phase 1: Session Daemon + Multiplexing
- Session persistence (attach/detach)
- Cross-platform IPC via interprocess crate
- Basic split panes (manual)
- TOML config (profiles, keybindings, colors)

### Phase 2: Passive AI Detection + Hotspots
- Port state_inference.rs with platform-abstracted paths
- Port claude_state.rs (agent session monitoring)
- Hotspot detection layer (file paths, URLs, errors, subagent spawns)
- Hotspot rendering (underline + cursor change on hover)

### Phase 3: MCP Workspace Protocol
- Built-in MCP server (panes, queries, geometry, hotspots, subscriptions)
- Coordinate schema with TabzChrome's workspace.browser.* tools

### Phase 4: Swarm Tiling + WebView Panes
- Auto-tiling when subagents spawn/finish
- Geometry-aware layout decisions
- WebView panes via wry for rich content
- PaneBackend abstraction: Terminal | WebView

### Phase 5: Overlay Widgets + Polish
- Port overlay system from thermal-conductor
- Semantic scrollback navigation
- Trust tier tracking
- TTS integration

---

## Open Questions

- [ ] License — MIT? Apache 2.0?
- [ ] MCP schema design — coordinate with TabzChrome's existing 85 tools
- [ ] Config format — TOML seems right (alacritty precedent, Rust ecosystem)
- [ ] Default theme — thermal aesthetic? Or neutral with thermal as an option?
- [ ] Distribution — cargo install? Homebrew? Winget? Flatpak?
- [ ] CI — cross-platform builds (Linux + macOS + Windows) from day one
