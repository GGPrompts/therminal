<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
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

### Competitive Position

No existing terminal combines GPU rendering, AI awareness, multiplexing, and extensibility:

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

| vs | Therminal's advantage |
|---|---|
| **Warp** | Open source, privacy-first, local-only, no login/telemetry |
| **Ghostty** | AI agent awareness, MCP integration, multiplexing, semantic scrollback |
| **Kitty** | Modern Rust/wgpu stack, AI features, Windows support |
| **Zellij** | Full terminal emulator with GPU rendering, AI, graphics protocol |
| **Rio** | AI, multiplexing, semantic history, MCP queryability |

---

## Core Features

### 1. Passive Agent Detection
The terminal watches the PTY stream and infers what's happening — no hooks, no plugins, no cooperation from the AI tool required. Works with Claude Code, Codex, local LLMs, anything that runs in a terminal.

Four detection layers, ordered by reliability:

**Layer 1 — Shell integration protocols (highest confidence):**
- OSC 133 (FinalTerm semantic prompts) — prompt/command/output boundaries
- OSC 633 (VS Code extension) — command text, cwd, nonce-based spoofing prevention
- OSC 7 (current working directory)
- OSC 1337 (iTerm2 user variables, remote host info)

**Layer 2 — Process tree inspection (high confidence):**
- `sysinfo` crate (cross-platform) to enumerate processes sharing the PTY's TTY
- Known agent signatures: `claude` (Node.js, env `CLAUDECODE=1`), `codex` (Rust), `aider` (Python), `copilot`
- Match by TTY device → walk parent-child tree → check names + env vars

**Layer 3 — Output stream analysis (medium-high confidence):**
- Human typing: 1-3 chars at 50-200ms intervals, ~4% backspace rate, high variance
- Agent output: large bursts (100-4000+ chars) at <1ms inter-character delay, zero backspaces, >500 chars/sec
- Spinner detection (Braille patterns, "Thinking..." via cursor control)

**Layer 4 — State machine inference (composite):**
Combine all signals into six states:

| State | Primary Signals | Confidence |
|-------|----------------|------------|
| **idle** | OSC 133 A→B, PTY quiescent >2s, low CPU | 95% |
| **processing** | Spinner pattern, no child processes | 80-85% |
| **streaming** | Sustained >500 chars/sec, monotonic, no backspaces | 90% |
| **tool_use** | New child processes under agent PID, OSC 133 C | 90% |
| **awaiting_input** | Output stops after question, `[y/N]` pattern | 75% |
| **thinking** | "Thinking" indicator, extended delay, no output | 75% |

**Future: Custom OSC 7777 extension** for cooperative agents to self-report state. Publish as an open spec.

### 2. Semantic Scrollback
The foundation everything else builds on. Go beyond flat text buffers with typed regions:

```
Region types: Prompt | Command | Output | Error | ToolCall | Thinking | Annotation
```

**Parse-once architecture** — no double-parsing:
```
PTY bytes → SequenceInterceptor (lightweight scan) → alacritty_terminal (parse once)
                    ↓
            Semantic index (metadata from scan + damage tracking)
```

The `SequenceInterceptor` is a trait between the VTE parser and Term handler — it scans for patterns (OSC markers, agent prompts, errors) without building grid state. Bytes pass through unchanged to alacritty. This is the xterm.js `parser.addOscHandler()` pattern, and it enables swapping to libghostty-vt later without rewriting the hook system.

**In-memory for MVP**, with structured regions queryable via MCP. Later: SQLite sidecar for persistent session history and full-text search across thousands of lines.

### 3. Queryable via MCP
The terminal exposes its state as MCP tools via `rmcp` (official Rust MCP SDK) over stdio transport. Any AI that speaks MCP gets environmental awareness:

| Tool | Type | Description |
|------|------|-------------|
| `list_panes` | Read-only | All panes with ID, title, shell, dimensions, agent state |
| `read_pane_content` | Read-only | Visible content, optionally with scrollback |
| `query_semantic_history` | Read-only | Search typed regions by pattern or filter |
| `get_pane_geometry` | Read-only | Dimensions, position, layout, available space |
| `get_hotspots` | Read-only | Actionable items detected on screen |
| `spawn_pane` | Destructive | Create pane, run command, set working directory |
| `send_input` | Destructive | Send text/keystrokes (requires trust tier 2+) |
| `wait_for_output` | Read-only | Block until pattern appears (with timeout) |
| `close_pane` | Destructive | Kill a terminal pane |

