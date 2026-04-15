#!/usr/bin/env bash
# Clean therminal state on Linux/WSL/macOS.
# Usage:
#   ./scripts/clean.sh              # interactive menu
#   ./scripts/clean.sh sessions     # wipe sessions only
#   ./scripts/clean.sh config       # wipe config only
#   ./scripts/clean.sh runtime      # wipe sockets/locks/pids
#   ./scripts/clean.sh all          # full reset (kill daemon first)
set -euo pipefail

# ── Resolve paths the same way therminal-runtime does ──────────────────────
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/therminal"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/therminal"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/therminal"
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    RUNTIME_DIR="$XDG_RUNTIME_DIR/therminal"
else
    RUNTIME_DIR="/tmp/therminal-${USER:-unknown}"
fi

# ── Helpers ────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BOLD='\033[1m'
RESET='\033[0m'

info()  { echo -e "${GREEN}[clean]${RESET} $*"; }
warn()  { echo -e "${YELLOW}[clean]${RESET} $*"; }
error() { echo -e "${RED}[clean]${RESET} $*"; }

show_paths() {
    echo -e "${BOLD}Therminal data locations:${RESET}"
    echo "  Config:   $CONFIG_DIR"
    echo "  Data:     $DATA_DIR"
    echo "  Cache:    $CACHE_DIR"
    echo "  Runtime:  $RUNTIME_DIR"
    echo ""
}

kill_daemon() {
    if pgrep -f therminal-daemon > /dev/null 2>&1; then
        warn "Killing therminal-daemon..."
        pkill -f therminal-daemon 2>/dev/null || true
        sleep 0.5
        if pgrep -f therminal-daemon > /dev/null 2>&1; then
            error "Daemon still running, sending SIGKILL..."
            pkill -9 -f therminal-daemon 2>/dev/null || true
        fi
        info "Daemon stopped."
    else
        info "No daemon running."
    fi
}

clean_sessions() {
    if [ -f "$DATA_DIR/sessions.json" ]; then
        rm -f "$DATA_DIR/sessions.json" "$DATA_DIR/sessions.json.tmp"
        info "Removed sessions.json"
    else
        info "No sessions.json found."
    fi
}

clean_config() {
    if [ -f "$CONFIG_DIR/therminal.toml" ]; then
        rm -f "$CONFIG_DIR/therminal.toml"
        info "Removed therminal.toml (will use defaults on next launch)"
    else
        info "No therminal.toml found."
    fi
}

clean_runtime() {
    if [ -d "$RUNTIME_DIR" ]; then
        rm -rf "$RUNTIME_DIR"
        info "Removed runtime dir ($RUNTIME_DIR)"
    else
        info "No runtime dir found."
    fi
}

clean_cache() {
    if [ -d "$CACHE_DIR" ]; then
        rm -rf "$CACHE_DIR"
        info "Removed cache dir ($CACHE_DIR)"
    else
        info "No cache dir found."
    fi
}

clean_all() {
    kill_daemon
    clean_sessions
    clean_config
    clean_runtime
    clean_cache
    info "Full reset complete."
}

# ── Command dispatch ───────────────────────────────────────────────────────
if [ $# -ge 1 ]; then
    case "$1" in
        sessions) clean_sessions ;;
        config)   clean_config ;;
        runtime)  kill_daemon; clean_runtime ;;
        cache)    clean_cache ;;
        all)      clean_all ;;
        paths)    show_paths ;;
        *)
            error "Unknown target: $1"
            echo "Usage: $0 {sessions|config|runtime|cache|all|paths}"
            exit 1
            ;;
    esac
    exit 0
fi

# ── Interactive menu ───────────────────────────────────────────────────────
show_paths

echo -e "${BOLD}What would you like to clean?${RESET}"
echo "  1) sessions  - Remove saved sessions (fixes stale pane errors)"
echo "  2) config    - Remove therminal.toml (reset to defaults)"
echo "  3) runtime   - Kill daemon + remove sockets/locks/pids"
echo "  4) cache     - Remove cache directory"
echo "  5) all       - Full reset (all of the above)"
echo "  6) paths     - Just show paths, don't delete anything"
echo "  q) quit"
echo ""
read -rp "Choice [1-6/q]: " choice

case "$choice" in
    1) clean_sessions ;;
    2) clean_config ;;
    3) kill_daemon; clean_runtime ;;
    4) clean_cache ;;
    5) clean_all ;;
    6) show_paths ;;
    q|Q) echo "Cancelled."; exit 0 ;;
    *) error "Invalid choice."; exit 1 ;;
esac
