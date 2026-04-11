# Claude Code Session Titles in Workspace Tabs

**Issues:** tn-lxq9, tn-5fgz (cache) · **Upstream:** [Claude Code hooks reference](https://code.claude.com/docs/en/hooks)

Therminal's workspace tabs normally show `"<id>: <cwd_basename>"`. When a
workspace's focused pane hosts a Claude Code session and that session has
a known **session title**, therminal uses the title instead:

| State                                        | Tab label          |
|---------------------------------------------|--------------------|
| Claude session with title `"fix login bug"` | `1: fix login bug` |
| Claude session, no title, cwd `therminal`   | `1: therminal`     |
| No pane info                                 | `1`                |

The feature is always-on — there is no config flag. Titles are truncated
to 24 characters with an ellipsis (`…`), preserving the `"<id>: "` prefix.
Truncation counts Unicode characters, not bytes, so emoji and CJK titles
stay intact up to the budget.

## Where the title comes from

Claude Code's [`UserPromptSubmit` hook](https://code.claude.com/docs/en/hooks)
can emit a structured output field `hookSpecificOutput.sessionTitle`. This
is a first-class signal that the model or a hook script sets per turn —
it's **inference-unrecoverable**: therminal's PTY byte-stream parser has
no way to derive a human-readable title from the rendered text alone.

The title propagates to therminal via the state-tracker path documented
in [`claude-code-hooks-audit.md`](claude-code-hooks-audit.md):

1. Your hook writes `session_title` into
   `/tmp/claude-code-state/<session-id>.json`.
2. `ClaudeStatePoller` (in `therminal-harness-claude`) picks it up on the
   next tick and publishes it through `ClaudeSessionState.session_title`.
3. The app-side `ClaudeCwdTracker` caches the title per process ID.
4. The render driver builds a per-workspace title map and passes it to
   `build_tab_labels()` in `therminal-app`.

The same signal already powers the pane header and the status bar — tab
labels are the third surface to consume it.

## Enabling it in your Claude Code config

This is a normal user-owned hook — therminal does not ship a hook script.
Add a `UserPromptSubmit` hook to `~/.claude/settings.json` (or your
`plugins/*/hooks.json` of choice) that emits a title based on whatever
signal makes sense for your workflow. The minimal pattern:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "/path/to/your/title-hook.sh"
          }
        ]
      }
    ]
  }
}
```

Your `title-hook.sh` (or whatever script you point at) reads the hook
JSON on stdin and writes a response on stdout. To set a session title,
emit:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "sessionTitle": "<your derived title here>"
  }
}
```

For example, the title could be the first line of the prompt, the current
git branch, the issue ID your agent is working on, or anything else
useful for identifying the session at a glance.

If you're already using the community `state-tracker.sh` from
`conductor-mcp`, you'll need to also teach it to write `session_title`
into the state JSON it maintains at `/tmp/claude-code-state/<sid>.json` —
see the recommendation in
[`claude-code-hooks-audit.md`](claude-code-hooks-audit.md#rec-1-read-hookspecificoutputsessiontitle-on-userpromptsubmit--inference-unrecoverable).

## Troubleshooting

- **Tab still shows the cwd basename:** the harness hasn't seen a title
  for this pane's session yet. Confirm
  `/tmp/claude-code-state/<sid>.json` has a populated `session_title`
  field; if not, the hook isn't writing it. The poller ticks every
  ~500 ms, so changes appear within a second of the file being written.
- **Title is cut off mid-word:** that's the 24-character cap. Shorter
  titles are strictly better — prefer 8–18 characters.
- **No title but a subagent is running:** subagent lineage lives on the
  `therminal://claude/events` stream, not the per-pane tab label. The
  tab always reflects the top-level Claude session in the focused pane.

## Related

- [`claude-code-hooks-audit.md`](claude-code-hooks-audit.md) — full hook
  survey, REC-1 covers `sessionTitle` in detail.
- `crates/therminal-harness-claude/CLAUDE.md` — JSONL tailer and state
  poller internals.
- `crates/therminal-app/src/claude_cwd.rs` — app-side
  `ClaudeCwdTracker`, `ClaudeChromeMeta`, and the header composition
  rules shared between tab labels, pane headers, and the status bar.
