# Nimbus User Guide

Nimbus is a content-addressed workload execution system. It pulls OCI
images, deduplicates them into a zero-copy on-disk DAG (rkyv + memmap2),
and runs them in **containers (runc), Firecracker microVMs (KVM), or
Apple Virtualization VMs (macOS)** — all from the same image.

## Platform capabilities

| Feature | macOS (Apple Silicon) | Linux (x86_64 / arm64) |
|---------|----------------------|------------------------|
| OCI pull/push into DAG store | ✅ via nimbusctl | ✅ via nimbusctl |
| Container backend (runc) | ❌ (runc is Linux-only) | ✅ `--backend container` |
| Rootless container backend | ❌ (runc + pasta) | ✅ `--backend container` (EUID != 0) |
| Firecracker VM backend | ❌ (needs KVM) | ✅ `--backend vm` |
| Apple Virt VMs via nimbusctl | ✅ (with entitlement, kernel in ~/.nimbus/kernels/) | ❌ |
| Apple Virt VM standalone tools | ✅ (with entitlement) | ❌ |
| nimbusctl CLI | ✅ | ✅ |
| nimbus-runtime daemon | ✅ (stores, sync, policy) | ✅ (all backends) |
| P2P DAG block sync | ✅ | ✅ |
| Policy engine (cosign, SBOM) | ✅ | ✅ |
| CRI shim + Kubernetes | ❌ (needs runc) | ✅ |
| `nimbusctl kernel install` | ✅ → arm64 | ✅ → amd64 |

**Key insight:** nimbusctl works identically on both platforms for all
store/sync/policy operations. `nimbusctl run --backend vm` works on
both macOS (Apple Virtualization) and Linux (Firecracker/KVM). On macOS
the daemon discovers the kernel from `~/.nimbus/kernels/` automatically;
no `--kernel-image` flag needed. Standalone tools (`apple-virt-smoke`,
`apple-virt-exec`) are also available for direct FFI testing.

## Quick start

```bash
# 1. Build nimbusctl (once, from any platform)
cd cli/nimbusctl && go build -o ../../bin/nimbusctl .

# The binary is at bin/nimbusctl. Add it to your PATH.

# 2. Pull an image (deduplicates into the on-disk DAG store).
nimbusctl pull alpine:3.18

# 3. Run as container (Linux) or use the standalone tools (macOS):
#    Linux:  nimbusctl run sha256:<digest> --backend container --cmd echo hello
#    macOS:  see "Apple VM backend" section below

# 4. Pull for a different architecture (multi-arch image indexes):
nimbusctl pull alpine:3.18 --platform linux/arm64
```

The `--direct` mode (default: true) spawns the nimbus-runtime daemon as
a child process automatically, so you don't need to start it manually.
To talk to an existing daemon, use `--socket /tmp/nimbus.sock` or
`--server host:port`.

## Architecture

```
                         ┌──────────────────────────────┐
                         │       nimbusctl (Go)         │
                         │  pull · run · inspect · build│
                         │  compose · stats · cp · exec │
                         │  secret · config · network   │
                         └──────────┬───────────────────┘
                                    │ gRPC (UDS or TCP)
                                    ▼
   ┌──────────────────────────────────────────────────────────────┐
   │              nimbus-runtime  (spawned by --direct)           │
   │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌───────────┐   │
   │  │  Pull    │  │  Run     │  │  Update  │  │  CopyFile │   │
   │  │ + policy │→ │ + policy │→ │ + stats  │→ │  RPC      │   │
   │  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬──────┘   │
   │       │              │              │             │          │
   │       ▼              ▼              ▼             ▼          │
   │   ┌──────────────────────────────────────────────────────┐   │
   │   │  MmapStore (rkyv + memmap2)                          │   │
   │   │  ┌─────────┐ ┌───────┐ ┌────────────────────────┐   │   │
   │   │  │Manifest │→│ Tree  │→│ Layer / Blob DAG + Cache│   │   │
   │   │  └─────────┘ └───────┘ └────────────────────────┘   │   │
   │   └──────────────────────────────────────────────────────┘   │
   │       │              │              │                        │
   │       ▼              ▼              ▼                        │
   │   ┌──────────┐  ┌──────────┐  ┌───────────────────────┐   │
   │   │LinuxCont.│  │Firecrack.│  │Health check watcher    │   │
   │   │(runc)    │  │(KVM)     │  │exec + state machine    │   │
   │   └────┬─────┘  └────┬─────┘  └───────────────────────┘   │
   └───────┼──────────────┼──────────────────────────────────────┘
           │              │
           ▼              ▼
   ┌───────────────────────────────────────────────────────────┐
   │  ProxyNetwork (10.42.0.0/16)                              │
   │  userspace TCP/UDP proxy + DNS + IPAM + iptables          │
   └───────────────────────────────────────────────────────────┘
```

