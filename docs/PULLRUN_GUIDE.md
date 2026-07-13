# Pullrun User Guide

Pullrun is a content-addressed workload execution system. It pulls OCI
images, deduplicates them into a zero-copy on-disk DAG (rkyv + memmap2),
and runs them in **containers (runc), Firecracker microVMs (KVM), or
Apple Virtualization VMs (macOS)** — all from the same image.

## Platform capabilities

| Feature | macOS (Apple Silicon) | Linux (x86_64 / arm64) |
|---------|----------------------|------------------------|
| OCI pull/push into DAG store | ✅ via pullrun | ✅ via pullrun |
| Container backend (runc) | ❌ (runc is Linux-only) | ✅ `--backend container` |
| Rootless container backend | ❌ (runc + pasta) | ✅ `--backend container` (EUID != 0) |
| Firecracker VM backend | ❌ (needs KVM) | ✅ `--backend vm` |
| Apple Virt VMs via pullrun | ✅ (with entitlement, kernel + initramfs in ~/.pullrun/) | ❌ |
| Apple Virt VM standalone tools | ✅ (with entitlement) | ❌ |
| pullrun CLI | ✅ | ✅ |
| pullrun-runtime daemon | ✅ (stores, sync, policy) | ✅ (all backends) |
| P2P DAG block sync | ✅ | ✅ |
| Policy engine (cosign, SBOM) | ✅ | ✅ |
| CRI shim + Kubernetes | ❌ (needs runc) | ✅ |
| `pullrun kernel install` | ✅ → arm64 | ✅ → amd64 |

**Key insight:** pullrun works identically on both platforms for all
store/sync/policy operations. `pullrun run --backend vm` works on
both macOS (Apple Virtualization) and Linux (Firecracker/KVM). On macOS
the daemon discovers the kernel and initramfs from `~/.pullrun/` automatically
(no `--kernel-image` flag needed); on Linux only a kernel is required
(no initramfs — see the Firecracker section below).

## Quick start

```bash
# 1. Build pullrun (once, from any platform)
make build
export PATH="$PWD/bin:$PATH"

# 2. Pull an image (deduplicates into the on-disk DAG store).
pullrun pull alpine:3.18

# 3. Linux — run as container:
pullrun run alpine:3.18 --backend container --cmd /bin/echo --cmd hello

# 4. macOS — one-time VM setup, then run:
make install-kernel         # download kata arm64 kernel
rustup target add aarch64-unknown-linux-musl   # required by build-initramfs
make build-initramfs        # build initramfs (busybox + pullrun-init)
make apple-sign-daemon      # sign runtime for Apple Virtualization

pullrun run alpine:3.18 --backend vm --cmd /bin/echo --cmd hello --attach

# 5. macOS — interactive VM shell (persistent, detach with Ctrl-P Ctrl-Q):
pullrun run alpine:3.18 --backend vm --tty --attach --cmd /bin/sh
```

> **`--cmd` takes separate arguments.** Use `--cmd /bin/echo --cmd hello`
> not `--cmd "echo hello"` (which is treated as a single command name).

The `--direct` mode (default: true) spawns the pullrun-runtime daemon as
a child process automatically, so you don't need to start it manually.
To talk to an existing daemon, use `--socket /tmp/pullrun.sock` or
`--server host:port`.

## Architecture

```
                         ┌──────────────────────────────┐
                         │       pullrun (Go)         │
                         │  pull · run · inspect · build│
                         │  compose · stats · cp · exec │
                         │  secret · config · network   │
                         └──────────┬───────────────────┘
                                    │ gRPC (UDS or TCP)
                                    ▼
   ┌──────────────────────────────────────────────────────────────┐
   │              pullrun-runtime  (spawned by --direct)           │
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
    │  userspace TCP/UDP proxy + DNS + IPAM + iptables/slirp    │
   └───────────────────────────────────────────────────────────┘
```

## Repository layout

```
proto/               Protobuf definitions (single source of truth)
proto-go/            Generated Go protobuf code
runtime/             Rust workspace: store, oci, net, policy, exec, vm, sync, runtime
cli/pullrun/       Go CLI (cobra) — full Docker-like interface
cri/pullrun-cri/      Go CRI shim (Kubernetes integration)
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

This builds `pullrun-runtime` (the gRPC daemon) and all library crates.
For a release build: `cargo build --workspace --release`.

The release binary is at `target/release/pullrun-runtime`.

### Go CLI (pullrun)

```bash
make build-go
# or directly:
cd cli/pullrun && go build -o ../../bin/pullrun .
```

The binary is at `bin/pullrun`. It communicates with the runtime over
a Unix domain socket (`--direct` mode auto-spawns the daemon).

### Cross-compilation

**Go** — trivially cross-compilable:
```bash
cd cli/pullrun && GOOS=linux GOARCH=amd64 go build -o pullrun-linux .
# The result is a statically-linked ELF binary.
```

**Rust** — cross-compile for Linux from macOS (needs target + linker):
```bash
# Install Linux targets
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

# Build pullrun-init (static musl, no C deps):
cargo build -p pullrun-init --target x86_64-unknown-linux-musl --release

# Build pullrun-runtime for x86_64 Linux:
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

Normally you don't need to start the daemon manually — `pullrun` uses
`--direct` mode (default) which spawns it as a child process. Start it
explicitly when you need a long-lived daemon (e.g. for Prometheus):

