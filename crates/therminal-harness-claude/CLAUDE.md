# therminal-harness-claude

First-class Claude Code integration for Therminal — JSONL tailer, state watcher, and event pipeline. This crate owns everything specific to the Claude Code harness (hook state files, structured session JSONL, subagent lineage) so the daemon can stay lean and focused on session / IPC / MCP routing.

Where the daemon's `AgentRegistry` answers "is a Claude process running in this pane?", this crate answers "what is that Claude session *doing* right now, and which subagents has it spawned?".

## Data flow

```
/tmp/claude-code-state/*.json      (written by Claude Code hooks)
          │
          ▼
ClaudeStatePoller  (src/state.rs)                       Hook scripts (Claude Code)
  notify-based file watcher →                                    │
  ClaudeSessionState updates                      therminal agent-event push
  (includes parent_session_id                      (IpcRequest::PushAgentEvent)
   for subagent tracking)                                        │
          │                                                      ▼
          ▼                                          HookPushSink (src/hook_push.rs)
ClaudeJsonlRegistry  (src/jsonl_tailer.rs)             HookSignal → TaggedAgentEvent
  ├─ SessionJsonlTailer per top-level session                    │
  │    byte-offset incremental reader over                       │
  │    ~/.claude/projects/{hash}/{sid}.jsonl                     │
  │                                                              │
  └─ Per-subagent SessionJsonlTailer (discovered by              │
     polling on each tick, read from offset 0)                   │
          │                                                      │
          ▼ TaggedAgentEvent { event, source }                   │
          ▼                                                      ▼
pipeline::spawn  (src/pipeline.rs)  ◄────── both publish to broadcast channel
  150ms tick driving poll_all, tokio::sync::broadcast fan-out
          │
          ▼
ClaudeHarness (src/lib.rs) — thin facade the daemon instantiates
          │
          ▼
therminal://claude/events  (MCP resource, owned by therminal-daemon)
  subscription-based, per-connection ring buffer, Observer-tier trust
```

## State file format

Claude Code's hook scripts write one JSON state file per session under `/tmp/claude-code-state/<session-id>.json`. The adapter scripts for Codex and Copilot follow the same schema in `/tmp/codex-state/` and `/tmp/copilot-state/` so a single poller serves all three. Fields the pipeline reads:

- `session_id: String` — Claude Code session UUID, matches the JSONL file stem under `~/.claude/projects/`.
- `parent_session_id: Option<String>` — present for subagents (Task-tool children), absent for top-level sessions. This is how the registry decides whether to install a top-level tailer or look under the parent's `subagents/` directory.
- `project_dir: String` — the project directory hashed into the `~/.claude/projects/{hash}/` path.
- `pid: u32` — the Claude Code process PID. The poller uses liveness checks + `SESSION_MAX_AGE` / `RECENT_UPDATE_GRACE` to retire stale sessions on a `PRUNE_INTERVAL` sweep.
- `status: ClaudeStatus` — `idle` / `processing` / `tool_use` / `awaiting_input`. Variants must stay in sync with `InferredStatus` in `therminal-terminal::state_inference::types` — the state inference engine is the writer for `daemon-pane-*.json` state files, and a mismatch causes the poller to reject its own output with an "unknown variant" serde error. See tn-hcq9.
- `current_tool: Option<ToolDetails>` — the currently-running tool, if any. `ToolDetails { name, args }` where `args` is a structured `ToolArgs` with a handful of commonly-inspected fields (`file_path`, `pattern`, `command`, `url`, `description`, `prompt`, `subagent_type`, `issue_id`).
- `model: Option<String>`, `context_percent: Option<f32>` — surfaced to the daemon's per-pane capacity cache.

The poller emits `ClaudeStateUpdate::Upserted(ClaudeSessionState)` on add/change and `ClaudeStateUpdate::Removed { path }` on file delete.

## JSONL schema

Top-level and subagent sessions are both tailed from `~/.claude/projects/{project-hash}/{...}.jsonl`. Claude Code writes one JSON object per line; the parser in `src/session_log.rs` is a pure function (`parse_session_event(&str) -> Vec<SessionEvent>`) that handles Claude Code's nested envelope:

```jsonc
{
  "type": "user" | "assistant" | "system",
  "message": {
    "role": "...",
    "content": "string"
    // or an array of blocks:
    // [
    //   { "type": "text", "text": "..." },
    //   { "type": "tool_use", "name": "...", "input": {...}, "id": "..." },
    //   { "type": "thinking", "thinking": "..." }
    // ]
  },
  "toolUseResult": { ... }        // only on user-role tool_result lines
}
```

One JSONL line can decompose into multiple `SessionEvent`s (e.g. an assistant turn that contains both `text` and `tool_use` blocks). The tailer then maps each `SessionEvent` to an `AgentEvent` variant via `session_event_to_agent_event`.

## EventSource + subagent lineage

```rust
pub enum EventSource {
    TopLevel { session_id: String },
    Subagent { parent_session_id: String, agent_id: String },
}
```

Every emitted `TaggedAgentEvent` carries an `EventSource` so consumers can rebuild the parent/child topology client-side. The registry does *not* filter server-side — the MCP resource URI is always `therminal://claude/events` (no per-session suffix). Per-session filtering is intentionally deferred; client-side filtering on `EventSource` is sufficient for every known use case and keeps the resource surface small.

**Top-level vs subagent tailers**:

- **Top-level tailers** seek to EOF on session switch (skip history — only live events).
- **Subagent tailers** read from offset 0 because subagent sessions are short-lived and consumers need the full lifecycle (including the `UserMessage` that kicked off the Task call).
- Subagent tailers are discovered by polling `~/.claude/projects/{hash}/{parent-sid}/subagents/agent-*.jsonl` on each tick.
- Subagent tailers are dropped when their parent session is removed from the registry.

## Module layout

```
src/
├── lib.rs                  # Crate root + ClaudeHarness facade
├── agent_events.rs         # AgentEvent enum (UserMessage, AssistantMessage, ToolUse, ...)
├── markers.rs              # OSC 1341 handler + activate() (tn-hkpz)
├── state.rs                # ClaudeSessionState, ClaudeStatePoller, ClaudeStateUpdate
├── session_log.rs          # SessionEvent + parse_session_event (pure parser)
├── jsonl_tailer.rs         # SessionJsonlTailer, ClaudeJsonlRegistry, TaggedAgentEvent, EventSource
├── pipeline.rs             # spawn() + spawn_with() tick loop + broadcast fan-out
├── hook_push.rs            # HookSignal + HookPushSink — push-based input path
├── tool_call_hotspots.rs   # Claude Code tool-call hotspot detector (tn-gidy)
└── bin/
    └── claude-events.rs    # Dev CLI: connects to MCP socket, subscribes, prints styled events
```

## Public surface

The daemon consumes the harness through a thin facade:

```rust
pub struct ClaudeHarness { /* private: Option<broadcast::Sender<TaggedAgentEvent>> */ }

impl ClaudeHarness {
    pub fn start(shutdown: Arc<Notify>, observer: Option<StateUpdateObserver>) -> Self;
    pub fn event_stream(&self) -> Option<&broadcast::Sender<TaggedAgentEvent>>;
    pub fn into_event_stream(self) -> Option<broadcast::Sender<TaggedAgentEvent>>;
}
```

`start()` mirrors the pre-extraction `claude_pipeline::spawn()` — same arguments, same semantics, same return shape. If the `notify` watcher fails to initialise (e.g. a stripped-down container with no inotify), `event_stream()` returns `None` and the pipeline is effectively disabled; the daemon logs and continues running.

The daemon uses `observer` to drain `ClaudeStateUpdate`s into its per-pane capacity cache (`therminal-daemon::pane_capacity`). This is the *only* capacity signal Therminal currently has, so the observer is always wired in the happy path. The harness stays free of any `SessionManager` / `AgentRegistry` knowledge by taking a type-erased `Arc<dyn Fn(&ClaudeStateUpdate) + Send + Sync>`.

Submodules (`agent_events`, `state`, `session_log`, `jsonl_tailer`, `pipeline`) are all `pub` so downstream code (notably the `claude-events` dev binary and any future tests) can reach into parsing details without going through the facade.

## MCP resource