## Repository layout

```
proto/               Protobuf definitions (single source of truth)
proto-go/            Generated Go protobuf code
runtime/             Rust workspace: store, oci, net, policy, exec, vm, sync, runtime
cli/nimbusctl/       Go CLI (cobra) — full Docker-like interface
cri/nimbus-cri/      Go CRI shim (Kubernetes integration)
control-plane/       Node registry stub
deploy/              Kubernetes manifests, Grafana dashboard
tools/               Standalone smoke-test binaries
├── apple-virt-smoke/    Apple Virt FFI pool test (macOS only)
├── apple-virt-exec/     Apple Virt VM workload tool (macOS only)
├── build-initramfs/     Initramfs builder for microVM guests
├── build-kernel-image/  Kernel OCI image builder
├── firecracker-smoke/   Firecracker boot test (Linux only)
├── vm-network-smoke/    VM outbound networking test
└── vm-outbound-smoke/   VM outbound connectivity test
docs/                Architecture, operations, policy docs
blobs/sha256/        Real OCI blob fixtures for testing
```

## Building from source

### Rust workspace (all crates)

```bash
cargo build --workspace
```

This builds `nimbus-runtime` (the gRPC daemon) and all library crates.
For a release build: `cargo build --workspace --release`.

The release binary is at `target/release/nimbus-runtime`.

### Go CLI (nimbusctl)

```bash
make build-go
# or directly:
cd cli/nimbusctl && go build -o ../../bin/nimbusctl .
```

The binary is at `bin/nimbusctl`. It communicates with the runtime over
a Unix domain socket (`--direct` mode auto-spawns the daemon).

### Cross-compilation

**Go** — trivially cross-compilable:
```bash
cd cli/nimbusctl && GOOS=linux GOARCH=amd64 go build -o nimbusctl-linux .
# The result is a statically-linked ELF binary.
```

**Rust** — cross-compile for Linux from macOS (needs target + linker):
```bash
# Install Linux targets
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# Build nimbus-init (static musl, no C deps):
cargo build -p nimbus-init --target x86_64-unknown-linux-musl --release

# Build nimbus-runtime for x86_64 Linux:
# (requires a cross-linker like `brew install filosottile/musl-cross/musl-cross`)
cargo build --target x86_64-unknown-linux-musl --release
```

### Standalone tools (sub-workspaces)

Each tool under `tools/` is a self-contained Cargo workspace (has its
own `target/`). Build individually:

```bash
cd tools/apple-virt-smoke    && cargo build   # macOS only
cd tools/apple-virt-exec     && cargo build   # macOS only
cd tools/build-initramfs     && cargo build   # any platform
cd tools/firecracker-smoke   && cargo build   # Linux only
```

## Running the runtime daemon

Normally you don't need to start the daemon manually — `nimbusctl` uses
`--direct` mode (default) which spawns it as a child process. Start it
explicitly when you need a long-lived daemon (e.g. for Prometheus):

```bash
# Minimal (auto-spawned by nimbusctl --direct)
nimbusctl pull alpine:3.18

# Long-lived daemon for production
nimbus-runtime daemon --socket /tmp/nimbus.sock

# With Prometheus metrics
nimbus-runtime daemon --metrics-addr 0.0.0.0:9090

# With VM backend (Linux — needs firecracker + kernel)
nimbus-runtime daemon \
    --vm-firecracker /usr/local/bin/firecracker \
    --vm-kernel ~/.nimbus/kernels/vmlinux-3.31.0

# With peer-to-peer block sync
nimbus-runtime daemon --sync-addr 0.0.0.0:9500

# With policy enforcement
nimbus-runtime daemon --require-signature --max-cvss 7.0
```

