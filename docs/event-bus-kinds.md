# Therminal Event Bus — Kind Vocabulary

**Prerequisite context:** See `docs/event-bus-spec.md` for the full
`TerminalEvent` record shape, cursor semantics, and filter grammar.

The `kind` field on every `TerminalEvent` is a free-form string. This document
defines a **recommended cross-source vocabulary** for kinds that appear across
harness crates, pattern packs, and core capabilities. Using this vocabulary
makes cross-source filtering useful and lets orchestrators write kind filters
that work regardless of which surface emits the event.

Conformance is **not enforced** at the bus level. Sources may emit any `kind`
string. The bus stores and forwards all events regardless of whether their kind
appears in this table.

---

## Cross-source kind vocabulary

| Kind | Emitted when | Typical body fields |
|---|---|---|
| `tool_call` | An agent invoked a tool (start of a tool-use turn) | `tool`, `input` (partial ok), `call_id` |
| `agent_state` | An agent's lifecycle state changed | `state` (see agent states below), `prev_state`, `agent_id` |
| `progress` | An ongoing task has measurable progress | `pct` (0–100 float), `label`, `step`, `total_steps` |
| `error` | Something went wrong | `severity` (`warn`/`error`/`fatal`), `message`, `code` |
| `waiting` | The agent is blocked pending human input | `reason` (`permission_prompt`/`question`/`confirmation`), `prompt` |
| `done` | A task completed | `ok` (bool), `summary`, `exit_code` |

### Agent states for `agent_state`

The `state` field in an `agent_state` event uses this vocabulary:

| State | Meaning |
|---|---|
| `idle` | Agent process is running but not doing work |
| `thinking` | Agent is processing / generating a response |
| `streaming` | Agent is streaming output to the terminal |
| `waiting` | Agent is blocked on human input (correlates with a `waiting` event) |
| `done` | Agent session has ended cleanly |

State transitions do not need to be exhaustive. An `agent_state` event may
skip states (e.g., `idle → streaming` without an intervening `thinking`). The
`prev_state` field is informational; orchestrators should not rely on it for
correctness.

---

## Source-specific kinds

Sources may emit kinds that do not appear in the cross-source vocabulary. By
convention, source-specific kinds are **dot-namespaced** with the `source_id`
as the prefix:

```
claude.thinking_started
claude.tool_result
codex.review_requested
codex.diff_applied
cargo-errors.compile_error
shell-integration.prompt_ready
```

This naming convention is not enforced by the bus. It is a recommendation so
that orchestrators can use glob filters (`kinds=claude.*`) to capture all kinds
emitted by a given source without enumerating them.

### Cross-source filtering vs. source-specific kinds

When all you need is "did an agent invoke a tool?", filter on `kind=tool_call`
regardless of source. When you need Claude-specific detail that has no
cross-source equivalent (e.g., `claude.thinking_started`), filter on the
namespaced kind directly.

If a source emits a source-specific kind that duplicates a cross-source kind
(e.g., emitting both `tool_call` and `claude.tool_call` for the same event),
those are separate events on the bus. Publishers should prefer the cross-source
kind for the primary event and use a namespaced kind only for detail that has no
cross-source equivalent.

---

## Body field conventions

These conventions apply across all kinds. Following them makes `jq`-based
dashboards and orchestrator logic simpler.

| Convention | Example |
|---|---|
| Boolean success flag in `done` is named `ok` | `{"ok": true, "summary": "3 tests passed"}` |
| Severity levels in `error` are lowercase | `"severity": "error"` |
| Progress percentage in `progress` is a float 0–100 | `"pct": 42.5` |
| Agent state strings are lowercase | `"state": "thinking"` |
| Tool names in `tool_call` match the tool's canonical identifier | `"tool": "bash"` |

These conventions are **not validated** by the bus. They are guidelines for
publishers.

---

## Adding new cross-source kinds

If a harness crate or pattern pack introduces a concept that applies to multiple
sources and is likely to be filtered cross-source by orchestrators, consider
proposing a new cross-source kind. The bar is: would an orchestrator reasonably
want to filter on this kind without caring which source emitted it?

File a beads issue with the proposed kind, its body schema, and at least two
concrete use cases before adding it to this table. Source-specific kinds need
no approval — just use the `source_id.` prefix convention.