```bash
# Minimal (auto-spawned by pullrun --direct)
pullrun pull alpine:3.18

# Long-lived daemon for production
pullrun-runtime daemon --socket /tmp/pullrun.sock

# With Prometheus metrics
pullrun-runtime daemon --metrics-addr 0.0.0.0:9090

# With VM backend (Linux — needs firecracker + kernel)
pullrun-runtime daemon \
    --vm-firecracker /usr/local/bin/firecracker \
    --vm-kernel ~/.pullrun/kernels/vmlinux-3.31.0

# With peer-to-peer block sync
pullrun-runtime daemon --sync-addr 0.0.0.0:9500

# With policy enforcement
pullrun-runtime daemon --require-signature --max-cvss 7.0
```

> **SSH tip:** When starting via SSH, use `nohup` and export PATH:
> ```bash
> ssh host 'export PATH="$HOME/.cargo/bin:$PATH"; nohup pullrun-runtime daemon > /tmp/daemon.log 2>&1 &'
> ```

### All daemon flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--socket` | `/tmp/pullrun.sock` | gRPC UDS path |
| `--store-root` | `/var/lib/pullrun` | DAG store directory |
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
| `--vm-root` | `/var/lib/pullrun/vm` | VM ext4 rootfs + sidecar directory |
| `--vm-vcpus` | 2 | vCPUs per VM |
| `--vm-mem-mib` | 512 | Memory per VM (MiB) |
| `--vm-size-mb` | 256 | Rootfs size per VM (MiB) |
| `--vm-warm-pool-size` | 0 | Pre-booted VMs in warm pool (0 = disabled) |
| `--sync-addr` | `0.0.0.0:0` | BlockSync gRPC (port 0 = disabled) |
| `--registrar-addr` | (none) | Host Registrar gRPC service |
| `--registrar-connect` | (none) | Connect to remote Registrar |

## pullrun — full CLI reference

The pullrun CLI is the main user interface, modelled after Docker.
It auto-spawns the runtime daemon (`--direct` mode) so you don't need
to manage it separately.

### Image management

```bash
# Pull an image from any OCI registry
pullrun pull alpine:3.18
pullrun pull alpine:3.18 --platform linux/arm64   # multi-arch
pullrun pull localhost:5000/myimg:latest           # private registry

# Push a DAG image to an OCI registry
pullrun push <root-digest> ghcr.io/myorg/myimg:latest

# Native DAG-aware build (Dockerfile, no Docker needed)
pullrun build ./Dockerfile . -t myapp:latest
pullrun build ./Dockerfile . -t myapp:latest --platform linux/arm64

# Export / import DAG images (save/load, NOT export/import)
pullrun save <digest> -o myimage.tar
pullrun load -i myimage.tar

# List images in the store
pullrun list

# Login / logout from registries (bearer token auth)
pullrun login docker.io           # prompts for username/password
pullrun login ghcr.io -u myuser   # explicit user, prompts for password
pullrun login localhost:5000      # plain-HTTP registry (needs --insecure-registry on daemon)
pullrun logout

# Configure the daemon for insecure (plain-HTTP) registries:
pullrun-runtime daemon --insecure-registry localhost:5000
```

### Running workloads

```bash
# Run a container (Linux)
pullrun run alpine:3.18 --backend container --cmd /bin/echo --cmd hello
pullrun run alpine:3.18 --backend container --cmd /bin/sleep --cmd 60

# Run a VM — works on macOS (Apple Virt) and Linux (Firecracker/KVM)
# On macOS, no --kernel-image needed: kernel auto-discovered from ~/.pullrun/kernels/
pullrun run alpine:3.18 --backend vm --cmd /bin/echo --cmd hello --attach

# With environment variables
pullrun run alpine:3.18 --backend container -e FOO=bar -e BAZ=qux \
    --cmd /usr/bin/printenv

# With resource limits
pullrun run alpine:3.18 --backend container --memory 268435456 --cpu 500 \
    --cmd /usr/bin/stress --cmd --cpu --cmd 1

# With published ports
pullrun run nginx:alpine --backend container -p 8080:80 \
    --cmd nginx --cmd -g --cmd 'daemon off;'

# With restart policy
pullrun run alpine:3.18 --backend container --restart on-failure \
    --cmd /bin/myapp

# With health check
pullrun run alpine:3.18 --backend container \
    --health-cmd 'curl -f http://localhost:80' --cmd /bin/sleep --cmd 3600

# Run and attach (single step — blocks until exit, propagates exit code)
pullrun run alpine:3.18 --backend container --cmd /bin/echo --cmd 'hello from attach' --attach
pullrun run alpine:3.18 --backend container --cmd /bin/sleep --cmd 30 -a

# Interactive shell with detach:
#   Ctrl-P Ctrl-Q — detach without exiting
#   pullrun exec --tty <id> -- /bin/sh — re-attach later
pullrun run alpine:3.18 --backend container --cmd /bin/sleep --cmd 3600 --tty --attach
```

### Managing running workloads

```bash
# List running workloads
pullrun list

# Get details
pullrun inspect <id>

# Stop a workload
pullrun stop <id>

# Live resource stats
pullrun stats <id>

# Update resource limits live
pullrun update <id> --cpu 2000 --memory 1073741824

# Execute a command (non-interactive, all backends).
# Works from any state (pending/running/exited).
pullrun exec <id> -- /bin/cat /etc/alpine-release

# Interactive shell with PTY (detach via Ctrl-P Ctrl-Q).
# Boots the VM if pending, re-attaches if running, boots fresh if exited.
pullrun exec <id> -t -- /bin/sh
pullrun exec --tty <id> -- /bin/sh    # long flag also works

# Workload lifecycle states:
#   pending → run --backend vm (placeholder, not yet booted)
#   running → exec -t boots the VM
#   exited  → exec -t from exited boots a fresh VM

# Attach to a running workload (bidi stdio, container or Apple Virt VM)
pullrun workload run <id>
pullrun workload run <id> --tty          # interactive shell

# Run and attach in one step (equivalent to 'docker run -it')
pullrun run alpine:3.18 --backend container --cmd /bin/echo --cmd hello --attach
pullrun run alpine:3.18 --backend container --cmd /bin/sleep --cmd 3600 -a

# Copy files
pullrun cp <id>:/path/to/file ./local-file
pullrun cp ./local-file <id>:/path/to/file

# Commit running container as new image layer
pullrun commit <id> myapp:snapshot-1

# Show file changes vs original image
pullrun diff <id>

# Remove stopped workloads
pullrun prune
```

