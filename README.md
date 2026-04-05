# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

**Early development.** The terminal renders, runs a shell, and handles keyboard input. Not yet feature-complete.

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
  therminal-terminal/    PTY management, OSC 633, state inference engine
  therminal-core/        Color palette, wgpu context, text renderer
  therminal-runtime/     Cross-platform IPC, locks, paths
  therminal-daemon/      Session manager, event bus, multiplexer, MCP server
  therminal-app/         winit window, grid renderer, overlays, tiling
```

### Core Stack

| Layer | Technology |
|-------|-----------|
| GPU rendering | wgpu + glyphon + cosmic-text |
| Terminal emulation | alacritty_terminal (vendored) |
| Windowing | winit (cross-platform) |
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
