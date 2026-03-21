#!/usr/bin/env bash
set -euo pipefail

GITHUB_REPO="metravod/zymi"
INSTALL_DIR="${HOME}/.local/bin"
DAEMON_INSTALL_DIR="/opt/zymi"
DAEMON_MODE=false
UNINSTALL_MODE=false
SOURCE_BUILD=false

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
dim()   { printf '\033[2m%s\033[0m\n' "$*"; }

usage() {
    echo "Usage: install.sh [--daemon] [--uninstall]"
    echo ""
    echo "  --daemon      Install as systemd service (requires root)"
    echo "  --uninstall   Remove zymi (auto-detects daemon vs user install)"
    echo ""
    echo "Without --daemon: installs binary to ~/.local/bin"
    echo "With --daemon:    installs binary + systemd service to /opt/zymi"
    echo ""
    echo "If run from the zymi source tree (Cargo.toml present),"
    echo "builds from source instead of downloading a release."
}

# ─── Parse args ──────────────────────────────────────────────────────

for arg in "$@"; do
    case "$arg" in
        --daemon)    DAEMON_MODE=true ;;
        --uninstall) UNINSTALL_MODE=true ;;
        --help|-h)   usage; exit 0 ;;
        *)           red "Unknown option: $arg"; usage; exit 1 ;;
    esac
done

if [ "$DAEMON_MODE" = true ] && [ "$(id -u)" -ne 0 ]; then
    red "Error: --daemon requires root (use sudo)"
    exit 1
fi

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

    if [ "$DAEMON_MODE" = true ]; then
        command -v systemctl &>/dev/null || missing+=(systemctl)
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

# ─── Install modes ──────────────────────────────────────────────────

install_user() {
    mkdir -p "$INSTALL_DIR"
    cp "$NEW_BIN" "$INSTALL_DIR/zymi"
    chmod +x "$INSTALL_DIR/zymi"

    green "Installed zymi $LATEST to $INSTALL_DIR/zymi"

    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        echo ""
        echo "Add ~/.local/bin to your PATH:"
        echo ""

        SHELL_NAME="$(basename "${SHELL:-/bin/bash}")"
        case "$SHELL_NAME" in
            zsh)
                dim "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc"
                dim "  source ~/.zshrc"
                ;;
            *)
                dim "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.bashrc"
                dim "  source ~/.bashrc"
                ;;
        esac
    fi

    echo ""
    echo "Get started:"
    echo "  zymi setup    # first-time setup wizard"
    echo "  zymi          # start daemon (Telegram + scheduler + healthcheck)"
    echo "  zymi cli      # interactive TUI mode"
}

