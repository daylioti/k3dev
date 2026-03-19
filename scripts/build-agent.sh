#!/usr/bin/env bash
# Build k3dev-agent for embedding into the main binary.
# Produces static musl binaries for x86_64 and aarch64.
#
# Prerequisites:
#   cargo install cross    (or: install musl targets directly)
#
# Usage:
#   ./scripts/build-agent.sh              # Build for current arch only
#   ./scripts/build-agent.sh --all        # Build for both architectures
#   ./scripts/build-agent.sh --cross      # Use cross tool for all architectures

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
AGENT_DIR="$PROJECT_DIR/agent"
ASSETS_DIR="$PROJECT_DIR/assets"

mkdir -p "$ASSETS_DIR"

build_native() {
    local arch
    arch=$(uname -m)

    case "$arch" in
        x86_64) local suffix="x86_64" ;;
        aarch64) local suffix="aarch64" ;;
        *)
            echo "Unsupported architecture: $arch"
            exit 1
            ;;
    esac

    local musl_target="${suffix}-unknown-linux-musl"
    if [ "$suffix" = "x86_64" ]; then
        musl_target="x86_64-unknown-linux-musl"
    else
        musl_target="aarch64-unknown-linux-musl"
    fi

    # Try musl target first (smallest binary), fall back to static glibc
    if rustup target add "$musl_target" 2>/dev/null && \
       cargo build --release --target "$musl_target" --manifest-path "$AGENT_DIR/Cargo.toml" 2>/dev/null; then
        local binary="$AGENT_DIR/target/$musl_target/release/k3dev-agent"
        echo "Built with musl target"
    else
        echo "musl target unavailable, building with static glibc..."
        RUSTFLAGS='-C target-feature=+crt-static' \
            cargo build --release --manifest-path "$AGENT_DIR/Cargo.toml"
        local binary="$AGENT_DIR/target/release/k3dev-agent"
    fi

    local dest="$ASSETS_DIR/k3dev-agent-$suffix"
    cp "$binary" "$dest"

    local size
    size=$(stat --printf='%s' "$dest" 2>/dev/null || stat -f '%z' "$dest" 2>/dev/null)
    echo "Built: $dest ($(( size / 1024 ))KB)"
}

build_cross() {
    if ! command -v cross &>/dev/null; then
        echo "Error: 'cross' not found. Install with: cargo install cross"
        exit 1
    fi

    for target in x86_64-unknown-linux-musl aarch64-unknown-linux-musl; do
        local suffix="${target%%-*}"
        echo "Building k3dev-agent for $target (via cross)..."
        cross build --release --target "$target" --manifest-path "$AGENT_DIR/Cargo.toml"

        local binary="$AGENT_DIR/target/$target/release/k3dev-agent"
        local dest="$ASSETS_DIR/k3dev-agent-$suffix"
        cp "$binary" "$dest"

        local size
        size=$(stat --printf='%s' "$dest" 2>/dev/null || stat -f '%z' "$dest" 2>/dev/null)
        echo "Built: $dest ($(( size / 1024 ))KB)"
    done
}

case "${1:-}" in
    --cross)
        build_cross
        ;;
    --all)
        # Try cross first, fall back to native
        if command -v cross &>/dev/null; then
            build_cross
        else
            echo "cross not found, building for current architecture only"
            build_native
        fi
        ;;
    *)
        build_native
        ;;
esac

echo "Done. Agent binaries in $ASSETS_DIR/"
