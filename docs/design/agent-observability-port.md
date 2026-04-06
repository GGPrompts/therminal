# Agent Observability Port (from thermal-desktop)

## Background

thermal-desktop already has working subagent observability that we want in therminal:

- **`ClaudeStatePoller`** (`thermal-core/src/claude_state.rs`) — watches `/tmp/claude-code-state/` and `/tmp/codex-state/` for JSON state files written by agent hooks. Tracks current tool, status (Idle/Processing/ToolUse/AwaitingInput), subagent count, context %, working dir, parent session.
- **`AgentTimeline`** (`thermal-conductor/src/agent_timeline.rs`) — records each tool change as a colored timeline entry (Read/Write/Execute/Thinking/Idle), rendered as a GPU bar at the bottom of the window. 500-entry ring buffer, scrollable.
- **`InjectWatcher`** (`thermal-conductor/src/inject.rs`) — file-based cross-pane prompt injection via `/tmp/thermal-inject/`. Therminal already does this via daemon IPC, so this piece does **not** need porting.

The original framing was "we want WebView panes for HTML rendering" — but the real need is just live subagent visibility and JSONL state tailing, which thermal-desktop already solves natively in the GPU renderer. No webview needed.

## Goals

1. Live subagent state visible in therminal (current tool, status, context %)
2. Tool-usage timeline rendered as a GPU overlay widget
3. Optional JSONL-tailing pane backend for structured agent state files
4. All data flows through the existing daemon event bus and AgentRegistry — no new IPC paths
5. MCP tool surface so external agents can drive observability views

## Non-goals

- WebView/HTML rendering (deferred indefinitely — see option D in earlier design discussion if revisited)
- Plugin system for harness-specific code paths (integrations stay documented, not built-in — see CLAUDE.md "Harness integrations" section)
- Markdown/SVG rich text rendering (separate future work if ever needed)

## Architecture

### Layer 1: State poller in `therminal-daemon`

Lift `claude_state.rs` from thermal-core into `therminal-daemon/src/agent_state_poller.rs`. The poller already matches therminal's existing `AgentRegistry` model — they're the same shape, just sourced differently:

- AgentRegistry today: process-tree detection + cadence analysis (passive)
- AgentStatePoller: filesystem state files written by hooks (cooperative)

Both should feed into a unified `AgentInfo` struct on `AgentRegistry`. The hook-based source is strictly more accurate when available (it knows the *current tool*, not just "an agent process is running"), so it should override the passive detection.

**Files to touch:**
- `crates/therminal-daemon/src/agent_state_poller.rs` (new) — port of `claude_state.rs`
- `crates/therminal-daemon/src/agent_registry.rs` (existing) — add hook-source fields, merge logic
- `crates/therminal-protocol/src/events.rs` (existing) — add `AgentToolChanged`, `AgentContextUpdated` event variants

**Lift estimate:** 1 day. Mostly path adjustments and AgentRegistry integration.

### Layer 2: AgentTimeline overlay widget in `therminal-app`

Port `agent_timeline.rs` from thermal-conductor. The render path is already prepared — `OverlayLayer::Widget` tier exists in `crates/therminal-app/src/overlay.rs` exactly for things like this.

**Files to touch:**
- `crates/therminal-app/src/widgets/agent_timeline.rs` (new) — port of thermal-conductor file
- `crates/therminal-app/src/window/render.rs` — add render pass that pushes timeline quads via `OverlayLayer::push_rect` with `OverlayTier::Widget`
- `crates/therminal-app/src/window/keybindings.rs` — add toggle keybinding (suggest `Ctrl+Shift+T` for "timeline")
- `crates/therminal-core/src/config.rs` — add `[widgets.agent_timeline] enabled, height_px, max_entries` config keys (must be wired, per project style guide)

The widget subscribes to `AgentRegistry` events from the daemon, maintains its own ring buffer, and re-renders on each frame when visible.

**Lift estimate:** 2 days.