> **SSH tip:** When starting via SSH, use `nohup` and export PATH:
> ```bash
> ssh host 'export PATH="$HOME/.cargo/bin:$PATH"; nohup nimbus-runtime daemon > /tmp/daemon.log 2>&1 &'
> ```

### All daemon flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--socket` | `/tmp/nimbus.sock` | gRPC UDS path |
| `--store-root` | `/var/lib/nimbus` | DAG store directory |
| `--metrics-addr` | (none) | Prometheus `/metrics` HTTP (no-value = `127.0.0.1:9090`) |
| `--require-signature` | false | Reject unsigned images |
| `--require-sbom` | false | Reject images without CycloneDX SBOM |
| `--max-cvss` | (none) | Reject images above this CVSS score |
| `--readonly-rootfs` | false | Declare rootfs must be read-only |
| `--no-new-privileges` | false | Forbid privilege escalation |
| `--deny-license` | (repeatable) | Banned SPDX license identifiers |
| `--trusted-key` | (repeatable) | Trusted cosign public key (`id:base64`) |
| `--insecure-registry` | (repeatable) | Plain-HTTP registries (e.g. `localhost:5000`) |
| `--vm-firecracker` | (none) | Path to firecracker binary (enables VM backend) |
| `--vm-kernel` | (none) | Path to vmlinux kernel for VMs |
| `--vm-root` | `/var/lib/nimbus/vm` | VM ext4 rootfs + sidecar directory |
| `--vm-vcpus` | 2 | vCPUs per VM |
| `--vm-mem-mib` | 512 | Memory per VM (MiB) |
| `--vm-size-mb` | 256 | Rootfs size per VM (MiB) |
| `--sync-addr` | `0.0.0.0:0` | BlockSync gRPC (port 0 = disabled) |
| `--registrar-addr` | (none) | Host Registrar gRPC service |
| `--registrar-connect` | (none) | Connect to remote Registrar |

## nimbusctl — full CLI reference

The nimbusctl CLI is the main user interface, modelled after Docker.
It auto-spawns the runtime daemon (`--direct` mode) so you don't need
to manage it separately.

### Image management

```bash
# Pull an image from any OCI registry
nimbusctl pull alpine:3.18
nimbusctl pull alpine:3.18 --platform linux/arm64   # multi-arch
nimbusctl pull localhost:5000/myimg:latest           # private registry

# Push a DAG image to an OCI registry
nimbusctl push <root-digest> --registry ghcr.io/myorg/myimg:latest

# Native DAG-aware build (Dockerfile, no Docker needed)
nimbusctl build -t myapp:latest .
nimbusctl build -t myapp:latest --platform linux/arm64 .

# Export / import DAG images
nimbusctl save <digest> -o myimage.tar
nimbusctl load -i myimage.tar

# List images in the store
nimbusctl list

# Login / logout from registries
nimbusctl login docker.io
nimbusctl logout
```

### Running workloads

```bash
# Run a container (Linux)
nimbusctl run alpine:3.18 --backend container --cmd echo hello
nimbusctl run sha256:<digest> --backend container --cmd sleep --cmd 60

# Run a VM — works on macOS (Apple Virt) and Linux (Firecracker/KVM)
# On macOS, no --kernel-image needed: kernel auto-discovered from ~/.nimbus/kernels/
nimbusctl run alpine:3.18 --backend vm --cmd echo hello
nimbusctl run alpine:3.18 --backend vm --cmd /bin/echo hello --attach  # single-step

# With environment variables
nimbusctl run alpine:3.18 -e FOO=bar -e BAZ=qux --cmd printenv

# With resource limits
nimbusctl run alpine:3.18 --memory 268435456 --cpu 500 --cmd stress

# With published ports
nimbusctl run nginx:alpine -p 8080:80 --cmd nginx -g 'daemon off;'

# With restart policy
nimbusctl run alpine:3.18 --restart on-failure --cmd /bin/myapp

# With health check
nimbusctl run alpine:3.18 --health-cmd 'curl -f http://localhost:80'

# Run and attach (single step — blocks until exit, propagates exit code)
nimbusctl run alpine:3.18 --cmd /bin/echo --cmd 'hello from attach' --attach
nimbusctl run alpine:3.18 --cmd /bin/sleep --cmd 30 -a               # -a short flag
```

