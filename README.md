# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

**Active development — Phases 0–5 complete, Phase 6 (overlay widgets) in progress.**

| Phase | Description | Status |
|-------|-------------|--------|
| 0 | GPU rendering + shell | Complete |
| 1 | OSC parsing, shell integration, agent detection | Complete |
| 2 | Daemon, IPC, multiplexing, config, pane chrome | Complete |
| 3 | AI detection, hotspots, UX polish | Complete |
| 4 | Daemon handoff, session persistence, MCP tools | Complete |
| 5 | Live agent registry, MCP Resources, auto-tiling | Complete |
| 6 | Overlay widgets (status bar, tool cards, indicators) | In Progress |

### What's implemented

**GPU rendering & input**
- GPU-accelerated terminal rendering (wgpu + glyphon + cosmic-text)
- Two-pass overlay rendering (grid pass + OverlayLayer pass with alpha blending)
- Full keyboard input (Kitty protocol) and mouse input (scroll, click, SGR 1006)
- Clipboard via arboard + OSC 52

**Shell integration & AI awareness**
- Shell integration scripts (bash, zsh, fish, PowerShell) emitting OSC 133 semantic marks
- SequenceInterceptor — AI-aware OSC parsing between VTE and terminal handler
- Semantic region index — typed regions (Prompt, Command, Output, Error, ToolCall, Thinking)
- Semantic scrollback navigation — jump between command blocks
- Process tree agent detection via sysinfo (Claude Code, Codex, Aider, Copilot)
- Output cadence analysis — distinguishes human typing from agent output
- Live AgentRegistry tracking agent status (Idle/Processing/Streaming/Thinking/ToolUse/AwaitingInput) across panes
- Live Claude Code session observability: `/tmp/claude-code-state/` watcher + JSONL tailers stream every `UserMessage`, `AssistantMessage`, `ToolUse`, `ToolResult`, `Thinking`, and `Progress` event from parent sessions and Task-tool subagents in real time. Events are tagged with `EventSource::{TopLevel, Subagent}` so consumers can reconstruct the session tree. Exposed over MCP as `therminal://claude/events` (see "Dev tools" below for a CLI viewer).

**Daemon & multiplexing**
- Session daemon with socket-as-lock lifecycle and zero-downtime handoff via SCM_RIGHTS FD passing
- Debounced session state persistence (`sessions.json`) — survives daemon restart
- IPC protocol (MessagePack framing, multiplexed request/response, event streaming)
- Session → Window → Pane hierarchy with persistent PTY workers
- Attach/detach with state snapshots — no byte replay needed

**Pane management & UX**
- Split panes with binary layout tree and keyboard/shortcut controls
- Mouse-drag separator resize (click and drag pane borders)
- Auto-tiling for agent swarms — panes spawn/reclaim as agents start and exit (debounced)
- PaneBackend abstraction (Terminal | WebView — WebView stubbed for future hybrid panes)
- Per-pane headers showing the real daemon `PaneId`, plus Claude session title or cwd basename when available
- Status bar pane identity and smarter workspace-tab labels based on the focused pane context
- Workspace tabs (Alt+1..9 to switch, Alt+Shift+1..9 to send pane)
- Pane swap (Alt+Shift+Arrow)
- Font size keybindings (Ctrl+Shift+=/-/0)
- Batch close and restore with layout snapshots
- GPU-rendered right-click context menus
- Keybinding help overlay (Ctrl+Shift+?)
- Auto-tiled pane spawn (Alt+Enter)
- TOML config with hot-reload via file watcher
- Control mode (tmux -CC style machine-readable protocol, --help-control flag)
- JSONL event logging for observability