Use MCP tool annotations: `destructiveHint: true` for spawn/send/close, `readOnlyHint: true` for list/read/query. Also expose pane content as MCP Resources for subscription-based clients.

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
When parallel agents spawn, panes appear automatically. When they finish, panes close (with a delay to read results). No manual splitting. Grid layout for ≤9 agents, summary tiles for larger swarms.

### 6. Sessions, Not Tabs
Each session has a type (agent, shell, build), a detected state, and a semantic history. Attach/detach from the daemon. Navigate scrollback by command boundaries and tool calls, not line numbers.

### 7. Actionable Hotspots
The terminal detects patterns in PTY output and makes them clickable:
- **File paths** (`/path/to/file.ts:42:15`) → open in editor or split a pane
- **URLs** → open in browser (or webview pane)
- **JSONL session paths** → open rendered session viewer
- **Error locations (file:line)** → jump to source
- **Subagent spawn events** → focus/split pane for that agent
- **Issue refs** (`#1234`, `JIRA-567`, `therm-42`) → show details
- **Git refs** → show git context
- **Commands** → re-run in a new pane

Implement OSC 8 hyperlink protocol natively. On hover, render a GPU overlay action palette. Ship configurable regex patterns with sensible defaults for common languages.

Exposed to AI via MCP: `get_hotspots(pane_id)` returns actionable items on screen.

### 8. Hybrid Panes (Terminal + WebView)
Some content is better as HTML — rendered JSONL tailing, git graphs, Mermaid diagrams, kanban boards, markdown docs. Therminal supports two pane backends:

```rust
enum PaneBackend {
    Terminal(PtySession),     // GPU-rendered, alacritty_terminal
    WebView(WebViewHandle),   // wry, serves from localhost
}
```

Both participate in tiling, geometry, MCP queries, and the semantic event bus. WebView panes are **separate OS child windows** managed by the tiling layout (the OS compositor handles layering — no in-surface compositing, which is unsolved). Optional behind a feature flag since WebKitGTK adds a heavy dependency on Linux.

### 9. GPU Overlay Widgets
Two-pass rendering: opaque terminal grid first, then semi-transparent overlays with alpha blending. Widgets pre-rasterized via tiny-skia into textures, only re-rasterized when data changes (not every frame):
- Context gauge (token usage)
- Tool call cards (what the agent is doing)
- Thinking indicator
- Result cards (auto-dismiss)
- Trust tier escalation modals
- Adapts to pane size — narrow panes get compact widgets

### 10. Trust Tiers
Per-agent permissions enforced at the MCP handler layer:

| Tier | Name | Capabilities |
|------|------|-------------|
| 0 | Observer | Read pane output only |
| 1 | Reader | Read + query semantic history |
| 2 | Writer | Read + send input + spawn panes within workspace |
| 3 | Admin | Full control |

Defined per-agent in `therminal.toml`. Destructive MCP tools require tier 2+. Escalation via GPU-rendered modal overlay requiring explicit user approval.