### Managing running workloads

```bash
# List running workloads
nimbusctl list

# Get details
nimbusctl inspect <id>

# Stop a workload
nimbusctl stop <id>

# Live resource stats
nimbusctl stats <id>

# Update resource limits live
nimbusctl update <id> --cpu 2000 --memory 1073741824

# Execute a command in a running container
nimbusctl exec <id> /bin/cat /etc/alpine-release

# Stream logs
nimbusctl logs <id>

# Attach to a running workload (bidi stdio, vm backend)
nimbusctl workload run <id>

# Run and attach in one step (equivalent to 'docker run -it')
nimbusctl run alpine:3.18 --cmd /bin/echo --cmd hello --attach
nimbusctl run alpine:3.18 --cmd /bin/sleep --cmd 3600 -a          # -a short flag

# Copy files
nimbusctl cp <id>:/path/to/file ./local-file
nimbusctl cp ./local-file <id>:/path/to/file

# Commit running container as new image layer
nimbusctl commit <id> myapp:snapshot-1

# Show file changes vs original image
nimbusctl diff <id>

# Remove stopped workloads
nimbusctl prune -f
```

### Secrets and configs

```bash
# Secrets (AES-256-GCM encrypted)
nimbusctl secret create db_password secret data
nimbusctl secret ls
nimbusctl secret inspect db_password
nimbusctl secret rm db_password

# Configs (plain text)
nimbusctl config create app_config '{"debug": true}'
nimbusctl config ls
nimbusctl config inspect app_config
nimbusctl config rm app_config

# Mount in containers
nimbusctl run myapp:latest --secret db_password --config app_config
```

### Networking

```bash
# Create a user-defined bridge network
nimbusctl network create mynet

# List networks
nimbusctl network ls

# Remove a network
nimbusctl network rm mynet
```

### System

```bash
# Runtime info
nimbusctl info

# Client version
nimbusctl version

# Events stream
nimbusctl events

# Compose
nimbusctl compose up -f docker-compose.yml
nimbusctl compose down
nimbusctl compose ps
nimbusctl compose logs
```

### Kernel management

```bash
# Install a kernel for VM backends
nimbusctl kernel install                      # arm64 (macOS default)
nimbusctl kernel install --arch amd64         # x86_64 (Linux)
nimbusctl kernel install --version 3.31.0     # specific version

# Installed to ~/.nimbus/kernels/vmlinux-<version>
```

On macOS, the daemon automatically discovers the kernel from
`~/.nimbus/kernels/` (and initramfs from `~/.nimbus/initramfs/`),
so `--kernel-image` is not required. Override with the `NIMBUS_KERNEL_PATH`
or `NIMBUS_INITRAMFS_PATH` environment variables.

### Run flags (nimbusctl run)

| Flag | Purpose |
|------|---------|
| `--backend` | `container` (Linux), `vm` (macOS+AppleVirt / Linux+KVM), `sandbox` |
| `--cmd` | Command + args (repeatable, overrides entrypoint) |
| `-e` / `--env` | Environment variables (`KEY=VALUE`) |
| `--name` | Workload name (auto-generated if empty) |
| `--memory` | Memory limit in bytes (default 512MiB) |
| `--cpu` | CPU millicores (1000 = 1 vCPU) |
| `--net` | Network mode: `isolated`, `host`, `none` |
| `-p` / `--publish` | Publish port (`host:container` or just `port`) |
| `--allow-inbound` | Expose port (repeatable) |
| `--allow-outbound` | Allow outbound (`tcp:host:port`) |
| `--kernel-image` | OCI kernel image for VM backend (optional on macOS when `~/.nimbus/kernels/` has one) |
| `--registry` | Registry for pulling the workload image |
| `--platform` | Target platform for pull (`linux/amd64`, `linux/arm64`) |
| `--restart` | Restart policy: `no`, `on-failure`, `always`, `unless-stopped` |
| `--health-cmd` | Health check command |
| `--secret` | Mount a secret at `/run/secrets/<name>` |
| `--config` | Mount a config at `/<name>` |
| `-a` / `--attach` | Attach after start: streams stdout/stderr (vm) or waits for exit (container) |
| `-v` / `--volume` | Bind mount (`source:destination[:options]`) |
| `--direct` | Spawn runtime as child (default: true) |
| `--socket` | Runtime UDS socket path (default `/tmp/nimbus.sock`) |
| `--server` | Remote control plane address (disables `--direct`) |

