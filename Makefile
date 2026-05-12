.PHONY: build test clean run-server run-client run-client-bg release release-linux release-all help bump-patch bump-minor bump-major update-formula

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
	zip -j target/release/tunnix_macos_arm64.zip target/release/tunnix
	zip -j $(LINUX_OUT)/tunnix_linux_x86_64.zip $(LINUX_OUT)/tunnix
	@echo ""
	@echo "All platform zips ready:"
	@echo "  target/release/tunnix_macos_arm64.zip"
	@echo "  $(LINUX_OUT)/tunnix_linux_x86_64.zip"

# Run all tests
test:
	cargo test --all

# Test crypto module
test-crypto:
	cargo test crypto -- --nocapture

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

# Run client in background with nohup, logging to tunnix.log
run-client-bg:
	@echo "Stopping any existing client..."
	@pkill -x tunnix 2>/dev/null || true
	@if [ -f tunnix.log ]; then mv tunnix.log tunnix.prev.log; fi
	@echo "Starting client in background, logs -> tunnix.log"
	RUST_LOG=tunnix=debug nohup tunnix client > tunnix.log 2>&1 &
	@echo "Client started in background (tail -f tunnix.log to watch)"

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
	cargo install --path .
	@echo "Installed to ~/.cargo/bin/"

## Bump the patch version (0.1.3 → 0.1.4) and update all version references
bump-patch:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	minor=$$(echo $$old | cut -d. -f2); \
	patch=$$(echo $$old | cut -d. -f3); \
	new="$$major.$$minor.$$((patch+1))"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/tunnix.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/tunnix.rb; \
	echo "$$old → $$new"

## Bump the minor version (0.1.4 → 0.2.0) and update all version references
bump-minor:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	minor=$$(echo $$old | cut -d. -f2); \
	new="$$major.$$((minor+1)).0"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/tunnix.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/tunnix.rb; \
	echo "$$old → $$new"

## Bump the major version (0.1.4 → 1.0.0) and update all version references
bump-major:
	@old=$$(grep '^version' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/'); \
	major=$$(echo $$old | cut -d. -f1); \
	new="$$((major+1)).0.0"; \
	sed -i '' "s/^version = \"$$old\"/version = \"$$new\"/" Cargo.toml; \
	sed -i '' "s/version \"$$old\"/version \"$$new\"/" Formula/tunnix.rb; \
	sed -i '' "s|/$$old/|/$$new/|g" Formula/tunnix.rb; \
	echo "$$old → $$new"

## Update Formula/tunnix.rb SHA256s from local release zips (run after release-all, before upload)
##   make update-formula
update-formula:
	@mac_zip="target/release/tunnix_macos_arm64.zip"; \
	linux_zip="$(LINUX_OUT)/tunnix_linux_x86_64.zip"; \
	echo "Computing SHA256 …"; \
	mac_sha=$$(shasum -a 256 "$$mac_zip" | cut -d' ' -f1); \
	linux_sha=$$(shasum -a 256 "$$linux_zip" | cut -d' ' -f1); \
	echo "macOS SHA256: $$mac_sha"; \
	echo "Linux  SHA256: $$linux_sha"; \
	sed -i '' "/on_macos/,/on_linux/ s/sha256 \"[a-f0-9]*\"/sha256 \"$$mac_sha\"/" Formula/tunnix.rb; \
	sed -i '' "/on_linux/,/def install/ s/sha256 \"[a-f0-9]*\"/sha256 \"$$linux_sha\"/" Formula/tunnix.rb; \
	echo "Formula/tunnix.rb updated"