`therminal://claude/events` is owned by `therminal-daemon`'s MCP router. It's a subscribe-only resource: `read_resource` drains a per-connection ring buffer as a JSON array, and `subscribe` attaches a per-connection forwarder that pushes buffered events and sends `notifications/resources/updated` as new events arrive. Trust-gated via `check_resource_access()` at Observer tier — same as pane content resources.

## OSC Grammar

The crate claims **OSC 1341** via the shared handler registry (tn-hkpz). The handler lives in `src/markers.rs` and is registered once at daemon startup from `ensure.rs` via `therminal_harness_claude::activate_markers(&registry)`. See `docs/osc-handler-registry.md` for the registration API and `docs/osc-code-registry.md` for the canonical table of claimed codes.

Wire format:

```text
ESC ] 1341 ; key=value [ ; key=value ]* ST
```

Recognised keys (v0):

| Key          | Value                                        | Meaning                                |
|--------------|----------------------------------------------|----------------------------------------|
| `state`      | `idle` / `processing` / `tool_use` / `awaiting_input` | Claude session status          |
| `tool`       | string                                       | Tool name (paired with `state=tool_use`) |
| `session_id` | string                                       | Claude session UUID                    |

Unknown keys are preserved as-is in an `extra` subobject in the emitted event body for forward-compatibility. The event `kind` is always `claude.state` for v0; future grammar extensions will add additional kinds (`claude.tool_call`, `claude.thinking_started`, …) as Claude Code grows the emitter.

OSC markers are **additive live signal** — the JSONL tailer and state poller remain the authoritative source for historical data and capacity metrics. Markers give sub-millisecond state-change latency without the ~150 ms poller tick.

## Scope boundary

This pipeline is deliberately separate from `AgentRegistry` (which lives in `therminal-terminal` and tags panes by process tree). The two compose but do not merge: `AgentRegistry` answers "is there a Claude process in this pane?"; this crate answers "what is that Claude session doing right now, and what subagents has it spawned?". A future overlay widget (tracked as `tn-x85k`) will render both via the same MCP consumer path.

Not in scope for this crate:

- Pattern matching on rendered terminal text — that's for `plugins/` pattern packs, not this crate.
- Other harnesses — Codex, Copilot, and OpenCode will each live in their own `therminal-harness-<name>/` crate. The `/tmp/{codex,copilot}-state/` directories are watched by this crate's poller for historical reasons; that wiring will move out of here when the corresponding harness crates land.

## Hook-push input path

The file-polling pipeline (state poller + JSONL tailer) is the primary data source, but it requires the daemon to have filesystem access to `~/.claude/projects/` and `/tmp/claude-code-state/`. When this is not possible — notably when the daemon runs as a Windows native process and Claude Code runs inside WSL2 — the **hook-push path** (`src/hook_push.rs`) inverts the data flow.

Claude Code hook scripts call the `therminal` CLI from within WSL, which delivers structured `HookSignal`s to the daemon via `IpcRequest::PushAgentEvent`. The daemon dispatches them to the harness's broadcast channel via `HookPushSink::inject`, producing the same `TaggedAgentEvent`s as the file-polling path.

**Wire type**: `HookSignal` is intentionally flat (all fields `Option<String>`) so new hook events can be added without a protocol version bump. Supported events: `session_start`, `session_stop` (`Stop` / `SessionEnd`), `tool_state` (`PreToolUse` / `PostToolUse`), `subagent_start`, `subagent_stop`, `stop_failure`.

**Graceful degradation**: On Linux/WSL-hosted daemons, both paths run simultaneously. The JSONL tailer remains authoritative for historical data and capacity metrics; hook events provide lower-latency lifecycle signals. On Windows native (where JSONL files are unreachable), hook-push becomes the only source of observability.

## `claude-events` dev binary

`src/bin/claude-events.rs` is a minimal raw JSON-RPC client that connects to the daemon's MCP socket, performs a handshake, subscribes to `therminal://claude/events`, and prints styled lines per event. Flags: `--filter top|sub|all`, `--session <sid>`, `--verbose`, `--no-color`, `--json`. Run via:

```bash
cargo run -p therminal-harness-claude --bin claude-events
```

The binary is also the reference implementation for consuming the subscription protocol — it's the smallest thing that can exercise the whole end-to-end pipeline.
