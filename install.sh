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

# ── Distro detection ──────────────────────────────────────────────────────────

detect_distro() {
    if [ -f /etc/os-release ]; then
        # shellcheck source=/dev/null
        . /etc/os-release
        case "${ID_LIKE:-} ${ID:-}" in
            *debian*|*ubuntu*)               echo "debian"; return ;;
            *arch*|*manjaro*)                echo "arch";   return ;;
            *fedora*|*rhel*|*centos*|*suse*) echo "fedora"; return ;;
        esac
        case "${ID:-}" in
            fedora|rhel|centos|almalinux|rocky|opensuse*) echo "fedora"; return ;;
        esac
    fi
    command -v pacman  &>/dev/null && echo "arch"   && return
    command -v apt-get &>/dev/null && echo "debian" && return
    command -v dnf     &>/dev/null && echo "fedora" && return
    command -v dnf5    &>/dev/null && echo "fedora" && return
    echo "unknown"
}

DISTRO="$(detect_distro)"

# ── Preflight ─────────────────────────────────────────────────────────────────

if [ ! -f "Cargo.toml" ] || ! grep -q 'name = "wryayer"' Cargo.toml; then
    error "Run install.sh from the wryayer project directory."
fi

if [ "$DISTRO" = "unknown" ]; then
    error "Unsupported distro — supported: Arch/CachyOS/Manjaro, Debian/Ubuntu, Fedora/RHEL/AlmaLinux/Rocky."
fi

info "Detected distro: $DISTRO"

# ── Choose which front-ends to build ──────────────────────────────────────────

heading "Which interfaces should be built?"
echo -e "  1) both  — terminal UI (TUI) + desktop GUI   ${BOLD}(default)${NC}"
echo "  2) tui   — terminal UI only"
echo "  3) gui   — desktop GUI only (GTK4 + libadwaita)"
echo "  4) none  — command-line only"

choice=""
if [ -e /dev/tty ]; then
    printf "  Select [1-4] (default 1): "
    read -r choice < /dev/tty || choice=""
    echo
fi

BUILD_TUI=1
BUILD_GUI=1
case "$(echo "${choice:-1}" | tr '[:upper:]' '[:lower:]')" in
    2|tui)          BUILD_TUI=1; BUILD_GUI=0 ;;
    3|gui)          BUILD_TUI=0; BUILD_GUI=1 ;;
    4|none|nothing) BUILD_TUI=0; BUILD_GUI=0 ;;
    *)              BUILD_TUI=1; BUILD_GUI=1 ;;
esac

FEATURES=""
[ "$BUILD_TUI" = 1 ] && FEATURES="$FEATURES tui"
[ "$BUILD_GUI" = 1 ] && FEATURES="$FEATURES gui"
FEATURES="$(echo "$FEATURES" | xargs)"

sel="none (CLI only)"
if [ "$BUILD_TUI" = 1 ] && [ "$BUILD_GUI" = 1 ]; then sel="TUI + GUI"
elif [ "$BUILD_TUI" = 1 ]; then sel="TUI"
elif [ "$BUILD_GUI" = 1 ]; then sel="GUI"; fi
info "Building: $sel"

# ── Install system dependencies ───────────────────────────────────────────────

heading "Installing system dependencies..."

