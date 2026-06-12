<div align="center">

<img src="https://raw.githubusercontent.com/pullrun/pullrun/main/assets/logo.png" alt="Pullrun Logo" width="400">

# **Pullrun**

### *Next-gen container runtime with zero-copy DAG storage and P2P image sync. Run OCI images as Linux containers, Firecracker microVMs, or Apple Silicon VMs.*

**Same OCI image. Any isolation level. No Docker daemon required.**

[![License](https://img.shields.io/github/license/pullrun/pullrun)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.77+-orange.svg)](https://www.rust-lang.org/)
[![Go](https://img.shields.io/badge/Go-1.22+-00ADD8.svg)](https://golang.org/)
[![Tests](https://img.shields.io/badge/tests-135%20passing-brightgreen.svg)](#testing)

</div>

---

## 🚀 What is Pullrun?

Pullrun is a next-generation container runtime that treats **content-addressed storage** as a first-class primitive. It pulls OCI images, deduplicates them into a zero-copy on-disk DAG ([rkyv](https://github.com/rkyv/rkyv) + [memmap2](https://github.com/danburkert/memmap-rs)), and runs them in whichever execution backend you choose.

| Backend | Isolation | Best For |
|---------|-----------|----------|
| 🐧 **Linux Containers** (runc) | Process-level | Developer workflows, CI/CD dense packing |
| 🔥 **Firecracker microVMs** (KVM) | Per-VM kernel | Multi-tenant, untrusted workloads, compliance |
| 🍎 **Apple Virtualization** (macOS) | Per-VM kernel | macOS dev environments, Apple Silicon CI |

> **The same image content can be booted as a container or as a VM — the only thing that changes is the backend.**

---

## ✨ Why Pullrun?

| Feature | Pullrun Advantage |
|---------|-------------------|
| **Zero-copy DAG store** | OCI layers stored as-is. No tar extraction, no overlayfs, no `dockerd`. Just `mmap()` and go. |
| **P2P image distribution** | Nodes share image blocks directly via gRPC + Bloom filters. One node pulls; the rest delta-sync peer-to-peer. |
| **Same image, any backend** | No separate "VM image" build step. The OCI manifest **IS** the VM rootfs. |
| **No overlayfs CVEs** | CVE-2026-31431, CVE-2023-0386, CVE-2023-32629 — all eliminated by per-VM kernel isolation. |

---

## 📦 Install

### macOS (Homebrew — pre-built binary, no Xcode/build deps)
```bash
brew tap pullrun/tap
brew install pullrun
```
> The formula downloads a pre-built ~18 MB tarball — no Rust, Go, LLVM, or Xcode required.

### Linux (APT — Debian/Ubuntu)
```bash
curl -fsSL https://github.com/pullrun/pullrun/raw/main/install.sh | bash
```
> The `.deb` package installs both `pullrun` and `pullrun-runtime` plus a systemd service.

### Any platform (direct download)
```bash
curl -fsSL https://github.com/pullrun/pullrun/raw/main/install.sh | bash
```
Detects the platform and installs via Homebrew (macOS), APT (Debian/Ubuntu), or direct binary download.

### From source
```bash
make build
export PATH="$PWD/bin:$PATH"
```

---

## 🚀 Quick Start

```bash
# ── Pull & Run ─────────────────────────────────────────────────────

# Pull an image (deduplicates into the on-disk DAG store)
pullrun pull alpine:3.18

# Run as a container (default) — exits after command completes
pullrun run alpine:3.18 --cmd /bin/echo --cmd 'hello pullrun'

# Run as a VM — same image, different backend
pullrun run alpine:3.18 --backend vm --cmd /bin/echo --cmd 'hello pullrun'

# Interactive shell with detach/re-attach
pullrun run alpine:3.18 --tty --attach --cmd /bin/sh
#   Ctrl-P Ctrl-Q  → detach (workload keeps running)
#   pullrun list   → find the workload ID
#   pullrun exec <id> -t /bin/sh → re-attach

# Run in background (daemon mode)
pullrun run alpine:3.18 --cmd /bin/sleep --cmd 3600

# ── Lifecycle ──────────────────────────────────────────────────────

# List all workloads (pending / running / exited)
pullrun list

# Stop a workload
pullrun stop <id>

# Execute a command in a running workload
pullrun exec <id> /bin/echo hello

# Execute with TTY (works even on exited workloads — boots fresh)
pullrun exec <id> -t /bin/sh

# Attach to a workload's stdio
pullrun attach <id>

# ── Build & Push ───────────────────────────────────────────────────

# Build an image from a Dockerfile (no Docker daemon needed)
pullrun build -t myapp:latest .

# Multi-platform build
pullrun build -t myapp:latest --platform linux/amd64,linux/arm64 .

# Push to a registry
pullrun push myapp:latest

# Export/import for air-gapped environments
pullrun export myapp:latest > myapp.tar
pullrun import < myapp.tar

# ── Compose ────────────────────────────────────────────────────────

pullrun compose up -f myapp/compose.yml
pullrun compose logs -f
pullrun compose down

# ── Secrets & Configs ──────────────────────────────────────────────

pullrun secret create db_password secret data
pullrun run myapp:latest --secret db_password

pullrun config create nginx.conf --file ./nginx.conf
pullrun run nginx:latest --config nginx.conf

# ── Networking ─────────────────────────────────────────────────────

pullrun network create my-net
pullrun run alpine:3.18 --network my-net

# ── P2P Sync ───────────────────────────────────────────────────────

pullrun-runtime daemon --sync-addr 0.0.0.0:9500
```

---

## 🍎 macOS — Apple VM Backend

Run any OCI image as a lightweight VM on Apple Silicon — no separate VM image build required.

```bash
# One-time setup
make install-kernel         # Download kata arm64 kernel
make build-initramfs        # Build initramfs with busybox + pullrun-init
make apple-sign-daemon      # Sign pullrun-runtime for Apple Virtualization

# Run any OCI image as a VM
pullrun pull alpine:3.18
pullrun run alpine:3.18 --backend vm \
    --cmd /bin/echo --cmd 'hello from pullrun VM' \
    --attach

# Interactive persistent shell with data volumes
pullrun run alpine:3.18 --backend vm \
    --volume /tmp/data:/mnt/data \
    --tty --attach --cmd /bin/sh
```

**Key macOS notes:**
- **Persistent VMs** — survive detach (Ctrl-P Ctrl-Q), re-attach with `pullrun exec <id>`
- **VirtioFS volumes** — host directories shared natively, no FUSE proxy
- **Auto kernel discovery** — from `~/.pullrun/kernels/`, no `--kernel-image` needed
- **Re-sign after `cargo build`** — `make apple-sign-daemon` restores entitlements

---

## 🐧 Linux — Container & Firecracker VM Backends

```bash
# Container backend (requires runc)
pullrun run alpine:3.18 --backend container --tty --attach --cmd /bin/sh

# Firecracker VM backend (requires KVM + vmlinux kernel)
pullrun run alpine:3.18 --backend vm --tty --attach --cmd /bin/sh
```

See [docs/PULLRUN_GUIDE.md](docs/PULLRUN_GUIDE.md) for kernel setup and full Linux configuration.

---

## 🖥️ Interactive Shells & Persistent Workloads

Both backends support interactive shells with **detach/re-attach** via `Ctrl-P Ctrl-Q`:

```bash
# Start an interactive shell
pullrun run alpine:3.18 --backend container --tty --attach --cmd /bin/sh
# Ctrl-P Ctrl-Q → detach, workload keeps running
# Re-attach: pullrun exec <id> -t /bin/sh
```

---

## 📦 Compose (Multi-Service Stacks)

Native Compose support — no separate `docker compose` or `docker-compose` binary needed:

```bash
# Start a multi-service stack
pullrun compose up -f myapp/compose.yml

# View service logs
pullrun compose logs -f

# Rebuild and restart a specific service
pullrun compose build web
pullrun compose up -d web

# Stop everything
pullrun compose down
```

Compose files follow the standard format with support for build, volumes, ports, environment, secrets, networks, health checks, restart policies, and service dependencies. Each service runs as a container or VM depending on its `--backend` label.

---

## 🔐 Secrets & Configs

First-class encrypted secrets — data is AES-256-GCM encrypted at rest and only decrypted into the workload's tmpfs at runtime:

```bash
# Create an encrypted secret
pullrun secret create db_password secret data   # stdin
pullrun secret create api_key --file key.txt     # from file

# List and inspect
pullrun secret list
pullrun secret inspect db_password

# Use in a workload
pullrun run myapp:latest --secret db_password

# Create a config file (mounted into the workload)
pullrun config create nginx.conf --file ./nginx.conf
pullrun run nginx:latest --config nginx.conf
```

Secrets survive host reboots and are scoped to the daemon's store. They can be shared across services in a compose stack.

---

## 🌐 P2P Image Distribution

Nodes share image blocks peer-to-peer — only one node pulls from the registry, the rest sync delta blocks via gRPC:

```bash
# Node A: start daemon with sync enabled
pullrun-runtime daemon --sync-addr 0.0.0.0:9500

# Node B: connect and fetch blocks from Node A
pullrun-runtime daemon --sync-addr 0.0.0.0:9501 \
  --sync-peers node-a.example.com:9500
```

Each block is verified by content hash before acceptance — no trust required. The Bloom filter cache avoids redundant transfers for blocks already seen. See [docs/cross-node-dag-sync.md](docs/cross-node-dag-sync.md) for the full design.

---

## 🏗️ Build & Push

Native Dockerfile build engine — no Docker daemon required:

```bash
# Build a single-platform image
pullrun build -t myapp:latest .

# Build for multiple platforms
pullrun build -t myapp:latest --platform linux/amd64,linux/arm64 .

# Push to a registry
pullrun push myapp:latest

# Export/import for air-gapped environments
pullrun export myapp:latest > myapp.tar
pullrun import < myapp.tar
```

Builds use the DAG store directly — layers are content-addressed and deduplicated across images automatically. Export produces a single OCI-compatible tarball.

---

## 📊 Performance

| Metric | Value |
|--------|-------|
| First `alpine:3.18` pull | **968 ms** (~2× faster than Docker) |
| Container run latency | **~400 ms** |
| Apple Virt VM boot | **~160 ms** |
| gRPC `ListWorkloads` (warm) | **< 1 ms** |
| Daemon RSS at idle | **24.6 MiB** |
| Release binary size | **12 MB** |
| Test coverage | **126 Rust + 9 Go** |

---

## 🔧 Feature Comparison

| Feature | Docker CE | Pullrun |
|---------|:---------:|:-------:|
| Multi-arch pull/push/run | ✅ | ✅ |
| Multi-arch build | ✅ | ✅ |
| Secrets / Configs | ✅ | ✅ (AES-256-GCM encrypted) |
| Health checks | ✅ | ✅ |
| Restart policies | ✅ | ✅ |
| User-defined networks | ✅ | ✅ |
| Compose (up/down/ps/logs/build) | ✅ | ✅ |
| **P2P image distribution** | ❌ | ✅ |
| **VM backend from OCI** | WSL2 only | ✅ (Firecracker + Apple Virt) |
| **Cosign / SBOM gating** | ❌ | ✅ |

Full feature comparison: [docs/PULLRUN_GUIDE.md](docs/PULLRUN_GUIDE.md)

---

## 🏗️ Architecture

```
                               pullrun (Go CLI)
                    pull · run · build · compose · inspect
                               │
                               │ gRPC (UDS or TCP)
                               ▼
                    ┌────────────────────────────────────┐
                    │        pullrun-runtime             │
                    │  ┌────────┐    ┌─────────────┐     │
                    │  │ store  │    │  executor   │     │
                    │  │ (DAG)  │    │ (runc / VM) │     │
                    │  └──┬───┘    └──────┬──────┘     │
                    │     │                │            │
                    │  ┌──┴────────────────┴──┐         │
                    │  │     ProxyNetwork     │         │
                    │  │  IPAM · DNS · TCP/UDP │         │
                    │  └──────────────────────┘         │
                    └────────────────────────────────────┘
                               │
                    ┌──────────┴──────────┐
                    │     Kubernetes      │
                    │  CRI shim · Runtime │
                    │  Class · Prometheus │
                    └─────────────────────┘
```

> **Core invariant:** A `sha256:` digest is globally consistent. Every node that has pulled `alpine:3.18` stores byte-identical files on disk. This makes cross-node block sync trivial: content-addressed blocks can be verified without trust, transferred delta-only, and deduplicated across the entire cluster automatically.

---

## 📋 Prerequisites

| Tool | Required For | Minimum Version | Install |
|------|-------------|-----------------|---------|
| Rust + Cargo | Building runtime from source | 1.77+ | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh` |
| Go | Building CLI from source | 1.22+ | `brew install go` or `apt install golang` |
| protoc | Regenerating protobuf bindings | 3.0+ | `brew install protobuf` or `apt install protobuf-compiler` |

> **Binary users:** You only need `pullrun` and `pullrun-runtime` from the release tarball. The tools above are only for compiling from source.

---

## 📁 Project Layout

```
proto/            # Protobuf definitions (single source of truth)
proto-go/         # Generated Go protobuf code
runtime/          # Rust workspace (core data plane)
  pullrun-store/   # zero-copy DAG store
  pullrun-oci/     # OCI client + DAG converter
  pullrun-exec/    # executor trait + runc wrapper
  pullrun-vm/      # Firecracker + Apple Virt backends
  pullrun-net/     # IPAM, proxy, DNS, iptables
  pullrun-sync/    # P2P block sync (Bloom, mDNS, gossip)
  pullrun-policy/  # cosign, SBOM, seccomp gates
  pullrun-runtime/ # gRPC daemon
cli/pullrun/      # Go CLI (cobra)
cri/pullrun-cri/  # Kubernetes CRI shim
deploy/           # K8s manifests, Grafana dashboard, alerts
docs/             # Architecture, operations, policy
```

---

## 📚 Documentation

| Document | What You'll Find |
|----------|-----------------|
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Zero-copy store design, executor trait, network model |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | Deploying, monitoring, troubleshooting |
| [docs/POLICY.md](docs/POLICY.md) | Policy engine (cosign, SBOM, CVSS, license) |
| [docs/cross-node-dag-sync.md](docs/cross-node-dag-sync.md) | P2P block sync design |
| [docs/PULLRUN_GUIDE.md](docs/PULLRUN_GUIDE.md) | Full user guide for all platforms |
| [docs/PULLRUN_GUIDE.md](docs/PULLRUN_GUIDE.md) | Full user guide, lifecycle, persistence |

---

## 🤝 Contributing

We welcome contributions! Please see our documentation for:
- Architecture deep-dives
- Operations guides
- Policy engine details

---

## 📄 License

Apache 2.0 — see [LICENSE](LICENSE). Contributions are subject to the terms of [CLA.md](CLA.md).

---

<div align="center">

**Built with Rust 🦀 and Go 🐹**

</div>
