#!/usr/bin/env bash
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

info()  { echo -e "${GREEN}✓${NC} $*"; }
warn()  { echo -e "${YELLOW}!${NC} $*"; }
skip()  { echo -e "  (skipped)"; }

ask() {
    local prompt="$1"
    local reply
    read -r -p "$(echo -e "${YELLOW}?${NC} ${prompt} [y/N] ")" reply
    [[ "$reply" =~ ^[Yy]$ ]]
}

echo -e "${BOLD}Uninstalling wryayer...${NC}"
echo

# ── Binary ────────────────────────────────────────────────────────────────────

BIN="${HOME}/bin/wryayer"
if [ -f "$BIN" ]; then
    rm -f "$BIN"
    info "Removed $BIN"
else
    warn "$BIN not found, nothing to remove."
fi

# ── Fish completions ──────────────────────────────────────────────────────────

FISH_COMP="${HOME}/.config/fish/completions/wryayer.fish"
if [ -f "$FISH_COMP" ]; then
    rm -f "$FISH_COMP"
    info "Removed $FISH_COMP"
else
    warn "$FISH_COMP not found, nothing to remove."
fi

# ── App data (~/.wryayer/) ─────────────────────────────────────────────────────

WRYAYER_DIR="${HOME}/.wryayer"
if [ -d "$WRYAYER_DIR" ]; then
    APP_COUNT=$(find "$WRYAYER_DIR" -maxdepth 1 -mindepth 1 -type d | wc -l)
    echo
    if ask "Remove $WRYAYER_DIR/ ($APP_COUNT installed app(s)) — this cannot be undone?"; then
        rm -rf "$WRYAYER_DIR"
        info "Removed $WRYAYER_DIR"
    else
        skip
        warn "Installed apps kept at $WRYAYER_DIR"
    fi
fi

# ── Build cache (~/.cache/wryayer/) ───────────────────────────────────────────

CACHE_DIR="${HOME}/.cache/wryayer"
if [ -d "$CACHE_DIR" ]; then
    CACHE_SIZE=$(du -sh "$CACHE_DIR" 2>/dev/null | cut -f1)
    echo
    if ask "Remove $CACHE_DIR/ (build cache, $CACHE_SIZE)?"; then
        rm -rf "$CACHE_DIR"
        info "Removed $CACHE_DIR"
    else
        skip
    fi
fi

echo
echo -e "${GREEN}${BOLD}Done.${NC}"