## Container backend (Linux)

The default backend runs workloads via runc:

```bash
# Start daemon with container backend (auto via --direct)
nimbusctl pull alpine:3.18
nimbusctl run sha256:<digest> --backend container --cmd /bin/echo --cmd 'hello'

# Inspect after completion shows exit code
nimbusctl inspect <id>
# Exit Code: 0
```

### Container networking

By default, containers use `NetworkMode::Loopback` (isolated). For
published ports, `--publish` auto-promotes to bridge mode:

```bash
nimbusctl run nginx:alpine -p 8080:80 --cmd nginx -g 'daemon off;'
# Verify: curl http://localhost:8080
```

### Running detached and exec-ing

```bash
# Run a long-lived container (detached — returns immediately)
nimbusctl run alpine:3.18 --backend container --name myserver --cmd /bin/sleep --cmd 3600

# Exec into it while running
nimbusctl exec myserver /bin/cat /etc/alpine-release   # → 3.18.12
nimbusctl exec myserver /bin/printenv                   # env vars
nimbusctl exec myserver /bin/echo 'hello'               # stdout capture

# Or run and attach in one step (blocks until exit, like docker run -it)
nimbusctl run alpine:3.18 --cmd /bin/sleep --cmd 10 -a

# Stop it
nimbusctl stop myserver
```

## Firecracker VM backend (Linux)

Firecracker boots each workload in a KVM-based microVM with a
dedicated kernel and initramfs — stronger isolation than containers.

### Prerequisites

```bash
# 1. Install the x86_64 kata kernel
nimbusctl kernel install --arch amd64
# Installed to: ~/.nimbus/kernels/vmlinux-3.31.0 (ELF x86-64, ~40 MB)

# 2. Build nimbus-init (static musl binary)
cargo build -p nimbus-init --target x86_64-unknown-linux-musl --release

# 3. Build the initramfs builder
cd tools/build-initramfs && cargo build --release

# 4. Get a static busybox binary
curl -fL -o /tmp/busybox-x86_64 \
  https://busybox.net/downloads/binaries/1.35.0-x86_64-linux-musl/busybox
chmod +x /tmp/busybox-x86_64

# 5. Build the initramfs
mkdir -p ~/.nimbus/initramfs
./target/release/build-initramfs \
  --busybox /tmp/busybox-x86_64 \
  --nimbus-init ../../target/x86_64-unknown-linux-musl/release/nimbus-init \
  --out ~/.nimbus/initramfs/nimbus-initramfs.cpio.gz
# Output: 1.6 MB gzipped cpio archive at ~/.nimbus/initramfs/
```

### Start daemon with VM backend

```bash
nimbus-runtime daemon \
  --vm-firecracker /usr/local/bin/firecracker \
  --vm-kernel ~/.nimbus/kernels/vmlinux-3.31.0 \
  --vm-root /var/lib/nimbus/vm &
```

Or use `--direct` mode via nimbusctl (auto-spawns the daemon).

### Run a VM workload

```bash
nimbusctl run alpine:3.18 --backend vm --cmd /bin/echo --cmd 'hello from nimbus VM via firecracker!'
```

The workload command runs inside the VM via nimbus-init (PID 1).
Output is streamed back over vsock. The VM gets a TAP+bridge
network and IPAM allocation.

### What happens inside (VM boot sequence)

1. Firecracker boots the kernel with initramfs
2. Kernel mounts `/proc`, `/sys`, `/dev` and execs `/init`
3. `/init` is a shell script that execs `/sbin/nimbus-init`
4. `nimbus-init` connects to the host via vsock port 42
5. Host sends `WorkloadSpec` (command, env, cwd)
6. Guest spawns the workload, captures stdio
7. Frames shuttle between guest ↔ host over vsock
8. Guest sends `WorkloadExit { exit_code, signal }` when done