install_daemon() {
    local ZUMI_USER="zymi"
    local ZUMI_GROUP="zymi"

    # ── Create system user & group ───────────────────────────────────

    if ! getent group "$ZUMI_GROUP" &>/dev/null; then
        echo "Creating system group '$ZUMI_GROUP'..."
        groupadd --system "$ZUMI_GROUP"
    fi

    if ! id "$ZUMI_USER" &>/dev/null; then
        echo "Creating system user '$ZUMI_USER'..."
        useradd --system \
            --gid "$ZUMI_GROUP" \
            --shell /usr/sbin/nologin \
            --home-dir "$DAEMON_INSTALL_DIR" \
            --no-create-home \
            "$ZUMI_USER"
    fi

    # ── Add to supplementary groups (if they exist) ──────────────────

    # docker — container management via /var/run/docker.sock
    # NOTE: docker group grants broad API access. Privilege escalation
    # vectors (--privileged, host root mounts, --pid=host, --cap-add=ALL)
    # are blocked by zymi's built-in policy engine (check_dangerous in policy.rs).
    if getent group docker &>/dev/null; then
        usermod -aG docker "$ZUMI_USER"
        echo "  Added $ZUMI_USER to docker group"
    fi

    # adm — read /var/log/* and journalctl access
    if getent group adm &>/dev/null; then
        usermod -aG adm "$ZUMI_USER"
        echo "  Added $ZUMI_USER to adm group"
    fi

    # systemd-journal — journalctl on systems without adm
    if getent group systemd-journal &>/dev/null; then
        usermod -aG systemd-journal "$ZUMI_USER"
        echo "  Added $ZUMI_USER to systemd-journal group"
    fi

    # ── Create directory structure ───────────────────────────────────

    mkdir -p "$DAEMON_INSTALL_DIR/memory"
    mkdir -p "$DAEMON_INSTALL_DIR/memory/baseline"
    mkdir -p "$DAEMON_INSTALL_DIR/memory/subagents"
    mkdir -p "$DAEMON_INSTALL_DIR/memory/evals"
    mkdir -p "$DAEMON_INSTALL_DIR/memory/eval_results"
    mkdir -p "$DAEMON_INSTALL_DIR/memory/workflow_scripts"
    mkdir -p "$DAEMON_INSTALL_DIR/memory/workflow_traces"

    # Install binary
    cp "$NEW_BIN" /usr/local/bin/zymi
    chmod 755 /usr/local/bin/zymi

    # Set ownership and permissions
    chown -R "$ZUMI_USER:$ZUMI_GROUP" "$DAEMON_INSTALL_DIR"
    chmod 750 "$DAEMON_INSTALL_DIR"
    chmod 700 "$DAEMON_INSTALL_DIR/memory"

    # ── Interactive .env setup ───────────────────────────────────────

    local ENV_FILE="$DAEMON_INSTALL_DIR/.env"
    local NEEDS_SETUP=false

    if [ ! -f "$ENV_FILE" ]; then
        NEEDS_SETUP=true
        touch "$ENV_FILE"
        chown "$ZUMI_USER:$ZUMI_GROUP" "$ENV_FILE"
        chmod 600 "$ENV_FILE"
    else
        # Check if required keys are set
        local has_token has_users has_llm
        has_token="$(grep -c '^TELOXIDE_TOKEN=.\+' "$ENV_FILE" 2>/dev/null)" || has_token=0
        has_users="$(grep -c '^ALLOWED_USERS=.\+' "$ENV_FILE" 2>/dev/null)" || has_users=0
        has_llm="$(grep -cE '^(OPENAI_API_KEY|ANTHROPIC_API_KEY)=.\+' "$ENV_FILE" 2>/dev/null)" || has_llm=0
        if [ "$has_token" -eq 0 ] || [ "$has_users" -eq 0 ] || [ "$has_llm" -eq 0 ]; then
            NEEDS_SETUP=true
        fi
    fi

    if [ "$NEEDS_SETUP" = true ]; then
        echo ""
        echo "─── Configuration ─────────────────────────────────────────"
        echo ""
        echo "Zymi needs a Telegram bot token, allowed user IDs, and an LLM API key."
        echo "Press Enter to keep the current value (shown in brackets)."
        echo ""

        # Helper: read existing value from .env
        env_val() { grep "^$1=" "$ENV_FILE" 2>/dev/null | head -1 | cut -d= -f2-; }

        # When piped via curl|bash, stdin is the pipe (EOF).
        # Read interactive input from /dev/tty instead.
        # If no tty available (non-interactive SSH), skip setup entirely.
        # Note: /dev/tty may exist as a device node but fail to open
        # when there is no controlling terminal, so we test with a trial open.
        if [ -t 0 ]; then
            exec 3<&0
        elif exec 3</dev/tty 2>/dev/null; then
            : # fd 3 is now open on /dev/tty
        else
            echo ""
            dim "  No interactive terminal available — skipping setup."
            echo ""
            echo "  Configure manually:"
            dim "    sudo nano $ENV_FILE"
            dim "    sudo systemctl restart zymi"
            NEEDS_SETUP=false
        fi
    fi

    if [ "$NEEDS_SETUP" = true ]; then
        # Telegram token
        local cur_token
        cur_token="$(env_val TELOXIDE_TOKEN)"
        if [ -n "$cur_token" ]; then
            local masked="${cur_token:0:8}...${cur_token: -4}"
            printf "  Telegram bot token [%s]: " "$masked"
        else
            printf "  Telegram bot token (from @BotFather): "
        fi
        read -r input_token <&3 || true
        local final_token="${input_token:-$cur_token}"

        # Allowed users
        local cur_users
        cur_users="$(env_val ALLOWED_USERS)"
        if [ -n "$cur_users" ]; then
            printf "  Allowed Telegram user IDs [%s]: " "$cur_users"
        else
            printf "  Allowed Telegram user IDs (comma-separated): "
        fi
        read -r input_users <&3 || true
        local final_users="${input_users:-$cur_users}"

        # LLM provider
        echo ""
        echo "  LLM provider:"
        echo "    [1] OpenAI (OPENAI_API_KEY)"
        echo "    [2] Anthropic (ANTHROPIC_API_KEY)"
        echo "    [3] ChatGPT Plus/Pro (OAuth — configured after install)"
        echo "    [4] Skip (configure manually later)"
        echo ""
        printf "  Choice [1]: "
        read -r llm_choice <&3 || true
        llm_choice="${llm_choice:-1}"

        local llm_key_name="" llm_key_val=""
        case "$llm_choice" in
            1)
                llm_key_name="OPENAI_API_KEY"
                local cur_openai
                cur_openai="$(env_val OPENAI_API_KEY)"
                if [ -n "$cur_openai" ]; then
                    printf "  OpenAI API key [%s...]: " "${cur_openai:0:8}"
                else
                    printf "  OpenAI API key: "
                fi
                read -r input_key <&3 || true
                llm_key_val="${input_key:-$cur_openai}"
                ;;
            2)
                llm_key_name="ANTHROPIC_API_KEY"
                local cur_anthropic
                cur_anthropic="$(env_val ANTHROPIC_API_KEY)"
                if [ -n "$cur_anthropic" ]; then
                    printf "  Anthropic API key [%s...]: " "${cur_anthropic:0:8}"
                else
                    printf "  Anthropic API key: "
                fi
                read -r input_key <&3 || true
                llm_key_val="${input_key:-$cur_anthropic}"
                ;;
            3)
                echo ""
                echo "  ChatGPT OAuth: run after install:"
                dim "    sudo -u zymi zymi login --remote"
                ;;
            *)
                echo "  Skipping LLM setup."
                ;;
        esac

        exec 3<&-

        # Save extra keys from old .env BEFORE overwriting
        local preserved=""
        for extra_key in TAVILY_API_KEY FIRECRAWL_API_KEY BLAND_API_KEY SUPADATA_API_KEY \
                         LANGFUSE_PUBLIC_KEY LANGFUSE_SECRET_KEY ANTHROPIC_API_KEY OPENAI_API_KEY; do
            [ "$extra_key" = "$llm_key_name" ] && continue
            local extra_val
            extra_val="$(env_val "$extra_key")"
            if [ -n "$extra_val" ]; then
                preserved="${preserved}${extra_key}=${extra_val}\n"
            fi
        done

        # Write .env
        cat > "$ENV_FILE" << ENVFILE
