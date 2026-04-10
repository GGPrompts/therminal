#!/usr/bin/env bash
# glossary-lookup.sh — example hook for the `glossary` pattern pack.
#
# WHAT IT DOES
#   Subscribes to `hotspot.explain` events emitted when the user clicks a
#   glossary term in a therminal pane. For each event it reads the term name
#   from the event body, looks it up in the terms file, and prints a card
#   widget marker to stdout so a host process (or the therminal event bus) can
#   render an inline definition card.
#
# USAGE
#   # Run in a spare pane (the hook stays alive until Ctrl-C):
#   bash plugins/examples/glossary-lookup.sh
#
#   # Point at a custom terms file:
#   GLOSSARY_TERMS=/path/to/my.terms.toml bash plugins/examples/glossary-lookup.sh
#
# DEPENDENCIES
#   - therminal CLI on $PATH  (for `therminal events --follow`)
#   - jq                      (JSON parsing)
#   - bash 4+
#
# HOW THE CARD IS EMITTED
#   The script writes OSC 9 ; therminal:widget:card ; <json> BEL to stdout.
#   When therminal is the parent process, it intercepts that sequence and
#   renders the card widget in the originating pane. Running the hook outside
#   therminal prints the raw escape sequence, which is harmless.
#
# END-TO-END FLOW
#   1. User opens a pane and runs something that prints "mutex" or "BFS".
#   2. glossary.toml matches the term and emits:
#        { kind: "hotspot.explain", body: { term: "mutex", ctx: "..." } }
#      OR the user clicks the hotspot (also emits hotspot.explain).
#   3. This hook receives the event via `therminal events --follow`.
#   4. It looks up "mutex" in glossary.terms.toml.
#   5. It emits a card widget marker back via stdout.

set -euo pipefail

# ── Configuration ──────────────────────────────────────────────────────────────

# Path to the terms file. Override with GLOSSARY_TERMS env var.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GLOSSARY_TERMS="${GLOSSARY_TERMS:-"${SCRIPT_DIR}/data/glossary.terms.toml"}"

# ── Dependency checks ──────────────────────────────────────────────────────────

if ! command -v therminal &>/dev/null; then
    echo "ERROR: 'therminal' not found on PATH. Add the therminal bin dir to PATH." >&2
    exit 1
fi

if ! command -v jq &>/dev/null; then
    echo "ERROR: 'jq' not found on PATH. Install it with your package manager." >&2
    exit 1
fi

if [[ ! -f "$GLOSSARY_TERMS" ]]; then
    echo "ERROR: terms file not found: $GLOSSARY_TERMS" >&2
    echo "       Set GLOSSARY_TERMS=/path/to/glossary.terms.toml to override." >&2
    exit 1
fi

# ── Term lookup ────────────────────────────────────────────────────────────────

# look_up TERM
#   Searches $GLOSSARY_TERMS for a [[term]] block whose `word` or `aliases`
#   entry matches TERM (case-insensitive). Prints the definition, or an empty
#   string if not found.
#
# The terms file is plain TOML. We parse it with simple awk — no TOML library
# required. Format assumed: `word = "..."` and `definition = "..."` lines
# inside `[[term]]` blocks, with values possibly spanning continuation lines
# but typically on one line.
look_up() {
    local query="${1,,}"   # lowercase query
    awk -v q="$query" '
        /^\[\[term\]\]/ {
            # Save previous block if it matched
            if (matched && def != "") { print def; exit }
            matched = 0; word = ""; def = ""; in_def = 0
        }
        /^word[[:space:]]*=/ {
            gsub(/^word[[:space:]]*=[[:space:]]*"/, "")
            gsub(/"[[:space:]]*$/, "")
            w = $0
            if (tolower(w) == q) matched = 1
        }
        /^aliases[[:space:]]*=/ {
            # aliases = ["Mutex", "mutual exclusion lock"]
            line = $0
            n = split(line, parts, /"/)
            for (i = 2; i <= n; i += 2) {
                if (tolower(parts[i]) == q) matched = 1
            }
        }
        /^definition[[:space:]]*=/ {
            gsub(/^definition[[:space:]]*=[[:space:]]*"/, "")
            gsub(/"[[:space:]]*$/, "")
            def = $0
            in_def = 0
        }
        END {
            if (matched && def != "") print def
        }
    ' "$GLOSSARY_TERMS"
}

# ── Card widget emitter ────────────────────────────────────────────────────────

# emit_card TERM DEFINITION CONTEXT
#   Writes an OSC 9 widget marker that therminal intercepts and renders as
#   an overlay card widget near the hotspot that was clicked.
emit_card() {
    local term="$1"
    local definition="$2"
    local ctx="$3"

    # Build a minimal JSON payload. jq handles quoting and escaping.
    local payload
    payload=$(jq -n \
        --arg kind  "card" \
        --arg title "$(tr '[:lower:]' '[:upper:]' <<< "${term:0:1}")${term:1}" \
        --arg body  "$definition" \
        --arg ctx   "$ctx" \
        '{ kind: $kind, title: $title, body: $body, context: $ctx }')

    # OSC 9 ; therminal:widget ; <json> BEL
    # shellcheck disable=SC2059
    printf '\033]9;therminal:widget;%s\007' "$payload"
    echo  # newline so stdout consumers see a clean line
}

# ── Main event loop ────────────────────────────────────────────────────────────

echo "glossary-lookup: watching for hotspot.explain events (Ctrl-C to stop)" >&2
echo "  terms file: $GLOSSARY_TERMS" >&2

therminal events --follow --json --kinds "hotspot.explain" | \
while IFS= read -r line; do
    # Each line is a JSON event envelope:
    # { "kind": "hotspot.explain", "body": { "term": "mutex", "ctx": "..." }, ... }

    term=$(echo "$line" | jq -r '.body.term // .captures.term // empty' 2>/dev/null)
    ctx=$(echo  "$line" | jq -r '.body.ctx  // .captures.ctx  // ""'    2>/dev/null)

    if [[ -z "$term" ]]; then
        continue
    fi

    definition=$(look_up "$term")

    if [[ -z "$definition" ]]; then
        echo "glossary-lookup: no definition for '$term'" >&2
        continue
    fi

    echo "glossary-lookup: matched '$term'" >&2
    emit_card "$term" "$definition" "$ctx"
done
