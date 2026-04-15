# ADR 0005 — Keep Core Clean; Push Agent Observability to Harness Crates

## Status

Accepted.

## Context

Therminal has two different kinds of responsibilities:

1. **Core terminal/orchestration substrate**
   - sessions, panes, workspaces
   - spawn / attach / detach / resize
   - PTY IO, transport, rendering, event bus
   - target/runtime abstractions (local, WSL, container, remote)
   - generic metadata and tagging

2. **Harness-specific observability**
   - Claude / Codex / Copilot session identity
   - hook integration
   - JSONL transcript parsing
   - harness-specific OSC marker grammars
   - tool-use semantics
   - subagent semantics
   - context/model/session-title extraction

Recent work made the architectural boundary visible. The substrate is durable and reusable across many runtimes. Harness observability is valuable, but it changes on the harness's release cadence and carries tool-specific assumptions, file formats, and platform-specific recovery logic.

The original `thermal-desktop` codebase is useful history here:

- It had some good ideas around authoritative hook-sourced identity and explicit `parent_session_id`.
- It also leaned heavily on file watchers and hook-driven state as primary architecture.

Therminal should keep the first idea and reject the second.

## Decision

Therminal core will **not** treat agent state tracking as a core invariant.

Core crates own the universal substrate only. Harness-specific observability belongs in `therminal-harness-*` crates and plugs into core through stable extension points.

### Core owns

- session / pane / workspace lifecycle
- target abstraction and orchestration
- PTY transport and attach semantics
- rendering and input
- event bus shape and transport
- generic pane/session metadata transport
- generic tagging and registry infrastructure

### Harness crates own

- harness-specific OSC parsers
- hook integration
- state file / transcript readers
- session identity reconciliation for that harness
- tool-use / subagent / context semantics
- adapter logic for external integration contracts

### Daemon/app composition rule

The daemon and app may compose harness crates, but harness assumptions must not leak downward into the substrate. If all harness crates were removed, core should still be a coherent terminal/orchestration system.

## Consequences

### Positive

- Core stays smaller, cleaner, and more portable.
- Adding a new harness does not require baking its semantics into the substrate.
- Tool-specific churn is isolated to harness crates.
- The architecture aligns better with therminal's longer-term value: orchestration across local / WSL / containers / remote targets through one terminal surface.

### Negative

- Some integrations may feel less "built in" unless the daemon/app composes the harness crates well.
- Shared UI features need normalized event shapes rather than direct access to harness internals.
- Legacy code paths that assumed Claude support was effectively core may need refactoring.

## Guidance

When deciding where code belongs:

- If the logic only makes sense because Claude/Codex/Copilot exists, it belongs in a harness crate.
- If the logic would still make sense for a generic terminal that orchestrates shells/containers/remotes, it belongs in core.
- If a feature depends on process guessing, cwd matching, or filesystem heuristics to recover identity, prefer moving toward explicit registration at launch time rather than expanding core inference.

## Notes

This ADR does **not** reject observability work. It rejects putting harness-specific observability in the substrate.

The preferred long-term shape is:

- explicit launch/registration for managed sessions
- harness crates as adapters/enrichers
- passive process detection and file polling as fallback/compatibility paths
