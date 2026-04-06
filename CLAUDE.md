# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

**Phases 0, 1, and 2 complete; Phase 3 UX in progress.** The terminal renders with wgpu, runs a shell, handles keyboard + mouse input, has a SequenceInterceptor for AI-aware OSC parsing, semantic region indexing, shell integration scripts, process tree agent detection, output cadence analysis, persistent multiplexed sessions via daemon with socket-as-lock, zero-downtime handoff, IPC protocol (MessagePack framing), split panes with mouse-drag separator resize, workspace tabs, pane swap, font size keybindings, keybinding help overlay, right-click context menus, TOML config with hot-reload, control mode, and an MCP server with trust tier enforcement.

## Architecture

Cargo workspace with six crates:

```
crates/
├── therminal-protocol/    # Wire types, MCP schema, semantic events
├── therminal-terminal/    # PTY, OSC parsing, state inference, agent detection, region index
├── therminal-core/        # Color palette, wgpu context, text renderer, TOML config, hot-reload
├── therminal-runtime/     # Cross-platform paths, runtime dir management
├── therminal-daemon/      # Session manager, event bus, multiplexer, MCP server, trust enforcement
└── therminal-app/         # winit window, grid renderer, mouse input, PTY wiring
vendor/
├── alacritty_terminal/    # Vendored v0.25.1
└── vte/                   # Vendored with SequenceInterceptor trait
resources/
└── shell-integration/     # bash, zsh, fish, PowerShell scripts
```

### Core Stack
- **GPU rendering**: wgpu + glyphon + cosmic-text
- **Terminal emulation**: alacritty_terminal (vendored) + VTE with SequenceInterceptor
- **Windowing**: winit (cross-platform)
- **PTY**: portable-pty (cross-platform)
- **Configuration**: toml + serde (TOML config), notify (file watcher for hot-reload)
- **Agent detection**: sysinfo (process tree), cadence analysis (output stream timing)
- **IPC**: interprocess crate (Unix sockets / named pipes)
- **Wire protocol**: MessagePack framing
- **Language**: Rust

### Daemon Lifecycle

The daemon uses a **socket-as-lock** pattern -- successful socket bind = ownership of the daemon role, no pidfiles needed.

**BUILD_HASH**: `build.rs` in `therminal-daemon` embeds `<git-short-hash>-<unix-timestamp>` at compile time via `env!("BUILD_HASH")`. Used for version-mismatch detection during handoff.

**State machine**: `Starting -> Binding -> Ready -> Running -> Draining -> Stopped`

**`ensure_daemon()` startup protocol**:
1. Try connect to daemon socket, send `Ping`, check `Pong { build_hash }`
2. Version match: reuse existing daemon (`EnsureResult::Reused`)
3. Version mismatch: send `GracefulShutdown`, wait for old daemon to drain, start new daemon
4. Connection refused / no socket: clean stale socket, start new daemon

**Zero-downtime handoff**: New daemon sends `GracefulShutdown` to old daemon, waits for socket to be released (5s timeout), then binds the canonical socket path. Rollback on crash removes temp socket.

**Health check**: `Ping` / `Pong { uptime, sessions, version, build_hash }` with 2s timeout over length-prefixed MessagePack framing.

**Idle exit**: Daemon exits when last session closes + configurable `keep_alive` duration (default 5 minutes).

Key files: `ensure.rs` (entry point), `lifecycle.rs` (state machine), `server.rs` (IPC server), `client.rs` (IPC client), `handoff.rs` (version handoff).

### IPC Protocol

The daemon exposes a multiplexed IPC protocol over Unix domain sockets with length-prefixed MessagePack framing.

**Wire format**: `[4-byte BE length][MessagePack payload]`. Max frame size: 1 MiB.

**Envelope** (`IpcMessage`): Three variants -- `Request { request_id, payload }`, `Response { request_id, payload }`, `Event { payload }`. The `request_id: u64` enables multiplexing multiple in-flight requests over one connection.

