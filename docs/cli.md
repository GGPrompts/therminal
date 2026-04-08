# `therminal` CLI

A cache-friendly command-line surface for therminal (tn-k13n). Wraps the
existing `therminal-daemon-client` so any MCP client (Claude Code, Codex,
shell scripts) can drive the same daemon the GUI talks to without paying
MCP framing costs.

## Why a CLI alongside MCP

- **MCP** is the right tool when the structured shape materially matters:
  subscriptions, resource URIs, schema introspection, semantic queries fed
  back into the tool-use loop as typed records.
- **CLI** is the right tool for everything else: writes, commands,
  fire-and-forget, tiny peeks, anything called N times in a row.

Same daemon-side work, dramatically different context impact for Claude
Code consumers. A `pane list` for 5 panes is ~50–150 bytes of TSV instead of
a multi-kB JSON tool result.

## Output discipline

- **Default output is terse.** Tab-separated, one record per line, no
  framing, no headers, no ANSI color, no timestamps.
- **`--json`** on most subcommands produces a single-line JSON document for
  scripts that want named fields.
- **Errors** go to stderr. Exit code is non-zero on any failure. Stdout
  stays clean so callers can pipe `pane peek` etc. into other tools.

The CLI auto-spawns `therminal-daemon` via the same chain the GUI uses
(`[daemon] binary_path`, sibling exe, `$PATH`) when no daemon is running.

## Subcommand reference

### `pane`

```text
therminal pane list [--session N] [--json]
therminal pane create [--from N] [--split horizontal|vertical] [--session N] [--json]
therminal pane destroy <pane_id>
therminal pane send <pane_id> <keys> [--raw]
therminal pane peek <pane_id> [--last N] [--trim] [--json]
therminal pane tag <pane_id> <key=value>...
therminal pane untag <pane_id> <key>... [--all]
therminal pane swap <a> <b>
therminal pane resize <pane_id> <cols>x<rows>
```

`pane list` TSV columns:

```
pane_id  session_id  cols×rows  cwd  last_exit_code  agent_name  tags
```

Empty fields are emitted as empty cells. Tags are encoded as
`key=value,key=value` sorted by key.

`pane send` interprets `\n`, `\r`, `\t`, `\\` by default. `--raw` disables
escape interpretation and forwards bytes verbatim.

`pane peek --last N` drops fully-empty trailing rows before keeping the tail
so `--last 5` returns the last five *content* rows, not five blank padding
rows. `--trim` (on by default) strips trailing whitespace per row.

`pane create` without `--from` walks the daemon for an existing pane to
split. If no panes exist anywhere, it spawns a fresh session first.

### `session`

```text
therminal session list [--json]
therminal session create [--name <name>] [--json]
therminal session destroy <session_id>
```

### `workspace`

```text
therminal workspace list [--session N] [--json]
therminal workspace switch --session N <workspace_id>
```

`workspace list` TSV columns:

```
session_id  workspace_id  name  pane_count  active(0|1)
```

`workspace switch` is currently a stub: workspace switching is GUI-driven
and the daemon does not yet expose a `SwitchWorkspace` IPC primitive. The
CLI prints a clear error so scripts notice the gap.

### `agents`

```text
therminal agents list [--pane N] [--json]
```

`agents list` TSV columns:

```
pane_id  agent_type  status  name  current_tool  pid
```

### `events`

```text
therminal events [--follow] [--kinds K1,K2,...] [--panes P1,P2,...] [--limit N]
```

Streams `DaemonEvent`s as one JSON document per line (JSON Lines). Even
though the rest of the CLI prefers TSV, an event stream is naturally
heterogeneous, so JSON Lines is the right shape: each event is still
~100 bytes and trivially `jq`-friendly.

Valid `--kinds`: `state_changed`, `session_created`, `session_destroyed`,
`pane_output`, `workspace_changed`, `pane_exited`. Empty = all kinds.

`--limit N` exits after printing `N` events (handy for tests).

### `semantic`

```text
therminal semantic commands <pane_id> [--limit N]
therminal semantic hotspots <pane_id> [--kind file|url|git_ref|issue] [--json]
```

`semantic hotspots` runs the same regex pass the GUI uses (file paths,
URLs, error locations, git refs, issue refs) over a `CapturePane` snapshot.

`semantic commands` is currently a stub — daemon does not expose
`query_commands` over its lightweight IPC channel. Use the
`terminal.semantic.query_commands` MCP tool until a CLI primitive lands
(tracked separately).

## Cache-friendliness notes

The default `pane list` output for 5 panes is well under 300 bytes. A
minimal `bd close tn-xxx`-style call against the CLI returns ~10–50 bytes
on success, comparable to a regular `Bash` tool call from Claude Code. By
contrast, an MCP `terminal.panes.list` invocation incurs JSON-RPC envelope
+ tool-result framing + a separate cache key segment per call.

Use the CLI for:

- Polling N panes in a loop (`for id in $(therminal pane list | cut -f1) ; do ... done`).
- Tagging panes with conductor-side metadata
  (`therminal pane tag 7 issue=tn-k13n worker=alice`).
- Sending input commands to specific panes
  (`therminal pane send 7 'cargo test\n'`).
- Streaming events into a `jq`-driven dashboard
  (`therminal events --follow | jq -c '.'`).

Use MCP for:

- Subscriptions where you need typed events fed back into Claude's tool-use
  loop with stable URIs (`terminal://pane/N/output`,
  `therminal://claude/events`).
- Semantic / OSC 633 queries where the structured shape matters
  (`terminal.semantic.query_commands`).
- Anything that benefits from MCP resource templates and discovery.

## Examples

```sh
# Poll a swarm of worker panes and report per-pane status.
for id in $(therminal pane list | cut -f1); do
  printf '%s\t' "$id"
  therminal pane peek "$id" --last 1
done

# Tag every Claude pane with its issue id, then list.
therminal agents list | awk '$2 == "claude" { print $1 }' | while read p; do
  therminal pane tag "$p" issue="$BD_ISSUE"
done
therminal pane list

# Stream session lifecycle events into jq.
therminal events --kinds session_created,session_destroyed --follow | jq -c '.'
```

See also `examples/cli/poll_swarm.sh` for an end-to-end shell loop.
