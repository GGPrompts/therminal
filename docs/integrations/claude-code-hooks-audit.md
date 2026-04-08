# Claude Code Hooks Audit

**Scope:** Hook additions from ~2026-03-11 through 2026-04-08 (v2.1.72 – v2.1.96).
**Sources:** [Official hooks reference](https://code.claude.com/docs/en/hooks) · [Changelog](https://code.claude.com/docs/en/changelog) · [GitHub releases](https://github.com/anthropics/claude-code/releases)
**Cross-referenced against:** `~/projects/conductor-mcp/plugins/conductor/hooks/scripts/state-tracker.sh`
**Related issue:** tn-dboq · **First-party hook port:** tn-97w8

---

## 1. Currently-Used Hooks and Fields

What `state-tracker.sh` handles today, wired in `~/.claude/settings.json`:

| Hook event | Arg passed | stdin fields read | hookSpecificOutput written |
|---|---|---|---|
| `SessionStart` | `session-start` | none | none |
| `UserPromptSubmit` | `user-prompt` | `.prompt` | none |
| `PreToolUse` | `pre-tool` | `.tool_name`, `.tool_input` (various) | none |
| `PostToolUse` | `post-tool` | `.tool_name`, `.tool_input` | none |
| `Stop` | `stop` | none | none |
| `SubagentStart` | `subagent-start` | `.agent_type` | none |
| `SubagentStop` | `subagent-stop` | none (uses counter file) | none |
| `Notification` | `notification` | `.notification_type` | none |

**Not wired:** `PreCompact` is configured but empty (`"PreCompact": []`). No other events wired.

**State file written:** `/tmp/claude-code-state/<session_id>.json` with fields:
`session_id`, `claude_session_id`, `status`, `current_tool`, `subagent_count`,
`context_percent` (from external statusline context file), `context_window.{size,input_tokens,output_tokens}`,
`working_dir`, `last_updated`, `tmux_pane`, `pid`, `hook_type`, `details`.

---

## 2. New Hooks and Fields Since ~2026-03-11

Version attribution is from changelog and GitHub releases. "Exact version not confirmed" is noted where the source text was ambiguous.

### New hook events (completely absent from state-tracker.sh)

| Version | Date | Event | Stdin fields | Decision control | Notes |
|---|---|---|---|---|---|
| 2.1.78 | Mar 17 | `StopFailure` | `error_type` (`rate_limit\|authentication_failed\|billing_error\|invalid_request\|server_error\|max_output_tokens\|unknown`) | none (output ignored) | Fires when turn ends due to API error |
| 2.1.83 | Mar 25 | `InstructionsLoaded` | `file_path`, `memory_type` (`User\|Project\|Local\|Managed`), `load_reason`, `globs?`, `trigger_file_path?`, `parent_file_path?` | none | Fires when CLAUDE.md or `.claude/rules/*.md` loads |
| 2.1.83 | Mar 25 | `TaskCreated` | `task_id`, `task_subject`, `task_description?`, `teammate_name?`, `team_name?` | exit 2 = block; `{"continue":false}` = stop teammate | Fires when Task tool creates a task |
| 2.1.84 | Mar 26 | `Elicitation` | `mcp_server_name`, `elicitation_request.fields[]` | `hookSpecificOutput.action` (`accept\|decline\|cancel`) + `content` | Intercept MCP server user-input requests |
| 2.1.84 | Mar 26 | `ElicitationResult` | `mcp_server_name`, `user_response` | override via `hookSpecificOutput` | Observe/override user response to MCP elicitation |
| 2.1.76 | Mar 14 | `PostCompact` | `compact_trigger` (`manual\|auto`) | none | Fires after compaction completes |
| 2.1.85 | Mar 26 | `CwdChanged` | `cwd` (updated) | none | `CLAUDE_ENV_FILE` available; useful for direnv |
| 2.1.85 | Mar 26 | `FileChanged` | `file_path`, `file_name` | none | Matcher on basename; `CLAUDE_ENV_FILE` available |
| 2.1.89 | Apr 1 | `PermissionDenied` | `tool_name`, `tool_input`, `tool_use_id`, `reason` | `{retry:true}` to allow model to retry | Fires after auto-mode classifier denials |
| ~2.1.69 | ~Mar 5 | `TeammateIdle` | (common fields only) | `{"continue":false,"stopReason":"..."}` | Fires when agent-team teammate goes idle |
| ~2.1.69 | ~Mar 5 | `TaskCompleted` | `task_id`, `task_subject`, `task_description?`, `teammate_name?`, `team_name?` | `{"continue":false}` to stop teammate | Companion to TaskCreated |
| ~prior | prior | `WorktreeCreate` | `worktree_path` | exit non-zero = creation fails | Existed before window; documented now |
| ~prior | prior | `WorktreeRemove` | `worktree_path` | none | — |
| ~prior | prior | `SessionEnd` | `session_end_reason` (`clear\|resume\|logout\|prompt_input_exit\|bypass_permissions_disabled\|other`) | none | Was broken (1.5s kill); fixed ~2.1.78 |
| ~prior | prior | `ConfigChange` | `config_source` | `decision:"block"` | Fires on any settings file change |

### New fields on already-wired events

| Version | Date | Event | Field added | Notes |
|---|---|---|---|---|
| 2.1.84 | Mar 26 | `UserPromptSubmit` hookSpecificOutput | `sessionTitle: string` | Set pane title from hook; **not yet read by state-tracker** |
| 2.1.83 | Mar 25 | `SubagentStart` / `SubagentStop` stdin | `agent_id: string` | Per-subagent UUID; `agent_type` already existed |
| 2.1.83 | Mar 25 | All events (subagent context) | `agent_id`, `agent_type` in common fields when running inside subagent | — |
| 2.1.78 | Mar 17 | `PostToolUse` stdin | `file_path` now absolute (was relative) | Bug fix; scripts relying on relative paths break |
| 2.1.83 | Mar 25 | Statusline command | `worktree.{name,path,branch,originalRepoDirectory}` | Only present in `--worktree` sessions |
| 2.1.85 | Mar 26 | Hook config | `if` field (permission rule syntax e.g. `Bash(git *)`) | Conditional execution; not a stdin field |
| 2.1.89 | Apr 1 | `PreToolUse` hookSpecificOutput | `"defer"` permission decision | Headless pause/resume; requires v2.1.89+ |
| 2.1.83 | Mar 25 | HTTP hooks | New `type:"http"` handler | Can POST to URL; returns JSON |

### Token/cost accounting

No hook event receives per-turn usage stats (token counts, cost) in stdin. The `/cost` and `/stats` commands show this data interactively but it is not exposed to hook stdin. Token data visible to state-tracker comes only from the external statusline script reading a separate context file (indirect, stale up to 60 s). **This is an upstream gap, not a missed field.**

---

## 3. Recommendations (sorted by impact)

### REC-1: Read `hookSpecificOutput.sessionTitle` on `UserPromptSubmit` — **inference-unrecoverable**

**What to add:** In the `user-prompt` branch of state-tracker.sh (and the tn-97w8 first-party script), write `sessionTitle` to the state file if the hook is expected to _emit_ it — or flip the direction: therminal's first-party hook should _output_ `hookSpecificOutput.sessionTitle` set to the current pane's workspace/session name so Claude Code's UI reflects it. The existing tn-5wrx issue tracks using this for the pane-header center title.

**Therminal feature unlocked:** Pane header shows the Claude session title as set by the first message or the hook. Currently therminal can only guess from PTY output.

**Inference-unrecoverable:** Yes. Claude Code sets this title internally; no OSC sequence carries it.

**tn-97w8 change:** In the `user-prompt` case, emit:
```json
{ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "sessionTitle": "<derived title>" } }
```

### REC-2: Read `agent_id` on `SubagentStart` / `SubagentStop` — **inference-unrecoverable**

**What to add:** The `agent_id` field (added v2.1.83) is a stable UUID per subagent instance. State-tracker currently only increments/decrements a counter. Reading `agent_id` allows tracking which specific subagent started/stopped, surviving race conditions in parallel agents.

**Therminal feature unlocked:** The agent registry (`AgentRegistry` in `therminal-daemon`) would gain per-subagent IDs for correlation with transcript paths and lifecycle events. The live agent overlay (pane header subagent count badge) could show IDs, and `terminal.panes.list` could expose `agent_ids[]`.

**Inference-unrecoverable:** Yes. The PTY stream does not carry per-subagent UUIDs.

**tn-97w8 change:** In `subagent-start` and `subagent-stop`, parse `.agent_id` from stdin and include it in the written state JSON. Also record `agent_transcript_path` from SubagentStop (already in the schema per the hooks reference).

### REC-3: Wire `StopFailure` event — **inference-recoverable but noisy without it**

**What to add:** Wire `StopFailure` in `~/.claude/settings.json` and add a `stop-failure` branch to state-tracker.sh that reads `.error_type` and sets `status:"error"` with the error type in details.

**Therminal feature unlocked:** Pane badge can distinguish "Claude stopped because it finished" from "Claude stopped because it hit a rate limit / billing error." Currently therminal infers this by watching for error text in PTY output — fragile and locale-dependent.

**Inference-unrecoverable:** Partially recoverable from PTY text, but `error_type` is a clean enum vs. regex on noisy output. High value for the toast system (tn-97w8 / pane-level error toasts).

**tn-97w8 change:** New `stop-failure` arg; set `status:"error"`, write `error_type` to details. Wire in settings.json.

### REC-4: Wire `SessionEnd` event with `session_end_reason` — **inference-unrecoverable**

**What to add:** Wire `SessionEnd` in settings.json (it was broken before ~2.1.78, now fixed). Read `.session_end_reason` (`clear|resume|logout|prompt_input_exit|bypass_permissions_disabled|other`).

**Therminal feature unlocked:** The daemon can mark a pane's Claude session as definitively ended rather than waiting for the process tree to go dark. `resume` vs. `clear` vs. `logout` gives the JSONL tailer better lifecycle anchors. Avoids 5-second stale-session window currently seen on session restart.

**Inference-unrecoverable:** Yes. The PTY stream does not emit a structured session-end signal.

**tn-97w8 change:** New `session-end` arg; write `session_end_reason` to state file; optionally remove the state file on `clear`/`logout`.

### REC-5: Emit `additionalContext` on `UserPromptSubmit` to inject pane metadata — **new capability**

**What to add:** The `UserPromptSubmit` hookSpecificOutput supports `additionalContext: string` (system context injected before the turn). The first-party hook (tn-97w8) could inject therminal pane context: pane ID, workspace name, session tags (tn-bbvf), current cwd.

**Therminal feature unlocked:** Claude Code running inside therminal automatically has its pane ID and workspace in context — enabling Claude to drive therminal MCP tools (`terminal.panes.*`) without requiring the user to paste pane IDs.

**Inference-unrecoverable:** N/A — this is a new outbound capability, not a data read.

**tn-97w8 change:** In `user-prompt` case, read `THERMINAL_PANE_ID` env var (set by therminal on PTY spawn) and output:
```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "Therminal pane: <pane_id>, workspace: <workspace>",
    "sessionTitle": "<workspace> / <short cwd>"
  }
}
```

---

## 4. Lower-priority items (not recommended for tn-97w8 v1)

| Item | Reason deferred |
|---|---|
| `InstructionsLoaded` | Audit/observability only. No therminal feature blocked on it. |
| `PostCompact` | Could update context_percent immediately after compaction; statusline already does this via polling. |
| `CwdChanged` | Therminal gets cwd from OSC 7 which is more reliable (fires on every prompt). |
| `PermissionDenied` + `retry` | Useful for policy enforcement tools, not for therminal's observability role. |
| `TaskCreated`/`TaskCompleted` | Relevant if therminal exposes a task panel; no such feature planned yet. |
| `Elicitation`/`ElicitationResult` | MCP elicitation UI is therminal's domain long-term, but not in scope for tn-97w8. |
| `if` conditional hook field | Optimization for state-tracker only after it grows; premature now. |
| HTTP hooks | Not useful for a local state-file writer. |

---

## 5. Version attribution confidence

| Claim | Confidence | Source |
|---|---|---|
| `hookSpecificOutput.sessionTitle` in v2.1.84 | High | Changelog + GitHub releases both cite v2.1.84 Mar 26 |
| `agent_id` added in v2.1.83 | High | Changelog entry explicitly: "Added `agent_id`…to hook events" |
| `StopFailure` added in v2.1.78 | High | Changelog Mar 17 entry |
| `SessionEnd` timeout fix in v2.1.78 | High | "Fixed `SessionEnd` hooks being killed after 1.5s" |
| `PermissionDenied` added in v2.1.89 | High | Two changelog entries agree (v2.1.89 Apr 1) |
| `PostCompact` added in v2.1.76 | Moderate | Changelog cites v2.1.76 but release page shows minimal detail |
| `TeammateIdle`/`TaskCompleted` fix in v2.1.69 | Moderate | Pre-window; exact "added" version not in changelog (may be older) |
| Token/cost fields not in hooks | High | Hooks reference schema has no usage fields; changelog confirms `/cost` is UI-only |