**Requests** (`IpcRequest`): `Ping`, `GracefulShutdown`, `Subscribe { filter }`, `Unsubscribe`, `ListSessions`, `GetSession`, `CreateSession`, `DestroySession`, `GetState`.

**Responses** (`IpcResponse`): `Pong`, `ShutdownAck`, `Subscribed`, `Unsubscribed`, `Sessions`, `SessionInfo`, `SessionCreated`, `SessionDestroyed`, `State`, `Error`.

**Events** (`DaemonEvent`): `StateChanged`, `SessionCreated`, `SessionDestroyed`, `PaneOutput`. Clients subscribe via `Subscribe { filter: Vec<EventKind> }` -- empty filter = all events.

**Client API** (`DaemonClient`): Persistent connection with `connect()`, `send_request()`, `ping()`, `shutdown()`, `subscribe_events()`, `recv_event()`. Uses internal reader/writer tasks for full-duplex communication.

**Server** (`IpcServer`): Accepts connections, dispatches to handlers, manages per-connection event subscriptions via `tokio::sync::broadcast`. Auto-detects legacy vs IPC protocol on first frame.

**Backward compatibility**: The server auto-detects legacy `DaemonRequest` frames (used by `ensure_daemon()` and handoff) vs new `IpcMessage` frames. Legacy single-shot `send_request()` function is preserved. `DaemonServer` is a type alias for `IpcServer`.

Protocol types live in `therminal-protocol/src/daemon.rs`. Server/client in `therminal-daemon/src/{server,client}.rs`.

### Session Manager

The daemon provides persistent multiplexed sessions via a `Session -> Window -> Pane` hierarchy managed by `SessionManager` in `therminal-daemon/src/session.rs`.

**Hierarchy**: `SessionManager` owns a `HashMap<SessionId, Session>`. Each `Session` contains `Vec<Window>`, each `Window` contains `Vec<Pane>`. A new session gets one default window with one pane.

**Pane PTY workers**: Each `Pane` spawns a shell via `therminal_terminal::pty::spawn_shell()` and owns:
- A `portable_pty::MasterPty` (kept alive to prevent PTY close)
- A `Box<dyn Write>` for forwarding client keystrokes to the PTY
- An `Arc<FairMutex<alacritty_terminal::Term>>` with a headless `EventListener` -- no GPU, just terminal state
- A dedicated reader thread that reads PTY output, feeds it through `vte::ansi::Processor` into the `Term`, and broadcasts `DaemonEvent::PaneOutput` to subscribed clients

**Attach/detach protocol**: On attach, the daemon takes a `PaneSnapshot` from each pane's `Term` state -- grid content (chars + bold flags), cursor position, and dimensions. This is a state snapshot, not a byte replay. The client renders this snapshot to immediately show the current terminal state.

**Session CRUD via IPC**: `CreateSession` spawns a real PTY and returns the session ID. `ListSessions`, `GetSession`, `DestroySession` operate on the session map. Session count is synced to the `Lifecycle` for idle-exit tracking.

**Keystroke forwarding**: Client sends input bytes via IPC, dispatched through `SessionManager::write_to_pane()` to the pane's PTY writer.

**Graceful shutdown**: `IpcServer::run()` calls `SessionManager::shutdown()` on exit, which destroys all sessions (dropping PTY masters, causing reader threads to get EOF and exit).

### Status Bar

A full-width status bar at the bottom of the window (24px tall) with three sections:

- **Left**: Agent indicator (`[agent: <name>]`) when a process-tree agent is detected and `trust.show_agent_indicator` is enabled. Hidden otherwise.
- **Center**: Current working directory (from OSC 7), with home directory abbreviated to `~`.
- **Right**: Pane dimensions (`cols x rows`) and last command exit code (from OSC 633 D mark), color-coded green (exit 0) or red (non-zero).

**Data flow**: The PTY reader thread in `pane.rs` drains `InterceptedEvent`s from the `TherminalInterceptor` and `ProcessDetector` results into a shared `Arc<Mutex<PaneStatus>>` on `PaneState`. The render loop reads this lock-free snapshot to populate `StatusBarInfo` passed to `draw_status_bar()` in `chrome.rs`.

