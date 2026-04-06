# therminal-protocol

Wire types, IPC message definitions, and protocol types shared between daemon and app.

## Design Constraint

This crate has **no GPU, async, or system dependencies** -- it is intentionally lightweight so that any crate in the workspace (including `therminal-terminal`) can depend on it without pulling in heavy transitive dependencies.

## Module Structure

```
src/
├── lib.rs         # Re-exports, canonical ID type aliases, RegionKind enum
├── bus_types.rs   # Message bus participant types (AgentId, TaskState, ClaudeStatus)
├── daemon.rs      # Daemon IPC protocol: envelope, requests, responses, events, framing
└── message.rs     # Message bus wire types (Message, MessageType)
```

## Canonical ID Types

Defined in `lib.rs` as `u64` type aliases -- `Copy`, `Eq`, `Hash`, cheap to pass over IPC. These are the single source of truth for entity IDs across all crates.

| Type | Purpose |
|------|---------|
| `SessionId` | Unique session identifier |
| `WindowId` | Window within a session |
| `PaneId` | Unique pane identifier |
| `WorkspaceId` | Workspace slot number (1-9) |

## Key Types

### Bus Types (`bus_types.rs`)

- **`AgentId`** -- Identifies a message bus participant as `type/key` (e.g. `claude/sess-1`). Implements `Display` and `FromStr`.
- **`TaskState`** -- Lifecycle enum: `Submitted`, `Working`, `Completed`, `Failed`, `InputRequired`.
- **`ClaudeStatus`** -- Agent session status: `Idle`, `Processing`, `ToolUse`, `AwaitingInput`.

### Daemon IPC (`daemon.rs`)

- **`IpcMessage`** -- Tagged envelope with three variants: `Request { request_id, payload }`, `Response { request_id, payload }`, `Event { payload }`. The `request_id: u64` enables multiplexing over a single connection.
- **`IpcRequest`** -- All client-to-daemon commands: `Ping`, `GracefulShutdown`, `Subscribe`, session CRUD, pane operations (`SendKeys`, `CapturePane`, `SplitPane`, `KillPane`, `SelectPane`), workspace state (`SetWorkspaceState`, `GetWorkspaces`), and `RequestHandoffFds`.
- **`IpcResponse`** -- Corresponding responses including `Pong` (with health data), `PaneCaptured` (grid snapshot), `HandoffReady`, `Workspaces`, and `Error`.
- **`DaemonEvent`** -- Server-pushed events: `StateChanged`, `SessionCreated`, `SessionDestroyed`, `PaneOutput`, `WorkspaceChanged`. Clients filter via `EventKind`.
- **`DaemonState`** -- Daemon lifecycle state machine: `Starting -> Binding -> Ready -> Running -> Draining -> Stopped`.
- **`WorkspaceInfo`** -- Workspace tab metadata (id, name, order, pane_ids, focused_pane). Stored by daemon for MCP queries.
- **`HandoffPaneMeta` / `HandoffPayload`** -- Metadata for zero-downtime PTY FD transfer via SCM_RIGHTS.
- **`PersistedState` / `PersistedSession` / `PersistedPane`** -- Serialized to `sessions.json` for session persistence across daemon restarts.

### Wire Format

- **Framing**: 4-byte big-endian length prefix + MessagePack payload. Max frame: 1 MiB (`MAX_FRAME_SIZE`).
- **`encode_ipc()` / `decode_ipc()`** -- Serialize/deserialize `IpcMessage` with length-prefixed framing.
- **`PROTOCOL_VERSION`** -- Bumped when IPC wire format changes in a way that requires daemon restart. Normal rebuilds reuse the running daemon.

### Message Bus (`message.rs`)

- **`Message`** -- Full message bus envelope: `seq`, `ts`, `from`/`to` (`AgentId`), `content`, flattened `msg_type`, and arbitrary `metadata` map.
- **`MessageType`** -- Discriminated kind: `AgentMsg`, `Subscribe`, `Ack`, `RingOverflow`, `TaskStatus`.

## Semantic Types

- **`RegionKind`** -- Scrollback region classification: `Prompt`, `Command`, `Output`, `Error`, `ToolCall`, `Thinking`, `Annotation`. Used by the semantic region index in `therminal-terminal`.
