#!/usr/bin/env bash
#
# poll_swarm.sh — cache-friendly worker-pane polling loop (tn-k13n).
#
# Usage:
#   examples/cli/poll_swarm.sh                  # one tick, all panes
#   examples/cli/poll_swarm.sh --watch          # tick every 2s forever
#   examples/cli/poll_swarm.sh --tag-filter t=v # only panes with the tag
#
# Demonstrates the end-to-end pattern: enumerate panes via the CLI, peek the
# tail of each pane, and report status. The default output for a 5-pane
# swarm is a few hundred bytes per tick — small enough that an MCP client
# polling this loop won't crush its prompt cache.
#
# The script intentionally calls `therminal pane list` once per tick (not
# per pane) and `therminal pane peek <id> --last 1` per pane. That's the
# minimum syscall set required to answer "what's each worker doing right
# now?" without paying for the full grid.

set -euo pipefail

THERMINAL=${THERMINAL:-therminal}
WATCH=0
INTERVAL=${INTERVAL:-2}
TAG_FILTER=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --watch) WATCH=1 ;;
    --tag-filter) TAG_FILTER="$2"; shift ;;
    --interval) INTERVAL="$2"; shift ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *) echo "unknown option: $1" >&2; exit 64 ;;
  esac
  shift
done

# One tick: list panes, peek the last line of each, print a status row.
tick() {
  # Format produced by `therminal pane list`:
  #   pane_id<TAB>session_id<TAB>colsxrows<TAB>cwd<TAB>last_exit<TAB>agent<TAB>tags
  while IFS=$'\t' read -r pane_id session_id dims cwd last_exit agent tags; do
    if [[ -n "$TAG_FILTER" && "$tags" != *"$TAG_FILTER"* ]]; then
      continue
    fi
    last_line=$("$THERMINAL" pane peek "$pane_id" --last 1 2>/dev/null | tail -n1)
    printf 'pane=%s ses=%s agent=%-7s exit=%-3s tags=%-20s | %s\n' \
      "$pane_id" "$session_id" "${agent:--}" "${last_exit:--}" "${tags:--}" "$last_line"
  done < <("$THERMINAL" pane list)
}

if [[ $WATCH -eq 1 ]]; then
  while true; do
    clear
    date '+--- %F %T ---'
    tick
    sleep "$INTERVAL"
  done
else
  tick
fi
