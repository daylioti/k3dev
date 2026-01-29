.PHONY: setup check fmt clippy test build release clean ci

# Setup development environment
setup:
	git config core.hooksPath .githooks
	@echo "Git hooks configured"

# Run all checks (same as CI)
check: fmt clippy test

# Check formatting
fmt:
	cargo fmt --all -- --check

# Run clippy
clippy:
	cargo clippy --all-targets --all-features -- -D warnings

# Run tests
test:
	cargo test --all-features

# Build debug
build:
	cargo build

# Build release
release:
	cargo build --release

# Clean build artifacts
clean:
	cargo clean

# Run CI workflow locally with act (requires: sudo pacman -S act)
ci:
	act -j check
