#!/usr/bin/env bash
# Install Nerd Font Mono families used by therminal's font selector.
# Works on Linux and macOS. Requires curl and unzip.
#
# Usage:
#   ./install-nerdfonts.sh              # Regular weights only
#   ./install-nerdfonts.sh --all        # All weights (Bold, Italic, etc.)

set -euo pipefail

FONTS=(
    JetBrainsMono
    FiraCode
    CascadiaCode
    Hack
    Inconsolata
    SourceCodePro
    UbuntuMono
    Iosevka
    RobotoMono
    Meslo
)

ALL_WEIGHTS=false
[[ "${1:-}" == "--all" ]] && ALL_WEIGHTS=true

# Font install directory
if [[ "$(uname)" == "Darwin" ]]; then
    FONT_DIR="$HOME/Library/Fonts"
else
    FONT_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/fonts"
fi
mkdir -p "$FONT_DIR"

for font in "${FONTS[@]}"; do
    echo "Installing $font Nerd Font..."
    url="https://github.com/ryanoasis/nerd-fonts/releases/latest/download/${font}.zip"
    tmp=$(mktemp -d)
    trap "rm -rf '$tmp'" EXIT

    if ! curl -fsSL "$url" -o "$tmp/$font.zip"; then
        echo "  -> FAILED to download $font"
        continue
    fi
    unzip -qo "$tmp/$font.zip" -d "$tmp/extracted"

    count=0
    while IFS= read -r -d '' ttf; do
        name=$(basename "$ttf")
        # Only Mono variants (not Propo)
        [[ "$name" =~ Mono ]] || continue
        [[ "$name" =~ Propo ]] && continue
        # Regular only unless --all
        if [[ "$ALL_WEIGHTS" == false ]] && ! [[ "$name" =~ Regular ]]; then
            continue
        fi
        cp "$ttf" "$FONT_DIR/"
        ((count++))
    done < <(find "$tmp/extracted" -name "*.ttf" -print0)

    echo "  -> $count TTFs installed"
    rm -rf "$tmp"
    trap - EXIT
done

# Rebuild font cache on Linux
if command -v fc-cache &>/dev/null; then
    fc-cache -f "$FONT_DIR"
fi

echo ""
echo "Done! Restart therminal to pick up new fonts."