### Secrets and configs

```bash
# Secrets (AES-256-GCM encrypted)
pullrun secret create db_password ./secret_file    # from file
pullrun secret create db_password -                 # from stdin
pullrun secret ls
pullrun secret inspect db_password
pullrun secret rm db_password

# Configs (plain text)
pullrun config create app_config ./nginx.conf
pullrun config ls
pullrun config inspect app_config
pullrun config rm app_config

# Mount in containers
pullrun run myapp:latest --secret db_password --config app_config
```

### Networking

```bash
# Create a user-defined bridge network
pullrun network create mynet

# List networks
pullrun network ls

# Remove a network
pullrun network rm mynet
```

### System

```bash
# Runtime info
pullrun info

# Client version
pullrun version

# Events stream
pullrun events

# Compose (separate binary)
pullrun-compose up -f docker-compose.yml          # containers (default on Linux)
pullrun-compose up -f docker-compose.yml --backend vm  # same compose, VM isolation
pullrun-compose down
pullrun-compose ps
pullrun-compose logs
```

### Kernel management

```bash
# Install a kernel for VM backends
pullrun kernel install                      # arm64 (macOS default)
pullrun kernel install --arch amd64         # x86_64 (Linux)
pullrun kernel install --version 3.31.0     # specific version

# Installed to ~/.pullrun/kernels/vmlinux-<version>
```

On macOS, the daemon automatically discovers the kernel from
`~/.pullrun/kernels/` (and initramfs from `~/.pullrun/initramfs/`),
so `--kernel-image` is not required. Override with the `PULLRUN_KERNEL_PATH`
or `PULLRUN_INITRAMFS_PATH` environment variables.

### Run flags (pullrun run)

| Flag | Purpose |
|------|---------|
| `--backend` | `container` (Linux), `vm` (macOS+AppleVirt / Linux+KVM), `sandbox` |
| `--cmd` | Command + args (repeatable, overrides entrypoint) |
| `-e` / `--env` | Environment variables (`KEY=VALUE`) |
| `--name` | Workload name (auto-generated if empty) |
| `--memory` | Memory limit in bytes (default 512MiB) |
| `--cpu` | CPU millicores (1000 = 1 vCPU) |
| `--net` | Network mode: `isolated`, `bridge`, `slirp`, `host`, `none` |
| `-p` / `--publish` | Publish port (`host:container` or just `port`) |
| `--allow-inbound` | Expose port (repeatable) |
| `--allow-outbound` | Allow outbound (`tcp:host:port`) |
| `--kernel-image` | OCI kernel image for VM backend (optional on macOS when `~/.pullrun/kernels/` has one) |
| `--registry` | Registry for pulling the workload image |
| `--platform` | Target platform for pull (`linux/amd64`, `linux/arm64`) |
| `--restart` | Restart policy: `no`, `on-failure`, `always`, `unless-stopped` |
| `--health-cmd` | Health check command |
| `--health-interval` | Health check interval (default 30s) |
| `--health-timeout` | Health check timeout (default 10s) |
| `--health-retries` | Consecutive failures before unhealthy (default 3) |
| `--health-start-period` | Grace period before first check (default 0s) |
| `--secret` | Mount a secret at `/run/secrets/<name>` |
| `--config` | Mount a config at `/<name>` |
| `-t` / `--tty` | Allocate a PTY (interactive shell) |
| `-a` / `--attach` | Attach after start: streams stdout/stderr (Apple Virt VM / container with `--tty`) or polls for exit code (Firecracker VM / container without `--tty`). Detach via Ctrl-P Ctrl-Q. For Apple Virt VMs the VM is persistent — re-attach later with `pullrun exec <id> -t -- /bin/sh`. |
| `-v` / `--volume` | Bind mount (`source:destination[:options]`). Supported on all backends: containers (Linux kernel bind mount), VMs (VirtioFS directory share). The `:ro` option enforces read-only. Multiple volumes allowed. |
| `--direct` | Spawn runtime as child (default: true) |
| `--socket` | Runtime UDS socket path (default `/tmp/pullrun.sock`) |
| `--server` | Remote control plane address (disables `--direct`) |

## Container backend (Linux)

The default backend runs workloads via runc:

```bash
pullrun pull alpine:3.18
pullrun run alpine:3.18 --backend container --cmd /bin/echo --cmd 'hello'

# Inspect after completion shows exit code
pullrun inspect <id>
# Exit Code: 0
```

### Container networking

By default, containers use `NetworkMode::Loopback` (isolated). For
published ports, `--publish` auto-promotes to bridge mode:

```bash
pullrun run nginx:alpine -p 8080:80 --cmd nginx -g 'daemon off;'
# Verify: curl http://localhost:8080
```

For VMs, the default is `--net slirp` (userspace NAT via
`slirp4netns` — no bridge, no iptables). Explicit modes:

| `--net`     | Backend      | Description |
|-------------|--------------|-------------|
| `isolated`  | any          | Loopback only, no outbound |
| `bridge`    | any          | Shared `pullrun-br0`, inter-workload comms |
| `slirp`     | vm           | Per-VM userspace NAT (rootless) |
| `host`      | container    | Host network namespace |

### Running detached and exec-ing

