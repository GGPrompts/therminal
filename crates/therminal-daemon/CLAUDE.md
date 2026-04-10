# therminal-daemon

Session manager, event bus, multiplexer, MCP server, trust enforcement.

## Daemon Lifecycle

The daemon uses a **socket-as-lock** pattern -- successful socket bind = ownership of the daemon role, no pidfiles needed.

**BUILD_HASH**: `build.rs` embeds `<git-short-hash>` at compile time via `env!("BUILD_HASH")`. Informational only — surfaced in startup logs and `Pong` responses. Daemon handoff is driven by `PROTOCOL_VERSION`, not by BUILD_HASH (see `ensure.rs::ensure_daemon`). The hash is stable across no-op rebuilds: `build.rs` declares only `rerun-if-changed=src/`, so two builds with identical source produce the same BUILD_HASH and the daemon crate does not relink. (Earlier versions appended `-<unix-timestamp>` and declared `rerun-if-changed=../../.git/HEAD`; both were removed because they invalidated cargo's incremental machinery on every build, especially in the Windows native build dir which rsyncs without `.git/`.)

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

## OS-level trust boundary

The daemon IPC channel (`IpcRequest` / `IpcResponse`) is **not authenticated at the protocol layer**. Any process that can open the daemon socket/pipe can issue mutating operations: `SplitPane`, `KillPane`, `ResizePane`, `SendKeys`, `DestroySession`, `SetWorkspaceState`. The MCP surface enforces trust tiers on top of this, but the native IPC path does not.

Access control is therefore pushed down to the OS:

- **Unix** — `IpcListener::bind` chmods the socket to `0o600` (owner read/write only) immediately after `UnixListener::bind`. If the chmod fails, the bind fails hard and unlinks the socket; we never continue on a world-accessible path. See `crates/therminal-daemon-client/src/ipc_transport.rs` and the `unix_socket_is_mode_0600` unit test.
- **Windows** — the named pipe is created via `tokio::net::windows::named_pipe::ServerOptions::first_pipe_instance(true).create()` with the **default DACL**. In practice this inherits the creating process token and limits connections to the current user, SYSTEM, and Administrators, but we do not currently construct an explicit owner-only `SECURITY_ATTRIBUTES` via `windows-sys`. Treat the Windows pipe as reachable by any code running under the same user account. A proper owner-only SECURITY_DESCRIPTOR is tracked as a follow-up.

**Rule for contributors**: when adding new destructive or privileged operations to `dispatch_ipc`, do not rely solely on "the socket is 0600" as a justification. Consider whether the operation also needs protocol-layer trust enforcement (e.g. a `TrustTier` check), especially for anything that can damage user data, exfiltrate pane content off-box, or execute commands in non-therminal contexts. OS-level access control is the *floor*, not the ceiling.

## IPC Protocol

The daemon exposes a multiplexed IPC protocol over Unix domain sockets with length-prefixed MessagePack framing.

**Wire format**: `[4-byte BE length][MessagePack payload]`. Max frame size: 1 MiB.

**Envelope** (`IpcMessage`): Three variants -- `Request { request_id, payload }`, `Response { request_id, payload }`, `Event { payload }`. The `request_id: u64` enables multiplexing multiple in-flight requests over one connection.

**Requests** (`IpcRequest`): `Ping`, `GracefulShutdown`, `Subscribe { filter }`, `Unsubscribe`, `ListSessions`, `GetSession`, `CreateSession`, `DestroySession`, `GetState`. `SplitPane` accepts `pane_id`, `horizontal`, `cwd`, `startup_command`, and `ratio: Option<f32>` (0.1..0.9, default 0.5). `CreateWorkspace` and `RenameWorkspace` provide CLI-driven workspace lifecycle without requiring the full `SetWorkspaceState` topology push.

**Responses** (`IpcResponse`): `Pong`, `ShutdownAck`, `Subscribed`, `Unsubscribed`, `Sessions`, `SessionInfo`, `SessionCreated`, `SessionDestroyed`, `State`, `Error`.

**Events** (`DaemonEvent`): `StateChanged`, `SessionCreated`, `SessionDestroyed`, `PaneOutput`. Clients subscribe via `Subscribe { filter: Vec<EventKind> }` -- empty filter = all events.

**Client API** (`DaemonClient`): Persistent connection with `connect()`, `send_request()`, `ping()`, `shutdown()`, `subscribe_events()`, `recv_event()`. Uses internal reader/writer tasks for full-duplex communication.

**Server** (`IpcServer`): Accepts connections, dispatches to handlers, manages per-connection event subscriptions via `tokio::sync::broadcast`. Auto-detects legacy vs IPC protocol on first frame.

**Backward compatibility**: The server auto-detects legacy `DaemonRequest` frames (used by `ensure_daemon()` and handoff) vs new `IpcMessage` frames. Legacy single-shot `send_request()` function is preserved. `DaemonServer` is a type alias for `IpcServer`.

Protocol types live in `therminal-protocol/src/daemon.rs`. Server/client in `src/{server,client}.rs`.

## Session Manager

Persistent multiplexed sessions via a `Session -> Window -> Pane` hierarchy managed by `SessionManager` in `src/session/`.

**Module structure** (split from monolithic `session.rs`):

| File | Responsibility |
|------|----------------|
| `session/mod.rs` | Re-exports, top-level `SessionManager` methods |
| `session/base.rs` | `Session`, `Window`, `Pane` struct definitions |
| `session/manager.rs` | `SessionManager` CRUD, session lifecycle |
| `session/pane.rs` | Pane creation, PTY spawn, pane-level operations |
| `session/window.rs` | Window/workspace management |
| `session/layout.rs` | Layout tree helpers, split/resize logic |
| `session/snapshots.rs` | `PaneStateSnapshot`, `PaneSnapshot`, state capture for MCP and reattach |

**Hierarchy**: `SessionManager` owns a `HashMap<SessionId, Session>`. Each `Session` contains `Vec<Window>`, each `Window` contains `Vec<Pane>`. A new session gets one default window with one pane.

**Pane PTY workers**: Both app and daemon use `PtyPaneCore` from `therminal-terminal/src/pty_runtime.rs` for shared PTY lifecycle (Term creation, PTY spawn, reader thread). The daemon implements `PtyReaderHandler` to broadcast `DaemonEvent::PaneOutput`.

**Attach/detach protocol**: tn-zamd landed structured state replay on reattach. On `build_remote_pane_state` the GUI issues `IpcRequest::CapturePaneState { pane_id }`, which returns a versioned `PaneStateSnapshot` (mode flags, cursor, visible grid) produced by `Pane::snapshot_state`. The GUI synthesizes DECSET/DECRST + CUP + grid paint escape sequences via `PaneStateSnapshot::to_replay_bytes` and feeds them directly into the freshly-constructed local `Term` *before* live `DaemonEvent::PaneOutput` forwarding begins. Captured flags: DECTCEM (`?25`), DECCKM (`?1`), LNM (`?7`), focus events (`?1004`), bracketed paste (`?2004`), mouse click/drag/motion (`?1000`/`?1002`/`?1003`), SGR mouse (`?1006`), alt screen (`?1049`), and application keypad (`ESC =` / `ESC >`). Scrollback is intentionally deferred in V1 — the live `PaneOutput` stream rebuilds it organically. The forwarder buffers post-subscribe bytes until snapshot replay completes, then drops the buffer and goes live. The older `PaneSnapshot` type in `session/snapshots.rs` is still used for MCP capture/grid queries and is unrelated to attach.

**Session CRUD via IPC**: `CreateSession` spawns a real PTY and returns the session ID. `ListSessions`, `GetSession`, `DestroySession` operate on the session map. Session count is synced to the `Lifecycle` for idle-exit tracking.

**Keystroke forwarding**: Client sends input bytes via IPC, dispatched through `SessionManager::write_to_pane()` to the pane's PTY writer.

**Graceful shutdown**: `IpcServer::run()` calls `SessionManager::shutdown()` on exit, which destroys all sessions (dropping PTY masters, causing reader threads to get EOF and exit).

## MCP Server

`src/mcp/` implements an MCP server (`rmcp` crate) with cross-platform IPC: Unix sockets on Linux/macOS (`<runtime_dir>/mcp.sock`), named pipes on Windows (`\\.\pipe\therminal-mcp`). Configurable via `[mcp] socket_path` in `therminal.toml`. `therminal-app/src/mcp_stdio.rs` provides a stdio bridge (`therminal mcp` subcommand) that proxies stdin/stdout to the daemon's IPC endpoint, enabling MCP clients like Claude Code to connect as a subprocess.

### CLI-vs-MCP routing (agents: read this first)

Most read operations have a CLI counterpart that is dramatically cheaper for
prompt-cache health. The `tools.rs` module-level doc comment carries the
per-tool classification table. The short version:

**Use CLI (`therminal` / `tn` wrapper) for:**
- `pane list`, `pane peek`, `pane send`, `pane tag/untag` — the hot path for
  conductor polling and write operations.
- `agents list`, `workspace list`, `session list/create` — simple reads.
- `semantic commands/hotspots` — TSV is sufficient for most callers.

**Use MCP for:**
- Any **subscription** (`terminal://pane/{id}/output`, `therminal://claude/events`,
  `therminal://agents/events`) — no CLI peer.
- `terminal.panes.wait_for_output` — blocking async wait; no CLI peer.
- `terminal.panes.get_summary` — MCP-only conductor tick primitive (~120 B).
- `terminal.agents.find_with_capacity`, `get_status`, `get_details`,
  `get_cadence` — typed shape feeds downstream tool calls.
- `terminal.workspaces.get_layout` — binary tree; structured shape required.
- `terminal.sessions.destroy`, `terminal.panes.destroy` — Admin tier; trust
  enforcement is enforced at the MCP layer, not the CLI layer.

Tools exposed (26 tools):

| Tool | Category | Description |
|------|----------|-------------|
| `terminal.sessions.list` | Observer | List all active session IDs |
| `terminal.sessions.get` | Observer | Get session metadata (name, creation time) |
| `terminal.sessions.create` | Writer | Spawn a new PTY session |
| `terminal.sessions.destroy` | Admin | Destroy a session and all its panes |
| `terminal.panes.list` | Observer | List all panes with dimensions, session membership, title, plus optional `cwd` (from OSC 7 / spawn), `last_exit_code` (from OSC 633 D), and `agent_name` (from `AgentRegistry`). Optional fields are omitted when unknown to preserve wire compatibility. |
| `terminal.panes.create` | Writer | Create a pane (split from existing or add to session). Optional `startup_command` is injected after the first OSC 133/633 prompt-start mark; if no shell integration arrives, the daemon falls back to a 300ms delay before writing. |
| `terminal.panes.destroy` | Admin | Destroy a pane and its PTY |
| `terminal.panes.get_content` | Observer | Read the visible grid snapshot with cursor position. **tn-sp3n behavior change:** trailing whitespace is now trimmed from every row by default — empty rows on a sparse pane are returned as `""` instead of `cols`-wide padding (typical 70-90% byte savings). Optional params: `trim_trailing_whitespace=false` restores the historical fixed-width grid, `compact=true` drops fully-blank rows, and `rows=N` returns only the last N visible / non-empty rows. The response now includes a stable `content_hash` (hex-encoded `DefaultHasher` over the full grid + cursor) so subscribers can short-circuit on unchanged screens. |
| `terminal.panes.get_summary` | Observer | (tn-sp3n) Lightweight (~100 bytes) status snapshot for one pane: `pane_id`, `cursor_col`, `cursor_line`, `content_hash`, `last_command` + `last_exit_code` (from `CommandTracker`), `hotspot_count`, `agent_name` + `agent_status` (from `AgentRegistry`), `timestamp_secs`. Designed for conductor polling — answers "is this pane idle, working, or done?" without pulling any grid content. Compare `content_hash` against the previous tick to decide whether to follow up with `get_content` / `peek`. |
| `terminal.panes.peek` | Observer | (tn-sp3n) Cheap "what just happened?" snapshot: returns the last N non-empty trimmed lines from a pane (`lines` param defaults to 10, capped server-side at 50) plus the same `content_hash` as `get_content` and a Unix timestamp. ~500 bytes for a typical mostly-empty pane; ideal when `get_summary` says the screen changed and you want a quick look without paying for the full grid. |
| `terminal.panes.get_geometry` | Observer | Get pane dimensions and split feasibility |
| `terminal.panes.write` | Writer | Send keystrokes or commands to a pane's PTY |
| `terminal.panes.tag` | Writer | Merge opaque key/value tags into a pane's metadata (tn-bbvf). Tags are arbitrary strings — therminal does not interpret them. Existing keys are overwritten; other keys are left untouched. Tags persist across daemon restarts via `sessions.json`. |
| `terminal.panes.untag` | Writer | Remove tags from a pane. Omit `keys` to clear all tags; pass a list to remove only the named keys. |
| `terminal.panes.wait_for_output` | Observer | Wait for output matching a pattern (string/regex) |
| `terminal.panes.query_events` | Observer | Snapshot recent structured lifecycle events from a pane's in-memory `EventLog` (spawn / status_change / command_start / command_finish / resize / pty_eof / bell). Supports `since_timestamp_secs` and `limit` (default 100). Backed by a per-pane `Arc<Mutex<EventLog>>` ring buffer (5000-entry cap); the JSONL file on disk is never read. |
| `terminal.semantic.query_history` | Observer | Query semantic region index (Prompt, Command, Output, Error) |
| `terminal.semantic.query_commands` | Observer | Return recent shell commands with exit codes and durations from the OSC 633 `CommandTracker`. Supports `since_line` and `limit` (default 20, capped at 20). Backed by a per-pane `Arc<Mutex<CommandTracker>>` shared between the reader thread's `TherminalInterceptor` and the daemon-side `Pane`; handlers take a cheap cloned snapshot under the lock. |
| `terminal.semantic.get_hotspots` | Observer | Scan pane for file paths, URLs, git refs, issue refs |
| `terminal.workspaces.list` | Observer | List workspace tabs with names, pane counts, active status |
| `terminal.workspaces.get_layout` | Observer | Get binary layout tree + focused_pane for a workspace. Currently returns a degraded horizontal cascade until the real `LayoutNode` tree is plumbed into the daemon (tn-vs0u). |
| `terminal.agents.list` | Observer | List detected AI agents with type, status, pane location |
| `terminal.agents.find_with_capacity` | Observer | Return agents whose REMAINING context-window capacity (`100 - context_percent`) is at least `threshold_percent`. Iterates `list_agents()`, joins with `pane_capacity()`, sorts descending by `remaining_percent`. Agents with unknown capacity (no `PaneCapacityCache` entry) are INCLUDED — treated as "potentially has capacity" so callers don't accidentally skip fresh panes — and sort last. |
| `terminal.agents.get_status` | Observer | Dynamic mode + capacity snapshot for a single pane's agent: `pane_id`, `agent_type`, `status`, `current_tool`, `context_percent`, `model`. Strict subset of `get_details` intended for sibling-agent coordination. Combines `AgentRegistry` (mode) with `pane_capacity()` (capacity). Errors when neither lookup yields anything for the pane. |
| `terminal.agents.get_details` | Observer | Get inference details for a pane's agent: `agent_type`, `model`, `context_percent`, `consecutive_failures`, `last_command`, `last_exit_code`, `last_command_duration_ms`. Backed by a per-pane `AgentStateInference` engine fed from the PTY reader thread; `agent_type` falls back from `AgentRegistry` to the engine's own detection when no registry entry exists. |
| `terminal.agents.get_cadence` | Observer | Get output cadence metrics for a pane's agent: `chunk_count`, `avg_arrival_ms`, `max_gap_ms`, `is_spinner`, `is_streaming`, plus `recent_samples` (oldest first, capped at 50). Backed by the per-pane `AgentStateInference` engine's chunk-stats sliding window. Sample timestamps are converted from monotonic `Instant` to wall-clock Unix seconds at snapshot time. Useful for predicting time-to-completion, animating progress, and distinguishing stalled vs thinking agents. Returns an error only if the pane does not exist; panes with no streaming activity return zero / false / empty defaults. |

Agent identity is extracted from the MCP `initialize` handshake and passed to trust enforcement on every tool call. Both the daemon and the stdio bridge read `[mcp]` config via `McpConfig::resolved_socket_path()` — a single source of truth in `therminal-core`.

### MCP Resources

The server also exposes MCP Resources for pane content access:

| Resource URI | Category | Description |
|-------------|----------|-------------|
| `terminal://pane/{id}/content` | Observer | Current visible grid snapshot as plain text. **tn-sp3n:** trailing whitespace is trimmed from every row (the resource protocol carries no params, so this is unconditional — callers needing the historical fixed-width grid should use `terminal.panes.get_content` with `trim_trailing_whitespace=false`). |
| `terminal://pane/{id}/output` | Observer | Live PTY output stream (subscribe for updates). Same trim-on-read behavior as `content`. |
| `terminal://pane/{id}/scrollback` | Observer | Historical scrollback above the visible grid (plain text, oldest first, capped at 10,000 lines, no subscriptions) |
| `therminal://claude/events` | Observer | Live Claude Code session events (subscribe-only, JSON `TaggedAgentEvent`s, per-connection ring buffer drained by `read_resource`) |
| `therminal://agents/events` | Observer | Live agent lifecycle events from `AgentRegistry` — `Registered` / `Unregistered` / `StatusChanged` across all panes (subscribe-only, JSON `TaggedAgentEvent { event, pane_id, timestamp_secs }`, per-connection ring buffer) |

**Resource listing**: `list_resources` returns concrete resources for each active pane. `list_resource_templates` returns URI templates for both content and output patterns.

**Resource reading**: `read_resource` snapshots the pane's current visible grid content as plain text lines (same data as the `terminal.panes.get_content` tool but via the MCP resource protocol).

**Resource subscriptions**: Subscribing to `terminal://pane/{id}/output` spawns a background task that listens to the `DaemonEvent::PaneOutput` broadcast channel and sends `notifications/resources/updated` to the MCP client whenever new PTY output arrives. The client can then call `read_resource` to fetch the updated content. Content resources do not support subscriptions. Unsubscribing cancels the background task.

**Trust enforcement**: All resource operations require Observer tier (Sandboxed minimum), matching the read-only nature of resource access. Trust is enforced via `check_resource_access()` in `trust.rs`.

## Claude Code Session Observability

Claude integration lives in `crates/therminal-harness-claude/` — see its `CLAUDE.md` for the JSONL tailer, state watcher, event pipeline, and the `claude-events` dev binary. The daemon instantiates [`ClaudeHarness`](../therminal-harness-claude/src/lib.rs) at startup from `ensure.rs` and hands the resulting broadcast sender to the MCP server for the `therminal://claude/events` resource. The per-pane capacity cache in `src/pane_capacity.rs` consumes `ClaudeStateUpdate`s via the harness's `StateUpdateObserver` hook to resolve `pane_id`s through the `AgentRegistry`.

## MCP Tool Naming

All MCP tools follow a `terminal.<domain>.<verb>` naming convention with dot-separated namespaces.

### Domains

| Domain | Scope |
|--------|-------|
| `terminal.sessions` | Session lifecycle (list, get, create, destroy) |
| `terminal.panes` | Pane I/O, state, and geometry (list, create, destroy, get_content, get_geometry, write, wait_for_output) |
| `terminal.semantic` | Semantic region queries (query_history, get_hotspots) |
| `terminal.workspaces` | Workspace tab introspection (list) |
| `terminal.agents` | Agent detection, status, and cadence (list, find_with_capacity, get_status, get_details, get_cadence) |

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
4. Add the `Tool::new()` entry in `tool_definitions()` (in `mcp/tools.rs`) and the match arm in `call_tool()` (in `mcp/mod.rs`).
5. Update the tool table in this file.
6. **Numeric params must use `deser_compat::*_flexible`** (see below).

### Numeric tool params: stringified-number quirk (tn-ad0g)

Some MCP clients — at least one version of Claude Code on Windows — serialize numeric tool arguments as JSON strings, sending `{"pane_id":"1"}` instead of `{"pane_id":1}`. A naive `#[derive(Deserialize)]` rejects this with `-32602 invalid type: string "1", expected u64`, which surfaces as a failed tool call even though the JSON Schema we advertise is correct.

As a defensive measure, every numeric field on a tool param struct in `crates/therminal-daemon/src/mcp/types.rs` uses one of the helpers in `mcp/deser_compat.rs`:

| Helper | Field type | Notes |
|--------|------------|-------|
| `u64_flexible` | `u64` | Accepts integer or stringified integer. Rejects negatives / fractional / garbage strings. Trims whitespace. |
| `u64_opt_flexible` | `Option<u64>` | Same, plus `null` / missing → `None`. Empty string is rejected (not silently coerced). Pair with `#[serde(default)]`. |
| `usize_opt_flexible` | `Option<usize>` | Wraps `u64_opt_flexible` and checks the value fits `usize`. |
| `u64_default_flexible` | `u64` with `#[serde(default = "fn")]` | For fields like `timeout_ms` that have both a default and permissive parsing. |
| `f32_flexible` | `f32` | Accepts number or stringified decimal. Rejects non-finite / empty. |

The `JsonSchema` derive is unaffected — we still advertise `"type":"integer"` / `"type":"number"` in `inputSchema`, and a regression test (`tool_schema_still_declares_pane_id_as_integer`) locks this down. Compliant clients continue to send integers; misbehaving clients get the permissive decode as a fallback.

**When adding a new tool param struct with any `u64` / `usize` / `f32` field, you MUST annotate it with the appropriate `deserialize_with = "deser_compat::…"` attribute.** Tests in `mod.rs::tests` (`pane_id_param_accepts_stringified_pane_id`, `find_with_capacity_accepts_stringified_threshold`, etc.) cover representative tools; add one for new structs if they introduce new numeric shapes.

## Trust Tier Enforcement

`src/trust.rs` maps MCP tools to three permission categories (Observer, Writer, Admin) and enforces access control on every call:

| Tier | Name | MCP Access |
|------|------|-----------|
| `Sandboxed` | Read-only | Observer tools only |
| `Supervised` | Default | Observer + Writer tools |
| `Trusted` | Full | All tools including Admin |

Agent tiers are set per-agent in `[trust]` config, with a `default_tier` fallback. Destructive (Admin) tools are additionally subject to a sliding-window rate limiter (configurable `max_destructive_per_minute`). All allow/deny decisions are audit-logged via `tracing`.

**MCP module structure** (split from monolithic `mcp.rs`):

| File | Responsibility |
|------|----------------|
| `mcp/mod.rs` | `TherminalMcpServer`, `ServerHandler` impl, dispatch, tests |
| `mcp/types.rs` | All param/result structs, `LayoutNodeJson` |
| `mcp/helpers.rs` | `json_content`, `parse_args`, `extract_agent_identity`, `build_content_preview`, grid rendering helpers |
| `mcp/tools.rs` | Tool definitions, `tool_definitions()`, per-tool classification table |
| `mcp/resources.rs` | MCP resource handlers (pane content, event streams) |
| `mcp/transport.rs` | IPC transport setup (Unix socket / named pipe) |
| `mcp/deser_compat.rs` | Stringified-number deserialization helpers |

Key files: `src/mcp/mod.rs` (server), `src/trust.rs` (enforcement + rate limiter), `src/persistence.rs` (session state persistence), `src/fd_passing.rs` (FD handoff), `therminal-app/src/mcp_stdio.rs` (stdio bridge), `therminal-core/src/config/mod.rs` (`McpConfig`).

## Persistence

`src/persistence.rs` implements debounced session state persistence to `<data_dir>/sessions.json`. A background task listens for dirty signals from the session manager and coalesces rapid changes with a 2-second debounce timer. On daemon shutdown, a final synchronous save ensures no state is lost. The `PersistenceHandle` is cloned into session mutation paths to trigger saves on topology changes (create, destroy, split).

## FD Passing

`src/fd_passing.rs` implements Unix SCM_RIGHTS file descriptor passing for zero-downtime daemon handoff. Uses `sendmsg`/`recvmsg` with ancillary data to transfer PTY master FDs from the old daemon to the new daemon over a temporary Unix socket. The in-band data carries a MessagePack-encoded `HandoffPayload` with session/pane metadata; the out-of-band ancillary data carries the actual FDs. Gated behind `#[cfg(unix)]` — on non-Unix platforms the handoff falls back to graceful restart.

## Control Mode

`src/control.rs` implements a machine-readable text protocol (tmux `-CC` style). The `--help-control` CLI flag prints the full protocol reference. The `help` command within a control session returns the same reference inline.
