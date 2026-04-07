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

Tools exposed (15 tools):

| Tool | Category | Description |
|------|----------|-------------|
| `terminal.sessions.list` | Observer | List all active session IDs |
| `terminal.sessions.get` | Observer | Get session metadata (name, creation time) |
| `terminal.sessions.create` | Writer | Spawn a new PTY session |
| `terminal.sessions.destroy` | Admin | Destroy a session and all its panes |
| `terminal.panes.list` | Observer | List all panes with dimensions, session membership, title, plus optional `cwd` (from OSC 7 / spawn), `last_exit_code` (from OSC 633 D), and `agent_name` (from `AgentRegistry`). Optional fields are omitted when unknown to preserve wire compatibility. |
| `terminal.panes.create` | Writer | Create a pane (split from existing or add to session) |
| `terminal.panes.destroy` | Admin | Destroy a pane and its PTY |
| `terminal.panes.get_content` | Observer | Read visible grid snapshot with cursor position |
| `terminal.panes.get_geometry` | Observer | Get pane dimensions and split feasibility |
| `terminal.panes.write` | Writer | Send keystrokes or commands to a pane's PTY |
| `terminal.panes.wait_for_output` | Observer | Wait for output matching a pattern (string/regex) |
| `terminal.semantic.query_history` | Observer | Query semantic region index (Prompt, Command, Output, Error) |
| `terminal.semantic.get_hotspots` | Observer | Scan pane for file paths, URLs, git refs, issue refs |
| `terminal.workspaces.list` | Observer | List workspace tabs with names, pane counts, active status |
| `terminal.agents.list` | Observer | List detected AI agents with type, status, pane location |

Agent identity is extracted from the MCP `initialize` handshake and passed to trust enforcement on every tool call. Both the daemon and the stdio bridge read `[mcp]` config via `McpConfig::resolved_socket_path()` — a single source of truth in `therminal-core`.

### MCP Resources

The server also exposes MCP Resources for pane content access:

| Resource URI | Category | Description |
|-------------|----------|-------------|
| `terminal://pane/{id}/content` | Observer | Current visible grid snapshot (plain text) |
| `terminal://pane/{id}/output` | Observer | Live PTY output stream (subscribe for updates) |
| `terminal://pane/{id}/scrollback` | Observer | Historical scrollback above the visible grid (plain text, oldest first, capped at 10,000 lines, no subscriptions) |

**Resource listing**: `list_resources` returns concrete resources for each active pane. `list_resource_templates` returns URI templates for both content and output patterns.

**Resource reading**: `read_resource` snapshots the pane's current visible grid content as plain text lines (same data as the `terminal.panes.get_content` tool but via the MCP resource protocol).

**Resource subscriptions**: Subscribing to `terminal://pane/{id}/output` spawns a background task that listens to the `DaemonEvent::PaneOutput` broadcast channel and sends `notifications/resources/updated` to the MCP client whenever new PTY output arrives. The client can then call `read_resource` to fetch the updated content. Content resources do not support subscriptions. Unsubscribing cancels the background task.

**Trust enforcement**: All resource operations require Observer tier (Sandboxed minimum), matching the read-only nature of resource access. Trust is enforced via `check_resource_access()` in `trust.rs`.

## Claude Code Session Observability

The daemon also runs a Claude Code session observability pipeline — a standalone data flow independent of the process-tree `AgentRegistry`. Where `AgentRegistry` answers "is an agent process running in this pane?", this pipeline answers "what is that agent *doing* right now, and which subagents has it spawned?".

**Data flow**:

```
/tmp/claude-code-state/*.json      (written by Claude Code hooks)
          │
          ▼
ClaudeStatePoller  (src/claude_state.rs)
  notify-based file watcher → ClaudeSessionState updates
  (includes parent_session_id: Option<String> for subagent tracking)
          │
          ▼
ClaudeJsonlRegistry  (src/claude_jsonl_tailer.rs)
  ├─ SessionJsonlTailer per top-level session
  │    byte-offset incremental reader over
  │    ~/.claude/projects/{hash}/{sid}.jsonl
  │
  └─ Per-subagent SessionJsonlTailer (discovered by polling
     ~/.claude/projects/{hash}/{parent-sid}/subagents/agent-*.jsonl
     on each tick, read from offset 0 to capture full lifecycle)
          │
          ▼ TaggedAgentEvent { event: AgentEvent, source: EventSource }
          ▼
ClaudePipeline  (src/claude_pipeline.rs)
  150ms tick driving poll_all, tokio::sync::broadcast fan-out
          │
          ▼
therminal://claude/events  (MCP resource, src/mcp.rs)
  subscription-based, per-connection ring buffer, Observer-tier trust
```

**Key types** (see source files for details):

