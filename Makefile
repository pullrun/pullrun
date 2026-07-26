.PHONY: all build test clean proto check fmt lint run help

# Default target
all: build

BIN_DIR = $(CURDIR)/bin

# Build everything
build: build-rust build-go

# Rust build targets
build-rust:
	@echo "=== Building Rust workspace ==="
	cargo build --workspace
	@mkdir -p $(BIN_DIR)
	cp target/debug/pullrun-runtime $(BIN_DIR)/pullrun-runtime

build-rust-release:
	cargo build --workspace --release

build-go:
	@echo "=== Building Go modules ==="
	cd cli/pullrun && go build -o $(BIN_DIR)/pullrun .
	cd cri/pullrun-cri && go build -o $(BIN_DIR)/pullrun-cri .
	cd cmd/pullrun-compose && go build -o $(BIN_DIR)/pullrun-compose .
	cd control-plane/api/cmd && go build -o $(BIN_DIR)/control-plane .

# Test
test: test-rust test-go

test-rust:
	@echo "=== Testing Rust workspace ==="
	cargo test --workspace

test-rust-include-integration:
	cargo test --workspace -- --include-ignored

test-go:
	@echo "=== Testing Go modules ==="
	cd cli/pullrun && go test ./...
	cd cri/pullrun-cri && go test ./...
	cd cmd/pullrun-compose && go test ./...
	cd control-plane/api/cmd && go test ./...

# Check
check: fmt lint test

fmt:
	cargo fmt --all -- --check
	cd cli/pullrun && go fmt ./...
	cd cri/pullrun-cri && go fmt ./...
	cd cmd/pullrun-compose && go fmt ./...
	cd control-plane/api/cmd && go fmt ./...

lint: lint-rust lint-go

lint-rust:
	cargo clippy --workspace -- -D warnings

lint-go:
	@echo "=== Running golangci-lint ==="
	cd cli/pullrun && golangci-lint run ./...
	cd cri/pullrun-cri && golangci-lint run ./...
	cd cmd/pullrun-compose && golangci-lint run ./...
	cd control-plane/api/cmd && golangci-lint run ./...