# Telegram bot
TELOXIDE_TOKEN=${final_token}
ALLOWED_USERS=${final_users}

# LLM provider
${llm_key_name:+${llm_key_name}=${llm_key_val}}
ENVFILE

        # Append preserved keys
        if [ -n "$preserved" ]; then
            printf "%b" "$preserved" >> "$ENV_FILE"
        fi

        chown "$ZUMI_USER:$ZUMI_GROUP" "$ENV_FILE"
        chmod 600 "$ENV_FILE"

        echo ""
        green "  Configuration saved to $ENV_FILE"
    fi

    # ── Sudoers for healthcheck commands ─────────────────────────────

    echo "Configuring sudoers for healthcheck commands..."
    cat > /etc/sudoers.d/zymi << 'SUDOERS'
# Zymi daemon — read-only system introspection for healthchecks
# No password required for these specific commands
#
# NOTE: journalctl is NOT here — adm/systemd-journal group membership
# already grants read access to the journal without privilege escalation.

# Crontab inspection (root's crontab)
zymi ALL=(ALL) NOPASSWD: /usr/bin/crontab -l

# Disk health (S.M.A.R.T)
zymi ALL=(ALL) NOPASSWD: /usr/sbin/smartctl -a *
zymi ALL=(ALL) NOPASSWD: /usr/sbin/smartctl -H *
zymi ALL=(ALL) NOPASSWD: /usr/sbin/smartctl --scan

