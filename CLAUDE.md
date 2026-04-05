# Therminal

The AI-native terminal emulator. Cross-platform, GPU-accelerated, built for the era of AI agents.

## Status

**Phase 0, 1, and 2 complete.** The terminal renders with wgpu, runs a shell, handles keyboard + mouse input, has a SequenceInterceptor for AI-aware OSC parsing, semantic region indexing, shell integration scripts, process tree agent detection, output cadence analysis, persistent multiplexed sessions via daemon with socket-as-lock, zero-downtime handoff, IPC protocol (MessagePack framing), split panes, TOML config with hot-reload, and control mode. Next: Phase 3 (AI Detection + Hotspots).

## Architecture

Cargo workspace with six crates:

```
crates/
├── therminal-protocol/    # Wire types, MCP schema, semantic events
├── therminal-terminal/    # PTY, OSC parsing, state inference, agent detection, region index
├── therminal-core/        # Color palette, wgpu context, text renderer, TOML config, hot-reload
├── therminal-runtime/     # Cross-platform paths, runtime dir management
├── therminal-daemon/      # Session manager, event bus, multiplexer, MCP server
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

## Configuration System

TOML-based config with hot-reload, implemented in `therminal-core`.

### Config File

Location: `therminal_runtime::paths::config_dir() / "therminal.toml"` (e.g. `~/.config/therminal/therminal.toml` on Linux).

Sections: `[general]` (window, scrollback, shell), `[font]` (family, size, line_height_scale), `[colors]` (hex overrides for palette), `[keybindings]` (key/action pairs), `[profiles]` (named session profiles), `[trust]` (agent trust tiers).

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

## Code Style

### Module Size
Prefer small, focused modules over large monolithic files. When a file exceeds ~500 lines, consider splitting it into submodules with a clear single responsibility each. Use `mod.rs` or named modules to organize.

For example, `therminal-app/src/window.rs` is overdue for splitting — mouse handling, keybinding dispatch, chrome rendering, and pane management should each be separate modules.

### General
- Keep functions short and focused — if a function needs a section comment, it's probably a candidate for extraction
- No premature abstraction, but do extract when the same pattern appears in 3+ places
- Rust idioms: prefer `?` over `.unwrap()`, use `thiserror` for public error types

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