- `AgentEvent` in `src/agent_events.rs` — `UserMessage`, `AssistantMessage`, `ToolUse`, `ToolResult`, `Thinking`, `Progress`, `SystemMessage`. Derive `Serialize`.
- `SessionEvent` / `SessionEventType` in `src/claude_session_log.rs` — parser for Claude Code's nested JSONL envelope (assistant `message.content` arrays with `text`/`tool_use`/`thinking` blocks). `parse_session_event` is a pure function.
- `TaggedAgentEvent` and `EventSource::{TopLevel { session_id }, Subagent { parent_session_id, agent_id }}` in `src/claude_jsonl_tailer.rs`. Consumers use `EventSource` to reconstruct the session tree — no server-side filtering required.

**Top-level vs subagent tailers**: Top-level tailers seek to end on session switch (skip history — only live events). Subagent tailers read from offset 0 because subagent sessions are short-lived and we want the full lifecycle. Subagent tailers are dropped when their parent session is removed.

**Startup**: `ClaudePipeline::spawn()` is called from `ensure.rs` during daemon bring-up. It owns a tokio task that consumes `ClaudeStateUpdate`s from the poller, applies them to the registry, ticks `poll_all` every 150ms, and re-broadcasts `TaggedAgentEvent`s.

**`therminal://claude/events` MCP resource**: Listed in `list_resources`. `read_resource` drains the per-connection ring buffer as a JSON array. `subscribe` attaches a per-connection forwarder that pushes buffered events and sends `notifications/resources/updated` as new events arrive. Trust-gated via `check_resource_access()` — Observer tier, same as pane content. Per-session URI filtering (`therminal://claude/events/{session_id}`) is intentionally deferred — consumers filter client-side on `EventSource`.

**`claude-events` dev binary** (`src/bin/claude-events.rs`): Minimal raw JSON-RPC client that connects to the daemon's MCP socket, subscribes to `therminal://claude/events`, and prints styled lines per event. Flags: `--filter top|sub|all`, `--session <sid>`, `--verbose`, `--no-color`, `--json`. Run via `cargo run -p therminal-daemon --bin claude-events`. README documents user-facing flags; the binary itself is also the reference implementation for consuming the subscription protocol.

**Scope boundary**: This pipeline is deliberately separate from `AgentRegistry`. The two compose but do not merge — `AgentRegistry` stays in `therminal-terminal` and tags panes by process tree; this pipeline lives in the daemon and exposes per-event session detail. A future overlay widget (tracked as `tn-x85k`) will render both via the same MCP consumer path.

Key files: `src/claude_state.rs`, `src/agent_events.rs`, `src/claude_session_log.rs`, `src/claude_jsonl_tailer.rs`, `src/claude_pipeline.rs`, `src/bin/claude-events.rs`, and the `therminal://claude/events` resource handling in `src/mcp.rs`.

## MCP Tool Naming

All MCP tools follow a `terminal.<domain>.<verb>` naming convention with dot-separated namespaces.

### Domains

| Domain | Scope |
|--------|-------|
| `terminal.sessions` | Session lifecycle (list, get, create, destroy) |
| `terminal.panes` | Pane I/O, state, and geometry (list, create, destroy, get_content, get_geometry, write, wait_for_output) |
| `terminal.semantic` | Semantic region queries (query_history, get_hotspots) |
| `terminal.workspaces` | Workspace tab introspection (list) |
| `terminal.agents` | Agent detection and status (list) |

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

Key files: `src/mcp.rs` (server), `src/trust.rs` (enforcement + rate limiter), `src/persistence.rs` (session state persistence), `src/fd_passing.rs` (FD handoff), `therminal-app/src/mcp_stdio.rs` (stdio bridge), `therminal-core/src/config/mod.rs` (`McpConfig`).

## Persistence

`src/persistence.rs` implements debounced session state persistence to `<data_dir>/sessions.json`. A background task listens for dirty signals from the session manager and coalesces rapid changes with a 2-second debounce timer. On daemon shutdown, a final synchronous save ensures no state is lost. The `PersistenceHandle` is cloned into session mutation paths to trigger saves on topology changes (create, destroy, split).

## FD Passing

`src/fd_passing.rs` implements Unix SCM_RIGHTS file descriptor passing for zero-downtime daemon handoff. Uses `sendmsg`/`recvmsg` with ancillary data to transfer PTY master FDs from the old daemon to the new daemon over a temporary Unix socket. The in-band data carries a MessagePack-encoded `HandoffPayload` with session/pane metadata; the out-of-band ancillary data carries the actual FDs. Gated behind `#[cfg(unix)]` — on non-Unix platforms the handoff falls back to graceful restart.

## Control Mode

`src/control.rs` implements a machine-readable text protocol (tmux `-CC` style). The `--help-control` CLI flag prints the full protocol reference. The `help` command within a control session returns the same reference inline.