# Kernel messages (OOM, hardware errors)
zymi ALL=(ALL) NOPASSWD: /usr/bin/dmesg
zymi ALL=(ALL) NOPASSWD: /usr/bin/dmesg --level=err\,warn
zymi ALL=(ALL) NOPASSWD: /usr/bin/dmesg -T

# Security updates check
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt list --upgradable
zymi ALL=(ALL) NOPASSWD: /usr/bin/yum check-update

# Package management (human approves via Telegram policy engine)
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt update
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt install *
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt upgrade *
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt remove *
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt-get update
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt-get install *
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt-get upgrade *
zymi ALL=(ALL) NOPASSWD: /usr/bin/apt-get remove *

# Service management (human approves via Telegram policy engine)
zymi ALL=(ALL) NOPASSWD: /usr/bin/systemctl start *
zymi ALL=(ALL) NOPASSWD: /usr/bin/systemctl stop *
zymi ALL=(ALL) NOPASSWD: /usr/bin/systemctl restart *
zymi ALL=(ALL) NOPASSWD: /usr/bin/systemctl enable *
zymi ALL=(ALL) NOPASSWD: /usr/bin/systemctl disable *
zymi ALL=(ALL) NOPASSWD: /usr/bin/systemctl status *
zymi ALL=(ALL) NOPASSWD: /usr/bin/systemctl daemon-reload

# NOTE: Docker access is via docker group membership, not sudoers.
# Privilege escalation vectors (--privileged, host root mounts, etc.)
# are blocked by zymi's built-in policy engine.
SUDOERS
    chmod 440 /etc/sudoers.d/zymi

    # Validate sudoers syntax
    if ! visudo -c -f /etc/sudoers.d/zymi &>/dev/null; then
        red "Warning: sudoers file has syntax errors, removing it"
        rm -f /etc/sudoers.d/zymi
    else
        green "  Sudoers configured"
    fi

    # ── systemd service ──────────────────────────────────────────────

    cat > /etc/systemd/system/zymi.service << 'UNIT'
[Unit]
Description=Zymi AI Agent Daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=forking
PIDFile=/opt/zymi/.zymi.pid
User=zymi
Group=zymi
WorkingDirectory=/opt/zymi
ExecStart=/usr/local/bin/zymi
Restart=on-failure
RestartSec=10
TimeoutStopSec=30

# Environment
EnvironmentFile=-/opt/zymi/.env
Environment=RUST_LOG=info
Environment=MEMORY_DIR=/opt/zymi/memory

# Security hardening
# NOTE: NoNewPrivileges is intentionally OFF — healthcheck commands
# use sudo for read-only introspection (smartctl, dmesg, docker).
# setuid is required for sudo to work.
# NOTE: ProtectKernelLogs is intentionally OFF — dmesg needs CAP_SYSLOG
# which systemd strips when ProtectKernelLogs=true.
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/zymi
PrivateTmp=true
ProtectClock=true
ProtectHostname=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictRealtime=true
RestrictNamespaces=true
LockPersonality=true
SystemCallArchitectures=native

# /proc access for healthchecks
ProcSubset=all
ProtectProc=default

# Resource limits
LimitNOFILE=65536
MemoryMax=512M
TasksMax=256

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=zymi