# Proto generation
proto:
	@echo "=== Generating protobuf code (Go) ==="
	@mkdir -p proto-go
	# Clean stale generated files
	@rm -rf proto-go/pullrun
	# Rust proto: handled by `tonic-build` in runtime/pullrun-runtime/build.rs
	# at compile time, so we don't run protoc --tonic_out here. Running it
	# manually requires `protoc-gen-tonic` from cargo, which emits files in
	# a different (nested) layout that conflicts with the build.rs version.
	# Go proto (grpc) — single shared module at proto-go/; all three Go binaries
	# import it via `replace pullrun/protoapi => ../proto-go` in their go.mod.
	protoc \
		--proto_path=proto \
		--go_out=proto-go \
		--go_opt=paths=import \
		--go_opt=Mruntime.proto=pullrun/protoapi/pullrun/runtime \
		--go_opt=Mcontrol.proto=pullrun/protoapi/pullrun/control \
		--go-grpc_out=proto-go \
		--go-grpc_opt=paths=import \
		--go-grpc_opt=Mruntime.proto=pullrun/protoapi/pullrun/runtime \
		--go-grpc_opt=Mcontrol.proto=pullrun/protoapi/pullrun/control \
		proto/pullrun/runtime.proto proto/pullrun/control.proto
	# Flatten: move generated files from <import-path-mirror> to <subdir>
	@cd proto-go && \
		mkdir -p pullrun/runtime pullrun/control && \
		mv pullrun/protoapi/pullrun/runtime/* pullrun/runtime/ 2>/dev/null && \
		mv pullrun/protoapi/pullrun/control/* pullrun/control/ 2>/dev/null && \
		rm -rf pullrun/protoapi
	@echo "=== Proto generation complete (Go only; Rust via tonic-build) ==="

# Check if protoc is available
check-protoc:
	@which protoc >/dev/null 2>&1 || (echo "protoc not found. Install: brew install protobuf" && exit 1)
	protoc --version

# Install Go proto plugins
install-go-proto:
	go install google.golang.org/protobuf/cmd/protoc-gen-go@latest
	go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@latest

# Install Rust proto plugin
install-rust-proto:
	cargo install --locked protoc-gen-tonic

# Clean
clean:
	cargo clean
	rm -rf bin/
	rm -rf cli/pullrun/proto/

# Run the runtime
run-runtime:
	cargo run -p pullrun-runtime -- daemon --socket /tmp/pullrun.sock

# Run the CLI
run-cli:
	cd cli/pullrun && go run . $(ARGS)

# Install binaries
install:
	cargo install --path runtime/pullrun-runtime
	cd cli/pullrun && go install ./...

# Install a Linux kernel image for VM-backed workloads.
# Default: Kata Containers' static-linked vmlinux.container
# (the same default Apple uses for `container` on macOS).
# Override KATA_VERSION to pin a different release.
KATA_VERSION ?= 3.31.0
KATA_ARCH ?= arm64
PULLRUN_KERNEL_DIR ?= $(HOME)/.pullrun/kernels
# PATH is exported because the `tar --use-compress-program`
# plugin may resolve `zstd` from /opt/homebrew/bin on macOS,
# which is not always in the default PATH for `make`.
export PATH := /opt/homebrew/bin:/usr/local/bin:$(PATH)
install-kernel:
	@mkdir -p $(PULLRUN_KERNEL_DIR)
	@echo "Downloading Kata Containers $(KATA_VERSION) ($(KATA_ARCH))..."
	cd $(PULLRUN_KERNEL_DIR) && \
		curl -fL --retry 3 -o kata-static.tar.zst \
		https://github.com/kata-containers/kata-containers/releases/download/$(KATA_VERSION)/kata-static-$(KATA_VERSION)-$(KATA_ARCH).tar.zst && \
		which zstd >/dev/null || (echo "zstd not in PATH; install with: brew install zstd" && exit 1) && \
		tar --use-compress-program=zstd -xf kata-static.tar.zst opt/kata/share/kata-containers/vmlinux.container && \
		mv opt/kata/share/kata-containers/vmlinux.container vmlinux-$(KATA_VERSION) && \
		rm -rf opt kata-static.tar.zst
	@echo "Installed: $(PULLRUN_KERNEL_DIR)/vmlinux-$(KATA_VERSION)"
	@echo "Set PULLRUN_KERNEL_PATH to use it:"
	@echo "  export PULLRUN_KERNEL_PATH=$(PULLRUN_KERNEL_DIR)/vmlinux-$(KATA_VERSION)"

# Sign the apple-virt-smoke binary with the
# com.apple.security.virtualization entitlement so the
# Apple Virtualization framework will let it create VMs.
# Required on macOS hosts; on Linux the smoke binary
# returns BackendUnavailable.
APPLE_ENTITLEMENTS = $(CURDIR)/tools/apple-virt-smoke/virt.entitlements
APPLE_VIRT_SMOKE = $(CURDIR)/tools/apple-virt-smoke/target/debug/apple-virt-smoke
apple-sign-smoke:
	@if [ "$$(uname -s)" != "Darwin" ]; then \
		echo "apple-sign-smoke only runs on macOS"; \
		exit 0; \
	fi
	codesign --force --sign - \
		--entitlements $(APPLE_ENTITLEMENTS) \
		--options runtime \
		$(APPLE_VIRT_SMOKE)
	@echo "Signed $(APPLE_VIRT_SMOKE) with com.apple.security.virtualization"

# Build the smoke binary AND sign it (macOS only).
apple-smoke-signed: apple-sign-smoke

# Build the smoke binary (its own sub-workspace).
build-apple-smoke:
	cd tools/apple-virt-smoke && cargo build

PULLRUN_RUNTIME = $(CURDIR)/target/debug/pullrun-runtime

build-initramfs:
	@echo "=== Building initramfs (busybox + pullrun-init) ==="
	@if [ ! -f /tmp/busybox-aarch64 ]; then \
		echo "Downloading busybox static binary (Alpine aarch64)..."; \
		curl -fL -o /tmp/busybox-static-aarch64.apk \
			https://dl-cdn.alpinelinux.org/alpine/latest-stable/main/aarch64/busybox-static-1.37.0-r31.apk; \
		cd /tmp && rm -rf busybox-extract && mkdir busybox-extract; \
		cd /tmp/busybox-extract && tar -xzf /tmp/busybox-static-aarch64.apk; \
		cp /tmp/busybox-extract/bin/busybox.static /tmp/busybox-aarch64; \
		rm -rf /tmp/busybox-extract /tmp/busybox-static-aarch64.apk; \
		chmod +x /tmp/busybox-aarch64; \
		echo "Downloaded: $$(/tmp/busybox-aarch64 --help 2>&1 | head -1)"; \
	fi
	@mkdir -p $(HOME)/.pullrun/initramfs
	cargo build -p pullrun-init --target aarch64-unknown-linux-musl --release
	cd tools/build-initramfs && cargo build
	./tools/build-initramfs/target/debug/build-initramfs \
		--busybox /tmp/busybox-aarch64 \
		--pullrun-init $(CURDIR)/target/aarch64-unknown-linux-musl/release/pullrun-init \
		--out $(HOME)/.pullrun/initramfs/pullrun-initramfs.cpio.gz
	@echo "Initramfs built: $(HOME)/.pullrun/initramfs/pullrun-initramfs.cpio.gz"
apple-sign-daemon:
	@if [ "$$(uname -s)" != "Darwin" ]; then \
		echo "apple-sign-daemon only runs on macOS"; \
		exit 0; \
	fi
	codesign --force --sign - \
		--entitlements $(APPLE_ENTITLEMENTS) \
		--options runtime \
		$(PULLRUN_RUNTIME)
	@if [ -f "$(BIN_DIR)/pullrun-runtime" ]; then \
		codesign --force --sign - \
			--entitlements $(APPLE_ENTITLEMENTS) \
			--options runtime \
			$(BIN_DIR)/pullrun-runtime; \
		echo "Signed $(BIN_DIR)/pullrun-runtime"; \
	fi
	@echo "Signed $(PULLRUN_RUNTIME) with com.apple.security.virtualization"

# Help
help:
	@echo "Pullrun Build System"
	@echo ""
	@echo "Targets:"
	@echo "  all                  Build everything (default)"
	@echo "  build                Build Rust workspace + all Go modules"
	@echo "  test                 Run all tests"
	@echo "  check                Format + lint + test"
	@echo "  lint-go              Run golangci-lint on all Go modules"
	@echo "  proto                Generate protobuf code"
	@echo "  clean                Remove build artifacts"
	@echo "  run-runtime          Start pullrun-runtime daemon"
	@echo "  run-cli              Run pullrun (set ARGS for command)"
	@echo "  install              Install binaries to PATH"
	@echo "  install-kernel       Download Kata's vmlinux.container to ~/.pullrun/kernels"
	@echo "  build-initramfs      Build initramfs (busybox + pullrun-init) to ~/.pullrun/initramfs"
	@echo "  apple-sign-smoke     Sign tools/apple-virt-smoke with the virt entitlement"
	@echo "  apple-sign-daemon    Sign pullrun-runtime with the virt entitlement (macOS VM)"