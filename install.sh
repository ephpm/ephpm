#!/bin/sh
# ePHPm installer — https://get.ephpm.dev
#
# Usage:
#   curl -fsSL https://get.ephpm.dev | sh
#   curl -fsSL https://get.ephpm.dev | sh -s -- --no-systemd
#   curl -fsSL https://get.ephpm.dev | EPHPM_VERSION=0.1.0 sh
#
# Options:
#   --no-systemd     Install binary only, skip systemd service setup
#   --no-config      Skip creating default config file
#   --uninstall      Remove ePHPm binary, service, and config
#
# Environment variables:
#   EPHPM_VERSION    Specific version to install (default: latest)
#   EPHPM_INSTALL_DIR  Binary install directory (default: /usr/local/bin)
#   EPHPM_CONFIG_DIR   Config directory (default: /etc/ephpm)
#   EPHPM_DATA_DIR     Data directory for sites (default: /var/www)

set -e

# --- defaults ---
GITHUB_REPO="ephpm/ephpm"
INSTALL_DIR="${EPHPM_INSTALL_DIR:-/usr/local/bin}"
CONFIG_DIR="${EPHPM_CONFIG_DIR:-/etc/ephpm}"
DATA_DIR="${EPHPM_DATA_DIR:-/var/www}"
SERVICE_NAME="ephpm"
SYSTEMD_DIR="/etc/systemd/system"
SETUP_SYSTEMD=true
SETUP_CONFIG=true
UNINSTALL=false

# --- colors ---
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info() { printf "${BLUE}[INFO]${NC} %s\n" "$1"; }
ok() { printf "${GREEN}[OK]${NC} %s\n" "$1"; }
warn() { printf "${YELLOW}[WARN]${NC} %s\n" "$1"; }
fatal() { printf "${RED}[ERROR]${NC} %s\n" "$1" >&2; exit 1; }

# --- parse args ---
for arg in "$@"; do
    case "$arg" in
        --no-systemd) SETUP_SYSTEMD=false ;;
        --no-config) SETUP_CONFIG=false ;;
        --uninstall) UNINSTALL=true ;;
        *) warn "unknown argument: $arg" ;;
    esac
done

# --- detect platform ---
detect_arch() {
    ARCH=$(uname -m)
    case "$ARCH" in
        x86_64|amd64) ARCH="x86_64" ;;
        aarch64|arm64) ARCH="aarch64" ;;
        *) fatal "unsupported architecture: $ARCH (supported: x86_64, aarch64)" ;;
    esac
}

detect_os() {
    OS=$(uname -s | tr '[:upper:]' '[:lower:]')
    case "$OS" in
        linux) OS="linux" ;;
        darwin) OS="darwin" ;;
        *) fatal "unsupported OS: $OS (supported: linux, darwin)" ;;
    esac
}

# --- check prerequisites ---
check_root() {
    if [ "$(id -u)" -ne 0 ]; then
        if [ "$SETUP_SYSTEMD" = true ]; then
            fatal "root privileges required for systemd setup. Run with sudo or use --no-systemd"
        fi
        if [ "$INSTALL_DIR" = "/usr/local/bin" ]; then
            warn "installing to /usr/local/bin requires root. Set EPHPM_INSTALL_DIR or run with sudo"
            INSTALL_DIR="$HOME/.local/bin"
            mkdir -p "$INSTALL_DIR"
            info "installing to $INSTALL_DIR instead"
        fi
    fi
}

check_commands() {
    for cmd in curl tar; do
        if ! command -v "$cmd" > /dev/null 2>&1; then
            fatal "$cmd is required but not installed"
        fi
    done
}

