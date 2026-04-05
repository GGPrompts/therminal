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
- Batch close and restore with layout snapshots
- GPU-rendered right-click context menus
- Keybinding help overlay (Ctrl+Shift+?)
- Auto-tiled pane spawn (Alt+Enter)
- TOML config with hot-reload via file watcher
- Control mode (tmux -CC style machine-readable protocol)

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

## Architecture

Cargo workspace with six crates:

```
crates/
  therminal-protocol/    Wire types, MCP schema, semantic events
  therminal-terminal/    PTY, OSC parsing, state inference, agent detection, region index
  therminal-core/        Color palette, wgpu context, text renderer, TOML config, hot-reload
  therminal-runtime/     Cross-platform paths, runtime dir management
  therminal-daemon/      Session manager, event bus, multiplexer, IPC server, MCP server (stub)
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

Therminal runs on WSL2 via WSLg (Wayland). Known limitations:

- **Maximize may crash** due to a WSLg compositor bug (SIGSEGV in the Vulkan driver). This does not affect native Linux, macOS, or Windows builds.
- **Software rendering** via llvmpipe if no GPU passthrough is configured. Performance is acceptable but not optimal.
- Vulkan backend is preferred automatically on Linux to avoid WSLg EGL/GLES instability.

For production use on Windows, the target is a native Windows build (not WSLg).

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