### Firecracker smoke test

```bash
cd tools/firecracker-smoke && cargo build
# Set env vars for the kernel path
NIMBUS_FC_VMLINUX=~/.nimbus/kernels/vmlinux-3.31.0 ./target/debug/firecracker-smoke
```

## Apple VM backend (macOS)

The Apple Virtualization framework allows running Linux VMs on Apple
Silicon macOS. The recommended way is `nimbusctl run --backend vm`,
which works identically to the Firecracker backend on Linux. Standalone
tools (`apple-virt-smoke`, `apple-virt-exec`) are also available for
direct FFI testing.

### Quickest start (one-shot VM workload)

```bash
# 1. Install kernel + initramfs (one-time setup)
make install-kernel
make build-initramfs          # builds busybox + nimbus-init for aarch64

# 2. Sign the daemon with the virtualization entitlement (one-time)
make apple-sign-daemon

# 3. Run a VM workload — no --kernel-image needed, discovered from ~/.nimbus/
nimbusctl run alpine:latest --backend vm --cmd /bin/echo "hello from nimbus VM via Apple Virt" --attach
```

### Setup (one-time)

#### 1. Kernel image

Download Kata Containers' arm64 kernel:

```bash
make install-kernel       # → ~/.nimbus/kernels/vmlinux-3.31.0 (ARM64)
# or: nimbusctl kernel install
```

The daemon looks for kernel images in `~/.nimbus/kernels/` (picks the
latest `vmlinux-*` file) or the `NIMBUS_KERNEL_PATH` env var.

#### 2. Initramfs with nimbus-init + busybox

```bash
# Build nimbus-init for arm64 musl
cargo build -p nimbus-init --target aarch64-unknown-linux-musl --release

# Build the initramfs builder
cd tools/build-initramfs && cargo build

# Get a static busybox binary
curl -fL -o /tmp/busybox-aarch64 \
  https://busybox.net/downloads/binaries/1.35.0-aarch64-linux-musl/busybox
chmod +x /tmp/busybox-aarch64

# Build the initramfs
mkdir -p ~/.nimbus/initramfs
./target/debug/build-initramfs \
  --busybox /tmp/busybox-aarch64 \
  --nimbus-init ../../target/aarch64-unknown-linux-musl/release/nimbus-init \
  --out ~/.nimbus/initramfs/nimbus-initramfs.cpio.gz
```

The initramfs layout:
```
/init                  → shell script that execs /sbin/nimbus-init
/sbin/nimbus-init      → the static nimbus-init binary
/bin/busybox           → busybox (shell, utilities)
/bin/{sh,cat,ls,...}   → symlinks to busybox applets
/dev/{console,null,tty}→ device nodes
/proc, /sys, /etc      → directories for mounting
```

#### 3. Code signing entitlement

The daemon process needs the `com.apple.security.virtualization`
entitlement to create VMs:

```bash
# One-time signing (re-sign after every `cargo build`)
make apple-sign-daemon
```

The entitlement XML used (`tools/apple-virt-smoke/virt.entitlements`):
```xml
<key>com.apple.security.virtualization</key>
<true/>
```

### Running VM workloads via nimbusctl

Once the kernel + initramfs are installed at `~/.nimbus/kernels/` and
`~/.nimbus/initramfs/`, the daemon picks them up automatically:

```bash
# Run and attach (single step — blocks until exit, streams output)
nimbusctl run alpine:latest --backend vm --cmd /bin/echo "hello from nimbus VM via Apple Virt" --attach

# Run with environment variables
nimbusctl run alpine:latest --backend vm -e FOO=bar --cmd /bin/printenv --attach

# Run detached, then attach separately
nimbusctl run alpine:latest --backend vm --name myvm --cmd /bin/sleep --cmd 60
nimbusctl workload run myvm          # attach to booted VM
```

The daemon discovers:
- Kernel from `~/.nimbus/kernels/` (latest `vmlinux-*` file) or `NIMBUS_KERNEL_PATH`
- Initramfs from `~/.nimbus/initramfs/nimbus-initramfs.cpio.gz` or `NIMBUS_INITRAMFS_PATH`