```bash
# Run a long-lived container (detached — returns immediately)
pullrun run alpine:3.18 --backend container --name myserver \
    --cmd /bin/sleep --cmd 3600

# Exec into it while running
pullrun exec myserver /bin/cat /etc/alpine-release   # → 3.18.12
pullrun exec myserver /usr/bin/printenv               # env vars
pullrun exec myserver /bin/echo 'hello'               # stdout capture

# Or run and attach in one step (blocks until exit, like docker run -it)
pullrun run alpine:3.18 --backend container --cmd /bin/sleep --cmd 10 -a

# Stop it
pullrun stop myserver
```

## Firecracker VM backend (Linux)

Firecracker boots each workload in a KVM-based microVM with a
dedicated kernel and ext4 rootfs — stronger isolation than containers.
No initramfs is used. The rootfs is materialized directly from the OCI
image as a bootable ext4 filesystem with an auto-generated `/init`
script (see "What happens inside" below).

### Prerequisites

```bash
# 1. Install a vmlinux kernel for x86_64
# Option A: Download from the Firecracker quickstart S3
curl -fL -o /var/lib/pullrun/vmlinux.bin \
  https://spec.ccfc.min/img/quickstart_guide/x86_64/kernels/vmlinux.bin

# Option B: Use pullrun kernel install (Kata kernel, ~40 MB)
pullrun kernel install --arch amd64

# 2. mkfs.ext4 (e2fsprogs >= 1.47.0) is required for rootfs creation
apt-get install -y e2fsprogs
```

No `pullrun-init`, initramfs, or busybox build is needed. The runtime
generates `/init` from the OCI image's ENTRYPOINT/CMD and embeds it
directly in the ext4 rootfs.

### Start daemon with VM backend

```bash
pullrun-runtime daemon \
  --vm-firecracker /usr/local/bin/firecracker \
  --vm-kernel /var/lib/pullrun/vmlinux.bin \
  --vm-root /var/lib/pullrun/vm &
```

Or use `--direct` mode via pullrun (auto-spawns the daemon).

### Run a VM workload

```bash
pullrun run alpine:3.18 --backend vm \
    --cmd /bin/echo --cmd 'hello from pullrun VM via firecracker!'
```

The workload command runs as PID 1 inside the VM via the injected
`/init` shell script. By default the VM uses **slirp networking**
(userspace NAT via `slirp4netns` — no bridge, no iptables). Pass
`--net bridge` for a shared bridge on the same L2 segment as
containers. With `--attach`, the CLI polls `GetWorkload` until the
workload exits and reports the exit code. (Real-time stdio streaming
is not yet supported on this backend — output goes to the VM serial
console.)

### What happens inside (VM boot sequence)

1. Runtime materializes the OCI image DAG into a temp directory
2. Runtime reads the image's ENTRYPOINT/CMD from the DAG manifest
3. Runtime writes `/init` as a shell script: `exec <entrypoint> <cmd>`
4. Runtime creates a sparse ext4 image (256 MB default) and runs
   `mkfs.ext4 -d <dir> <image>` to populate it — no root required
5. Firecracker boots the kernel with `root=/dev/vda rw init=/init`
   and the ext4 image as the root block device
6. Kernel runs `/init`, which `exec`s the OCI entrypoint/cmd directly
7. When the command exits, firecracker exits
8. Daemon detects the exit via `waitpid(WNOHANG)` in the VM executor's
   `status()` call

### Firecracker smoke test

The standalone `firecracker-smoke` tool (under `tools/`) builds a tiny
ext4 rootfs and boots a real Firecracker microVM:

```bash
cd tools/firecracker-smoke && cargo build
# Required env vars:
PULLRUN_FC_BIN=/usr/local/bin/firecracker \
PULLRUN_FC_VMLINUX=~/.pullrun/kernels/vmlinux-3.31.0 \
  ./target/debug/firecracker-smoke
```

## Apple VM backend (macOS)

The Apple Virtualization framework runs Linux VMs on Apple Silicon.
`pullrun run --backend vm` boots a fresh VM per workload, shares the OCI
image rootfs via VirtioFS, and streams the workload's stdio back to your
terminal over vsock — all from a regular OCI image (no VM image build step).

### One-time setup

```bash
# 0. Build everything (Rust runtime + Go CLI)
make build

# 1. Kernel — download Kata Containers' arm64 kernel
make install-kernel
# Installed to ~/.pullrun/kernels/vmlinux-3.31.0

# 2. Initramfs — build busybox + pullrun-init for aarch64
#    (requires rustup target: run `rustup target add aarch64-unknown-linux-musl` first)
make build-initramfs
# Installed to ~/.pullrun/initramfs/pullrun-initramfs.cpio.gz

# 3. Code signing — grant com.apple.security.virtualization
make apple-sign-daemon
```

**Re-sign after every `cargo build`** — the build strips the
entitlement from the binary.

### Run a VM workload

```bash
# Minimal — run and attach in one step
pullrun pull alpine:3.18
pullrun run alpine:3.18 --backend vm \
    --cmd /bin/echo \
    --cmd 'hello from pullrun VM' \
    --attach
```

The VM boots (~2–3 s), executes the workload, streams output, and the
CLI exits when the workload exits.

### `--cmd` usage (important)

The `--cmd` flag is **repeatable** — each flag is one argument:

```bash
# Correct — separate --cmd for each arg:
pullrun run alpine:3.18 --backend vm \
    --cmd /bin/echo --cmd hello --cmd world --attach

# Wrong — "hello world" as a single string is treated as one argument:
pullrun run alpine:3.18 --backend vm --cmd "echo hello" --attach   # fails
```

For the VM backend, `--cmd /bin/sh --cmd -c --cmd 'echo hello'`
runs the command through the shell (though most commands work directly
with busybox applets from the initramfs).