**MCP server & trust**
- MCP server in the daemon with stdio bridge (`therminal mcp`) for Claude Code integration
- Cross-platform IPC: Unix sockets (Linux/macOS), named pipes (Windows)
- 29 tools covering sessions, panes, semantic queries, workspaces, agents, and event stats
- MCP Resources with subscription-based pane content streaming (`terminal://pane/{id}/output`) and live Claude Code event streaming (`therminal://claude/events`)
- Per-agent trust tiers (Sandboxed / Supervised / Trusted) enforced at the MCP handler layer
- Sliding-window rate limiter for destructive tools; all decisions audit-logged
- Configure in Claude Code: `{ "command": "therminal", "args": ["mcp"] }` in `.mcp.json`

### Integration philosophy

Therminal is **the platform, not the monolith**. AI harness integrations (Claude Code, Codex, Aider, etc.) are **documented**, not built as a plugin system. The MCP server is the stable interface — any harness that speaks MCP can use therminal's tools and resources without therminal shipping harness-specific code. See `docs/integrations/` for per-harness setup notes.

## Building

### Prerequisites

**Rust toolchain** (1.75+):
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**System dependencies (Ubuntu/Debian/WSL2):**
```bash
# GPU and windowing
sudo apt-get install -y libwayland-dev libxkbcommon-dev vulkan-tools

# Fonts
sudo apt-get install -y fonts-noto-color-emoji
```

**Recommended font:**

