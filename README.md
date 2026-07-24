<div align="center">

<img src="https://raw.githubusercontent.com/pullrun/pullrun/main/assets/pullrun.png" alt="Pullrun Logo" width="400">

# **Pullrun**

### *One OCI image. Any execution target. CLI + runtime under 25 MB.*

**Run the same OCI image as a container, Firecracker microVM, Apple Silicon VM, Kubernetes workload, or AI agent task. No daemon required. No overlayfs. No separate VM images.**

[![CI](https://img.shields.io/github/actions/workflow/status/pullrun/pullrun/ci.yml?branch=main&logo=github&label=CI)](https://github.com/pullrun/pullrun/actions/workflows/ci.yml)
[![DOI](https://img.shields.io/badge/DOI-10.5281%2Fzenodo.20679669-007ec6?logo=doi)](https://doi.org/10.5281/zenodo.20679669)
[![Version](https://img.shields.io/badge/version-0.6.8-6A1B9A?logo=git)](https://github.com/pullrun/pullrun/releases)logo=git)](https://github.com/pullrun/pullrun/releases)
[![License](https://img.shields.io/github/license/pullrun/pullrun?logo=apache)](LICENSE)
[![macOS](https://img.shields.io/badge/macOS-Apple_Silicon-333?logo=apple&logoColor=white)](docs/PULLRUN_GUIDE.md)
[![Linux](https://img.shields.io/badge/Linux-x86__64_%7C_arm64-333?logo=linux&logoColor=white)](docs/PULLRUN_GUIDE.md)
[![Windows](https://img.shields.io/badge/Windows-WSL2_%7C_runc_%7C_Firecracker-0078D6?logo=windows&logoColor=white)](docs/WINDOWS.md)
[![Kubernetes](https://img.shields.io/badge/Kubernetes-CRI_shim-326CE5?logo=kubernetes&logoColor=white)](cri/pullrun-cri/)
[![MCP](https://img.shields.io/badge/MCP-native-6A1B9A?logo=protocol)](docs/ALL_MCP.md)
[![Rust](https://img.shields.io/badge/Rust-1.78+-dca282?logo=rust)](https://www.rust-lang.org)
[![Go](https://img.shields.io/badge/Go-1.25+-00ADD8?logo=go)](https://golang.org)
[![Tests](https://img.shields.io/badge/tests-175%20passing-brightgreen?logo=checkmarx)](#testing)
[![PRs](https://img.shields.io/badge/PRs-welcome-brightgreen?logo=gitpullrequest)](CONTRIBUTING.md)

</div>

<p align="center">
  <a href="#-install"><b>⚡ 30-Second Install</b></a> •
  <a href="#-quick-start"><b>🚀 Quick Start</b></a> •
  <a href="#-architecture"><b>🏗️ Architecture</b></a> •
  <a href="#-why-pullrun"><b>🎯 Why Pullrun?</b></a> •
  <a href="#-features"><b>🗺️ Feature Map</b></a> •
  <a href="#-kubernetes"><b>☸️ Kubernetes</b></a> •
  <a href="#-mcp-ai-integration"><b>🤖 AI Agents</b></a>
</p>

---

## ⚡ What is Pullrun?

Pullrun runs the same OCI image as a container or a VM. It stores layers in a content-addressed DAG (no overlayfs), syncs blocks peer-to-peer, and ships as a ~14 MB CLI + ~6 MB runtime daemon.

**Why this matters:** Modern infrastructure uses too many execution engines — Docker for dev, containerd for production, Firecracker for isolation, CRI for Kubernetes, MCP agents for AI. Each has its own image format, storage, and operational model — even though they all run the same OCI images. Pullrun collapses these layers into one runtime.

**Key differentiators:**
- **Containers and VMs from the same image** — no separate VM build step, no separate VM image format
- **Content-addressed DAG store** — zero-copy mmap reads, deduplicated by content hash, byte-identical across every node
- **P2P image distribution** — one registry pull per cluster, rest sync peer-to-peer at LAN speed
- **~14 MB CLI + ~6 MB runtime daemon** (stripped) — no daemon required by default (CLI-only `pullrun run`), optional daemon for background services

**Also included:** Kubernetes CRI shim (beta), Docker Compose support, MCP server for AI agents, policy engine (Cosign, SBOM, seccomp), P2P sync layer, and AES-256-GCM encrypted secrets — all in the same binary.

```bash
# Apple Silicon VM (macOS default) — 3 s
pullrun run alpine:3.18 --cmd "echo" --cmd "hello pullrun" --attach -t

# Firecracker microVM (Linux) — 400 ms
pullrun run alpine:3.18 --backend vm --cmd "echo" --cmd "hello pullrun" --attach -t

# Container (Linux) — 400 ms
pullrun run alpine:3.18 --cmd "echo" --cmd "hello pullrun"

# Windows WSL2 — same image, same command, same store
pullrun.exe run alpine:3.18 --cmd "echo" --cmd "hello pullrun"
```

---

## 📦 Install

```bash
# One command, any platform
curl -fsSL https://github.com/pullrun/pullrun/raw/main/install.sh | bash
```

| Platform | What you get |
|----------|-------------|
| **macOS** | `brew tap pullrun/tap && brew install pullrun` → native binary, no Xcode |
| **Linux** | APT package or direct download, systemd service (requires `runc` for containers, `/dev/kvm` for VMs) |
| **Windows** | `pullrun.exe` + WSL2 auto-provisioning, runc + Firecracker |

<details>
<summary>Manual install options</summary>

```bash
# macOS (Homebrew — pre-built, no build deps)
brew tap pullrun/tap && brew install pullrun

# From source
make build && export PATH="$PWD/bin:$PATH"

# Cross-compile Windows CLI
cd cli/pullrun && GOOS=windows GOARCH=amd64 CGO_ENABLED=0 go build -o pullrun.exe .
```
</details>

---

## 🚀 Quick Start

```bash
# ── Pull any OCI image ──────────────────────────────────────────
pullrun pull alpine:3.18     # 968 ms — benchmark script at hack/bench.sh

# ── Run as a container (Linux) ─────────────────────────────────
pullrun run alpine:3.18 --cmd "echo" --cmd "hello pullrun"

# ── Run as a microVM (macOS/Linux) ─────────────────────────────
pullrun run alpine:3.18 --backend vm --cmd "echo" --cmd "hello" --attach -t

# ── Interactive shell with detach ──────────────────────────────
pullrun run alpine:3.18 --tty --attach --cmd /bin/sh
#   Ctrl-P Ctrl-Q  → detach (workload keeps running)
#   pullrun exec <id> -t /bin/sh → re-attach

# ── Background workload ────────────────────────────────────────
pullrun run alpine:3.18 --cmd /bin/sleep --cmd 3600

# ── Build & push (no Docker daemon needed) ─────────────────────
pullrun build ./Dockerfile . -t myapp:latest
pullrun build ./Dockerfile . -t myapp:latest --platform linux/amd64,linux/arm64
pullrun push <root-digest> ghcr.io/myorg/myimg:latest

# ── Export/import for air-gapped ───────────────────────────────
pullrun save <digest> -o myimage.tar
pullrun load -i myimage.tar

# ── Compose (separate binary) ────────────────────────────────
pullrun-compose up -f myapp/compose.yml               # containers (default on Linux)
pullrun-compose up -f myapp/compose.yml --backend vm   # same compose, VM isolation
pullrun-compose logs -f
pullrun-compose down

# ── Lifecycle ──────────────────────────────────────────────────
pullrun list                  # all workloads (pending/running/exited)
pullrun stop <id>
pullrun exec <id> /bin/echo hello
# Re-attach to a detached workload (allocates a fresh PTY)
pullrun exec <id> -t -- /bin/sh
# Or, for bidirectional I/O streaming without a new shell:
pullrun workload run <id>

# ── Image management ──────────────────────────────────────────
pullrun images                 # list pulled images in local DAG store
pullrun rmi alpine:3.18       # remove an image, cascade-delete unreachable layers
pullrun rmi sha256:abc...     # also works by digest

# ── GC (DAG store garbage collection) ─────────────────────────
pullrun gc                    # dry-run — report what would be deleted
pullrun gc --apply            # actually delete unreachable nodes
pullrun gc --apply --force    # bypass 90% safety guard

# ── Events & Stats ─────────────────────────────────────────────
pullrun events --types=WORKLOAD_STARTED,POLICY_DENIED
pullrun stats <id>

# ── Network ────────────────────────────────────────────────────
pullrun network create my-net --subnet 10.43.1.0/24
pullrun run alpine:3.18 --net bridge

# ── Secrets ────────────────────────────────────────────────────
pullrun secret create db_password -                     # from stdin
pullrun run myapp:latest --secret db_password

# ── Configs ────────────────────────────────────────────────────
pullrun config create nginx.conf ./nginx.conf       # from file
pullrun config create nginx.conf -                   # from stdin
pullrun run nginx:latest --config nginx.conf

# ── Diff & Inspect ─────────────────────────────────────────────
pullrun diff <id>
pullrun inspect <id>
pullrun commit <id> myapp:snapshot

# ── Login ──────────────────────────────────────────────────────
pullrun login ghcr.io
pullrun logout

# ── MCP (AI agents) ────────────────────────────────────────────
pullrun mcp                   # stdio mode (for opencode, Claude Code)
pullrun mcp --sse :8080       # SSE mode (for remote agents)

# ── P2P sync ───────────────────────────────────────────────────
pullrun-runtime daemon --sync-addr 0.0.0.0:9500
```

---

## 🎯 Why Pullrun?

### 🏗️ Architecture is Everything

Docker's overlayfs store is a filesystem overlay — CVEs in overlayfs (CVE-2023-0386, CVE-2023-32629) can let a container escape to the host. Pullrun's content-addressed DAG store stores layers **as-is**, verified by content hash. No overlayfs, no escape.

Pullrun also mitigates **shared-kernel cross-container risks** that aren't specific to any storage driver — for example, kernel page-cache bugs like CVE-2026-31431 ("Copy Fail") affect all containers sharing a host kernel, regardless of the storage backend. Per-VM kernel isolation (Firecracker, Apple Virtualization) fully contains these. The DAG store makes the VM path zero-cost: the same image, no separate VM build step.

Pullrun is also **rootless by default** — no `sudo` needed, no daemon listening on a TCP socket, no attack surface from a central dockerd.

### 🔥 The Only Runtime with Containers + VMs from the Same Image

| Backend | Isolation | Pullrun | Docker | Podman |
|---------|-----------|:-------:|:------:|:------:|
| Linux containers (runc) | Process | ✅ | ✅ | ✅ |
| Firecracker microVMs | Per-VM kernel | ✅ | ❌ | ❌ |
| Apple Silicon VMs | Per-VM kernel | ✅ | ❌ | ❌ |
| Windows WSL2 containers | Process | ✅ | ✅ | ❌ |
| Windows Firecracker VMs | Per-VM kernel | ✅ | ❌ | ❌ |

**Same image, any isolation level.** No separate VM image build step. The OCI manifest IS the VM rootfs.

### 🧬 Content-Addressed DAG Store

Pullrun's store is built on [rkyv](https://github.com/rkyv/rkyv) + [mmap](https://github.com/RazrFalcon/memmap2-rs). OCI layers are stored once, deduplicated by content hash, and `mmap()`'d directly — no tar extraction, no overlayfs, no `dockerd` process owning the data.

> A `sha256:` digest is globally consistent. Every node that has pulled `alpine:3.18` stores **byte-identical** files on disk. This makes P2P sync trivial: blocks are verified by content hash, transferred delta-only, and deduplicated across the entire cluster.

### 📊 By the Numbers

| Metric | Pullrun | Docker |
|--------|:-------:|:------:|
| First `alpine:3.18` pull | **968 ms** | ~2 s |
| Container run latency | **~400 ms** | ~800 ms |
| Apple Virt VM boot | **~160 ms** | N/A |
| Firecracker VM cold boot | **~500 ms** | N/A |
| Firecracker VM warm pool | **~200 ms** | N/A |
| Idle daemon RSS | **24.6 MiB** | ~90 MiB |
| Binary size (stripped) | **~20 MB** (CLI + runtime) | ~75 MB |
| Rootless by default | ✅ | ❌ (`dockerd` as root) |
| Central daemon | Optional | Required |

<sup>Benchmarks: single-node, cold cache, `alpine:3.18` on Apple M3 (macOS 14) for Pullrun vs Docker Desktop 4.27. Container run latency measured from `run` command exit to workload PID alive. Apple VM boot measured to interactive `exec` prompt. Firecracker measurements on Linux x86_64 with KVM; warm pool configured with `--vm-warm-pool-size 4`. Pullrun daemon RSS measured after `pullrun pull alpine:3.18` + `pullrun run alpine:3.18 --tty --attach --cmd /bin/sh` then detached. Docker RSS from `docker run -d --name idle alpine:3.18 sleep 3600`.

Benchmarked with [`hyperfine`](https://github.com/sharkdp/hyperfine) (≥10 iterations, mean ± stddev reported). The benchmark script lives at [`hack/bench.sh`](hack/bench.sh) — run it yourself to reproduce the numbers.</sup>

---

## ✨ Features

### 🏃 Workload Lifecycle
`pull`, `run`, `stop`, `exec`, `attach`, `list`, `logs`, `stats`, `events`, `inspect`, `prune`, `rmi`, `gc` plus:

* **`commit`** — create a new image layer from a running container's current filesystem. The DAG store computes the delta against the original image, producing a content-addressed layer.
* **`diff`** — show file-level changes between a running workload and its original image. The store compares DAG trees to produce an add/modify/delete listing.
* **`update`** — live resource limit changes (CPU millicores, memory bytes) without restarting the workload.
* **`cp`** — bidirectional file copy between host and workload paths.
* **`save` / `load`** — OCI-compatible single-file tarball export/import for air-gapped environments. Re-import produces identical content hashes.
* **`login` / `logout`** — bearer token auth for private OCI registries.

### 🔧 Build
Native Dockerfile builder (`FROM`, `RUN`, `COPY`, `ADD`, `WORKDIR`, `ENV`, `CMD`, `ENTRYPOINT`) with content-addressed layer caching by instruction hash. Uses runc directly for `RUN` — no Docker daemon. Multi-platform builds via `--platform linux/amd64,linux/arm64`.

### 🏗️ OCI Kernel Images
Kernel binaries are first-class OCI content. `pullrun kernel install` downloads a vmlinux from an OCI registry into the DAG store. The `--kernel-image` flag lets any workload specify a custom kernel by OCI reference — same store-backed, content-addressed pipeline: verified by digest, cached forever, pushable to any registry.

### 🐳 Compose
Full Docker Compose-compatible workflow: `up`, `down`, `logs`, `ps`, `build`. Supports dependency ordering (topological sort), port mapping, environment variables, volumes (bind mounts), resource limits (CPU/memory), labels, and per-project bridge networks for isolation. Parses standard `docker-compose.yml` files via the [`compose-spec/compose-go`](https://github.com/compose-spec/compose-go) library.

Each service can run as a container or VM — use `--backend vm` to boot all services as Firecracker microVMs from the same compose file, no changes required.

### ☸️ Kubernetes CRI
Drop-in CRI shim at [`cri/pullrun-cri/`](cri/pullrun-cri/) — implement `RuntimeService` and `ImageService` from the Kubernetes CRI API. Maps pod sandboxes to pullrun workloads, supports RuntimeClass (`pullrun-container` / `pullrun-vm`), pod annotations for image/CPU/memory overrides, and streaming (exec, attach, port-forward).

A native **control-plane stub** lives in [`control-plane/`](control-plane/): `pullrun-controller` (scheduler) + `pullrun-agent` (per-node deployer) communicating over gRPC. This is a v1 work-in-progress with etcd, `.pullrun.local` DNS, and admission control planned — but the wire protocol is stable today.

### 🤖 MCP AI Integration
Native [Model Context Protocol](https://modelcontextprotocol.io) server exposing 15 runtime operations as MCP tools — `run`, `stop`, `exec`, `list`, `get`, `inspect`, `logs`, `stats`, `pull_image`, `list_images`, `build`, `push`, `prune`, `compose_up`, `compose_down` plus MCP resources (`pullrun://workload/{id}`, `pullrun://workload/{id}/logs`, `pullrun://store/info`, `pullrun://images`). Works in stdio mode (for opencode, Claude Code, Cursor) or SSE mode (for remote agents via HTTP).

### 🔐 Policy Engine
Gate workloads before they run — built-in support for:
- **Cosign** signature verification (Ed25519 key pairs, key ID matching)
- **SBOM** evaluation (CVSS scoring, ban by license)
- **Seccomp** profiles (`default` allowlist of ~50 syscalls, `unconfined`, or custom JSON)
- **Read-only rootfs** — prevents runtime tampering (write only to `--volume` mounts)
- **`no_new_privileges`** — blocks `setuid` and `capset` escalation
- Policy is declarative: `required_signature: true`, `max_cvss_score: 7.0`, `deny_licenses: ["GPL-3.0"]`

Compose all four: `--require-signature --readonly-rootfs --no-new-privileges --seccomp-profile default`.

### 🌐 P2P Image Distribution
Nodes share image blocks peer-to-peer via gRPC + Bloom filters. One node pulls from the registry, the rest delta-sync from each other. Features: mDNS/discovery for zero-config LAN peer finding, Bloom filter cache to avoid redundant transfers, gossip protocol for peer state, delta computation, registrar service for peer tracking.

### 📡 Networking
User-defined bridge networks with IPAM, inbound/outbound port forwarding, DNS resolution, and `iptables` integration. Four modes per workload via `--net`: **`isolated`** (default for containers, loopback only with host proxy on `10.42.0.1`), **`bridge`** (shared `pullrun-br0` bridge, inter-workload communication), **`slirp`** (default for VMs, userspace NAT via slirp4netns — no bridge, no iptables), **`host`** (shares host namespace), **`none`** (no network). Isolation is enforced by the proxy, not per-workload VLANs.

### 🗝️ Encrypted Secrets
AES-256-GCM encryption at rest, decrypted into workload tmpfs at runtime. `pullrun secret create/get/ls/inspect/rm` — data stays encrypted on disk, only the runtime process can decrypt.

### 🔄 Export/Import
Single-file OCI-compatible tarball export for air-gapped environments. Re-import produces identical content hashes.

### 📊 Events & Observability
Real-time event stream via `pullrun events` — `IMAGE_PULLED`, `WORKLOAD_STARTED`, `POLICY_DENIED`, etc. Per-workload `stats` with CPU/memory. Prometheus metrics exporter built into the daemon. PrometheusRule alerting config in [`deploy/`](deploy/).

### 🧹 DAG Store Garbage Collection & Reference-Counted rmi
`pullrun rmi` removes an image by tag or digest with **immediate cascade deletion** of unreachable subtree nodes. Per-node refcounts (`node.refcount` sidecar files) preserve shared layers — a layer referenced by another image is never deleted until the last referencing image is removed. Crash recovery via `recompute_all_refcounts` on daemon startup rebuilds refcounts from all tagged image and workload roots.

`pullrun gc` reclaims unreachable DAG nodes (orphaned layers, manifests, blobs) that are no longer reachable from any tagged image or running workload. Features: dry-run by default (report only), `--apply` to actually delete, `--force` to bypass the 90% safety guard, op-lock protection for in-flight operations, VM kernel image pinning via `kernel_image_digest`. The store no longer grows forever.

### 🖥️ Interactive Shells
Full TTY support with **detach/re-attach** via `Ctrl-P Ctrl-Q`. Works across all backends (container, Firecracker VM, Apple VM). Detached workloads keep running — re-attach with `pullrun exec --tty <id> /bin/sh`. Even works on exited workloads (daemon starts a fresh sleep container).

### ❤️ Health Checks & Restart Policies
`pullrun run --health-cmd 'curl -f http://localhost:80'` — periodic health checks with configurable interval, timeout, and retries. `--restart on-failure|always|unless-stopped` controls automatic restart on exit. Health state is surfaced via `pullrun inspect` and `pullrun events`.

### 🗄️ VM State Persistence
Stopped VMs behave like hibernated machines — all writes preserved, restart on demand with `exec`. Backend specifics: **Apple Virt** rootfs persists via VirtioFS, **Firecracker** retains the ext4 image, **Container (runc)** rootfs is ephemeral (only `--volume` mounts survive). No re-pull needed.

### 🔐 Private Registries
`pullrun login <registry>` for bearer token auth. Daemon accepts `--insecure-registry` for plain-HTTP mirrors and air-gapped LAN caches.

---

## ☸ Kubernetes

Pullrun ships a CRI shim in [`cri/pullrun-cri/`](cri/pullrun-cri/) that implements the Kubernetes Container Runtime Interface. It maps pod sandboxes to pullrun workloads and supports:

- `RuntimeClass` — `pullrun-container` for runc containers, `pullrun-vm` for Firecracker VMs
- Pod annotations: `pullrun.io/image`, `pullrun.io/cpu-millicores`, `pullrun.io/memory-bytes`
- Streaming: exec, attach, port-forward
- Image management via the DAG store

Deploy as a DaemonSet with manifests in [`deploy/`](deploy/):

```bash
kubectl apply -f deploy/runtime-daemon.yaml
kubectl apply -f deploy/serviceaccount.yaml
kubectl apply -f deploy/servicemonitor.yaml
```

---

## 🤖 MCP AI Integration

Any MCP-compatible AI agent (opencode, Claude Code, Cursor) can control pullrun through natural language:

```bash
# Start the MCP server
pullrun mcp

# In opencode or Claude Code, the agent can now:
#   "pull alpine and run it as a VM"
#   "exec into my-app and check the logs"
#   "show me all running workloads"
#   "run docker-compose up from my project"
```

The MCP server exposes 15 tools and 4 resource types. SSE mode for remote agents: `pullrun mcp --sse :8080`.

---

## 🌐 P2P Image Distribution

```bash
# Node A — seed
pullrun-runtime daemon --sync-addr 0.0.0.0:9500

# Node B — pulls delta blocks from Node A
pullrun-runtime daemon --sync-addr 0.0.0.0:9501 \
  --sync-peers node-a.example.com:9500
```

Each block verified by content hash before acceptance — no trust required.

---

## 🔧 Backend Comparison

| Backend | Isolation | Platform | Boot Time | Best For |
|---------|-----------|----------|:---------:|----------|
| 🐧 Linux Containers (runc) | Process (namespace) | Linux, macOS, Windows (WSL2) | ~400 ms | Dev, CI/CD, dense packing |
| 🔥 Firecracker microVM | Per-VM kernel (KVM) | Linux x86_64, Windows (WSL2 x86_64) | ~500 ms cold / ~200 ms warm pool | Multi-tenant, untrusted workloads, compliance, fast-recycling sandboxes |
| 🍎 Apple Virtualization | Per-VM kernel (Hypervisor.framework) | macOS Apple Silicon | ~160 ms | macOS dev, Apple Silicon CI |

---

## 🏗️ Architecture

```
                              pullrun (Go CLI)
                   pull · run · build · compose · inspect
                   events · stats · network · secret · mcp
                              │
                              │ gRPC (UDS or TCP)
                              ▼
                   ┌─────────────────────────────────────┐
                   │         pullrun-runtime             │
                    │  ┌──────────┐   ┌──────────────────┐ │
                    │  │ Store    │   │ Executor         │ │
                    │  │ (DAG)    │   │ runc / VM        │ │
                    │  │ └ rkyv   │   │ └ VmPool (warm)  │ │
                    │  │ └ mmap   │   │                  │ │
                    │  │ └ DashMap│   │                  │ │
                   │  └────┬─────┘   └──────┬───────┘    │
                   │       │                │            │
                   │  ┌────┴────────────────┴────┐       │
                   │  │       Network            │       │
                   │  │  IPAM · Proxy · DNS      │       │
                   │  └──────────────────────────┘       │
                   │                                      │
                   │  Sync · Policy · Secrets · Metrics   │
                   └──────────────────────────────────────┘
                              │
                   ┌──────────┴──────────┐
                   │      Kubernetes     │
                   │  CRI shim · Runtime │
                   │  Class · Prometheus │
                   └─────────────────────┘
```

The store uses `DashMap<Digest, Arc<Mmap>>` for lock-free concurrent reads — the first reader pays for `mmap()` + page faults, every subsequent reader is a single atomic load. The Rust workspace is eleven crates (store, oci, exec, vm, net, dns, vsock, sync, policy, runtime, init); the Go side is the CLI, CRI shim, compose, and control-plane stub.

---

## 📁 Project Layout

```
proto/            # Protobuf definitions (single source of truth)
runtime/          # Rust workspace — core data plane
  pullrun-store/   # Zero-copy DAG store (rkyv + mmap + DashMap concurrent cache)
  pullrun-oci/     # OCI client + DAG converter
  pullrun-exec/    # Executor trait + runc wrapper
  pullrun-vm/      # Firecracker + Apple Virt backends, warm VM pool (pool.rs), pullrun-init guest agent
  pullrun-net/     # IPAM, proxy, DNS, iptables, slirp4netns
  pullrun-dns/     # In-process DNS server for workload resolution
  pullrun-vsock/   # Vsock transport layer for VM guest-host communication
  pullrun-sync/    # P2P block sync (Bloom, mDNS, gossip)
  pullrun-policy/  # Cosign, SBOM, seccomp gates
  pullrun-runtime/ # gRPC daemon
cli/pullrun/      # Go CLI (cobra) — 30+ commands
cri/pullrun-cri/  # Kubernetes CRI shim
control-plane/    # Multi-node orchestration stub: pullrun-controller + pullrun-agent (v1 WIP)
cmd/pullrun-compose/ # Docker Compose-compatible CLI
deploy/           # K8s manifests, Prometheus rules, alerts
docs/             # Architecture, operations, policy, MCP
tools/            # Standalone smoke-test workspaces (apple-virt-exec, firecracker-smoke, etc.)
```

---

## 📚 Documentation

| Document | What You'll Find |
|----------|-----------------|
| [docs/PULLRUN_GUIDE.md](docs/PULLRUN_GUIDE.md) | Full user guide for all platforms |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | DAG store design, executor trait, network model |
| [docs/OPERATIONS.md](docs/OPERATIONS.md) | Deploying, monitoring, troubleshooting |
| [docs/POLICY.md](docs/POLICY.md) | Policy engine (cosign, SBOM, CVSS, license) |
| [docs/WINDOWS.md](docs/WINDOWS.md) | Windows/WSL2 setup, Firecracker, known issues |
| [docs/ALL_MCP.md](docs/ALL_MCP.md) | MCP server reference (AI agent integration) |
| [docs/cross-node-dag-sync.md](docs/cross-node-dag-sync.md) | P2P block sync design |

---

## 📄 Technical Report

A technical report describing the architecture is available in [`/paper`](./paper/pullrun_paper.pdf) and archived on Zenodo: https://doi.org/10.5281/zenodo.20679669

---

## 📄 License

Apache 2.0 — see [LICENSE](LICENSE). Contributions subject to [CLA.md](CLA.md).

---

<div align="center">

**Built with Rust 🦀 and Go 🐹**

</div>
