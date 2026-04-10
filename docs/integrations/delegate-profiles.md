# Delegate Profiles

Delegate profiles define isolated AI agent siblings that can be spawned into
a new therminal pane with a known role, working-directory policy, and
MCP/permission envelope. Therminal parses and validates
`[delegate.profiles.<name>]` in `therminal.toml` at config load time.

**Runtime spawning** is available via the `/gg-delegate` Claude Code skill
(`resources/skills/gg-delegate/`). The skill resolves a named profile into a
concrete `terminal.panes.create` invocation with `startup_command`, enforces
per-profile active-sibling limits, polls for completion, and captures the
result via `terminal.panes.capture_result`.

See also: [tn-ztv3 epic](../../.beads/) — sibling Claude delegation pattern
and the [architecture note](../../CLAUDE.md#integration-taxonomy) on when to
use spawned siblings vs. in-process subagents.

---

## Configuration schema

Profiles live under `[delegate.profiles.<name>]` in `therminal.toml`.  The
`[delegate]` table itself holds only `profiles`; unknown keys are rejected at
load time.

```toml
[delegate.profiles.<name>]
description     = "..."          # human-readable label (optional)
command         = "..."          # REQUIRED — launch template
working_dir     = "same"         # "same" | "worktree" | "scratch/{random}"
mcp_enabled     = []             # list of MCP tool-domain prefixes
permission_mode = "default"      # forwarded verbatim to the delegate
```

### Field reference

| Field | Type | Required | Default | Notes |
|---|---|---|---|---|
| `description` | string | no | `""` | Shown in `therminal agents` listings and UI |
| `command` | string | **yes** | — | Launch template; empty produces a load-time warning and the profile is unusable |
| `working_dir` | enum | no | `"same"` | See [Working-directory modes](#working-directory-modes) |
| `mcp_enabled` | list of strings | no | `[]` | MCP tool-domain prefix allowlist; empty = no extra grants |
| `permission_mode` | string | no | `"default"` | Passed verbatim to the delegate process at spawn time |

### Command template tokens

The `command` field is a shell-style string. The `/gg-delegate` skill
substitutes the following tokens at spawn time:

| Token | Replaced with |
|---|---|
| `{pane_id}` | The pane ID of the caller |
| `{session_id}` | The daemon session ID |
| `{cwd}` | The resolved working directory (after `working_dir` policy) |

### Working-directory modes

| Value | Behaviour |
|---|---|
| `"same"` | Inherit the cwd of the triggering pane (default) |
| `"worktree"` | Walk up from the triggering pane's cwd to find the nearest `.git` root; fall back to `"same"` if none found |
| `"scratch/{random}"` | Create a temporary directory under `<runtime_dir>/scratch/<uuid>`; removed when the delegate exits |

These values are validated at config load time. The `/gg-delegate` skill
resolves them at spawn time (e.g. `worktree` runs
`git rev-parse --show-toplevel`).

### Validation

- Unknown keys inside `[delegate]` or `[delegate.profiles.<name>]` produce a
  **hard deserialization error** (`deny_unknown_fields`) so typos surface
  immediately on config load.
- An empty `command` produces a **load-time warning** and the profile is left
  in the map but will be unusable until corrected.

---

## Example profiles

### 1. Planner

A read-only strategic planning agent.  It can observe pane content and session
state but has no shell-execution permission and runs from the git worktree root
so it has full project context.

```toml
[delegate.profiles.planner]
description     = "Strategic planning agent — read-only, no shell execution"
command         = "claude --pane {pane_id} --role planner"
working_dir     = "worktree"
mcp_enabled     = ["terminal.panes", "terminal.sessions"]
permission_mode = "plan"
```

**Why `working_dir = "worktree"`?**  The planner needs to see the full project
tree to reason about architecture.  Starting it at the git root avoids
path-relative confusion when the triggering pane is deep inside a subdirectory.

**Why `permission_mode = "plan"`?**  In Claude Code's permission model, `plan`
mode suppresses file-write and shell-execute tool calls, making it safe to give
the planner broad MCP access.

---

### 2. Browser-research

A web-research agent confined to browser MCP tools and a throw-away scratch
directory.  No local file writes, no access to pane content.

```toml
[delegate.profiles.browser-research]
description     = "Web research agent — browser MCP only, no local file writes"
command         = "claude --pane {pane_id} --role researcher"
working_dir     = "scratch/{random}"
mcp_enabled     = ["browser"]
permission_mode = "default"
```

**Why `working_dir = "scratch/{random}"`?**  Research tasks are stateless by
design; a fresh temp directory prevents accidental bleed into the project tree
and is cleaned up automatically when the delegate exits.

**Why only `["browser"]` in `mcp_enabled`?**  The researcher does not need to
read terminal state.  Restricting the MCP domain to `browser` reduces the
blast radius if the delegate is manipulated into unintended actions.

---

### 3. Adversarial-review

A second Claude instance that reads the same pane output and semantic regions
as the primary agent and critiques its reasoning.  Runs in the same directory
so it can inspect local files, but is not granted write permission.

```toml
[delegate.profiles.adversarial-review]
description     = "Adversarial code reviewer — full read access, no writes"
command         = "claude --pane {pane_id} --role adversarial-reviewer"
working_dir     = "same"
mcp_enabled     = ["terminal.panes", "terminal.semantic"]
permission_mode = "default"
```

**Why `working_dir = "same"`?**  The reviewer needs to inspect the same files
the primary agent is editing; inheriting the caller's cwd gives it the correct
relative context without any extra configuration.

**Why `terminal.semantic` in `mcp_enabled`?**  The `terminal.semantic`
domain exposes command transcripts and hotspot regions — the reviewer can query
what commands were run and what output was produced, giving it the context it
needs to critique the primary agent's decisions.

**When to use adversarial review:**  Subscribe `source_class=harness` on the
event bus to observe all harness events regardless of origin, then spawn this
profile in response to significant tool-call clusters or before committing a
diff.  See `docs/event-bus-spec.md` for the event envelope schema.

---

## Result capture

After a delegate finishes, the orchestrator recovers its output via
`terminal.panes.capture_result`.  The tool uses a two-path strategy:

### Primary path: OSC 633 transcript

When the delegate pane's shell emits OSC 633 marks (Therminal's shell
integration scripts do this automatically), the `CommandTracker` records
each command's start/end grid lines.  `capture_result` finds the last
finished command block and extracts the output lines between its prompt
line and completion line from the pane's scrollback + visible grid.

The response includes:
- `source: "transcript"` -- confirms the transcript path was used.
- `command` -- the command text (from OSC 633 E mark), if the shell
  provided it.
- `exit_code` -- from the OSC 633 D mark.
- `lines` -- the output text, oldest first, with blank lines stripped.

### Fallback path: grid content

When no finished OSC 633 command block exists (the delegate's shell lacks
shell integration, the delegate wrote output without running a shell
command, or no commands have completed), `capture_result` falls back to
returning the last N non-empty lines from the pane's scrollback + visible
grid.

The response includes:
- `source: "grid_fallback"` -- signals the fallback was used.
- `command` and `exit_code` are both `null`.
- `lines` -- the tail of visible output, oldest first.

### Limits

- `max_lines` (default 200, capped at 500) controls the maximum number of
  output lines returned.  When output exceeds the limit, the oldest lines
  are dropped and `truncated_lines` reports how many were cut.
- The transcript path only captures the *last* finished command.  If the
  delegate ran multiple commands, only the final one's output is returned.
  Use `terminal.semantic.query_commands` to enumerate all commands.
- The grid fallback does not distinguish command output from prompts or
  other terminal chrome.  It returns raw visible text.
- Output that has scrolled beyond the scrollback buffer limit (10,000
  lines) is lost to both paths.

### Usage from `/gg-delegate`

```
1. Spawn delegate via terminal.panes.create with startup_command
2. Poll terminal.panes.get_summary until agent_status shows idle/done
3. Call terminal.panes.capture_result { pane_id }
4. Read result.lines (check result.source to know which path was used)
5. Optionally destroy the delegate pane
```

---

## Tips

- Profile names may contain hyphens and underscores but must be valid TOML
  bare keys.  Quote names with unusual characters: `["delegate.profiles.my
  profile"]` is not valid; use `my-profile` instead.
- The `mcp_enabled` list is an **additive allowlist** on top of the trust tier
  already granted by `[trust]`.  A `sandboxed` trust tier combined with
  `mcp_enabled = ["terminal.panes"]` still restricts destructive operations —
  `mcp_enabled` only widens within what the tier allows.
- `permission_mode` is passed verbatim and is not validated by Therminal.
  Consult the harness documentation (e.g. Claude Code's `--permission-mode`
  flag) for the accepted values.
