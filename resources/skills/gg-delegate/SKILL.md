---
name: gg-delegate
description: >
  Use when the user wants to spawn a sibling Claude session into a visible
  therminal pane using a named delegate profile. Resolves profiles from
  therminal.toml, enforces active-sibling limits, waits for completion,
  and reports the recovered result back to the orchestrator.
---

# gg-delegate skill

You are spawning a **sibling Claude session** into a dedicated therminal pane.
The sibling runs in isolation — its own context window, its own MCP grants, its
own working directory — while you (the orchestrator) retain visibility and can
capture its output when it finishes.

This is the capability-distribution counterpart to in-process subagents:
subagents share your cache but inherit your tool surface; siblings trade cache
for full capability isolation (different tools, persona, working directory).

## When to use siblings vs subagents

| Signal | Use subagent (Agent tool) | Use sibling (/gg-delegate) |
|--------|--------------------------|---------------------------|
| Needs your same tools/files | Yes | No |
| Needs different MCP grants | No | Yes |
| Needs a different persona/role | No | Yes |
| Needs a clean working directory | No | Yes |
| Batch of similar tasks | Yes (cheaper) | No |
| Long-running autonomous work | No | Yes |
| Result must feed back into your context | Both work | Yes (capture_result) |

## Prerequisites

1. **Delegate profiles configured** in `therminal.toml` under
   `[delegate.profiles.<name>]`. See `docs/integrations/delegate-profiles.md`
   for the schema and examples.
2. **therminal daemon running** — the MCP tools must be available.
3. **Shell integration active** in delegate panes for best result capture
   (OSC 633 marks). Works without it via grid fallback, but transcript
   capture is more precise.

## Trigger phrases

Load this skill when the user says things like:

- "/gg-delegate planner 'refine the backlog'"
- "spawn a sibling to review this code"
- "delegate browser research to a sibling"
- "run an adversarial review in a separate pane"
- "spawn a Claude sibling for..."

## Invocation format

```
/gg-delegate <profile-name> <task description>
```

- `<profile-name>` — must match a key under `[delegate.profiles.*]` in
  `therminal.toml`
- `<task description>` — the prompt/instruction passed to the sibling

## Execution protocol

Follow these steps exactly. Each step uses existing MCP tools or CLI commands.

### Step 1: Resolve the profile

Read the delegate profile from config. The profile defines `command`,
`working_dir`, `mcp_enabled`, and `permission_mode`.

```bash
# Verify the profile exists and inspect its configuration
therminal config delegate-profiles 2>/dev/null || \
  grep -A 10 'delegate.profiles.<PROFILE>' ~/.config/therminal/therminal.toml
```

If the profile does not exist, stop and tell the user:
> Profile `<name>` not found in therminal.toml. Add it under
> `[delegate.profiles.<name>]`. See `docs/integrations/delegate-profiles.md`.

### Step 2: Check active-sibling limit

Before spawning, verify no more than **1 active sibling per profile** is
already running. This prevents runaway spawning.

```bash
# List all panes and check for active delegates with the same profile tag
therminal pane list --json 2>/dev/null | \
  jq '[.[] | select(.tags.delegate_profile == "<PROFILE>" and .tags.delegate_status == "active")] | length'
```

Or via MCP:

```
terminal.panes.list {}
# Filter results for panes tagged with delegate_profile=<PROFILE>
# and delegate_status=active
```

If the count is >= 1, stop and tell the user:
> A sibling with profile `<name>` is already active in pane <id>.
> Wait for it to finish, or destroy it with `terminal.panes.destroy`.

The user can override this by explicitly saying "spawn another" or
"ignore the limit".

### Step 3: Show first-spawn cost notice

On the **first** spawn of any delegate profile within this conversation,
display a cost notice. Skip it on subsequent spawns of the same profile.

> **Cost notice:** Spawning a sibling Claude session starts a fresh context
> window (~5-8k tokens for initialization). The sibling runs independently
> and will consume tokens at its own rate.
>
> Profile: `<name>` -- <description from profile>
> Working dir: <resolved working_dir>
> Permission mode: <permission_mode>
>
> Proceed? [Y/n]

If the user says no, abort. If yes (or if this is a subsequent spawn of the
same profile), continue.

Track which profiles have been spawned in this conversation to suppress the
notice on repeats.

### Step 4: Build the startup command

Construct the command that will be injected into the new pane after the shell
prompt appears. This is the profile's `command` template with tokens
substituted, plus the task description appended as a prompt argument.

**Template token substitution:**

| Token | Value |
|-------|-------|
| `{pane_id}` | The orchestrator's current pane ID (from `terminal.panes.list`) |
| `{session_id}` | The daemon session ID |
| `{cwd}` | The resolved working directory |

**Building the final command:**

The startup_command should launch the Claude session with the task. A typical
resolved command looks like:

```
claude --print "Your task: <task description>"
```

Or, using the profile's command template:

```
claude --pane <pane_id> --role <role> --print "<task description>"
```

The exact command depends on the profile's `command` field. Substitute tokens
and append the task as appropriate for the harness.

**Important:** Use `--print` mode (or equivalent) so the sibling runs the task
and exits cleanly when done. This makes result capture reliable. If the
profile's command already includes interactive flags, respect them — the
sibling will stay alive and you will need to poll for completion instead of
waiting for exit.

### Step 5: Spawn the delegate pane

Use `terminal.panes.create` to spawn a new pane with the constructed
startup_command:

```
terminal.panes.create {
  "split_from": <orchestrator_pane_id>,
  "split_direction": "horizontal",
  "startup_command": "<resolved command>",
  "cwd": "<resolved working directory>"
}
```

**Capture the returned `pane_id`** — you need it for all subsequent steps.

Immediately **tag the pane** to mark it as a delegate:

```
terminal.panes.tag {
  "pane_id": <new_pane_id>,
  "tags": {
    "delegate_profile": "<profile_name>",
    "delegate_status": "active",
    "delegate_task": "<brief task summary>"
  }
}
```

### Step 6: Wait for the sibling to finish

Poll the delegate pane until the sibling transitions to idle/done. Use
`terminal.panes.get_summary` as the cheapest polling primitive (~100 bytes
per call):

```
# Poll loop — check every 10-15 seconds
terminal.panes.get_summary { "pane_id": <delegate_pane_id> }
```

**Completion signals** (check in order):

1. **`agent_status` is null/absent** — the agent process has exited (best
   signal for `--print` mode delegates).
2. **`agent_status` is `"idle"` or `"awaiting_input"`** — the agent finished
   its task but the session is still alive.
3. **`content_hash` stops changing** across 3+ consecutive polls — the pane
   has settled (fallback heuristic for non-integrated shells).

While waiting, you can continue with other work. The sibling runs
independently in its own pane.

**Timeout:** If 10 minutes pass with no completion signal, warn the user:
> Sibling in pane <id> (profile: <name>) has been running for 10 minutes
> without completing. It may be stuck. Check the pane or destroy it.

### Step 7: Capture the result

Once the sibling is done, recover its output:

```
terminal.panes.capture_result {
  "pane_id": <delegate_pane_id>,
  "max_lines": 200
}
```

The response includes:
- `source` — `"transcript"` (preferred, from OSC 633) or `"grid_fallback"`
- `lines` — the output text, oldest first
- `command` — the command that was run (if transcript source)
- `exit_code` — the process exit code (if transcript source)
- `truncated_lines` — how many lines were dropped to fit max_lines

### Step 8: Update tags and report

Update the delegate pane's status tag:

```
terminal.panes.tag {
  "pane_id": <delegate_pane_id>,
  "tags": {
    "delegate_status": "done"
  }
}
```

Report the result back to the user/orchestrator:

> **Delegate complete** (profile: `<name>`, pane: <id>)
> Source: <transcript|grid_fallback>
> Exit code: <code or "unknown">
>
> **Result:**
> <captured output lines, summarized if very long>
>
> The delegate pane is still open for inspection. Destroy it with
> `terminal.panes.destroy { "pane_id": <id> }` when done.

## Error handling

### Profile not found
Stop immediately. The user must add the profile to `therminal.toml`.

### Pane creation fails
Report the error. Common causes: no daemon running, session does not exist,
no space for split. Suggest the user check `therminal pane list`.

### Sibling crashes or exits with non-zero
Capture whatever output exists via `capture_result` (the grid fallback still
works even without shell integration). Report the exit code and output. Tag
the pane as `delegate_status: "failed"`.

### Result capture returns empty
The sibling may have produced no output, or scrollback was exhausted. Try
`terminal.panes.peek` as a last resort, then report what you found.

## Example: full delegation flow

```
User: /gg-delegate planner "refine the backlog for phase 7"

Orchestrator:
1. Resolves profile "planner" from therminal.toml:
   - command: "claude --pane {pane_id} --role planner"
   - working_dir: worktree
   - permission_mode: plan

2. Checks active siblings: 0 with profile "planner" -> OK

3. Shows cost notice (first spawn):
   "Spawning sibling Claude session. Profile: planner..."
   User confirms.

4. Builds startup command:
   "claude --pane 1 --role planner --print 'refine the backlog for phase 7'"

5. Creates pane:
   terminal.panes.create { split_from: 1, startup_command: "...", cwd: "/project" }
   -> pane_id: 5

6. Tags pane:
   terminal.panes.tag { pane_id: 5, tags: { delegate_profile: "planner", ... } }

7. Polls get_summary until agent_status is null/idle

8. Captures result:
   terminal.panes.capture_result { pane_id: 5 }
   -> { source: "transcript", lines: [...], exit_code: 0 }

9. Reports back with the planner's output
```

## Working directory resolution

The profile's `working_dir` field controls where the delegate starts:

| Value | Resolution |
|-------|-----------|
| `"same"` | Use the orchestrator's current working directory |
| `"worktree"` | Walk up from cwd to find the nearest `.git` root. Fall back to `"same"` if none found. In practice, use `git rev-parse --show-toplevel` |
| `"scratch/{random}"` | Create a temp directory. Pass it as `cwd` to `panes.create`. The delegate is responsible for cleanup, or you can destroy the pane (which kills the PTY) |

For `"worktree"`:
```bash
CWD=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
```

## Constraints

- **One active sibling per profile** by default. This is a soft limit enforced
  by tag checking, not a hard daemon constraint. The user can override it.
- **Cost awareness**: Each sibling is a full Claude session. Don't spawn
  siblings for tasks a subagent could handle.
- **No nested delegation**: Siblings should not spawn their own siblings.
  Keep the delegation tree flat (orchestrator -> siblings, not
  orchestrator -> sibling -> sibling).
- **Result capture is best-effort**: The transcript path requires shell
  integration. Without it, you get the grid fallback which may include
  prompts and other terminal chrome mixed in with the output.
