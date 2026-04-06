# therminal-daemon

Session manager, event bus, multiplexer, MCP server, trust enforcement.

## Daemon Lifecycle

The daemon uses a **socket-as-lock** pattern -- successful socket bind = ownership of the daemon role, no pidfiles needed.

**BUILD_HASH**: `build.rs` embeds `<git-short-hash>-<unix-timestamp>` at compile time via `env!("BUILD_HASH")`. Used for version-mismatch detection during handoff.

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

## IPC Protocol

The daemon exposes a multiplexed IPC protocol over Unix domain sockets with length-prefixed MessagePack framing.

**Wire format**: `[4-byte BE length][MessagePack payload]`. Max frame size: 1 MiB.

**Envelope** (`IpcMessage`): Three variants -- `Request { request_id, payload }`, `Response { request_id, payload }`, `Event { payload }`. The `request_id: u64` enables multiplexing multiple in-flight requests over one connection.

**Requests** (`IpcRequest`): `Ping`, `GracefulShutdown`, `Subscribe { filter }`, `Unsubscribe`, `ListSessions`, `GetSession`, `CreateSession`, `DestroySession`, `GetState`.

**Responses** (`IpcResponse`): `Pong`, `ShutdownAck`, `Subscribed`, `Unsubscribed`, `Sessions`, `SessionInfo`, `SessionCreated`, `SessionDestroyed`, `State`, `Error`.

**Events** (`DaemonEvent`): `StateChanged`, `SessionCreated`, `SessionDestroyed`, `PaneOutput`. Clients subscribe via `Subscribe { filter: Vec<EventKind> }` -- empty filter = all events.

**Client API** (`DaemonClient`): Persistent connection with `connect()`, `send_request()`, `ping()`, `shutdown()`, `subscribe_events()`, `recv_event()`. Uses internal reader/writer tasks for full-duplex communication.

**Server** (`IpcServer`): Accepts connections, dispatches to handlers, manages per-connection event subscriptions via `tokio::sync::broadcast`. Auto-detects legacy vs IPC protocol on first frame.

**Backward compatibility**: The server auto-detects legacy `DaemonRequest` frames (used by `ensure_daemon()` and handoff) vs new `IpcMessage` frames. Legacy single-shot `send_request()` function is preserved. `DaemonServer` is a type alias for `IpcServer`.

Protocol types live in `therminal-protocol/src/daemon.rs`. Server/client in `src/{server,client}.rs`.

## Session Manager

Persistent multiplexed sessions via a `Session -> Window -> Pane` hierarchy managed by `SessionManager` in `src/session.rs`.

**Hierarchy**: `SessionManager` owns a `HashMap<SessionId, Session>`. Each `Session` contains `Vec<Window>`, each `Window` contains `Vec<Pane>`. A new session gets one default window with one pane.

**Pane PTY workers**: Both app and daemon use `PtyPaneCore` from `therminal-terminal/src/pty_runtime.rs` for shared PTY lifecycle (Term creation, PTY spawn, reader thread). The daemon implements `PtyReaderHandler` to broadcast `DaemonEvent::PaneOutput`.

**Attach/detach protocol**: On attach, the daemon takes a `PaneSnapshot` from each pane's `Term` state -- grid content (chars + bold flags), cursor position, and dimensions. This is a state snapshot, not a byte replay. The client renders this snapshot to immediately show the current terminal state.

**Session CRUD via IPC**: `CreateSession` spawns a real PTY and returns the session ID. `ListSessions`, `GetSession`, `DestroySession` operate on the session map. Session count is synced to the `Lifecycle` for idle-exit tracking.

**Keystroke forwarding**: Client sends input bytes via IPC, dispatched through `SessionManager::write_to_pane()` to the pane's PTY writer.

**Graceful shutdown**: `IpcServer::run()` calls `SessionManager::shutdown()` on exit, which destroys all sessions (dropping PTY masters, causing reader threads to get EOF and exit).

## MCP Server

`src/mcp.rs` implements an MCP server (`rmcp` crate) with cross-platform IPC: Unix sockets on Linux/macOS (`<runtime_dir>/mcp.sock`), named pipes on Windows (`\\.\pipe\therminal-mcp`). Configurable via `[mcp] socket_path` in `therminal.toml`. `therminal-app/src/mcp_stdio.rs` provides a stdio bridge (`therminal mcp` subcommand) that proxies stdin/stdout to the daemon's IPC endpoint, enabling MCP clients like Claude Code to connect as a subprocess.

