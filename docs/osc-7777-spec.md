# OSC 7777 — Cooperative Agent Self-Reporting Protocol

Version 1.0  
Status: Draft  
Reference implementation: [`crates/therminal-terminal/src/interceptor.rs`](../crates/therminal-terminal/src/interceptor.rs)

---

## Overview

OSC 7777 is a terminal escape sequence protocol that lets AI coding agents voluntarily
report their identity and current state to the terminal emulator. When an agent emits
these sequences, the terminal has authoritative, real-time information about what the
agent is doing — without resorting to heuristics such as process-tree scanning or
output-cadence analysis.

The protocol is intentionally minimal: one sequence format, a small fixed set of keys,
and an open extension mechanism via unknown-key passthrough.

---

## Wire Format

```
ESC ] 7777 ; key=value [ ; key=value ... ] ST
```

- `ESC ]` is the OSC introducer (`\x1b\x5d` or `\x9d` in 8-bit environments).
- `7777` is the OSC parameter identifying this protocol.
- Each `key=value` pair is separated by a semicolon (`;`).
- `ST` is the String Terminator: either `ESC \` (`\x1b\x5c`) or BEL (`\x07`).

The VTE parser splits OSC content on `;`, so the terminal receives the pairs as
individual byte slices. Key and value are separated by the first `=` character in each
slice; a slice without `=` is silently ignored.

---

## Fields

### Required

| Key     | Type   | Description |
|---------|--------|-------------|
| `agent` | string | Agent identifier. Non-empty. Examples: `claude-code`, `aider`, `copilot`, `codex`. |

A sequence that is missing the `agent` key, or has an empty `agent` value, is consumed
by the terminal but produces no event. The terminal must not treat such a sequence as an
error.

### Optional

| Key      | Type    | Description |
|----------|---------|-------------|
| `state`  | string  | Agent's current operational state. See [State Values](#state-values). |
| `tool`   | string  | Name of the tool currently being invoked. Meaningful only when `state=tool_use`. |
| `tokens` | integer | Cumulative token count for the current request or session (decimal, unsigned). |
| `model`  | string  | Model identifier. Examples: `opus-4`, `gpt-4o`, `claude-sonnet-4-5`. |

Unknown keys are silently ignored. This allows future versions to add keys without
breaking existing terminals.

---

## State Values

The `state` field communicates where in its processing cycle the agent currently is.

| Value           | Meaning |
|-----------------|---------|
| `idle`          | Agent is not actively working. Prompt is visible; waiting for user input or the next task. |
| `processing`    | Agent has received input and is working, but has not yet begun producing visible output. Covers initial reasoning, request parsing, and any pre-output computation. |
| `streaming`     | Agent is actively streaming text output to the terminal. |
| `thinking`      | Agent is performing extended internal reasoning (e.g., extended thinking mode) before or between outputs. |
| `tool_use`      | Agent is invoking a tool. The `tool` field should be set to the tool name. |
| `awaiting_input` | Agent has paused and is waiting for the user to provide additional input before continuing. |

Agents should emit a state-transition sequence whenever their state changes. There is no
required ordering; any state may follow any other state.

---

## Sequence Examples

### Minimal — agent identity only

```
\x1b]7777;agent=claude-code\x07
```

Announces the agent without reporting a state. Sufficient for terminals that only need
to identify which agent is running.

### State transition to idle

```
\x1b]7777;agent=claude-code;state=idle\x07
```

### Active tool use

```
\x1b]7777;agent=claude-code;state=tool_use;tool=Edit\x07
```

### Full report

```
\x1b]7777;agent=claude-code;state=tool_use;tool=Edit;tokens=12345;model=opus-4\x07
```

### State transition to streaming

```
\x1b]7777;agent=aider;state=streaming;model=gpt-4o\x07
```

### Awaiting input

```
\x1b]7777;agent=codex;state=awaiting_input\x07
```

---

## Design Rationale

### Why a new OSC number?

OSC 7777 was selected because it does not conflict with any registered or widely-deployed
terminal extension:

| OSC   | Protocol        | Purpose                    |
|-------|-----------------|----------------------------|
| 0/2   | ECMA-48         | Window/icon title          |
| 7     | Standard        | Current working directory  |
| 8     | Various         | Hyperlinks                 |
| 10/11 | XTerm           | Color query/set            |
| 52    | XTerm           | Clipboard access           |
| 133   | FinalTerm       | Shell integration marks    |
| 633   | VS Code         | Shell integration (extended)|
| 1337  | iTerm2          | Image protocol, misc       |
| 7777  | **This spec**   | Agent self-reporting       |

The number 7777 was chosen to be far from the dense 0–200 range of existing assignments,
easy to remember, and available in all known terminal emulators at the time of writing.

### Why semicolon-delimited key=value?

This format is consistent with iTerm2's OSC 1337 and VS Code's OSC 633. It is trivially
parseable in any language without a JSON parser, and the VTE standard splits OSC content
on `;` naturally. Keys and values are plain ASCII, avoiding encoding ambiguity.

### Why not JSON?

JSON would require an embedded parser in the terminal's VTE dispatch path, add overhead
per sequence, and introduce potential parse errors on malformed payloads. The key=value
format provides the same information with zero parsing complexity. Robustness was
preferred over expressiveness.

---

## For Terminal Implementors

### Parsing

1. Match OSC sequences where `params[0] == b"7777"`.
2. Iterate `params[1..]`. For each param, split on the first `=` to get `(key, value)`.
   Params without `=` are skipped without error.
3. Collect the known keys (`agent`, `state`, `tool`, `tokens`, `model`). Ignore unknown
   keys silently — this is the extension mechanism.
4. If `agent` is absent or empty, consume the sequence (return `true` to suppress
   "unhandled OSC" noise) but emit no event.
5. Emit an agent-report event with the collected fields.

### Integration with agent detection

OSC 7777 reports are authoritative. When an agent emits this sequence, the terminal
should prefer it over heuristic detection (process-tree scanning, output-cadence
analysis). The self-reported state and identity should take priority in the agent
registry.

### State display

Terminals may map the `state` values to UI indicators. Suggested interpretations:

- `idle` / `awaiting_input`: passive indicator (e.g., neutral icon or no badge)
- `processing` / `thinking`: spinner or "thinking" badge
- `streaming`: active output indicator
- `tool_use`: tool name badge, active indicator

### Forward compatibility

Terminals must silently ignore unknown `state` values and unknown keys. New states and
keys may be added in future versions of this spec.

### Reference parser

The Therminal reference implementation is in
[`crates/therminal-terminal/src/interceptor.rs`](../crates/therminal-terminal/src/interceptor.rs),
in the `handle_osc_7777` method of `TherminalInterceptor`. It includes a full test suite
covering the normal path, missing/empty `agent`, unknown keys, malformed params, and
invalid `tokens` values.

---

## For Agent Implementors

### When to emit

Emit an OSC 7777 sequence whenever your state changes. At minimum:

- On startup, emit `state=idle` to announce presence.
- Before beginning work on a user request, emit `state=processing`.
- When streaming output to the terminal, emit `state=streaming`.
- When invoking a tool, emit `state=tool_use` with `tool=<tool_name>`.
- When waiting for the user, emit `state=awaiting_input`.
- When returning to idle after completing a task, emit `state=idle`.

### How to emit

Write the raw bytes to stdout. In most languages:

**Shell / bash:**
```bash
printf '\033]7777;agent=my-agent;state=idle\007'
```

**Python:**
```python
import sys
def report_state(agent: str, state: str, tool: str = None, tokens: int = None, model: str = None):
    parts = [f"agent={agent}", f"state={state}"]
    if tool:
        parts.append(f"tool={tool}")
    if tokens is not None:
        parts.append(f"tokens={tokens}")
    if model:
        parts.append(f"model={model}")
    sys.stdout.write(f"\x1b]7777;{';'.join(parts)}\x07")
    sys.stdout.flush()