# --- uninstall ---
do_uninstall() {
    info "uninstalling ePHPm..."

    if [ -f "$SYSTEMD_DIR/$SERVICE_NAME.service" ]; then
        systemctl stop "$SERVICE_NAME" 2>/dev/null || true
        systemctl disable "$SERVICE_NAME" 2>/dev/null || true
        rm -f "$SYSTEMD_DIR/$SERVICE_NAME.service"
        systemctl daemon-reload 2>/dev/null || true
        ok "removed systemd service"
    fi

    if [ -f "$INSTALL_DIR/ephpm" ]; then
        rm -f "$INSTALL_DIR/ephpm"
        ok "removed $INSTALL_DIR/ephpm"
    fi

    if [ -d "$CONFIG_DIR" ]; then
        warn "config directory $CONFIG_DIR was NOT removed (contains your config)"
        warn "remove manually: rm -rf $CONFIG_DIR"
    fi

    if [ -d "$DATA_DIR" ]; then
        warn "data directory $DATA_DIR was NOT removed (contains your sites)"
    fi

    ok "ePHPm uninstalled"
    exit 0
}

# --- resolve version ---
resolve_version() {
    if [ -n "$EPHPM_VERSION" ]; then
        VERSION="$EPHPM_VERSION"
        info "using specified version: $VERSION"
        return
    fi

    info "finding latest version..."
    VERSION=$(curl -fsSL "https://api.github.com/repos/$GITHUB_REPO/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"tag_name":\s*"([^"]+)".*/\1/' \
        | sed 's/^v//')

    if [ -z "$VERSION" ]; then
        fatal "could not determine latest version. Set EPHPM_VERSION manually."
    fi

    info "latest version: $VERSION"
}

# --- download and install ---
download_binary() {
    BINARY_NAME="ephpm-${OS}-${ARCH}"
    DOWNLOAD_URL="https://github.com/$GITHUB_REPO/releases/download/v${VERSION}/${BINARY_NAME}.tar.gz"
    TMP_DIR=$(mktemp -d)

    info "downloading ePHPm v${VERSION} for ${OS}/${ARCH}..."
    info "url: $DOWNLOAD_URL"

    if ! curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/ephpm.tar.gz"; then
        # Try without .tar.gz (raw binary)
        DOWNLOAD_URL="https://github.com/$GITHUB_REPO/releases/download/v${VERSION}/${BINARY_NAME}"
        info "trying raw binary: $DOWNLOAD_URL"
        if ! curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/ephpm"; then
            rm -rf "$TMP_DIR"
            fatal "download failed. Check https://github.com/$GITHUB_REPO/releases for available assets."
        fi
    else
        tar -xzf "$TMP_DIR/ephpm.tar.gz" -C "$TMP_DIR" 2>/dev/null || \
        tar -xf "$TMP_DIR/ephpm.tar.gz" -C "$TMP_DIR" 2>/dev/null || \
        fatal "failed to extract archive"
    fi

    # Find the binary (might be in a subdirectory)
    EPHPM_BIN=$(find "$TMP_DIR" -name "ephpm" -type f | head -1)
    if [ -z "$EPHPM_BIN" ]; then
        fatal "ephpm binary not found in archive"
    fi

    chmod +x "$EPHPM_BIN"
    mv "$EPHPM_BIN" "$INSTALL_DIR/ephpm"
    rm -rf "$TMP_DIR"

    ok "installed $INSTALL_DIR/ephpm"
}

# --- check for existing installation ---
check_existing() {
    if [ -f "$INSTALL_DIR/ephpm" ]; then
        CURRENT=$("$INSTALL_DIR/ephpm" --version 2>/dev/null | awk '{print $2}' || echo "unknown")
        info "existing installation found: v${CURRENT}"
        info "upgrading to v${VERSION}"
    fi
}

# --- create default config ---
create_config() {
    if [ "$SETUP_CONFIG" = false ]; then
        return
    fi

    mkdir -p "$CONFIG_DIR"
    mkdir -p "$DATA_DIR/html"
    mkdir -p "$DATA_DIR/sites"

    if [ -f "$CONFIG_DIR/ephpm.toml" ]; then
        info "config already exists at $CONFIG_DIR/ephpm.toml, skipping"
        return
    fi

    cat > "$CONFIG_DIR/ephpm.toml" << 'TOML'
# ePHPm configuration
# Full reference: https://github.com/ephpm/ephpm/blob/main/docs/architecture/sql.md

[server]
listen = "0.0.0.0:8080"
document_root = "/var/www/html"
# sites_dir = "/var/www/sites"   # uncomment for virtual hosting

[php]
memory_limit = "128M"
max_execution_time = 30

# Uncomment for automatic HTTPS:
# [server.tls]
# acme_domains = ["example.com"]
# acme_email = "you@example.com"

# Uncomment for embedded SQLite database:
# [db.sqlite]
# path = "/var/www/data/ephpm.db"
TOML

    ok "created $CONFIG_DIR/ephpm.toml"

    # Create a default index page
    if [ ! -f "$DATA_DIR/html/index.php" ]; then
        cat > "$DATA_DIR/html/index.php" << 'PHP'
<?php
echo "<h1>ePHPm is running!</h1>";
echo "<p>PHP " . PHP_VERSION . "</p>";
echo "<p>Edit /var/www/html/index.php or configure your site.</p>";
PHP
        ok "created default index.php"
    fi
}

# --- setup systemd ---
setup_systemd() {
    if [ "$SETUP_SYSTEMD" = false ]; then
        return
    fi

    if ! command -v systemctl > /dev/null 2>&1; then
        warn "systemctl not found, skipping systemd setup"
        return
    fi

    cat > "$SYSTEMD_DIR/$SERVICE_NAME.service" << EOF
[Unit]
Description=ePHPm PHP Application Server
Documentation=https://github.com/ephpm/ephpm
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$INSTALL_DIR/ephpm --config $CONFIG_DIR/ephpm.toml
Restart=always
RestartSec=5
LimitNOFILE=65536

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$DATA_DIR
ReadWritePaths=/tmp

[Install]
WantedBy=multi-user.target
EOF

    systemctl daemon-reload
    ok "created systemd service: $SERVICE_NAME"

    if systemctl is-active --quiet "$SERVICE_NAME"; then
        systemctl restart "$SERVICE_NAME"
        ok "restarted $SERVICE_NAME"
    else
        systemctl enable "$SERVICE_NAME"
        systemctl start "$SERVICE_NAME"
        ok "started $SERVICE_NAME"
    fi
}

# --- summary ---
print_summary() {
    echo ""
    echo "────────────────────────────────────────────"
    printf "${GREEN}ePHPm v${VERSION} installed successfully${NC}\n"
    echo "────────────────────────────────────────────"
    echo ""
    echo "  Binary:    $INSTALL_DIR/ephpm"
    if [ "$SETUP_CONFIG" = true ]; then
        echo "  Config:    $CONFIG_DIR/ephpm.toml"
        echo "  Doc root:  $DATA_DIR/html"
    fi
    if [ "$SETUP_SYSTEMD" = true ] && command -v systemctl > /dev/null 2>&1; then
        echo "  Service:   systemctl status $SERVICE_NAME"
        echo ""
        echo "  Your site is live at: http://$(hostname -I 2>/dev/null | awk '{print $1}' || echo 'your-server'):8080"
    fi
    echo ""
    echo "  Quick start:"
    echo "    ephpm --config $CONFIG_DIR/ephpm.toml"
    echo ""
    echo "  Docs:      https://github.com/ephpm/ephpm"
    echo "  Migration: https://github.com/ephpm/ephpm/tree/main/docs/migration"
    echo ""
}

# --- main ---
main() {
    echo ""
    printf "${GREEN}ePHPm Installer${NC}\n"
    echo ""

    detect_arch
    detect_os
    check_commands

    if [ "$UNINSTALL" = true ]; then
        check_root
        do_uninstall
    fi

    check_root
    resolve_version
    check_existing
    download_binary
    create_config
    setup_systemd
    print_summary
}

main