Tools exposed:

| Tool | Category | Description |
|------|----------|-------------|
| `terminal.sessions.list` | Observer | List all session IDs |
| `terminal.sessions.get` | Observer | Get session metadata |
| `terminal.sessions.create` | Writer | Spawn a new PTY session |
| `terminal.sessions.destroy` | Admin | Kill a session |
| `terminal.panes.get_content` | Observer | Read visible pane content |
| `terminal.panes.write` | Writer | Send input to a pane's PTY |
| `terminal.semantic.get_hotspots` | Observer | Scan pane for file paths, URLs, git refs, issue refs |

Agent identity is extracted from the MCP `initialize` handshake and passed to trust enforcement on every tool call. Both the daemon and the stdio bridge read `[mcp]` config via `McpConfig::resolved_socket_path()` — a single source of truth in `therminal-core`.

### MCP Resources

The server also exposes MCP Resources for pane content access:

| Resource URI | Category | Description |
|-------------|----------|-------------|
| `terminal://pane/{id}/content` | Observer | Current visible grid snapshot (plain text) |
| `terminal://pane/{id}/output` | Observer | Live PTY output stream (subscribe for updates) |

**Resource listing**: `list_resources` returns concrete resources for each active pane. `list_resource_templates` returns URI templates for both content and output patterns.

**Resource reading**: `read_resource` snapshots the pane's current visible grid content as plain text lines (same data as the `terminal.panes.get_content` tool but via the MCP resource protocol).

**Resource subscriptions**: Subscribing to `terminal://pane/{id}/output` spawns a background task that listens to the `DaemonEvent::PaneOutput` broadcast channel and sends `notifications/resources/updated` to the MCP client whenever new PTY output arrives. The client can then call `read_resource` to fetch the updated content. Content resources do not support subscriptions. Unsubscribing cancels the background task.

**Trust enforcement**: All resource operations require Observer tier (Sandboxed minimum), matching the read-only nature of resource access. Trust is enforced via `check_resource_access()` in `trust.rs`.

## MCP Tool Naming

All MCP tools follow a `terminal.<domain>.<verb>` naming convention with dot-separated namespaces.

### Domains

| Domain | Scope |
|--------|-------|
| `terminal.sessions` | Session lifecycle (list, get, create, destroy) |
| `terminal.panes` | Pane I/O and state (get_content, write) |
| `terminal.semantic` | Reserved for semantic region queries (Phase 4) |

### Standard Verbs

| Verb | Meaning |
|------|---------|
| `list` | Return all IDs/summaries for a domain |
| `get` | Return details for a single resource by ID |
| `get_content` | Return content/payload for a resource (distinct from metadata) |
| `create` | Spawn a new resource |
| `destroy` | Tear down a resource (destructive, Admin tier) |
| `write` | Send input/data to a resource |
| `query` | Search or filter within a domain |

### Adding New Tools

1. Pick the correct domain. If none fits, propose a new `terminal.<domain>` in a PR.
2. Use a standard verb from the table above. Compound verbs use underscores (e.g. `get_content`).
3. Add the tool name to `tool_category()` in `trust.rs` with the appropriate tier.
4. Add the `Tool::new()` entry in `tool_definitions()` and the match arm in `call_tool()` in `mcp.rs`.
5. Update the tool table in this file.

## Trust Tier Enforcement

`src/trust.rs` maps MCP tools to three permission categories (Observer, Writer, Admin) and enforces access control on every call:

| Tier | Name | MCP Access |
|------|------|-----------|
| `Sandboxed` | Read-only | Observer tools only |
| `Supervised` | Default | Observer + Writer tools |
| `Trusted` | Full | All tools including Admin |

Agent tiers are set per-agent in `[trust]` config, with a `default_tier` fallback. Destructive (Admin) tools are additionally subject to a sliding-window rate limiter (configurable `max_destructive_per_minute`). All allow/deny decisions are audit-logged via `tracing`.

Key files: `src/mcp.rs` (server), `src/trust.rs` (enforcement + rate limiter), `therminal-app/src/mcp_stdio.rs` (stdio bridge), `therminal-core/src/config/mod.rs` (`McpConfig`).

## Control Mode

`src/control.rs` implements a machine-readable text protocol (tmux `-CC` style). The `--help-control` CLI flag prints the full protocol reference. The `help` command within a control session returns the same reference inline.
