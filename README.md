# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

**Active development — Phases 0–2 complete, Phase 3 in progress.**

| Phase | Description | Status |
|-------|-------------|--------|
| 0 | GPU rendering + shell | Complete |
| 1 | OSC parsing, shell integration, agent detection | Complete |
| 2 | Daemon, IPC, multiplexing, config, pane chrome | Complete |
| 3 | AI Detection + Hotspots + UX | In Progress |

### What's implemented

**GPU rendering & input**
- GPU-accelerated terminal rendering (wgpu + glyphon + cosmic-text)
- Full keyboard input (Kitty protocol) and mouse input (scroll, click, SGR 1006)
- Clipboard via arboard + OSC 52

**Shell integration & AI awareness**
- Shell integration scripts (bash, zsh, fish, PowerShell) emitting OSC 133 semantic marks
- SequenceInterceptor — AI-aware OSC parsing between VTE and terminal handler
- Semantic region index — typed regions (Prompt, Command, Output, Error, ToolCall, Thinking)
- Process tree agent detection via sysinfo (Claude Code, Codex, Aider, Copilot)
- Output cadence analysis — distinguishes human typing from agent output

**Daemon & multiplexing**
- Session daemon with socket-as-lock lifecycle and zero-downtime handoff
- IPC protocol (MessagePack framing, multiplexed request/response, event streaming)
- Session → Window → Pane hierarchy with persistent PTY workers
- Attach/detach with state snapshots — no byte replay needed

**Pane management & UX**
- Split panes with binary layout tree and keyboard/shortcut controls
- Mouse-drag separator resize (click and drag pane borders)
- Workspace tabs (Alt+1..9 to switch, Alt+Shift+1..9 to send pane)
- Pane swap (Alt+Shift+Arrow)
- Font size keybindings (Ctrl+Shift+=/-/0)
- Batch close and restore with layout snapshots
- GPU-rendered right-click context menus
- Keybinding help overlay (Ctrl+Shift+?)
- Auto-tiled pane spawn (Alt+Enter)
- TOML config with hot-reload via file watcher
- Control mode (tmux -CC style machine-readable protocol, --help-control flag)

**MCP server & trust**
- MCP server in the daemon with stdio bridge (`therminal mcp`) for Claude Code integration
- Cross-platform IPC: Unix sockets (Linux/macOS), named pipes (Windows)
- Tools: list_sessions, get_session, read_pane_content, create_session, write_to_pane, destroy_session
- Per-agent trust tiers (Sandboxed / Supervised / Trusted) enforced at the MCP handler layer
- Sliding-window rate limiter for destructive tools; all decisions audit-logged
- Configure in Claude Code: `{ "command": "therminal", "args": ["mcp"] }` in `.mcp.json`

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

Cargo workspace with six crates:

```
crates/
  therminal-protocol/    Wire types, MCP schema, semantic events
  therminal-terminal/    PTY, OSC parsing, state inference, agent detection, region index
  therminal-core/        Color palette, wgpu context, text renderer, TOML config, hot-reload
  therminal-runtime/     Cross-platform paths, runtime dir management
  therminal-daemon/      Session manager, event bus, multiplexer, IPC server, MCP server, trust enforcement
  therminal-app/         winit window, grid renderer, mouse input, PTY wiring
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
