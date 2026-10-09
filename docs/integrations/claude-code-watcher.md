# Claude Code transcript watcher

Therminal can open read-only transcript panes alongside Claude Code as subagents
run. These native terminal-grid panes show assistant prose in full, with compact
tool summaries and expandable details. Claude's internal bookkeeping records are
hidden from this view.

## Enable automatic panes

In your existing `therminal.toml` `[general]` section, enable auto-tiling:

```toml
[general]
auto_tile = true
swarm_watch_scope = "all"
```

`all` discovers active subagent transcripts across Claude sessions. Set
`swarm_watch_scope = "current"` to filter file discovery to parent sessions owned
by this Therminal instance. If session identity is still unavailable after a
five-second startup grace period, discovery falls back to `all`.

The file scanner checks
`~/.claude/projects/*/*/subagents/agent-*.jsonl` every 500 ms; hook events can also
trigger panes. Windows can discover transcripts through the default WSL distro's
home directory. Debouncing means panes may take a few seconds to appear.

Panes are reclaimed when a stop event arrives or the scanner sees no transcript
updates for 30 seconds. That timeout is an inactivity heuristic, so a quiet agent
can lose its watcher pane before it finishes. Completed reports are not retained
in a permanent agent list. A hook event without a transcript path opens a regular
terminal pane instead.

## Read and navigate

Assistant messages support **bold**, *italics*, headings, lists, task lists,
blockquotes, strikethrough, inline code, fenced code, and compact HTTP(S) links.
Code preserves literal Markdown characters. This is a terminal Markdown renderer;
it does not render images or rich tables, and HTML remains text.

Tool calls and results initially occupy a single summary row; errors include a
short preview. Long user messages are collapsed, while assistant prose stays
visible. Expanded tool details are capped at 50 logical lines.

Hover over the watcher to scroll with the mouse wheel. Focus it for keyboard
controls:

| Control | Action |
| --- | --- |
| Mouse wheel | Scroll the transcript under the pointer |
| Up / `k`, Down / `j` | Scroll one row |
| Page Up / Page Down | Scroll one page |
| Home / `g` | Jump to the beginning of retained content |
| End / `G` | Jump to the bottom and follow new messages |
| `f` | Toggle live-follow; enabling it jumps to the bottom |
| Enter / Space | Toggle the first expandable entry visible in the viewport |
| `e` | Expand or collapse all expandable details |
| `t` / `T` | Jump to the next / previous tool event |

The default Shift+Page Up/Down and Shift+Home/End scroll shortcuts also operate
on the transcript. Scrolling upward pauses live-follow. New messages preserve
your reading position while paused; scrolling back to the bottom resumes follow.
History is bounded, so sufficiently old events can eventually be discarded.

The header shows the event count, `live` or `paused`, and the visible row range.
The control hints stay pinned at the bottom and shorten in narrow panes; the
keyboard controls remain available even when their hints do not fit.

## Related interfaces

This guide describes the native `JsonlTail` viewer. The
[`claude-events` CLI subscriber](../../README.md#claude-events--live-claude-code-session-viewer)
and `therminal://claude/events` MCP resource expose a separate event stream.
The native viewer's presentation filtering does not change that stream.

The MCP tool `terminal.panes.create_tail` currently creates a regular PTY running
`tail -F`; it does not open this native Markdown viewer. WebView panes can load
websites, but a WebView-based subagent chat viewer is not implemented.
