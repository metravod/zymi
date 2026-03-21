#!/usr/bin/env bash
set -euo pipefail

GITHUB_REPO="metravod/zymi"
INSTALL_DIR="/usr/local/bin"
UNINSTALL_MODE=false
SOURCE_BUILD=false

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
dim()   { printf '\033[2m%s\033[0m\n' "$*"; }

usage() {
    echo "Usage: install.sh [--uninstall]"
    echo ""
    echo "  --uninstall   Remove zymi (binary + systemd service if present)"
    echo ""
    echo "Installs the zymi binary to /usr/local/bin (requires root)."
    echo "After install, run 'zymi setup' to configure."
    echo ""
    echo "If run from the zymi source tree (Cargo.toml present),"
    echo "builds from source instead of downloading a release."
}

# ─── Parse args ──────────────────────────────────────────────────────

for arg in "$@"; do
    case "$arg" in
        --uninstall) UNINSTALL_MODE=true ;;
        --help|-h)   usage; exit 0 ;;
        *)           red "Unknown option: $arg"; usage; exit 1 ;;
    esac
done

# ─── Source tree detection ─────────────────────────────────────────────

is_source_tree() {
    [ -f "Cargo.toml" ] && grep -q '^name = "zymi"' Cargo.toml 2>/dev/null
}

if is_source_tree && command -v cargo &>/dev/null; then
    SOURCE_BUILD=true
fi

# ─── Dependency checks ──────────────────────────────────────────────

check_deps() {
    local missing=()

    if [ "$SOURCE_BUILD" = true ]; then
        command -v cargo &>/dev/null || missing+=(cargo)
    else
        command -v curl  &>/dev/null || missing+=(curl)
        command -v tar   &>/dev/null || missing+=(tar)

        if ! command -v sha256sum &>/dev/null && ! command -v shasum &>/dev/null; then
            missing+=("sha256sum or shasum")
        fi
    fi

    if [ ${#missing[@]} -gt 0 ]; then
        red "Missing required tools: ${missing[*]}"
        exit 1
    fi
}

sha256_check() {
    local file="$1" expected="$2"
    local actual
    if command -v sha256sum &>/dev/null; then
        actual="$(sha256sum "$file" | awk '{print $1}')"
    else
        actual="$(shasum -a 256 "$file" | awk '{print $1}')"
    fi

    if [ "$actual" != "$expected" ]; then
        red "Checksum verification FAILED!"
        red "  Expected: $expected"
        red "  Actual:   $actual"
        exit 1
    fi
}

# ─── Detect platform ────────────────────────────────────────────────

detect_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"

    case "$os" in
        Linux)  os="unknown-linux-musl" ;;
        Darwin) os="apple-darwin" ;;
        *)      red "Unsupported OS: $os"; exit 1 ;;
    esac

    case "$arch" in
        x86_64)        arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *)             red "Unsupported architecture: $arch"; exit 1 ;;
    esac

    echo "${arch}-${os}"
}

# ─── Build from source ──────────────────────────────────────────────

build_from_source() {
    LATEST="v$(grep '^version = ' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')"

    echo "Building Zymi from source..."
    echo ""
    echo "  Version: $LATEST"
    echo ""

    cargo build --release

    NEW_BIN="$(pwd)/target/release/zymi"
    if [ ! -f "$NEW_BIN" ]; then
        red "Build failed: binary not found."
        exit 1
    fi

    green "Build complete"
    echo ""
}

# ─── Download & verify ──────────────────────────────────────────────

download_binary() {
    echo "Installing Zymi..."
    echo ""

    LATEST="$(curl -sL "https://api.github.com/repos/$GITHUB_REPO/releases/latest" \
        | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"

    if [ -z "$LATEST" ]; then
        red "Failed to fetch latest release from GitHub API."
        exit 1
    fi

    TARGET="$(detect_target)"
    ARCHIVE="zymi-${LATEST}-${TARGET}.tar.gz"
    BASE_URL="https://github.com/$GITHUB_REPO/releases/download/${LATEST}"

    echo "  Version:  $LATEST"
    echo "  Platform: $TARGET"
    echo ""

    TMPDIR="$(mktemp -d)"
    trap 'rm -rf "$TMPDIR"' EXIT

    echo "Downloading archive..."
    if ! curl -fSL "$BASE_URL/$ARCHIVE" -o "$TMPDIR/$ARCHIVE"; then
        red "Download failed. Check that a release exists for your platform ($TARGET)."
        exit 1
    fi

    echo "Downloading checksums..."
    if ! curl -fSL "$BASE_URL/checksums.txt" -o "$TMPDIR/checksums.txt"; then
        red "Failed to download checksums.txt — cannot verify integrity."
        exit 1
    fi

    EXPECTED="$(grep "$ARCHIVE" "$TMPDIR/checksums.txt" | awk '{print $1}')"
    if [ -z "$EXPECTED" ]; then
        red "No checksum found for $ARCHIVE in checksums.txt"
        exit 1
    fi

    echo "Verifying SHA-256 checksum..."
    sha256_check "$TMPDIR/$ARCHIVE" "$EXPECTED"
    green "  Checksum OK"
    echo ""

    tar xzf "$TMPDIR/$ARCHIVE" -C "$TMPDIR"

    NEW_BIN="$TMPDIR/zymi-${LATEST}-${TARGET}/zymi"
    if [ ! -f "$NEW_BIN" ]; then
        red "Binary not found in archive."
        exit 1
    fi
}

