#!/usr/bin/env bash
# Build all embedded assets (k3dev-agent + socat) for the target architecture.
#
# Usage:
#   ./scripts/build-assets.sh              # Build for current arch only
#   ./scripts/build-assets.sh --all        # Build for both x86_64 and aarch64
#   ./scripts/build-assets.sh --ci         # CI mode: real x86_64, placeholders for aarch64
#
# Prerequisites:
#   - Rust toolchain with musl target
#   - musl-tools (apt: musl-tools)
#   - For --all: cross (cargo install cross) or aarch64 cross-compiler
#   - For socat: gcc, musl-gcc, autoconf (build deps)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
AGENT_DIR="$PROJECT_DIR/agent"
ASSETS_DIR="$PROJECT_DIR/assets"
SOCAT_VERSION="1.8.0.2"

mkdir -p "$ASSETS_DIR"

build_agent_native() {
    local arch
    arch=$(uname -m)

    echo "Building k3dev-agent for $arch..."
    local musl_target="${arch}-unknown-linux-musl"
    rustup target add "$musl_target" 2>/dev/null || true
    cargo build --release --target "$musl_target" --manifest-path "$AGENT_DIR/Cargo.toml"
    cp "$AGENT_DIR/target/$musl_target/release/k3dev-agent" "$ASSETS_DIR/k3dev-agent-$arch"
    echo "Built: assets/k3dev-agent-$arch ($(du -h "$ASSETS_DIR/k3dev-agent-$arch" | cut -f1))"
}

build_agent_cross() {
    local target="$1"
    local suffix="${target%%-*}"

    echo "Building k3dev-agent for $suffix (via cross)..."
    cross build --release --target "$target" --manifest-path "$AGENT_DIR/Cargo.toml"
    cp "$AGENT_DIR/target/$target/release/k3dev-agent" "$ASSETS_DIR/k3dev-agent-$suffix"
    echo "Built: assets/k3dev-agent-$suffix ($(du -h "$ASSETS_DIR/k3dev-agent-$suffix" | cut -f1))"
}

build_socat_native() {
    local arch
    arch=$(uname -m)

    if [[ -f "$ASSETS_DIR/socat-$arch" ]]; then
        echo "socat-$arch already exists, skipping"
        return
    fi

    echo "Building socat $SOCAT_VERSION for $arch..."
    local tmpdir
    tmpdir=$(mktemp -d)
    trap "rm -rf $tmpdir" RETURN

    cd "$tmpdir"
    curl -sSfL "http://www.dest-unreach.org/socat/download/socat-${SOCAT_VERSION}.tar.gz" -o socat.tar.gz
    tar xzf socat.tar.gz
    cd "socat-${SOCAT_VERSION}"

    # Build static with musl, no optional deps to keep it small
    CC=musl-gcc CFLAGS="-static -Os" ./configure \
        --disable-openssl \
        --disable-readline \
        --disable-libwrap \
        --disable-fips \
        >/dev/null 2>&1

    make -j"$(nproc)" >/dev/null 2>&1
    strip socat

    cp socat "$ASSETS_DIR/socat-$arch"
    echo "Built: assets/socat-$arch ($(du -h "$ASSETS_DIR/socat-$arch" | cut -f1))"
}

create_placeholders() {
    # Create minimal placeholder files for the non-target architecture.
    # include_bytes! guarded by #[cfg(target_arch)] won't compile these,
    # but the files must exist for cargo to parse the source.
    local arch
    arch=$(uname -m)

    if [[ "$arch" == "x86_64" ]]; then
        [[ -f "$ASSETS_DIR/k3dev-agent-aarch64" ]] || echo -n "placeholder" > "$ASSETS_DIR/k3dev-agent-aarch64"
        [[ -f "$ASSETS_DIR/socat-aarch64" ]] || echo -n "placeholder" > "$ASSETS_DIR/socat-aarch64"
    else
        [[ -f "$ASSETS_DIR/k3dev-agent-x86_64" ]] || echo -n "placeholder" > "$ASSETS_DIR/k3dev-agent-x86_64"
        [[ -f "$ASSETS_DIR/socat-x86_64" ]] || echo -n "placeholder" > "$ASSETS_DIR/socat-x86_64"
    fi
}

case "${1:-}" in
    --ci)
        # CI mode: build real binaries for host arch, placeholders for the other
        build_agent_native
        build_socat_native
        create_placeholders
        ;;
    --all)
        # Build everything for both architectures
        build_agent_native
        build_socat_native

        if command -v cross &>/dev/null; then
            local_arch=$(uname -m)
            if [[ "$local_arch" == "x86_64" ]]; then
                build_agent_cross "aarch64-unknown-linux-musl"
            else
                build_agent_cross "x86_64-unknown-linux-musl"
            fi
        else
            echo "Warning: 'cross' not found, only built for current architecture"
            create_placeholders
        fi
        ;;
    *)
        # Default: build for current arch, placeholders for other
        build_agent_native
        build_socat_native
        create_placeholders
        ;;
esac

echo "Done. Assets in $ASSETS_DIR/:"
ls -lh "$ASSETS_DIR/"
