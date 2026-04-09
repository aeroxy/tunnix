.PHONY: build test clean run-server run-client run-client-bg release release-linux release-all help

LINUX_TARGET = x86_64-unknown-linux-gnu
LINUX_OUT    = target/$(LINUX_TARGET)/release

# Default target
help:
	@echo "tunnix Build Targets:"
	@echo "  make build         - Build debug binary (native)"
	@echo "  make release       - Build release binary (native)"
	@echo "  make release-linux - Build release binary for Linux x86_64 (via zigbuild)"
	@echo "  make release-all   - Build release binary for macOS (native) and Linux (zigbuild)"
	@echo "  make test          - Run all tests"
	@echo "  make test-crypto   - Run crypto tests only"
	@echo "  make run-server    - Run server in debug mode"
	@echo "  make run-client    - Run client in debug mode"
	@echo "  make run-client-bg - Run client in background with nohup"
	@echo "  make clean         - Clean build artifacts"
	@echo "  make fmt           - Format code"
	@echo "  make clippy        - Run clippy lints"

# Build debug binary (native)
build:
	cargo build

# Build release binary (native)
release:
	cargo build --release
	@echo ""
	@echo "Native release binary:"
	@ls -lh target/release/tunnix 2>/dev/null || true

# Build release binary for Linux x86_64 using Zig cross-compiler
release-linux:
	cargo zigbuild --release --target $(LINUX_TARGET)
	@echo ""
	@echo "Linux x86_64 release binary:"
	@ls -lh $(LINUX_OUT)/tunnix

# Build release binaries for both macOS (native) and Linux (zigbuild)
release-all: release release-linux
	@echo ""
	@echo "All platform binaries built."

# Run all tests
test:
	cargo test --all

# Test crypto module
test-crypto:
	cargo test --package tunnix-common crypto -- --nocapture

# Run server (debug)
run-server:
	@echo "Starting server on 127.0.0.1:8080"
	cargo run --bin tunnix -- server \
		--log-level debug

# Run client (debug)
run-client:
	@echo "Starting client"
	cargo run --bin tunnix -- client \
		--log-level debug

# Run client in background with nohup
run-client-bg:
	@echo "Stopping any existing client..."
	@pkill -x tunnix 2>/dev/null || true
	@echo "Starting client in background..."
	nohup tunnix client > /dev/null 2>&1 &
	@echo "Client started in background"

# Clean build artifacts
clean:
	cargo clean

# Format code
fmt:
	cargo fmt --all

# Run clippy
clippy:
	cargo clippy --all -- -D warnings

# Check everything
check: fmt clippy test
	@echo "All checks passed!"

# Install binary to ~/.cargo/bin
install: release
	cargo install --path tunnix
	@echo "Installed to ~/.cargo/bin/"