No `--kernel-image` flag is needed when these local paths are present.
To use an OCI-packaged kernel image from a registry instead, pass
`--kernel-image <ref>`.

### Standalone tools

For direct FFI testing without the daemon:

#### apple-virt-smoke (FFI pool test)

```bash
cd tools/apple-virt-smoke && cargo build
make apple-sign-smoke

./target/debug/apple-virt-smoke \
  --kernel ~/.nimbus/kernels/vmlinux-3.31.0 \
  --store /tmp/apple-virt-store
```

Success output:
```
INFO staging pre-built kernel path=~/.nimbus/kernels/vmlinux-3.31.0
INFO creating Apple Virt VM index=0 total=3
...
INFO PASS: Apple Virt FFI round-trip succeeded
```

#### apple-virt-exec (full VM workload)

```bash
cd tools/apple-virt-exec && cargo build

./target/debug/apple-virt-exec \
  --kernel ~/.nimbus/kernels/vmlinux-3.31.0 \
  --initramfs ~/.nimbus/initramfs/nimbus-initramfs.cpio.gz \
  --rootfs /tmp/nimbus-rootfs \
  --store /tmp/nimbus-store \
  --timeout 30 \
  -- /bin/echo 'hello from nimbus VM'
```

Output:
```
hello from nimbus VM!
INFO workload completed elapsed_ms=159 exit_code=0
```

#### apple-virt-exec flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--kernel` | (required) | Path to vmlinux kernel |
| `--kernel-image` | (mutually exclusive) | OCI reference for kernel image |
| `--initramfs` | (required with `--kernel`) | Path to initramfs cpio.gz |
| `--rootfs` | (required) | Host dir for VirtioFS share |
| `--store` | same as `--rootfs` | Second VirtioFS share tag `nimbus-store` |
| `--cpus` | 1 | vCPUs per VM |
| `--mem-mib` | 512 | Memory per VM |
| `--timeout` | 30 | Session timeout (seconds) |
| `--cwd` | `/` | Working directory inside guest |
| `--env` | (repeatable) | Environment variables |
| `--console-log` | `/tmp/nimbus-exec-console.log` | Guest serial console output |
| `-q` / `--quiet` | false | Suppress info output |

### Kernel details

The Kata Containers kernel at `~/.nimbus/kernels/vmlinux-3.31.0` is
downloaded via:
```bash
make install-kernel                  # arm64 (macOS)
nimbusctl kernel install --arch amd64  # x86_64 (Linux)
```

The source URL is:
```
https://github.com/kata-containers/kata-containers/releases/download/3.31.0/kata-static-3.31.0-{arch}.tar.zst
```

To verify:
```bash
file ~/.nimbus/kernels/vmlinux-3.31.0
# macOS: → Linux kernel ARM64 boot executable Image, little-endian
# Linux: → ELF 64-bit LSB executable, x86-64, statically linked
```

## Initramfs details

The initramfs is a `cpio` archive (newc format) compressed with gzip,
built by `tools/build-initramfs/src/main.rs` — no host `cpio` binary.

### Available busybox applets

```
cat, sh, mount, umount, ls, echo, env, true, false,
mkdir, rm, ln, cp, mv, ps, sleep, test, uname
```

These are symlinks to `/bin/busybox`. To inspect the initramfs:
```bash
mkdir -p /tmp/ramfs-inspect && cd /tmp/ramfs-inspect
gzip -dc ~/.nimbus/initramfs/nimbus-initramfs.cpio.gz | cpio -idm
ls -la bin/ sbin/ init
ls -la bin/ sbin/ init
```

## Testing

### Unit tests (cross-platform)

```bash
cargo test --workspace
```

Runs 118 unit tests across all Rust crates (nimbus-store, nimbus-sync,
nimbus-exec, nimbus-oci, nimbus-policy, nimbus-runtime, nimbus-vm,
nimbus-vsock, nimbus-net, nimbus-init, nimbus-dns).

### Including integration tests

```bash
cargo test --workspace -- --include-ignored
```