**Config**: `general.show_status_bar` (default `true`) controls visibility. When disabled, panes use the full window height. `trust.show_agent_indicator` controls whether the agent name appears in the left section.

Key files: `crates/therminal-app/src/window/chrome.rs` (rendering), `crates/therminal-app/src/pane.rs` (`PaneStatus`, PTY reader wiring).

### Workspace Tabs

Named workspaces (`WorkspaceManager` in `pane.rs`) let users group pane layouts under numbered slots. Each workspace independently owns a `LayoutNode` binary tree. Switching workspaces swaps the entire layout; the previous workspace's layout is preserved in memory.

**Keybindings**: `Alt+1`..`Alt+9` switch to workspace N; `Alt+Shift+1`..`Alt+Shift+9` send the focused pane to workspace N (actions `SwitchWorkspace(n)` / `SendToWorkspace(n)` in config).

### Pane Swap

`LayoutNode::swap_pane(a, b)` walks the binary tree and swaps the positions of two leaves in place, preserving the split structure. Exposed via keyboard actions `SwapNext` (default `Alt+Shift+Right`) and `SwapPrev` (default `Alt+Shift+Left`).

### Mouse-Drag Separator Resize

Clicking and dragging a pane separator adjusts the `split_ratio` of the enclosing `LayoutNode::Split`. Implemented in `window/mouse.rs` (`try_start_separator_drag`, `update_separator_drag`, `end_separator_drag`). Hit-testing uses a 6 px threshold around each separator.

### Font Size Keybindings

`GridRenderer` exposes `adjust_font_size(delta)` (clamped to 8–32 pt) and `reset_font_size()`. Default keybindings: `Ctrl+=` increase (also `Ctrl+Shift+=`), `Ctrl+-` decrease, `Ctrl+0` reset (actions `FontSizeUp`, `FontSizeDown`, `FontSizeReset`).

### Keybinding Help Overlay

`Ctrl+Shift+?` toggles a full-window overlay (`show_help_overlay` in `App`) rendered by `window/help_overlay.rs`. The overlay reads all active bindings from the config-driven binding map and groups them by category (General, Font, Pane Management). Pressing any key or clicking outside closes it.

### Right-Click Context Menu

`window/mouse.rs` detects right-click and calls `menu::show_context_menu()`, which renders a GPU-drawn floating menu (`crates/therminal-app/src/menu.rs`). Menu items cover common pane actions (split, close, zoom, copy, paste).

### MCP Server

`crates/therminal-daemon/src/mcp.rs` implements an MCP server (`rmcp` crate) listening on a Unix socket (configurable via `[mcp] socket_path` in `therminal.toml`, defaults to `<runtime_dir>/mcp.sock`). `crates/therminal-app/src/mcp_stdio.rs` provides a stdio bridge (`therminal mcp` subcommand) that proxies stdin/stdout to the daemon's MCP socket, enabling MCP clients like Claude Code to connect as a subprocess.

Tools exposed:

| Tool | Category | Description |
|------|----------|-------------|
| `list_sessions` | Observer | List all session IDs |
| `get_session` | Observer | Get session metadata |
| `read_pane_content` | Observer | Read visible pane content |
| `create_session` | Writer | Spawn a new PTY session |
| `write_to_pane` | Writer | Send input to a pane's PTY |
| `destroy_session` | Admin | Kill a session |

Agent identity is extracted from the MCP `initialize` handshake and passed to trust enforcement on every tool call. Both the daemon and the stdio bridge read `[mcp]` config via `McpConfig::resolved_socket_path()` — a single source of truth in `therminal-core`.

### Trust Tier Enforcement

`crates/therminal-daemon/src/trust.rs` maps MCP tools to three permission categories (Observer, Writer, Admin) and enforces access control on every call:

| Tier | Name | MCP Access |
|------|------|-----------|
| `Sandboxed` | Read-only | Observer tools only |
| `Supervised` | Default | Observer + Writer tools |
| `Trusted` | Full | All tools including Admin |