### 11. Control Mode
A machine-readable text protocol (like tmux's `-CC` control mode) so AI agents and scripts can drive Therminal programmatically. Critical for the agent ecosystem where tools like claude-squad currently depend on tmux's scripting API.

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
├── vendor/
│   └── alacritty_terminal/    # Vendored v0.25.1, already cross-platform (ConPTY)
├── Cargo.toml                 # Workspace root
├── PLAN.md
├── CLAUDE.md
└── README.md
```

### Byte Flow (parse-once, index-alongside)
```
PTY output (bytes)
  ↓
SequenceInterceptor trait (lightweight scan — OSC 633, agent patterns, errors)
  ↓                          ↓
alacritty_terminal       Semantic index
(parse once, mutate grid)  (typed regions, hotspots, agent state)
  ↓                          ↓
Damage tracking          MCP tools query this
  ↓
GPU renderer (only redraws changed rows)
```

### Tech Stack
- **GPU rendering**: wgpu (Vulkan/Metal/DX12) + glyphon + cosmic-text + swash (pure Rust, no FreeType)
- **Terminal emulation**: alacritty_terminal (vendored, pin to commit)
- **Windowing**: winit 0.30 (cross-platform; trait-based abstraction for future native backends)
- **PTY**: portable-pty (forked for modern ConPTY flags: PASSTHROUGH_MODE, WIN32_INPUT_MODE, RESIZE_QUIRK)
- **IPC**: interprocess crate (Unix sockets on Linux/macOS, named pipes on Windows)
- **Wire protocol**: MessagePack via rmp-serde (~35ns serialize, ~125ns deserialize)
- **MCP**: rmcp (official Rust MCP SDK) over stdio transport
- **Clipboard**: arboard (maintained by 1Password) + OSC 52 for remote clipboard
- **Process inspection**: sysinfo crate (cross-platform process tree walking)
- **File watching**: notify
- **Audio**: rodio (WASAPI/CoreAudio/ALSA)
- **Language**: Rust

### Performance Targets
- ≤5ms input latency (Alacritty achieves ~3ms, Ghostty ~2ms)
- ≥100 MB/s terminal parsing throughput
- ≤3ms per-frame render time
- ≤80 MB base memory footprint
- Event-driven rendering (not continuous) with monitor refresh rate as FPS cap

### Rendering Architecture
Multi-pass pipeline modeled on Ghostty:
1. Background fill
2. Cell backgrounds
3. Text from glyphon atlas (single instanced draw call)
4. Cursor
5. Images/graphics protocol
6. Overlay widgets (semi-transparent, alpha blended)

Use Rio's dirty-tracking pattern — only redraw lines that changed. Default to `LowPower` GPU preference for integrated GPU selection. Use **linear alpha blending** from the start (Ghostty's v1.2 rework showed gamma-incorrect blending causes text artifacts). Use grayscale antialiasing with subpixel *positioning* (cosmic-text 4×4 SubpixelBin), skip subpixel color rendering (incompatible with GPU compositing, invisible on HiDPI).

### Cross-Platform Status

| Component | Linux | macOS | Windows | Notes |
|---|---|---|---|---|
| wgpu rendering | Vulkan | Metal | DX12 | Ready |
| alacritty_terminal | PTY | PTY | ConPTY | Ready (vendored) |
| winit windowing | Wayland/X11 | Cocoa | Win32 | Ready |
| portable-pty | forkpty | forkpty | ConPTY | Fork for modern flags |
| IPC (interprocess) | Unix socket | Unix socket | Named pipe | Ready |
| File watching (notify) | inotify | FSEvents | ReadDirectoryChanges | Ready |
| Clipboard (arboard) | X11/Wayland | AppKit | Win32 | Ready |
| Process tree (sysinfo) | /proc | sysctl | WMI | Ready |
| Audio (rodio) | ALSA/Pulse | CoreAudio | WASAPI | Ready |
| MCP (rmcp) | stdio | stdio | stdio | Ready |
| Notifications | D-Bus | mac-notification-sys | winrt-notification | Use notify-rust |
| WebView (wry) | WebKitGTK | WebKit | WebView2 | Optional feature flag |

### Platform-Specific Notes
- **Linux**: Wayland-first (becoming default). Font discovery via fontconfig. DBus for notifications. Consider systemd socket activation as optional.
- **macOS**: Proper `.app` bundle with notarization pipeline early. Handle Retina scale factors (≥2.0). wgpu auto-selects Metal.
- **Windows**: ConPTY always outputs UTF-8 and adds trailing spaces — handle both quirks. Target Windows 10 1809+. Fork portable-pty for `PASSTHROUGH_MODE` (Win11 22H2+).

### Relationship to thermal-desktop

thermal-desktop becomes a consumer of therminal's core crates, adding:
- Wayland layer-shell surfaces (bar, HUD)
- Hyprland workspace integration
- The full thermal desktop shell experience

therminal is the standalone, cross-platform product. thermal-desktop is the opinionated Linux environment built on top of it.

---

## Implementation Roadmap

### Phase 0: Basic GPU Terminal Window ✅ COMPLETE

A single-window terminal that opens, runs a shell, renders with wgpu, handles input. Cross-platform from day one.

**Completed:**
- Cargo workspace scaffold with all six crates
- Ported therminal-protocol wire types, therminal-core (palette, wgpu context, text renderer)
- Ported therminal-terminal (input encoding with Kitty protocol, OSC 633 parser, state inference)
- Ported therminal-app (grid renderer, color mapping, URL detection)
- Vendored alacritty_terminal v0.25.1 and VTE
- Cross-platform PTY via portable-pty
- winit 0.30 window + event loop
- Clipboard via arboard + OSC 52
- Mouse input: scroll (scrollback navigation), click (SGR 1006 mouse reporting), cursor tracking
- CI: GitHub Actions cross-platform matrix
- Runtime paths via dirs crate
- Cross-platform font discovery with JetBrainsMono Nerd Font

### Phase 1: Semantic Scrollback + SequenceInterceptor ✅ COMPLETE

Built the semantic foundation that everything else queries.

**Completed:**
- `SequenceInterceptor` trait in vendored VTE — intercepts OSC/DCS/APC sequences before they reach the terminal handler (xterm.js `addOscHandler()` pattern)
- `TherminalInterceptor` implementation handling OSC 133, 633, 7, 1337 families
- In-memory semantic region index with typed regions (Prompt, Command, Output, Error, ToolCall, Thinking, Annotation) queryable by kind, line, or recency
- Shell integration scripts for bash, zsh, fish, and PowerShell emitting OSC 133 marks
- Ghostty-style TERM_PROGRAM detection (TERM_PROGRAM=therminal, THERMINAL_RESOURCES_DIR env vars)
- Process tree agent detection via sysinfo (Claude Code, Codex, Aider, Copilot)
- Output cadence analysis — classifies output as Human, Agent, Burst, or Unknown based on timing, chunk size, and backspace patterns
- Spinner pattern detection (cursor-control-heavy output)

### Phase 2: Session Daemon + Multiplexing ✅ COMPLETE

#### Daemon Lifecycle (learned from thermal-desktop pain)

The biggest source of bugs in thermal-desktop was daemon lifecycle: duplicate daemons after rebuilds, stale sockets from killed processes, systemd restarting old binaries while new ones launched. Therminal solves this from day one.

**Problem**: During development, `cargo install` overwrites the binary but the old process keeps running the old code. Starting a new daemon creates duplicates. The old one holds sockets. Clients connect to the wrong one. Chaos.

**Solution: Socket-as-lock + binary version check**

No pidfiles. No systemd. The listening socket IS the liveness proof:

```
ensure_daemon():
  1. Try connect to socket
  2. Connected → send Ping, check version
     a. Version matches → use this daemon (return Connected)
     b. Version mismatch → send GracefulShutdown, wait, start new daemon
  3. Connection refused → stale socket, unlink it, start new daemon
  4. No socket exists → start new daemon
```

Every `therminal` launch calls `ensure_daemon()`. Zero explicit start/stop commands.

**Zero-downtime handoff** (no race condition, safe rollback on crash):
```
1. New daemon binds TEMPORARY socket (therminal.sock.new)
2. Connects to old daemon via therminal.sock, sends GracefulShutdown
3. Old daemon stops accepting NEW connections, keeps serving existing ones
4. Old daemon replies: DrainReady
5. New daemon renames therminal.sock.new → therminal.sock (atomic)
6. New daemon sends: HandoffComplete to old daemon
7. Old daemon closes its fd and exits
```
**Rollback safety**: The old daemon never closes its fd until it receives `HandoffComplete`. If the new daemon crashes at any point before step 6, the old daemon hits a timeout (5s), resumes accepting connections on its existing fd (still valid — rename changes the path, not the fd), and logs a warning. Next `therminal` launch retries the handoff or starts fresh.

**Binary version tracking**: The daemon embeds its build hash at compile time:
```rust
const BUILD_HASH: &str = env!("THERMINAL_BUILD_HASH"); // set by build.rs from binary content hash
```
The `Ping`/`Pong` health check includes this hash. When a client connects with a newer hash, the old daemon knows it's stale and shuts down gracefully — draining sessions to the new daemon. No orphans, no duplicates.

**Daemon state machine**:
```
Starting → Binding → Ready → Running → Draining → Stopped
         ↗                              ↑
  (stale socket cleanup)     (GracefulShutdown received,
                              or binary version mismatch,
                              or last session closed)
```

**Idle exit**: Daemon exits when last session closes + no clients connected (configurable via `keep_alive = true`).

**Health check built into protocol**:
```
Client: Ping
Daemon: Pong { uptime, sessions, version, build_hash }
```
No response in 2 seconds = dead, regardless of filesystem state.

#### Daemon Architecture
Hybrid model inspired by WezTerm: central daemon process manages session registry, event bus, and IPC listener. Per-session PTY workers (Rust threads, not separate processes) provide isolation through ownership semantics. Each worker maintains headless alacritty_terminal state, so reconnection sends a state snapshot rather than replaying bytes.

Note: daemon mode means double emulation (daemon headless + client render). WezTerm's insight: default to in-process mux (single parse, Phase 0) and only use daemon path when persistence is needed. The daemon is opt-in, not required.

#### Other Phase 2 features
- Session persistence (attach/detach)
- Cross-platform IPC via interprocess crate
- Basic split panes (manual)
- TOML config (profiles, keybindings, colors)
- Control mode (machine-readable protocol for scripting, like tmux -CC)

#### Phase 2 UX extensions (shipped alongside Phase 3)
- Workspace tabs (`WorkspaceManager`, Alt+1..9 / Alt+Shift+1..9)
- Mouse-drag separator resize
- Pane swap (Alt+Shift+Arrow, `SwapNext`/`SwapPrev` actions)
- Font size keybindings (Ctrl+Shift+=/-/0)
- Keybinding help overlay (Ctrl+Shift+?)
- GPU-rendered right-click context menus
- Status bar (agent indicator, CWD from OSC 7, exit code from OSC 633 D)

### Phase 3: AI Detection + Hotspots ✅ COMPLETE

All four detection layers shipped, plus MCP server, hotspot engine, and rendering.

**Completed:**
- MCP server in daemon (`mcp.rs`, `rmcp` crate, Unix socket) with stdio bridge (`therminal mcp` subcommand for Claude Code integration)
- Per-agent trust tiers (Sandboxed / Supervised / Trusted) enforced at MCP layer (`trust.rs`)
- Sliding-window rate limiter for destructive MCP tools
- Audit logging for all MCP tool invocations
- MCP socket path configurable via `[mcp]` in `therminal.toml`
- Layer 4: Composite state machine (`InferredStatus` enum) combining all signals into six states (Idle, Processing, Streaming, ToolUse, AwaitingInput, Thinking) with confidence tracking
- Custom OSC 7777 extension for cooperative agent self-reporting (JSON payload: agent, state, tool, tokens, model)
- Hotspot detection engine (file paths with :line:col, URLs, error locations, git refs, issue refs) with priority-based overlap suppression
- OSC 8 hyperlink protocol (solid underline for OSC 8, dashed for regex URLs, dotted for hotspots)
- Hotspot rendering: underline styles, pointer cursor on hover, GPU-rendered action palette on click

### Phase 4: MCP Workspace Protocol ✅ COMPLETE

**Completed:**
- 15 MCP tools across 5 domains (sessions, panes, semantic, workspaces, agents) with `workspace.terminal.*` naming convention
- MCP Resources: `terminal://pane/{id}/content` (snapshot) and `terminal://pane/{id}/output` (live stream with subscriptions)
- Trust tier enforcement with rate limiting and audit logging
- Session/workspace state persistence and sync with daemon

**Deferred:**
- Coordinate schema with TabzChrome's `workspace.browser.*` tools (tracked: tn-auig)

### Phase 5: Swarm Tiling + WebView Panes ✅ COMPLETE (WebView deferred)

**Completed:**
- Auto-tiling when subagents spawn/finish (`AutoTileDebouncer` with debounced spawn/exit)
- Geometry-aware layout (`LayoutNode` binary tree with split ratio tracking)
- Agent registry with live status tracking per pane (`AgentRegistry` with event channel)
- PaneBackend abstraction: Terminal | WebView (trait-based)
- WM-style split targeting: split largest pane, enforce minimums

**Deferred:**
- WebView panes via wry (tn-437, deferred to June 2026) — stub backend exists, wry integration pending

### Phase 6: Overlay Widgets + Polish (In Progress)

Overlay infrastructure exists (dual text renderers, caching, chrome rendering). Specific widgets remaining.

**Completed:**
- Overlay rendering infrastructure (dual atlas, text cache, shaped buffer management)
- Chrome overlays: pane headers, separators, status bar, tab bar, CSD buttons, focus border

**Remaining (tracked in beads):**
- Two-pass GPU rendering with alpha-blended overlay layer (tn-9k2) — foundation for all widgets
- Widget plugin/extension architecture (tn-8hdt) — trait-based plugin system so specific widgets (context gauge, tool cards, thinking indicator) live outside core. Keeps harness-specific integrations maintainable as the agent ecosystem grows.
- Widget pre-rasterization via tiny-skia (tn-npd)
- Semantic scrollback navigation UI (tn-bh9)
- Trust tier escalation modals (tn-b99)
- Optional TTS integration (tn-b58)

---

## Open Questions

- [x] License — MIT
- [x] Config format — TOML (`therminal.toml`)
- [x] CI — GitHub Actions cross-platform matrix
- [x] OSC 7777 spec — designed and implemented (JSON payload over `OSC 7777 ; {json} ST`); publish as open standard (tn-pjg1)
- [ ] Default theme — thermal aesthetic? Or neutral with thermal as an option? (tn-8ytq)
- [ ] Distribution — cargo install? Homebrew? Winget? Flatpak? (tn-jern)
- [ ] libghostty-vt — monitor maturity for potential future backend swap
- [ ] Atuin integration — SQLite schema compatibility for shell history portability
