#!/usr/bin/env bash
#
# k3dev installer script
# Usage: curl -fsSL https://raw.githubusercontent.com/daylioti/k3dev/main/install.sh | bash
#

set -euo pipefail

REPO="daylioti/k3dev"
INSTALL_DIR="${K3DEV_INSTALL_DIR:-$HOME/.local/bin}"
BINARY_NAME="k3dev"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

info() { echo -e "${BLUE}==>${NC} $1"; }
success() { echo -e "${GREEN}==>${NC} $1"; }
warn() { echo -e "${YELLOW}==>${NC} $1"; }
error() { echo -e "${RED}Error:${NC} $1" >&2; exit 1; }

# Detect OS
detect_os() {
    case "$(uname -s)" in
        Linux*) echo "linux" ;;
        Darwin*) echo "macos" ;;
        *) error "Unsupported OS: $(uname -s)" ;;
    esac
}

# Detect architecture
detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64) echo "x86_64" ;;
        aarch64|arm64) echo "aarch64" ;;
        *) error "Unsupported architecture: $(uname -m)" ;;
    esac
}

# Check if musl libc (Linux only)
is_musl() {
    if command -v ldd >/dev/null 2>&1; then
        ldd --version 2>&1 | grep -qi musl && return 0
    fi
    # Check if /lib/ld-musl exists
    [ -f /lib/ld-musl-*.so.1 ] && return 0
    return 1
}

# Get latest release version
get_latest_version() {
    local version
    version=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
    if [ -z "$version" ]; then
        error "Failed to get latest version"
    fi
    echo "$version"
}

# Download and install
install() {
    local os arch variant artifact version download_url

    os=$(detect_os)
    arch=$(detect_arch)

    # Determine variant (musl or gnu, Linux only)
    variant=""
    if [[ "$os" == "linux" ]] && is_musl; then
        variant="-musl"
        info "Detected musl libc"
    fi

    artifact="${BINARY_NAME}-${os}-${arch}${variant}"

    info "Detecting latest version..."
    version=$(get_latest_version)
    info "Latest version: ${version}"

    download_url="https://github.com/${REPO}/releases/download/${version}/${artifact}"

    info "Downloading ${artifact}..."
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT

    if ! curl -fsSL "$download_url" -o "${tmp_dir}/${BINARY_NAME}"; then
        error "Failed to download from ${download_url}"
    fi

    # Verify checksum if available
    checksum_url="${download_url}.sha256"
    if curl -fsSL "$checksum_url" -o "${tmp_dir}/checksum.sha256" 2>/dev/null; then
        info "Verifying checksum..."
        cd "$tmp_dir"
        # Use shasum on macOS, sha256sum on Linux
        if command -v sha256sum >/dev/null 2>&1; then
            sha_cmd="sha256sum"
        elif command -v shasum >/dev/null 2>&1; then
            sha_cmd="shasum -a 256"
        else
            warn "No checksum tool found, skipping verification"
            sha_cmd=""
        fi
        if [ -n "$sha_cmd" ]; then
            expected=$(awk '{print $1}' checksum.sha256)
            actual=$($sha_cmd "${BINARY_NAME}" | awk '{print $1}')
            if [ "$expected" != "$actual" ]; then
                error "Checksum verification failed"
            fi
            success "Checksum verified"
        fi
        cd - >/dev/null
    fi

    # Create install directory
    mkdir -p "$INSTALL_DIR"

    # Install binary
    info "Installing to ${INSTALL_DIR}/${BINARY_NAME}..."
    mv "${tmp_dir}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
    chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

    success "k3dev ${version} installed successfully!"

    # Check if install dir is in PATH
    if [[ ":$PATH:" != *":${INSTALL_DIR}:"* ]]; then
        warn "Add ${INSTALL_DIR} to your PATH:"
        echo ""
        echo "  export PATH=\"\$PATH:${INSTALL_DIR}\""
        echo ""
        echo "  # Add to ~/.bashrc or ~/.zshrc to make permanent"
    fi

    echo ""
    info "Run 'k3dev --help' to get started"
}

# Main
main() {
    echo ""
    echo "  k3dev installer"
    echo "  ==============="
    echo ""

    # Check requirements
    if ! command -v curl >/dev/null 2>&1; then
        error "curl is required but not installed"
    fi

    install

    # Post-install checks
    local os
    os=$(detect_os)
    echo ""
    if ! command -v docker >/dev/null 2>&1; then
        warn "Docker is not installed. k3dev requires Docker to run."
        if [[ "$os" == "macos" ]]; then
            echo "  Install Docker Desktop: https://www.docker.com/products/docker-desktop/"
        fi
    elif ! docker info >/dev/null 2>&1; then
        warn "Docker is not running or not accessible."
        if [[ "$os" == "macos" ]]; then
            echo "  Start Docker Desktop from Applications."
        fi
    fi
}

main "$@"