Agent tiers are set per-agent in `[trust]` config, with a `default_tier` fallback. Destructive (Admin) tools are additionally subject to a sliding-window rate limiter (configurable `max_destructive_per_minute`). All allow/deny decisions are audit-logged via `tracing`.

Key files: `crates/therminal-daemon/src/mcp.rs` (server), `crates/therminal-daemon/src/trust.rs` (enforcement + rate limiter), `crates/therminal-app/src/mcp_stdio.rs` (stdio bridge), `crates/therminal-core/src/config.rs` (`McpConfig`).

### Control Mode

`crates/therminal-daemon/src/control.rs` implements a machine-readable text protocol (tmux `-CC` style). The `--help-control` CLI flag prints the full protocol reference. The `help` command within a control session returns the same reference inline.

## Configuration System

TOML-based config with hot-reload, implemented in `therminal-core`.

### Config File

Location: `therminal_runtime::paths::config_dir() / "therminal.toml"` (e.g. `~/.config/therminal/therminal.toml` on Linux).

Sections: `[general]` (window, scrollback, shell), `[font]` (family, size, line_height_scale), `[colors]` (hex overrides for palette), `[keybindings]` (key/action pairs), `[profiles]` (named session profiles), `[trust]` (agent trust tiers), `[mcp]` (MCP server enable/disable, socket path).

All fields have sensible defaults matching the current hardcoded values. Missing fields fall back to defaults. Invalid TOML logs a warning and uses full defaults.

### Hot-Reload

`ConfigWatcher` (in `config_watcher.rs`) uses the `notify` crate to watch the config directory. Events are debounced (500ms) to handle editor atomic-write patterns. On change, the config is reloaded and a `ConfigChanged` event is sent to the winit event loop via a bridge thread. The `App::apply_config()` method applies changes (window title, font metrics, grid resize) without restart.

### Key Files

| File | Purpose |
|------|---------|
| `crates/therminal-core/src/config.rs` | `TherminalConfig` struct, TOML serde, load/save |
| `crates/therminal-core/src/config_watcher.rs` | `ConfigWatcher`, debounced file watching |
| `crates/therminal-app/src/window.rs` | Config wiring into event loop, `apply_config()` |

## Shell Integration

Therminal uses Ghostty-style `TERM_PROGRAM` detection. When spawning a PTY, three env vars are set:

- `TERM_PROGRAM=therminal` -- shells use this to detect the terminal and auto-source integration scripts
- `TERM_PROGRAM_VERSION` -- the crate version from `Cargo.toml`
- `THERMINAL_RESOURCES_DIR` -- absolute path to the resources directory containing shell scripts

Shell integration scripts live in `resources/shell-integration/` (bash, zsh, fish, PowerShell). Each script emits OSC 133 marks (A=PromptStart, B=PromptEnd, C=PreExec, D=CommandFinished) and OSC 7 for current directory. All scripts guard against double-sourcing via `__THERMINAL_SHELL_INTEGRATION`.

## WSL2

Therminal runs fine on WSL2. The quirks below are documented for future contributors.

### What works without changes

- **PTY & resize**: `portable-pty` uses `forkpty(3)` + `TIOCSWINSZ` ioctl — identical behaviour on WSL2 Linux PTYs. `SIGWINCH` delivery works correctly.
- **`TERM` variable**: therminal does not call `alacritty_terminal::tty::setup_env()`, so `TERM` is inherited from the parent process. Under Windows Terminal this is typically `xterm-256color`; the `alacritty` terminfo entry is also present on Ubuntu-24.04 WSL2, so either works.
- **Env var forwarding / `WSLENV`**: `portable-pty`'s `CommandBuilder::new()` initialises from `std::env::vars_os()`, so every env var in the therminal process — including `WSLENV`, `WSL_DISTRO_NAME`, `WSL_INTEROP`, and anything forwarded by Windows Terminal — is automatically inherited by child shells. No explicit forwarding code is needed.
- **XDG directories**: `dirs` crate reads `$XDG_CONFIG_HOME`, `$XDG_RUNTIME_DIR`, etc., all of which are set correctly by WSL2's systemd/elogind shim. Config, data, cache, and socket paths resolve to expected Linux locations under `~/.config/`, `~/.cache/`, and `/run/user/<uid>/`.
- **OSC 7 CWD reporting**: shell integration scripts emit `file://$(hostname)${PWD}`. When inside a Linux directory (`/home/...`) the interceptor strips the hostname and passes the Linux path to the status bar. Home-dir abbreviation to `~` works correctly.

