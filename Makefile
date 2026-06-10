.PHONY: all build test clean proto check fmt lint run help

# Default target
all: build

# Build everything
build: build-rust build-go

# Rust build targets
build-rust:
	@echo "=== Building Rust workspace ==="
	cargo build --workspace

build-rust-release:
	cargo build --workspace --release

build-go:
	@echo "=== Building Go CLI ==="
	cd cli/nimbusctl && go build -o ../../bin/nimbusctl ./...

# Test
test: test-rust test-go

test-rust:
	@echo "=== Testing Rust workspace ==="
	cargo test --workspace

test-rust-include-integration:
	cargo test --workspace -- --include-ignored

test-go:
	@echo "=== Testing Go CLI ==="
	cd cli/nimbusctl && go test ./...

# Check
check: fmt lint test

fmt:
	cargo fmt --all -- --check
	cd cli/nimbusctl && go fmt ./...

lint:
	cargo clippy --workspace -- -D warnings

# Proto generation
proto:
	@echo "=== Generating protobuf code (Go) ==="
	@mkdir -p proto-go
	# Clean stale generated files
	@rm -rf proto-go/nimbus
	# Rust proto: handled by `tonic-build` in runtime/nimbus-runtime/build.rs
	# at compile time, so we don't run protoc --tonic_out here. Running it
	# manually requires `protoc-gen-tonic` from cargo, which emits files in
	# a different (nested) layout that conflicts with the build.rs version.
	# Go proto (grpc) — single shared module at proto-go/; all three Go binaries
	# import it via `replace nimbus/protoapi => ../proto-go` in their go.mod.
	protoc \
		--proto_path=proto \
		--go_out=proto-go \
		--go_opt=paths=import \
		--go_opt=Mruntime.proto=nimbus/protoapi/nimbus/runtime \
		--go_opt=Mcontrol.proto=nimbus/protoapi/nimbus/control \
		--go-grpc_out=proto-go \
		--go-grpc_opt=paths=import \
		--go-grpc_opt=Mruntime.proto=nimbus/protoapi/nimbus/runtime \
		--go-grpc_opt=Mcontrol.proto=nimbus/protoapi/nimbus/control \
		proto/nimbus/runtime.proto proto/nimbus/control.proto
	# Flatten: move generated files from <import-path-mirror> to <subdir>
	@cd proto-go && \
		mkdir -p nimbus/runtime nimbus/control && \
		mv nimbus/protoapi/nimbus/runtime/* nimbus/runtime/ 2>/dev/null && \
		mv nimbus/protoapi/nimbus/control/* nimbus/control/ 2>/dev/null && \
		rm -rf nimbus/protoapi
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
	rm -rf cli/nimbusctl/proto/

# Run the runtime
run-runtime:
	cargo run -p nimbus-runtime -- daemon --socket /tmp/nimbus.sock

# Run the CLI
run-cli:
	cd cli/nimbusctl && go run . $(ARGS)

# Install binaries
install:
	cargo install --path runtime/nimbus-runtime
	cd cli/nimbusctl && go install ./...

# Install a Linux kernel image for VM-backed workloads.
# Default: Kata Containers' static-linked vmlinux.container
# (the same default Apple uses for `container` on macOS).
# Override KATA_VERSION to pin a different release.
KATA_VERSION ?= 3.31.0
KATA_ARCH ?= arm64
NIMBUS_KERNEL_DIR ?= $(HOME)/.nimbus/kernels
# PATH is exported because the `tar --use-compress-program`
# plugin may resolve `zstd` from /opt/homebrew/bin on macOS,
# which is not always in the default PATH for `make`.
export PATH := /opt/homebrew/bin:/usr/local/bin:$(PATH)
install-kernel:
	@mkdir -p $(NIMBUS_KERNEL_DIR)
	@echo "Downloading Kata Containers $(KATA_VERSION) ($(KATA_ARCH))..."
	cd $(NIMBUS_KERNEL_DIR) && \
		curl -fL --retry 3 -o kata-static.tar.zst \
		https://github.com/kata-containers/kata-containers/releases/download/$(KATA_VERSION)/kata-static-$(KATA_VERSION)-$(KATA_ARCH).tar.zst && \
		which zstd >/dev/null || (echo "zstd not in PATH; install with: brew install zstd" && exit 1) && \
		tar --use-compress-program=zstd -xf kata-static.tar.zst opt/kata/share/kata-containers/vmlinux.container && \
		mv opt/kata/share/kata-containers/vmlinux.container vmlinux-$(KATA_VERSION) && \
		rm -rf opt kata-static.tar.zst
	@echo "Installed: $(NIMBUS_KERNEL_DIR)/vmlinux-$(KATA_VERSION)"
	@echo "Set NIMBUS_KERNEL_PATH to use it:"
	@echo "  export NIMBUS_KERNEL_PATH=$(NIMBUS_KERNEL_DIR)/vmlinux-$(KATA_VERSION)"

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

NIMBUS_RUNTIME = $(CURDIR)/target/debug/nimbus-runtime

build-initramfs:
	@echo "=== Building initramfs (busybox + nimbus-init) ==="
	@if [ ! -f /tmp/busybox-aarch64 ]; then \
		echo "Downloading busybox static binary..."; \
		curl -fL -o /tmp/busybox-aarch64 \
			https://busybox.net/downloads/binaries/1.35.0-aarch64-linux-musl/busybox; \
		chmod +x /tmp/busybox-aarch64; \
	fi
	@mkdir -p $(HOME)/.nimbus/initramfs
	cargo build -p nimbus-init --target aarch64-unknown-linux-musl --release
	cd tools/build-initramfs && cargo build
	./tools/build-initramfs/target/debug/build-initramfs \
		--busybox /tmp/busybox-aarch64 \
		--nimbus-init $(CURDIR)/target/aarch64-unknown-linux-musl/release/nimbus-init \
		--out $(HOME)/.nimbus/initramfs/nimbus-initramfs.cpio.gz
	@echo "Initramfs built: $(HOME)/.nimbus/initramfs/nimbus-initramfs.cpio.gz"
apple-sign-daemon:
	@if [ "$$(uname -s)" != "Darwin" ]; then \
		echo "apple-sign-daemon only runs on macOS"; \
		exit 0; \
	fi
	codesign --force --sign - \
		--entitlements $(APPLE_ENTITLEMENTS) \
		--options runtime \
		$(NIMBUS_RUNTIME)
	@echo "Signed $(NIMBUS_RUNTIME) with com.apple.security.virtualization"

# Help
help:
	@echo "Nimbus Build System"
	@echo ""
	@echo "Targets:"
	@echo "  all                  Build everything (default)"
	@echo "  build                Build Rust workspace + Go CLI"
	@echo "  test                 Run all tests"
	@echo "  check                Format + lint + test"
	@echo "  proto                Generate protobuf code"
	@echo "  clean                Remove build artifacts"
	@echo "  run-runtime          Start nimbus-runtime daemon"
	@echo "  run-cli              Run nimbusctl (set ARGS for command)"
	@echo "  install              Install binaries to PATH"
	@echo "  install-kernel       Download Kata's vmlinux.container to ~/.nimbus/kernels"
	@echo "  build-initramfs      Build initramfs (busybox + nimbus-init) to ~/.nimbus/initramfs"
	@echo "  apple-sign-smoke     Sign tools/apple-virt-smoke with the virt entitlement"
	@echo "  apple-sign-daemon    Sign nimbus-runtime with the virt entitlement (macOS VM)"