### With environment variables

```bash
pullrun run alpine:3.18 --backend vm \
    -e FOO=bar -e GREETING='hello world' \
    --cmd /usr/bin/printenv \
    --attach
```

Environment variables are embedded in the kernel command line and
injected into the workload process by `pullrun-init` inside the guest
(macOS Apple Virt path). The rootfs is the full OCI image content
(alpine's `/etc`, `/usr`, etc.) shared via VirtioFS — so
`/usr/bin/printenv` resolves correctly.

### Longer-running workloads

```bash
# Run detached, attach later
pullrun run alpine:3.18 --backend vm \
    --name myvm \
    --cmd /bin/sleep --cmd 60
# → Started wl-abc123

# Attach to stream I/O
pullrun workload run wl-abc123
```

### Detach and re-attach (all backends)

Both container (runc) and Apple Virt VM backends support **persistent
workloads** — the process keeps running after you detach, and you can
re-attach any number of times:

```bash
# ── Container backend (Linux) ──────────────────────────────

# Start an interactive container shell with TTY + attach
pullrun run alpine:3.18 --backend container -t -a --cmd /bin/sh
/ # echo hello
hello
/ / #
# Press Ctrl-P Ctrl-Q to detach (container keeps running)

# Re-attach later — gets a fresh shell prompt
pullrun exec wl-abc123 -t /bin/sh
/ # uname
Linux
/ / #
# Press Ctrl-P Ctrl-Q to detach again

# ── Apple Virt VM backend (macOS) ──────────────────────────

pullrun run alpine:3.18 --backend vm -t -a --cmd /bin/sh
/ # echo hello
hello
/ / #
# Press Ctrl-P Ctrl-Q to detach (VM keeps running)

# Re-attach with `--` separator (recommended for VMs)
pullrun exec wl-def456 -t -- /bin/sh
/ # uname
Linux
/ / #

# Multiple workloads can run simultaneously:
pullrun run alpine:3.18 --backend container -t -a --cmd /bin/sh   # Container 1
pullrun run alpine:3.18 --backend container -t -a --cmd /bin/sh   # Container 2
# Detach from both, re-attach to either at any time
pullrun exec wl-c1 -t /bin/sh
pullrun exec wl-c2 -t /bin/sh
```

Key details:

- Use `-t -a` to start an interactive shell (`-t` allocates a PTY, `-a`
  attaches to the session).
- Detach with **Ctrl-P Ctrl-Q**. The workload process keeps running.
- Re-attach with `pullrun exec <id> -t <command>`. On TTY attach, the
  daemon allocates a host PTY and passes the slave side to `runc exec -t`
  (or creates a fresh VM console session).
- The `--` separator between `pullrun exec` flags and the command is
  **optional** for `pullrun exec` now (the CLI strips it automatically),
  but still recommended on shells that expand wildcards or flags.
- Multiple workloads can be detached and re-attached independently.
- The daemon must be running (or use `--direct` mode, which is the default).

### Workload lifecycle

All backends share three states. What `exited` means depends on whether
the backend preserves filesystem state across restarts:

| State | Process? | CLI |
|-------|----------|-----|
| `pending` | Never started | `run` creates the placeholder |
| `running` | Active | `exec -t` transitions from `pending` or `exited` |
| `exited` | Stopped | `stop`, natural exit, or crash |

**`exec <id> -t -- /bin/sh` works from any state:**
- **pending** → boots the workload, attaches
- **running** → re-attaches (Ctrl-P Ctrl-Q to detach)
- **exited** → boots a fresh workload from the same config

#### What persists across restarts (exited → exec)

| Backend | Rootfs | `--volume` mounts |
|---------|--------|-------------------|
| **Container** (runc) | Ephemeral — fresh OCI rootfs on each boot | ✅ Survives — same source path |
| **VM** (Apple Virt) | ✅ Persistent — VirtioFS shares the host rootfs directory directly. Every file write hits the host filesystem. Next boot sees everything. | ✅ Survives |
| **VM** (Firecracker) | ✅ Persistent — ext4 image file. Currently no `exec` restart support for Firecracker. | ✅ Survives |

Key takeaway: a stopped VM remembers all filesystem state. A stopped
container only remembers its `--volume` mounts. Both restart on demand
with `exec` — no need to re-pull the image or re-declare flags.

**Apple Virt only:** Re-sign after every `cargo build`: `make apple-sign-daemon`.

If `exec` returns immediately with no output, a stale daemon (from before
the last build) may be running. Kill it: `pkill -f pullrun-runtime` and
retry.

If `exec` returns immediately with no output, a stale daemon (from before
the last build) may be running. Kill it: `pkill -f pullrun-runtime` and
retry.

### Persistent data volumes

Mount host directories into workloads for storage that survives exec
reconnections and daemon restarts. Both backends support `--volume`/`-v`:

```bash
# ── Container backend (Linux — runc bind mount) ────────────

# Mount a host directory at /mnt/data inside the container
pullrun run alpine:3.18 --backend container \
    --volume /tmp/my-data:/mnt/data \
    --cmd /bin/sleep --cmd 60

# Verify the mount from another shell
pullrun exec wl-abc123 /bin/cat /mnt/data/hello.txt
hello from host

# Read-only volume (:ro suffix)
pullrun run alpine:3.18 --backend container \
    --volume /tmp/readonly-data:/mnt/data:ro \
    --cmd /bin/touch /mnt/data/test
# → touch: /mnt/data/test: Read-only file system

# ── Apple Virt VM backend (macOS — VirtioFS) ───────────────

pullrun run alpine:3.18 --backend vm \
    --volume /tmp/my-data:/mnt/data \
    -t -a --cmd /bin/sh

# Data is accessible inside the VM
/ # ls /mnt/data
myfile.txt
/ # echo "persistent data" > /mnt/data/hello.txt

# Detach (Ctrl-P Ctrl-Q), re-attach later — data is still there
pullrun exec wl-def456 -t /bin/sh
/ # cat /mnt/data/hello.txt
persistent data

# Read-only volume (:ro suffix)
pullrun run alpine:3.18 --backend vm \
    --volume /tmp/readonly-data:/mnt/data:ro \
    -t -a --cmd /bin/sh
/ # echo "write attempt" > /mnt/data/test
sh: can't create /mnt/data/test: Read-only file system

# Multiple volumes
pullrun run alpine:3.18 --backend container \
    --volume /tmp/data:/mnt/data \
    --volume /tmp/config:/etc/myapp:ro \
    --cmd /bin/sleep --cmd 60
```

How it works:

- **Container backend (runc):** Each `--volume` adds a `bind` mount entry
  to the OCI `config.json`. The daemon appends `rbind` and `rprivate`
  options automatically, plus any user-specified options (e.g. `ro`).
  No special kernel modules or userspace proxies are needed — runc
  handles bind mounts natively.
- **Apple Virt VM backend:** Each `--volume` creates a
  `VZVirtioFileSystemDeviceConfiguration` — the same Apple Virtualization
  API used for the rootfs share. Inside the VM, `pullrun-init` calls
  `create_dir_all` on the destination and mounts a `virtiofs` filesystem
  at that path. No FUSE daemon, no kernel module — the hypervisor handles
  all shared-file I/O.
- Data lives on the host filesystem. The workload rootfs (OCI image
  content) is ephemeral and re-materialized on each fresh run.

### Initramfs layout (macOS Apple Virt only)

```
/init                  → shell script (exec /sbin/pullrun-init)
/sbin/pullrun-init      → static binary (aarch64)
/bin/busybox           → busybox (aarch64, from Alpine package)
/bin/{sh,cat,ls,...}   → symlinks to busybox applets
/dev/{console,null,tty}→ device nodes
/proc, /sys, /etc, /mnt, /tmp → mount target directories
```

Both `pullrun-init` and `busybox` are **64-bit aarch64** statically
linked binaries. The kata kernel does not support 32-bit ARM compat
(`CONFIG_COMPAT` is disabled), so 32-bit binaries will fail with
`ENOEXEC`.

> **Linux (Firecracker):** no initramfs is used. The rootfs is an ext4
> image with an auto-generated `/init` script.

### Auto-discovery (macOS)

On macOS the daemon finds kernel + initramfs without flags:

| Artifact | Path | Env override |
|----------|------|-------------|
| Kernel | `~/.pullrun/kernels/vmlinux-<version>` (latest) | `PULLRUN_KERNEL_PATH` |
| Initramfs | `~/.pullrun/initramfs/pullrun-initramfs.cpio.gz` | `PULLRUN_INITRAMFS_PATH` |

On Linux the kernel is configured via `--vm-kernel` or `PULLRUN_KERNEL_PATH`;
no initramfs is used.

To use an OCI-packaged kernel from a registry instead, pass
`--kernel-image <ref>`. This works on both platforms.

### Console log

Guest kernel + init messages are written to
`/tmp/pullrun-attach-console.log`. Override with
`PULLRUN_VM_CONSOLE_LOG`:

```bash
PULLRUN_VM_CONSOLE_LOG=/tmp/my-vm.log \
    pullrun run alpine:3.18 --backend vm --cmd /bin/uname --cmd -a --attach
```

Useful for debugging boot failures.

### Standalone tools

For direct FFI testing without the daemon:

#### apple-virt-smoke (FFI pool test)

```bash
cd tools/apple-virt-smoke && cargo build
make apple-sign-smoke

./target/debug/apple-virt-smoke \
  --kernel ~/.pullrun/kernels/vmlinux-3.31.0 \
  --store /tmp/apple-virt-store
```

Success output:
```
INFO staging pre-built kernel path=~/.pullrun/kernels/vmlinux-3.31.0
INFO creating Apple Virt VM index=0 total=3
...
INFO PASS: Apple Virt FFI round-trip succeeded
```

#### apple-virt-exec (full VM workload)

```bash
cd tools/apple-virt-exec && cargo build

# Sign (re-sign after every build)
codesign --force --sign - \
    --entitlements tools/apple-virt-exec/virt.entitlements \
    --options runtime \
    tools/apple-virt-exec/target/debug/apple-virt-exec

# Run a workload
mkdir -p /tmp/my-rootfs
./target/debug/apple-virt-exec \
  --kernel ~/.pullrun/kernels/vmlinux-3.31.0 \
  --initramfs ~/.pullrun/initramfs/pullrun-initramfs.cpio.gz \
  --rootfs /tmp/my-rootfs \
  --timeout 30 \
  -- /bin/echo 'hello from apple-virt-exec'
```

Output:
```
hello from apple-virt-exec
INFO workload completed elapsed_ms=186 exit_code=0
```

#### apple-virt-exec flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--kernel` | (required) | Path to vmlinux kernel |
| `--kernel-image` | (mutually exclusive) | OCI reference for kernel image |
| `--initramfs` | (required with `--kernel`) | Path to initramfs cpio.gz |
| `--rootfs` | (required) | Host dir for VirtioFS share |
| `--cpus` | 1 | vCPUs per VM |
| `--mem-mib` | 512 | Memory per VM |
| `--timeout` | 30 | Session timeout (seconds) |
| `--cwd` | `/` | Working directory inside guest |
| `--env` | (repeatable) | Environment variables |
| `--console-log` | `/tmp/pullrun-exec-console.log` | Guest serial console output |
| `-q` / `--quiet` | false | Suppress info output |

The command is positional (everything after `--`). Args are passed
directly — no quoting needed:
```bash
./apple-virt-exec --kernel ... --initramfs ... --rootfs /tmp/r \
    -- /bin/echo hello world       # runs: echo hello world
./apple-virt-exec --kernel ... --initramfs ... --rootfs /tmp/r \
    -- /bin/uname -a                # runs: uname -a
```

### Kernel details

The Kata Containers kernel at `~/.pullrun/kernels/vmlinux-3.31.0` is
downloaded via:
```bash
make install-kernel                  # arm64 (macOS)
pullrun kernel install --arch amd64  # x86_64 (Linux)
```

The source URL is:
```
https://github.com/kata-containers/kata-containers/releases/download/3.31.0/kata-static-3.31.0-{arch}.tar.zst
```

To verify:
```bash
file ~/.pullrun/kernels/vmlinux-3.31.0
# macOS: → Linux kernel ARM64 boot executable Image, little-endian
# Linux: → ELF 64-bit LSB executable, x86-64, statically linked
```

## Initramfs details (macOS Apple Virt only)

The initramfs is a `cpio` archive (newc format) compressed with gzip,
built by `tools/build-initramfs/src/main.rs` — no host `cpio` binary.
The Firecracker VM backend (Linux) does not use initramfs; see the
Firecracker section above.

### Available busybox applets

```
cat, sh, mount, umount, ls, echo, env, true, false,
mkdir, rm, ln, cp, mv, ps, sleep, test, uname
```

The initramfs also includes `/mnt`, `/tmp`, `/proc`, `/sys`, `/dev`, `/etc`,
`/bin`, and `/sbin` directories. Volume mount destinations (e.g. `/mnt/data`)
are created automatically inside the VM by `pullrun-init`.

These are symlinks to `/bin/busybox`. To inspect the initramfs:
```bash
mkdir -p /tmp/ramfs-inspect && cd /tmp/ramfs-inspect
gzip -dc ~/.pullrun/initramfs/pullrun-initramfs.cpio.gz | cpio -idm
ls -la bin/ sbin/ init
ls -la bin/ sbin/ init
```

## Testing

### Unit tests (cross-platform)

```bash
cargo test --workspace
```

Runs 118 unit tests across all Rust crates (pullrun-store, pullrun-sync,
pullrun-exec, pullrun-oci, pullrun-policy, pullrun-runtime, pullrun-vm,
pullrun-vsock, pullrun-net, pullrun-init, pullrun-dns).

### Including integration tests

```bash
cargo test --workspace -- --include-ignored
```

Discovers additional tests gated by `#[ignore]` (need real hardware,
KVM, or specific binaries).

### Go tests

```bash
cd cli/pullrun && go test ./...
```

9 Go tests for CLI flag parsing.

### End-to-end tests

| Test | Platform | Command |
|------|----------|---------|
| OCI pull round-trip | Both | `pullrun pull alpine:3.18` |
| Container run + exec + stop | Linux | `pullrun run alpine:3.18 --backend container --cmd /bin/echo --cmd hello` |
| Container TTY shell (detach/re-attach) | Linux | `pullrun run alpine:3.18 --backend container -t -a --cmd /bin/sh` then Ctrl-P Ctrl-Q, then `pullrun exec <id> -t /bin/sh` |
| Container with volume mount | Linux | `pullrun run alpine:3.18 --backend container --volume /tmp/data:/mnt/data --cmd /bin/cat,/mnt/data/test.txt` |
| Container read-only volume | Linux | `pullrun run alpine:3.18 --backend container --volume /tmp/data:/mnt/data:ro --cmd /bin/touch,/mnt/data/test 2>&1; echo exit=$?` |
| nginx with port forwarding | Linux | `pullrun run nginx:alpine --backend container -p 8080:80 --cmd nginx --cmd -g --cmd 'daemon off;'` |
| Firecracker VM workload | Linux | `pullrun run alpine:3.18 --backend vm --cmd /bin/echo --cmd hello` |
| Apple VM via pullrun | macOS | `pullrun run alpine:3.18 --backend vm --cmd /bin/echo --cmd hello --attach` |
| Apple VM with volume mount | macOS | `pullrun run alpine:3.18 --backend vm --volume /tmp/data:/mnt/data --cmd ls,-la,/mnt/data --attach` |
| Apple VM read-only volume | macOS | `pullrun run alpine:3.18 --backend vm --volume /tmp/data:/mnt/data:ro --cmd sh,-c,'echo test > /mnt/data/foo 2>&1; echo exit=$?' --attach` |
| Apple VM standalone | macOS | `apple-virt-exec --kernel ... --initramfs ... --rootfs /tmp/r -- /bin/echo hello` |
| Build from Dockerfile | Both | `pullrun build -t myapp:latest .` |
| Secret / Config lifecycle | Both | `pullrun secret create x y; pullrun secret rm x` |
| P2P block sync | Both | `pullrun-runtime daemon --sync-addr 0.0.0.0:9500` |

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

## Detach key

Press **Ctrl-P Ctrl-Q** to detach from an interactive TTY session
(`pullrun run -t -a`, `pullrun exec -t`) without exiting the workload.
The workload keeps running; re-attach later with
`pullrun exec <id> -t <shell>`.

### How it works per backend

| Backend | Detach behavior | Re-attach mechanism | Limitations |
|---------|----------------|---------------------|-------------|
| **Container (runc)** | The `runc exec` process receives EOF on stdin and exits. The parent container (`runc run -d`) continues running unaffected. | New `pullrun exec -t` spawns a fresh `runc exec` with a host‑allocated PTY. Each re-attach gets a new shell. | Container process (from `--cmd`) must still be running |
| **Apple Virt VM** | The VM thread receives `DetachClient`, removes the output sink, and keeps buffering output. The VM and its shell stay alive. | New connection sends `AttachClient` to the same VM thread — reconnects to the same shell session. | macOS only; requires code signing |
| **Firecracker VM** | Interactive attach is not supported. Use `--attach` (non‑TTY) to stream console log until the VM exits. | N/A | Console log is read-only, no stdin forwarding |

Key details:

- On **containers**, each `exec -t` creates a new shell process. Commands
  and state (env vars, working directory) are not shared between re-attaches.
- On **Apple Virt VMs**, the same shell process persists across re-attaches.
  Variables and state set in one session are visible in the next.
- The daemon uses a **host PTY** for `runc exec -t` — it allocates a PTY
  pair via `posix_openpt`/`grantpt`/`unlockpt` and passes the slave fd
  as stdin/stdout/stderr. This avoids `runc exec -t` failing with "open
  /dev/tty: no such device or address" when the daemon has no terminal.
- Multiple workloads can be detached and re-attached independently.

## Known issues

1. **Container backend unavailable on macOS** — runc, cgroups, and
   Linux namespaces are not available. Use `--backend vm` for VMs.

2. **Initramfs busybox applets are fixed** — applets are hardcoded in
   `build-initramfs/src/main.rs`. Add more or rebuild with a different
   busybox config if needed.

3. **No store garbage collection** — the DAG store grows monotonically.
   Monitor disk usage or periodically `rm -rf /var/lib/pullrun` and
   re-pull.

4. **Apple Virt without entitlement** — running unsigned will get:
   ```text
   ERROR Invalid virtual machine configuration.
   The process doesn't have the "com.apple.security.virtualization" entitlement.
   ```
   Fix: `make apple-sign-daemon` (re-sign after every `cargo build`).

5. **`--cmd "echo hello"` is treated as one argument** — use separate
   `--cmd` flags for each argument: `--cmd /bin/echo --cmd hello`.
   Cobra's `StringSliceVar` splits on commas, not spaces.

6. **32-bit busybox causes ENOEXEC** — the kata Firecracker kernel
   disables `CONFIG_COMPAT` (32-bit ARM compat). The Makefile
   `build-initramfs` target downloads a proper 64-bit aarch64 busybox
   from Alpine. If you download busybox manually, make sure it is
   **64-bit aarch64** (run `file busybox` to verify).

7. **`make install-kernel` fails on macOS with `tar --use-compress-program=zstd`**
   — macOS BSD tar has a buggy `--use-compress-program`. Extract
   manually:
   ```bash
   brew install zstd
   curl -fL -o /tmp/kata.tar.zst \
     https://github.com/kata-containers/kata-containers/releases/download/3.31.0/kata-static-3.31.0-arm64.tar.zst
   mkdir -p ~/.pullrun/kernels
   zstd -d /tmp/kata.tar.zst -o /tmp/kata.tar
   tar -xf /tmp/kata.tar opt/kata/share/kata-containers/vmlinux.container
   mv opt/kata/share/kata-containers/vmlinux.container ~/.pullrun/kernels/vmlinux-3.31.0
   rm -rf opt /tmp/kata.tar /tmp/kata.tar.zst
   ```

8. **`--publish` flag in `pullrun-runtime run` CLI is parsed but
   unused** — the daemon's gRPC `RunWorkload` handles ports correctly;
   use `pullrun run -p` instead of `pullrun-runtime run --publish`.

9. **Stale daemon socket after crash** — if the daemon exits uncleanly,
   the socket file `/tmp/pullrun.sock` may remain. Delete it before
   restarting: `rm -f /tmp/pullrun.sock`.

10. **`exec` no longer requires `--` before the command** — the CLI now strips
    the `--` separator from args automatically (cobra included it in positional
    args when `SetInterspersed` was false). Both forms work:
    ```bash
    pullrun exec <id> -t /bin/sh          # works
    pullrun exec <id> -t -- /bin/sh       # also works (recommended for safety)
    ```
    The `--` form is still recommended on shells that expand wildcards or flags.

11. **Volume mount inside VM requires `/mnt` parent** — the initramfs now includes
    `/mnt` and `/tmp` by default. For custom mount paths not under these
    directories, `pullrun-init` creates parent directories automatically via
    `create_dir_all`.

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

## 🔌 MCP Server (AI Agent Integration)

Pullrun exposes its full runtime API through the Model Context Protocol (MCP),
allowing AI agents like opencode, Claude Code, and Cursor to manage workloads
through natural language.

```bash
# Start the MCP server (stdio mode, for opencode/Claude Code)
pullrun mcp

# Start the MCP server over HTTP (SSE mode, for remote agents)
pullrun mcp --sse :8080
```

See [docs/ALL_MCP.md](ALL_MCP.md) for the complete tool and resource reference.

## Environment variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `PULLRUN_KERNEL_PATH` | Path to kernel for VM backends | `~/.pullrun/kernels/vmlinux-<version>` |
| `PULLRUN_KERNEL_DIR` | Kernel installation directory | `~/.pullrun/kernels` |
| `PULLRUN_FC_VMLINUX` | Firecracker kernel path for smoke tests | (none, required) |
| `PULLRUN_FC_BIN` | Firecracker binary path for smoke tests | (none, required) |
| `PULLRUN_SUBUID_BASE` | Rootless UID mapping base | 100000 |
| `PULLRUN_RUNTIME_BIN` | Path to pullrun-runtime for kernel image building | (none) |
| `PULLRUN_INITRAMFS` | Path to custom initramfs for kernel building | (none) |
| `PULLRUN_STORE` | Override store root for CLI | (none — uses daemon config) |