Discovers additional tests gated by `#[ignore]` (need real hardware,
KVM, or specific binaries).

### Go tests

```bash
cd cli/nimbusctl && go test ./...
```

9 Go tests for CLI flag parsing.

### End-to-end tests

| Test | Platform | Command |
|------|----------|---------|
| OCI pull round-trip | Both | `nimbusctl pull alpine:3.18` |
| Container run + exec + stop | Linux | `nimbusctl run --backend container --cmd echo hello` |
| nginx with port forwarding | Linux | `nimbusctl run nginx:alpine -p 8080:80` |
| Firecracker VM workload | Linux | `nimbusctl run --backend vm --cmd echo hello` |
| Apple VM FFI pool | macOS | `apple-virt-smoke --kernel ~/.nimbus/kernels/vmlinux-*` |
| Apple VM workload exec | macOS | `apple-virt-exec --kernel ... --initramfs ... -- /bin/echo hello` |
| Build from Dockerfile | Both | `nimbusctl build -t myapp:latest .` |
| Secret / Config lifecycle | Both | `nimbusctl secret create x y; nimbusctl secret rm x` |
| P2P block sync | Both | `nimbus-runtime daemon --sync-addr 0.0.0.0:9500` |

## Running on low-disk machines

The Rust build with debuginfo produces a large `target/` directory
(~3–5 GB). On machines with limited disk:

```bash
# Use  minimal debug info to shrink target/ by ~60%
CARGO_PROFILE_DEV_DEBUG=0 cargo build

# Cross-compile on macOS for Linux (target is ~6x smaller than macOS)
cargo build --target x86_64-unknown-linux-musl --release

# Remove target/ after building (re-downloads deps next time)
rm -rf target
```

## Known issues

1. **Container backend unavailable on macOS** — runc, cgroups, and
   Linux namespaces are not available. Use `--backend vm` for VMs.

2. **Initramfs busybox applets are fixed** — applets are hardcoded in
   `build-initramfs/src/main.rs`. Add more or rebuild with a different
   busybox config if needed.

3. **No store garbage collection** — the DAG store grows monotonically.
   Monitor disk usage or periodically `rm -rf /var/lib/nimbus` and
   re-pull.

4. **Apple Virt without entitlement** — running unsigned will get:
   ```text
   ERROR AppleVirtPool::new failed: Invalid virtual machine configuration.
   The process doesn't have the "com.apple.security.virtualization" entitlement.
   ```
   Fix: `make apple-sign-daemon`.

5. **`--publish` flag in `nimbus-runtime run` CLI is parsed but
   unused** — the daemon's gRPC `RunWorkload` handles ports correctly;
   use `nimbusctl run -p` instead of `nimbus-runtime run --publish`.

## Two-thread dispatch model (Apple VM tools)

Both `apple-virt-smoke` and `apple-virt-exec` use a two-thread model:

- **Main thread** — calls `dispatch2::dispatch_main()` to pump the
  main dispatch queue. The Apple Virtualization framework submits all
  its async completion handlers to this queue.

- **Body thread** — does all actual work (kernel staging, pool
  operations, vsock transport). When done, calls `libc::_exit(code)`
  because the main thread is blocked in `dispatch_main()` and can't
  observe the return value.

A panic hook catches body-thread panics and calls `_exit(1)`;
otherwise a panic would leave the process stuck in `dispatch_main()`.

## Environment variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `NIMBUS_KERNEL_PATH` | Path to kernel for VM backends | `~/.nimbus/kernels/vmlinux-<version>` |
| `NIMBUS_KERNEL_DIR` | Kernel installation directory | `~/.nimbus/kernels` |
| `NIMBUS_FC_VMLINUX` | Firecracker kernel path for smoke tests | (none, required) |
| `NIMBUS_FC_BIN` | Firecracker binary path for smoke tests | (none, required) |
| `NIMBUS_SUBUID_BASE` | Rootless UID mapping base | 100000 |
| `NIMBUS_RUNTIME_BIN` | Path to nimbus-runtime for kernel image building | (none) |
| `NIMBUS_INITRAMFS` | Path to custom initramfs for kernel building | (none) |
| `NIMBUS_STORE` | Override store root for CLI | (none — uses daemon config) |
