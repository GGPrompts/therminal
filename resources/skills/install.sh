#!/usr/bin/env bash
# Install therminal skills into ~/.claude/skills/
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="${HOME}/.claude/skills"

install_skill() {
    local skill_name="$1"
    local src="${SCRIPT_DIR}/${skill_name}"

    if [[ ! -d "$src" ]]; then
        echo "ERROR: skill directory not found: $src" >&2
        return 1
    fi

    local dst="${DEST}/${skill_name}"
    mkdir -p "$dst"
    cp -r "${src}/." "$dst/"
    echo "Installed ${skill_name} -> ${dst}"
}

mkdir -p "$DEST"
install_skill "therminal-plugin"
install_skill "gg-delegate"

echo "Done. Skills installed in ${DEST}"
