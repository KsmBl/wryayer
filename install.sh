#!/usr/bin/env bash
set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BOLD='\033[1m'
NC='\033[0m'

info()    { echo -e "${GREEN}✓${NC} $*"; }
warn()    { echo -e "${YELLOW}!${NC} $*"; }
error()   { echo -e "${RED}✗ $*${NC}" >&2; exit 1; }
heading() { echo -e "\n${BOLD}$*${NC}"; }

# ── Preflight ─────────────────────────────────────────────────────────────────

if [ ! -f "Cargo.toml" ] || ! grep -q 'name = "wryayer"' Cargo.toml; then
    error "Run install.sh from the wryayer project directory."
fi

heading "Checking dependencies..."

check_dep() {
    if command -v "$1" &>/dev/null; then
        info "$1 found ($(command -v "$1"))"
    else
        error "$1 not found — please install it first."
    fi
}

CARGO="${HOME}/.cargo/bin/cargo"
[ -x "$CARGO" ] || CARGO="$(command -v cargo 2>/dev/null)" || error "cargo not found. Install rustup: https://rustup.rs"
info "cargo found ($CARGO)"

check_dep git
check_dep makepkg
check_dep pacman

# ── Build ─────────────────────────────────────────────────────────────────────

heading "Building wryayer..."
"$CARGO" build --release 2>&1
info "Build complete."

# ── Install binary ────────────────────────────────────────────────────────────

heading "Installing binary..."
BIN_DIR="${HOME}/bin"
mkdir -p "$BIN_DIR"
cp target/release/wryayer "$BIN_DIR/wryayer"
chmod +x "$BIN_DIR/wryayer"
info "Installed binary to $BIN_DIR/wryayer"

# ── Fish completions ──────────────────────────────────────────────────────────

if command -v fish &>/dev/null; then
    heading "Installing fish completions..."
    FISH_COMP_DIR="${HOME}/.config/fish/completions"
    mkdir -p "$FISH_COMP_DIR"
    cp completions/wryayer.fish "$FISH_COMP_DIR/wryayer.fish"
    info "Installed completions to $FISH_COMP_DIR/wryayer.fish"

    # Add ~/bin to fish_user_paths if it isn't already there
    if ! fish -c 'contains -- "$HOME/bin" $fish_user_paths' 2>/dev/null \
       && ! fish -c "contains -- $BIN_DIR \$fish_user_paths" 2>/dev/null; then
        fish -c "fish_add_path '$BIN_DIR'"
        info "Added $BIN_DIR to fish_user_paths"
    else
        info "$BIN_DIR already in fish_user_paths"
    fi
else
    warn "fish not found — skipping completions. Copy completions/wryayer.fish manually if needed."
fi

# ── Done ──────────────────────────────────────────────────────────────────────

echo
echo -e "${GREEN}${BOLD}wryayer installed successfully!${NC}"
echo
echo "  Start a new fish session (or run 'source ~/.config/fish/config.fish')"
echo "  then try:  wryayer --help"
echo "             wryayer install jq"