### WSL2-specific fixes applied

- **Windows home abbreviation in status bar** (`crates/therminal-app/src/window/chrome.rs`): When navigating Windows-side directories (e.g., `/mnt/c/Users/alice/Documents`), the Linux `$HOME` prefix doesn't match, so the full path would appear. The `abbreviate_path()` function now detects WSL2 via `WSL_DISTRO_NAME` and reads `USERPROFILE`/`HOMEDRIVE`+`HOMEPATH` (forwarded by Windows Terminal) to abbreviate `/mnt/c/Users/<user>` to `~win`.
- **Process detector false positives from Windows interop** (`crates/therminal-terminal/src/process_detector.rs`): Windows `.exe` files launched via WSL2 binfmt_misc interop appear as Linux PIDs whose cmdline begins with `/init /mnt/c/...`. A Windows `node.exe` with `claude` in its path would otherwise trigger the Claude agent detector. `is_wsl2_interop_process()` filters these out before classification.

### Known limitations

- **OSC 7 in `/mnt/c/` paths**: The hostname in the OSC 7 URI is the WSL2 distro hostname (e.g., `matt`), not `localhost`. The interceptor already handles arbitrary hostnames correctly (strips the hostname portion from `file://hostname/path`), so this is a non-issue.
- **`USERPROFILE` forwarding**: The `~win` abbreviation in the status bar only works if `USERPROFILE` (or `HOMEDRIVE`+`HOMEPATH`) is forwarded from Windows. Windows Terminal forwards these by default; other launchers may not. If not forwarded, `/mnt/c/` paths appear unabbreviated — acceptable fallback.
- **Windows process visibility**: `sysinfo` reads `/proc` and sees Windows interop processes as regular Linux PIDs. The interop guard in `process_detector.rs` prevents false agent detections, but `scan()` still traverses these PIDs during the BFS walk. This adds a small overhead on systems with many Windows interop processes. Not a correctness issue.
- **GPU / wgpu on WSL2**: wgpu requires a Vulkan ICD. Under WSL2 this works via `d3d12` (WSL2 GPU paravirtualisation) when running with a GUI. Headless WSL2 sessions (no `DISPLAY` / `WAYLAND_DISPLAY`) will fail at window creation — this is expected; therminal is a GUI app.

## Code Style

### Module Size
Prefer small, focused modules over large monolithic files. When a file exceeds ~500 lines, consider splitting it into submodules with a clear single responsibility each. Use `mod.rs` or named modules to organize.

`therminal-app/src/window/` was split this way: `mod.rs` (event loop), `mouse.rs`, `keybindings.rs`, `chrome.rs`, `help_overlay.rs`, `pane_ops.rs`, `render.rs`.

### General
- Keep functions short and focused — if a function needs a section comment, it's probably a candidate for extraction
- No premature abstraction, but do extract when the same pattern appears in 3+ places
- Rust idioms: prefer `?` over `.unwrap()`, use `thiserror` for public error types
- **Config fields must be wired**: if a config struct has a field, code must read it. Don't declare config options that nothing uses — dead config misleads users and future contributors. `TherminalConfig` in `therminal-core` is the single source of truth; other crates consume it, not duplicate it.

## Building & Testing

Run `./scripts/ci.sh` before committing code changes. This runs the same checks as GitHub Actions CI:

```bash
./scripts/ci.sh        # fmt check, clippy, build, test — all workspace
```

Individual steps if needed:
```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo build --workspace
cargo test --workspace
```

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
   bd-push
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