if [ "$DISTRO" = "arch" ]; then
    PKGS=()
    command -v bwrap    &>/dev/null || PKGS+=(bubblewrap)
    command -v curl     &>/dev/null || PKGS+=(curl)
    command -v git      &>/dev/null || PKGS+=(git)
    command -v makepkg  &>/dev/null || PKGS+=(base-devel)
    command -v readelf  &>/dev/null || PKGS+=(binutils)
    command -v ldconfig &>/dev/null || PKGS+=(glibc)
    if [ "$BUILD_GUI" = 1 ]; then
        command -v pkg-config &>/dev/null || PKGS+=(pkgconf)
        pkg-config --exists gtk4 2>/dev/null || PKGS+=(gtk4)
    fi

    if [ ${#PKGS[@]} -gt 0 ]; then
        echo "  Installing: ${PKGS[*]}"
        sudo pacman -S --needed --noconfirm "${PKGS[@]}"
    else
        info "All Arch dependencies already installed"
    fi

elif [ "$DISTRO" = "debian" ]; then
    PKGS=()
    command -v bwrap    &>/dev/null || PKGS+=(bubblewrap)
    command -v curl     &>/dev/null || PKGS+=(curl)
    command -v git      &>/dev/null || PKGS+=(git)
    command -v readelf  &>/dev/null || PKGS+=(binutils)
    command -v ldconfig &>/dev/null || PKGS+=(libc-bin)
    # dpkg and apt are always present on Debian/Ubuntu
    if [ "$BUILD_GUI" = 1 ]; then
        command -v pkg-config &>/dev/null || PKGS+=(pkg-config)
        pkg-config --exists gtk4 2>/dev/null || PKGS+=(libgtk-4-dev)
    fi

    if [ ${#PKGS[@]} -gt 0 ]; then
        echo "  Installing: ${PKGS[*]}"
        sudo apt-get update -qq
        sudo apt-get install -y "${PKGS[@]}"
    else
        info "All Debian/Ubuntu dependencies already installed"
    fi

    # apt-file is optional but enables soname-to-package lookup for uninstalled packages.
    if ! command -v apt-file &>/dev/null; then
        warn "apt-file not found — soname lookup for packages not installed on the host will be limited."
        warn "Install it with: sudo apt-get install apt-file && sudo apt-file update"
    fi

elif [ "$DISTRO" = "fedora" ]; then
    PKGS=()
    command -v bwrap    &>/dev/null || PKGS+=(bubblewrap)
    command -v curl     &>/dev/null || PKGS+=(curl)
    command -v git      &>/dev/null || PKGS+=(git)
    command -v readelf  &>/dev/null || PKGS+=(binutils)
    command -v ldconfig &>/dev/null || PKGS+=(glibc)
    command -v rpm2cpio &>/dev/null || PKGS+=(rpm2cpio)
    command -v cpio     &>/dev/null || PKGS+=(cpio)
    # dnf download is built-in on dnf5; on dnf4 it needs dnf-plugins-core
    dnf help download &>/dev/null 2>&1 || PKGS+=(dnf-plugins-core)
    if [ "$BUILD_GUI" = 1 ]; then
        command -v pkg-config &>/dev/null || PKGS+=(pkgconf-pkg-config)
        pkg-config --exists gtk4 2>/dev/null || PKGS+=(gtk4-devel)
    fi

    if [ ${#PKGS[@]} -gt 0 ]; then
        echo "  Installing: ${PKGS[*]}"
        sudo dnf install -y "${PKGS[@]}"
    else
        info "All Fedora/RHEL dependencies already installed"
    fi
fi

# ── Rust toolchain ────────────────────────────────────────────────────────────

heading "Checking Rust toolchain..."

RUST_MIN="1.88"

# Parse "x.y.z" → integer xxyyzz for numeric comparison
rust_version_int() {
    "$1" --version 2>/dev/null \
        | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' | head -1 \
        | awk -F. '{ printf "%d%02d%02d", $1, $2, ($3+0) }'
}

MIN_INT="$(echo "$RUST_MIN" | awk -F. '{ printf "%d%02d%02d", $1, $2, ($3+0) }')"

install_rustup() {
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    # shellcheck source=/dev/null
    [ -f "${HOME}/.cargo/env" ] && source "${HOME}/.cargo/env"
}

CARGO=""

# Prefer rustup-managed cargo — it's always up to date
if [ -x "${HOME}/.cargo/bin/cargo" ]; then
    CARGO="${HOME}/.cargo/bin/cargo"
    VER_INT="$(rust_version_int "$CARGO")"
    if [ "$VER_INT" -lt "$MIN_INT" ]; then
        echo "  rustup cargo is $(${CARGO} --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1), need $RUST_MIN — updating..."
        "${HOME}/.cargo/bin/rustup" update stable
    fi
    info "cargo ready: $(${CARGO} --version)"
else
    # Fall back to system cargo, but check version
    SYS_CARGO="$(command -v cargo 2>/dev/null)" || SYS_CARGO=""
    if [ -n "$SYS_CARGO" ]; then
        VER_INT="$(rust_version_int "$SYS_CARGO")"
        if [ "$VER_INT" -ge "$MIN_INT" ]; then
            CARGO="$SYS_CARGO"
            info "cargo ready: $(${CARGO} --version)"
        else
            echo "  System cargo $(${SYS_CARGO} --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1) is too old (need $RUST_MIN) — installing rustup..."
            install_rustup
            CARGO="${HOME}/.cargo/bin/cargo"
            info "Rust installed via rustup: $(${CARGO} --version)"
        fi
    else
        echo "  cargo not found — installing rustup..."
        install_rustup
        CARGO="${HOME}/.cargo/bin/cargo"
        info "Rust installed via rustup: $(${CARGO} --version)"
    fi
fi

# ── Build ─────────────────────────────────────────────────────────────────────

heading "Building wryayer..."
if [ -n "$FEATURES" ]; then
    echo "  Features: $FEATURES"
    "$CARGO" build --release --no-default-features --features "$FEATURES"
else
    echo "  Features: none (command-line only)"
    "$CARGO" build --release --no-default-features
fi
info "Build complete."

# ── Install binary ────────────────────────────────────────────────────────────

heading "Installing binary..."
BIN_DIR="${HOME}/bin"
mkdir -p "$BIN_DIR"
# Atomic install: avoids "Text file busy" if the binary is currently running.
cp target/release/wryayer "${BIN_DIR}/wryayer.new"
mv "${BIN_DIR}/wryayer.new" "${BIN_DIR}/wryayer"
chmod +x "${BIN_DIR}/wryayer"
info "Installed binary to ${BIN_DIR}/wryayer"

# Add ~/bin to PATH for bash/zsh if not already there
for RC in "${HOME}/.bashrc" "${HOME}/.zshrc"; do
    if [ -f "$RC" ] && ! grep -q 'HOME/bin\|~/bin' "$RC"; then
        echo 'export PATH="$HOME/bin:$PATH"' >> "$RC"
        info "Added ~/bin to PATH in $RC"
    fi
done

# ── Shell completions ─────────────────────────────────────────────────────────

if command -v fish &>/dev/null; then
    heading "Installing fish completions..."
    FISH_COMP_DIR="${HOME}/.config/fish/completions"
    mkdir -p "$FISH_COMP_DIR"
    cp completions/wryayer.fish "$FISH_COMP_DIR/wryayer.fish"

    # Reflect the built front-ends: the committed file advertises 'tui'; drop it
    # if the TUI wasn't built, and add 'gui' when the desktop GUI was built.
    if [ "$BUILD_TUI" != 1 ]; then
        sed -i '/-a tui .*Launch the interactive TUI/d' "$FISH_COMP_DIR/wryayer.fish"
    fi
    if [ "$BUILD_GUI" = 1 ]; then
        cat >> "$FISH_COMP_DIR/wryayer.fish" <<'FISH_GUI'

# ── gui (added by install.sh because the desktop GUI was built) ────────────────
complete -c wryayer -n "not __fish_seen_subcommand_from $cmds" -a gui -d 'Launch the native GTK desktop GUI'
FISH_GUI
    fi
    info "Installed completions to $FISH_COMP_DIR/wryayer.fish"

    if ! fish -c "contains -- $BIN_DIR \$fish_user_paths" 2>/dev/null; then
        fish -c "fish_add_path '$BIN_DIR'"
        info "Added $BIN_DIR to fish_user_paths"
    else
        info "$BIN_DIR already in fish_user_paths"
    fi
else
    warn "fish not found — skipping fish completions."
fi

if command -v bash &>/dev/null && [ -f "${HOME}/.bashrc" ]; then
    "$BIN_DIR/wryayer" completions bash >> "${HOME}/.bashrc"
    info "Added bash completions to ~/.bashrc"
fi

if command -v zsh &>/dev/null && [ -f "${HOME}/.zshrc" ]; then
    "$BIN_DIR/wryayer" completions zsh >> "${HOME}/.zshrc"
    info "Added zsh completions to ~/.zshrc"
fi

# ── Done ──────────────────────────────────────────────────────────────────────

echo
echo -e "${GREEN}${BOLD}wryayer installed successfully!${NC}"
echo
if command -v fish &>/dev/null; then
    echo "  Start a new fish session (or run 'source ~/.config/fish/config.fish')"
else
    echo "  Start a new shell session (or run 'source ~/.bashrc' / 'source ~/.zshrc')"
fi
echo "  then try:  wryayer --help"
echo "             wryayer install jq"
[ "$BUILD_TUI" = 1 ] && echo "             wryayer tui"
[ "$BUILD_GUI" = 1 ] && echo "             wryayer gui"