### Layer 3: JSONL tail pane backend (new — not in thermal-desktop)

A new `PaneBackendKind::JsonlTail` variant that watches a file and renders structured rows in the existing grid. Useful for raw subagent state files, build logs, structured app logs.

**Files to touch:**
- `crates/therminal-app/src/pane/backend.rs` — add `JsonlTail { path, parser, rows }` variant + `PaneBackend` impl
- `crates/therminal-app/src/pane/jsonl_tail.rs` (new) — file watcher (notify crate), JSONL parser, row layout with color-coded fields
- `crates/therminal-daemon/src/mcp/tools/panes.rs` — add `terminal.panes.create_tail({ path, format })` MCP tool
- `crates/therminal-protocol/src/mcp.rs` — schema for the new tool

The JSONL parser is format-aware: known formats (`claude-state`, `codex-state`, `generic`) get column layouts and color schemes; unknown files render as raw colored JSON.

**Lift estimate:** 3–5 days.

### Layer 4: MCP surface

New tools so external agents can spawn observability views without therminal shipping harness-specific code:

- `terminal.panes.create_tail({ path, format })` — spawn a JSONL tail pane
- `terminal.agents.get_state({ session_id })` — read current AgentRegistry state for a session
- `terminal.widgets.timeline.toggle({ visible })` — show/hide the agent timeline widget
- Subscribe to existing `terminal://agents` resource for live updates

All gated by trust tier — observation tools at Sandboxed, widget toggles at Supervised.

**Lift estimate:** 1 day (mostly schema + handler stubs, since AgentRegistry already exists).

## Total estimate

| Layer | Effort |
|---|---|
| 1. State poller port | 1 day |
| 2. Timeline overlay widget | 2 days |
| 3. JSONL tail pane backend | 3–5 days |
| 4. MCP tool surface | 1 day |
| **Total** | **~7–9 days (~1.5 weeks)** |

Compare to ~4–8 weeks for a production WebView pane.

## Open questions

1. **Hook discovery:** thermal-desktop hardcodes `/tmp/claude-code-state/` and `/tmp/codex-state/`. Should therminal make this configurable per harness via `docs/integrations/<harness>.md` setup snippets? Probably yes — the directory location is the integration contract.
2. **Timeline persistence:** thermal-desktop's timeline is in-memory only. Should therminal persist it across daemon restarts via the existing `sessions.json` debounced persistence? Probably no for v1 — it's observability, not state.
3. **JSONL pane format negotiation:** how does the user choose between built-in formats and "render as raw colored JSON"? Suggest: if `format` is omitted, sniff the first line; if it matches a known schema, use it.
4. **Codex state adapter:** thermal-desktop ships a `scripts/codex-state-adapter.sh` that translates Codex output into the Claude state file format. Should therminal vendor this script under `resources/integrations/codex/`? Yes — keeps the integration contract self-contained.

## Source references

All paths under `~/projects/thermal-desktop`:

- `crates/thermal-core/src/claude_state.rs` (502 lines) — state poller, ClaudeSessionState, file watcher
- `crates/thermal-conductor/src/agent_timeline.rs` (181 lines) — timeline ring buffer, ToolCategory, render data
- `crates/thermal-conductor/src/inject.rs` (235 lines) — reference only, **do not port** (use daemon IPC)
- `scripts/codex-state-adapter.sh` — Codex → Claude state format adapter

## Beads issues to file

Once `bd` is back up, file these as a tn epic + 4 children (P2):

1. **tn-epic:** Port agent observability from thermal-desktop
2. **tn-child:** State poller → daemon AgentRegistry integration
3. **tn-child:** Timeline overlay widget (Phase 6 widget tier)
4. **tn-child:** JSONL tail pane backend
5. **tn-child:** MCP tools for observability + integration docs in `docs/integrations/`

Dependencies: 2 blocks 3 and 5. 4 is independent. 5 depends on 3 and 4 for full surface.