Install [JetBrainsMono Nerd Font](https://github.com/ryanoasis/nerd-fonts/releases/latest) for Nerd Font glyph support (powerline symbols, file icons, etc.):
```bash
mkdir -p ~/.local/share/fonts
cd ~/.local/share/fonts
curl -fLo JetBrainsMono.tar.xz \
  "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.tar.xz"
tar xf JetBrainsMono.tar.xz && rm JetBrainsMono.tar.xz
fc-cache -fv
```

The default font is `JetBrainsMono Nerd Font Mono` with `Noto Color Emoji` as fallback. If neither is installed, the terminal falls back to the system monospace font.

### Build & Run

```bash
# Build
cargo build --workspace

# Run
cargo run --bin therminal

# Run with log capture
./scripts/run.sh

# Run with debug logging
./scripts/run.sh --verbose
```

### CI

```bash
./scripts/ci.sh    # fmt check, clippy, build, test
```

### Dev tools

#### `claude-events` — live Claude Code session viewer

A small CLI subscriber that connects to the running daemon, subscribes to the `therminal://claude/events` MCP resource, and prints live `TaggedAgentEvent`s as styled terminal lines. Run it in one pane while Claude Code runs in another to see tool calls, results, and subagent activity stream in real time. Handy while waiting out long Task-tool subagent runs that would otherwise look like silence.

```bash
# Basic run — prints tool use, results, thinking, progress
cargo run -p therminal-harness-claude --bin claude-events

# Include UserMessage + AssistantMessage (noisy)
cargo run -p therminal-harness-claude --bin claude-events -- --verbose

# Only show subagent events
cargo run -p therminal-harness-claude --bin claude-events -- --filter sub

# Filter to one top-level session and its subagents
cargo run -p therminal-harness-claude --bin claude-events -- --session <session-uuid>

# Machine-readable JSON per line, pipe to jq
cargo run -p therminal-harness-claude --bin claude-events -- --json | jq .

# Disable ANSI colors (for logs / non-tty output)
cargo run -p therminal-harness-claude --bin claude-events -- --no-color
```

Output format (non-`--json`):

```
HH:MM:SS [top abc12345]  Bash        ls -la
HH:MM:SS [top abc12345]  ✓
HH:MM:SS [sub 7ee3d21a]    Grep        jsonl_tailer
HH:MM:SS [sub 7ee3d21a]    ✓
```

Top-level lines are tagged `top <sid8>`, subagent lines are tagged `sub <agent8>` and indented. Tool names are cyan, successful results green, errors red, thinking yellow.

This is a stopgap dev tool until the GPU timeline overlay widget (tracked as `tn-x85k`) ships with Phase 6. Any MCP client that speaks the resource subscription protocol can consume the same stream; the binary is also a working reference implementation of how to do so.

### Windows-native build

For native Windows GPU rendering, fullscreen, snap/tiling, and window-manager integration, build and run Therminal on Windows rather than through WSLg.

**Prerequisites (Windows side):**
- Rust toolchain: `winget install --id Rustlang.Rustup -e`
- MSVC Build Tools: `winget install --id Microsoft.VisualStudio.2022.BuildTools -e` (select "Desktop development with C++")
- If Windows Defender / Smart App Control blocks build scripts, add an exclusion: `Add-MpExclusion -Path "C:\Users\<you>\therminal-build\target"` (admin PowerShell)

**From WSL (recommended):**

```bash
# Syncs repo to Windows, builds natively, copies exe + resources to Desktop
./scripts/build-windows.sh

# Debug build
./scripts/build-windows.sh --debug
```

**From PowerShell on Windows:**

```powershell
.\scripts\build-windows.ps1
.\scripts\build-windows.ps1 -Debug -NoCopy
```

**Shell detection on Windows:** Therminal auto-detects the best available shell: WSL (if installed) > PowerShell 7 > PowerShell 5.1 > cmd.exe. WSL shells start in the Linux home directory with `TERM_PROGRAM=therminal` forwarded for shell integration. Override in `therminal.toml`:

```toml
[general]
shell = "powershell.exe"  # or "wsl.exe", "pwsh.exe", "cmd.exe"
```

## Architecture

Cargo workspace with nine crates:

```
crates/
  therminal-protocol/    Wire types, MCP schema, semantic events
  therminal-terminal/    PTY, OSC parsing, state inference, agent detection, region index
  therminal-core/        Color palette, wgpu context, text renderer, TOML config, hot-reload
  therminal-runtime/     Cross-platform paths, runtime dir management
  therminal-daemon/      Session manager, event bus, multiplexer, IPC server, MCP server, trust enforcement
  therminal-daemon-client/ Lightweight IPC client shared by the GUI, CLI, and harnesses
  therminal-harness-claude/ Claude Code integration, state watcher, and `claude-events` dev tool
  therminal-app/         winit window, grid renderer, mouse input, PTY wiring
  therminal-integration-tests/ End-to-end daemon + PTY scenarios
vendor/
  alacritty_terminal/    Vendored v0.25.1
  vte/                   Vendored with SequenceInterceptor trait
resources/
  shell-integration/     bash, zsh, fish, PowerShell integration scripts
```

### Core Stack

| Layer | Technology |
|-------|-----------|
| GPU rendering | wgpu + glyphon + cosmic-text |
| Terminal emulation | alacritty_terminal (vendored) + VTE with SequenceInterceptor |
| Windowing | winit (cross-platform) |
| PTY | portable-pty (cross-platform) |
| Agent detection | sysinfo (process tree) + cadence analysis (output timing) |
| IPC | interprocess (Unix sockets / named pipes) |
| Wire protocol | MessagePack framing |

## Platform Notes

### WSL2

**Recommended:** Build therminal natively on Windows (`./scripts/build-windows.sh`) — it auto-detects WSL and launches your Linux shell with native Windows GPU rendering. This avoids WSLg compositor issues entirely.

**Running inside WSL2 via WSLg** (Wayland) also works but has limitations:

- **Fullscreen rendering issues** due to WSLg compositor / Vulkan paravirtualization quirks. Does not affect native Windows, Linux, or macOS builds.
- **Software rendering** via llvmpipe if no GPU passthrough is configured.
- Vulkan backend is preferred automatically to avoid WSLg EGL/GLES instability.

### macOS

Requires Metal-capable GPU (any Mac from 2012+). No additional dependencies.

### Linux (native)

Works with Wayland and X11. Install Vulkan drivers for your GPU:
```bash
# NVIDIA
sudo apt-get install -y nvidia-driver-XXX

# AMD
sudo apt-get install -y mesa-vulkan-drivers

# Intel
sudo apt-get install -y mesa-vulkan-drivers
```

## License

MIT