```

**Rust:**
```rust
fn report_state(agent: &str, state: &str) {
    print!("\x1b]7777;agent={};state={}\x07", agent, state);
    // flush stdout if buffered
}
```

**TypeScript / Node.js:**
```typescript
function reportState(agent: string, state: string, extras?: { tool?: string; tokens?: number; model?: string }) {
    const parts = [`agent=${agent}`, `state=${state}`];
    if (extras?.tool) parts.push(`tool=${extras.tool}`);
    if (extras?.tokens !== undefined) parts.push(`tokens=${extras.tokens}`);
    if (extras?.model) parts.push(`model=${extras.model}`);
    process.stdout.write(`\x1b]7777;${parts.join(';')}\x07`);
}
```

### Constraints

- The `agent` identifier should be a stable, lowercase, hyphenated string. Use the same
  value in every sequence from the same agent binary (e.g., always `claude-code`, not
  varying between sessions).
- Values must not contain `;` or `\x07` (BEL), as these are the field separator and
  sequence terminator respectively. Percent-encode or strip such characters if they could
  appear in a tool name or model identifier.
- The `tokens` field is an unsigned decimal integer. Do not include commas, units, or
  other formatting.
- Sequences are best-effort. If the terminal does not support OSC 7777, the sequence is
  silently discarded. Do not rely on acknowledgement.

### Detection

To detect whether the running terminal supports OSC 7777, check the `TERM_PROGRAM`
environment variable. Therminal sets `TERM_PROGRAM=therminal`. Other terminals may adopt
the protocol and advertise support through their own mechanisms.

Agents may emit OSC 7777 unconditionally — non-supporting terminals ignore unknown OSC
sequences without rendering artifacts.

---

## Changelog

| Version | Date       | Changes |
|---------|------------|---------|
| 1.0     | 2026-04-06 | Initial specification, based on Therminal reference implementation. |