# ─── Install ─────────────────────────────────────────────────────────

install_binary() {
    if [ "$(id -u)" -ne 0 ]; then
        red "Error: install requires root (use sudo)"
        exit 1
    fi

    # Stop service if running (upgrade case)
    if systemctl is-active --quiet zymi.service 2>/dev/null; then
        echo "Stopping running zymi service..."
        systemctl stop zymi.service
    fi

    cp "$NEW_BIN" "$INSTALL_DIR/zymi"
    chmod 755 "$INSTALL_DIR/zymi"

    green "Installed zymi $LATEST to $INSTALL_DIR/zymi"
    echo ""
    echo "Get started:"
    echo "  zymi setup    # configure Telegram, API keys, systemd service"
    echo "  zymi          # start daemon"
    echo "  zymi cli      # interactive TUI"

    # Restart service if it was installed
    if [ -f /etc/systemd/system/zymi.service ]; then
        echo ""
        echo "Restarting zymi service..."
        systemctl start zymi.service
        sleep 2
        if systemctl is-active --quiet zymi.service 2>/dev/null; then
            green "Zymi is running."
        else
            red "Zymi failed to start. Check: sudo journalctl -u zymi -n 30"
        fi
    fi
}

# ─── Uninstall ─────────────────────────────────────────────────────────

uninstall() {
    if [ "$(id -u)" -ne 0 ]; then
        red "Error: uninstall requires root (use sudo)"
        exit 1
    fi

    echo "Uninstalling Zymi..."
    echo ""

    # Stop and remove systemd service
    if systemctl is-active --quiet zymi.service 2>/dev/null; then
        echo "Stopping zymi service..."
        systemctl stop zymi.service
    fi
    if systemctl is-enabled --quiet zymi.service 2>/dev/null; then
        systemctl disable zymi.service
    fi
    if [ -f /etc/systemd/system/zymi.service ]; then
        rm /etc/systemd/system/zymi.service
        systemctl daemon-reload
        green "  Removed systemd service"
    fi

    # Remove binary
    if [ -f "$INSTALL_DIR/zymi" ]; then
        rm "$INSTALL_DIR/zymi"
        green "  Removed $INSTALL_DIR/zymi"
    fi

    # Remove sudoers
    if [ -f /etc/sudoers.d/zymi ]; then
        rm /etc/sudoers.d/zymi
        green "  Removed sudoers rules"
    fi

    # Remove data directory
    if [ -d /opt/zymi ]; then
        echo ""
        echo "  Data directory: /opt/zymi"
        dim "    Contains: .env, memory/, conversation history"
        echo ""
        printf "  Delete /opt/zymi? [y/N]: "

        local confirm=""
        if exec 3</dev/tty 2>/dev/null; then
            read -r confirm <&3 || true
            exec 3<&-
        fi

        if [ "$confirm" = "y" ] || [ "$confirm" = "Y" ]; then
            rm -rf /opt/zymi
            green "  Removed /opt/zymi"
        else
            dim "  Kept /opt/zymi"
        fi
    fi

    # Remove system user and group
    if id zymi &>/dev/null; then
        userdel zymi
        green "  Removed system user 'zymi'"
    fi
    if getent group zymi &>/dev/null; then
        groupdel zymi 2>/dev/null || true
        green "  Removed system group 'zymi'"
    fi

    echo ""
    green "Zymi uninstalled."
}

# ─── Main ────────────────────────────────────────────────────────────

if [ "$UNINSTALL_MODE" = true ]; then
    uninstall
    exit 0
fi

check_deps

if [ "$SOURCE_BUILD" = true ]; then
    build_from_source
else
    download_binary
fi

install_binary