[Install]
WantedBy=multi-user.target
UNIT

    systemctl daemon-reload
    systemctl enable zymi.service

    green "Installed zymi $LATEST as systemd service"
    echo ""
    echo "  User:      $ZUMI_USER ($(id -nG "$ZUMI_USER" 2>/dev/null || echo "$ZUMI_GROUP"))"
    echo "  Home:      $DAEMON_INSTALL_DIR"
    echo "  Config:    $DAEMON_INSTALL_DIR/.env"
    echo "  Sudoers:   /etc/sudoers.d/zymi"
    echo "  Service:   zymi.service"

    # ── Auto-start / restart ─────────────────────────────────────────

    local has_token has_users
    has_token="$(grep -c '^TELOXIDE_TOKEN=.\+' "$DAEMON_INSTALL_DIR/.env" 2>/dev/null)" || has_token=0
    has_users="$(grep -c '^ALLOWED_USERS=.\+' "$DAEMON_INSTALL_DIR/.env" 2>/dev/null)" || has_users=0

    echo ""
    if [ "$has_token" -gt 0 ] && [ "$has_users" -gt 0 ]; then
        if systemctl is-active --quiet zymi.service 2>/dev/null; then
            echo "Restarting zymi..."
            systemctl restart zymi.service
        else
            echo "Starting zymi..."
            systemctl start zymi.service
        fi
        sleep 2
        if systemctl is-active --quiet zymi.service 2>/dev/null; then
            green "Zymi is running."
            dim "  sudo journalctl -u zymi -f"
        else
            red "Zymi failed to start. Check logs:"
            dim "  sudo journalctl -u zymi --no-pager -n 30"
        fi
    else
        echo "Telegram not configured — skipping auto-start."
        echo ""
        echo "To configure later:"
        dim "  sudo nano $DAEMON_INSTALL_DIR/.env"
        dim "  sudo systemctl start zymi"
    fi

    # ChatGPT OAuth hint
    if grep -qE '^(OPENAI_API_KEY|ANTHROPIC_API_KEY)=.\+' "$DAEMON_INSTALL_DIR/.env" 2>/dev/null; then
        : # LLM key present, no hint needed
    else
        echo ""
        echo "ChatGPT OAuth login:"
        dim "  sudo -u zymi zymi login --remote"
    fi
}

# ─── Uninstall ─────────────────────────────────────────────────────────

uninstall_daemon() {
    echo "Uninstalling Zymi daemon..."
    echo ""

    # Stop and disable service
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
    if [ -f /usr/local/bin/zymi ]; then
        rm /usr/local/bin/zymi
        green "  Removed /usr/local/bin/zymi"
    fi

    # Remove sudoers
    if [ -f /etc/sudoers.d/zymi ]; then
        rm /etc/sudoers.d/zymi
        green "  Removed sudoers rules"
    fi

    # Remove data directory
    if [ -d "$DAEMON_INSTALL_DIR" ]; then
        echo ""
        echo "  Data directory: $DAEMON_INSTALL_DIR"
        dim "    Contains: .env, memory/, conversation history"
        echo ""
        printf "  Delete %s? [y/N]: " "$DAEMON_INSTALL_DIR"

        local confirm=""
        if [ -e /dev/tty ]; then
            read -r confirm </dev/tty || true
        else
            # Non-interactive (e.g. ssh "cmd") — default to no
            confirm=""
        fi

        if [ "$confirm" = "y" ] || [ "$confirm" = "Y" ]; then
            rm -rf "$DAEMON_INSTALL_DIR"
            green "  Removed $DAEMON_INSTALL_DIR"
        else
            dim "  Kept $DAEMON_INSTALL_DIR"
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
    green "Zymi daemon uninstalled."
}

uninstall_user() {
    echo "Uninstalling Zymi..."
    echo ""

    if [ -f "$INSTALL_DIR/zymi" ]; then
        rm "$INSTALL_DIR/zymi"
        green "  Removed $INSTALL_DIR/zymi"
    else
        dim "  Binary not found at $INSTALL_DIR/zymi"
    fi

    echo ""
    green "Zymi uninstalled."
}

# ─── Main ────────────────────────────────────────────────────────────

if [ "$UNINSTALL_MODE" = true ]; then
    # Auto-detect: daemon install exists?
    if [ -f /etc/systemd/system/zymi.service ] || [ -d "$DAEMON_INSTALL_DIR" ] || [ -f /usr/local/bin/zymi ]; then
        if [ "$(id -u)" -ne 0 ]; then
            red "Error: uninstalling daemon requires root (use sudo)"
            exit 1
        fi
        uninstall_daemon
    else
        uninstall_user
    fi
    exit 0
fi

check_deps

if [ "$SOURCE_BUILD" = true ]; then
    build_from_source
else
    download_binary
fi

if [ "$DAEMON_MODE" = true ]; then
    install_daemon
else
    install_user
fi
