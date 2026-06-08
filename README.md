# Nimbus

> **A content-addressed workload execution system where the runtime backend is an implementation detail. Fully Docker-independent.**

Nimbus pulls OCI images, deduplicates them into a zero-copy on-disk DAG
(rkyv + memmap2), and runs them in **whichever execution backend the
operator chooses** — Linux containers (runc), Firecracker microVMs, or
Apple Virtualization on macOS. The same image content can be
booted as a container or as a VM; the only thing that changes is the
backend.

## What it is — and what it isn't

| What Nimbus **is** | What Nimbus **is not** |
|---|---|
| A content-addressed storage + execution layer for OCI images | A Docker Desktop replacement (yet) |
| A multi-backend runtime (containers + VMs from the same DAG) | A container image builder |
| A policy-enforced execution path (cosign signatures, SBOM checks, license/CVSS gates) | A general-purpose orchestrator (no Kubernetes scheduler replacement; it integrates *with* Kubernetes via CRI) |
| A K8s RuntimeClass provider (`nimbus-container`, `nimbus-vm`) | An OCI registry (it pulls from registries, not serves them) |
| A reproducible artifact store (the same `sha256:abc` always runs identically) | A live-migration / CRIU snapshot product |

Nimbus is a **complement** to the container ecosystem, not a fork of
it. It pulls standard OCI images from any registry and runs them
through a layer that enforces policy, deduplicates storage, and gives
operators a choice of execution isolation level.

## Quick start

Prerequisites: Linux with `/dev/kvm` (for the VM backend; the
container backend needs only `runc`). macOS support is in progress
(Apple Virtualization). Build with `cargo build --release` and the
two binaries `target/release/nimbus-runtime` and
`target/release/nimbusctl` will appear.

```bash
# 1. Pull an image (deduplicates into the on-disk DAG store).
nimbusctl pull alpine:3.18

# 2. Run it. Choose the backend explicitly: container or vm.
nimbusctl run sha256:6a... \
    --backend container \
    --cmd /bin/sh,-c,'echo hello from nimbus'

# 3. Inspect the workload: state, DAG, network rules, policy decisions.
nimbusctl inspect wl-abc123

# 4. Stream runtime events (pulls, runs, exits, policy decisions).
nimbusctl events --follow
```

The CLI spawns the runtime as a child process over a Unix domain
socket; no daemon to manage. Use `--server host:port` to talk to a
long-lived runtime instead.

## Architecture

```
                              ┌───────────────────────┐
                              │     nimbusctl (Go)    │
                              │  pull · run · inspect │
                              │  events · list · logs │
                              └──────────┬────────────┘
                                         │ gRPC (UDS or TCP)
                                         ▼
   ┌─────────────────────────────────────────────────────────────┐
   │                       nimbus-runtime                         │
   │                                                             │
   │   ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
   │   │  pull_image │  │ run_workload│  │   EventBus          │ │
   │   │  + policy   │─▶│  + policy   │  │  (broadcast, 1024)  │ │
   │   └──────┬──────┘  └──────┬──────┘  └─────────────────────┘ │
   │          │                │                                 │
   │          ▼                ▼                                 │
   │   ┌─────────────────────────────────────────────────────┐   │
   │   │           MmapStore  (rkyv + memmap2)               │   │
   │   │  ┌──────────┐  ┌──────────┐  ┌──────────────────┐   │   │
   │   │  │ Manifest │─▶│   Tree   │─▶│ Layer / Blob DAG │   │   │
   │   │  └──────────┘  └──────────┘  └──────────────────┘   │   │
   │   │     content-addressed, lock-free DashMap cache       │   │
   │   └─────────────────────────────────────────────────────┘   │
   │          │                │                                 │
   │          ▼                ▼                                 │
   │   ┌──────────────┐  ┌────────────────┐                     │
   │   │  LinuxContai-│  │ Firecracker    │  (same IPAM,         │
   │   │  nerExecutor │  │ Executor       │   same bridge)      │
   │   │   (runc)     │  │   (KVM)        │                      │
   │   └──────────────┘  └────────────────┘                     │
   │          │                │                                 │
   │          ▼                ▼                                 │
   │   ┌─────────────────────────────────────────────────────┐   │
   │   │          ProxyNetwork (10.42.0.0/16)                │   │
   │   │   userspace TCP/UDP proxy + DNS + IPAM + iptables   │   │
   │   └─────────────────────────────────────────────────────┘   │
   └─────────────────────────────────────────────────────────────┘
                                         │
                                         ▼
   ┌─────────────────────────────────────────────────────────────┐
   │      Kubernetes integration (CRI shim, RuntimeClass)         │
   │      Prometheus metrics (axum /metrics, ServiceMonitor)      │
   │      Grafana dashboard (6 panels, 5 alerts)                 │
   └─────────────────────────────────────────────────────────────┘
```

**The single most important property**: a `sha256:` digest of an OCI
image is the *same* on every node that has pulled it. The DAG nodes
are content-addressed; the on-disk file names are the digests. Two
nodes that have pulled `alpine:3.18` will have byte-identical files
on disk. This is what makes a workload reproducible across the
cluster.

### What lives where

| Crate / module | Purpose |
|---|---|
| `runtime/nimbus-store` | rkyv-encoded DAG nodes stored in mmap'd files; DashMap cache for zero-copy reads |
| `runtime/nimbus-oci` | OCI registry client (token auth, manifest fetch, layer fetch) + converter to DAG |
| `runtime/nimbus-exec` | Execution backends: `LinuxContainerExecutor` (runc) and the executor trait |
| `runtime/nimbus-vm` | Firecracker microVM executor (KVM on Linux) + Apple Virt executor (macOS) |
| `runtime/nimbus-net` | IPAM, userspace TCP/UDP proxy, DNS, iptables MASQUERADE for VM outbound |
| `runtime/nimbus-policy` | Cosign signature verification, CycloneDX SBOM scanning, CVSS / license gates |
| `runtime/nimbus-runtime` | The gRPC daemon: pulls, runs, inspects, streams events, exposes `/metrics` |
| `cli/nimbusctl` | Go CLI; thin wrapper over the gRPC API |
| `cri/nimbus-cri` | Kubernetes CRI shim exposing `nimbus-container` and `nimbus-vm` RuntimeClasses |
| `proto-go/` | Shared Go module: `nimbus/protoapi` (rebuilt via `make proto`) |
| `control-plane/` | Multi-node control plane stub (gRPC API server + agent; persistence deferred to v1) |
| `deploy/` | Kubernetes manifests: DaemonSet, ServiceMonitor, PrometheusRule, Grafana dashboard |
| `tools/` | Standalone smoke-test binaries (e.g. `vm-outbound-smoke`) for hosts without the full workspace |

## Kubernetes integration

Nimbus registers as a CRI runtime. Pods can request a `RuntimeClass`
to choose the backend:

```yaml
# runtimeclass.yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: nimbus-vm
handler: nimbus-vm
---
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: nimbus-container
handler: nimbus-container
```

A pod that wants the VM isolation level just specifies
`runtimeClassName: nimbus-vm`. The CRI shim maps the RuntimeHandler
to the appropriate executor. **No code change to the pod; no
custom admission webhook.**

For observability, the runtime exposes a Prometheus `/metrics` HTTP
endpoint. The `deploy/` directory ships a `ServiceMonitor` (scrape
every 30s) and a `PrometheusRule` with five alerts:

- `NimbusRuntimeDown` (critical) — `up == 0` for 2 minutes
- `NimbusPullFailureRate` (warning) — failed pulls > 25% over 5 minutes
- `NimbusWorkloadCrashLoop` (warning) — non-zero exit codes > 0.1/s over 10 minutes
- `NimbusPullLatencyHigh` (warning) — pull p95 > 30s over 10 minutes
- `NimbusStoreGrowingFast` (info) — store growing > 1 GB / hour over 30 minutes

A Grafana dashboard (`deploy/grafana-dashboard.json`) covers pull
rate, workloads running, pull/start latency p50/p95/p99, exit
codes, and store size.

## Performance: what we measure, not what we promise

Honest numbers from real hosts:

**Linux + KVM dev host** (Scaleway `root@51.159.130.114`, Ubuntu
24.04, AMD EPYC 7282, 2 vCPUs, 2 GB RAM, ~1 GB free disk):

| Metric | Value | How it was measured |
|---|---|---|
| Firecracker smoke test (boot + shutdown) | **4.9 s** | `tools/firecracker-smoke`: host kernel + Alpine minirootfs |
| OCI image pull (alpine:3.18, first pull) | **968 ms** (nimbus) vs **1805 ms** (Docker) | `nimbusctl pull` vs `docker pull`; nimbus ~2x faster |
| OCI image pull (cached, second pull) | **921 ms** (nimbus) vs **898 ms** (Docker) | Both cached locally; comparable |
| OCI image pull (cross-image alpine:3.19) | **956 ms** (nimbus) vs **1789 ms** (Docker) | nimbus file-level dedup skips identical files |
| Storage per image | **18 MB** (nimbus, 444 files) vs **11.5 MB** (Docker overlay) | DAG store has per-file overhead but dedups across images |
| Workload run latency | **~5 s** (nimbus VM boot + workload + shutdown) vs **~0.4 s** (Docker container) | VM boot is the overhead; containers start near-instantly |
| nimbus-runtime release binary | **12 MB** | `cargo build --release -p nimbus-runtime` on x86_64 Linux |
| Linux build fix verified | Both `nimbus-vm` + `nimbus-runtime` compile on x86_64 | `AppleVirtAttachHandle` ZST + `dispatch2` cfg-gate |

**Architectural moat** — nimbus eliminates CVE classes Docker cannot fix:

| CVE | Docker (overlayfs) | Nimbus (per-VM kernel) |
|---|---|---|
| CVE-2026-31431 | Shared overlayfs page cache → cross-container read | Per-VM kernel, no shared page cache |
| CVE-2023-0386 / CVE-2023-32629 | overlayfs copy-up container escape | read-only VirtioFS from host DAG store |
| General overlayfs CVEs | Every kernel release has ~5-10 new overlayfs bugs | No overlayfs in the data path; rootfs is a DAG-store-backed file system |

**macOS dev host** (Apple M-series, this repo's dev machine):

| Metric | Value | How it was measured |
|---|---|---|
| Apple Virt VM boot + workload exec (standalone) | **~2.2 s** | `tools/apple-virt-exec` — boots VM, runs `echo HELLO_FROM_SH`, exits |
| `nimbusctl run --backend=vm` end-to-end | **verified** | `nimbusctl` → runtime gRPC → VM boot → vsock connect → workload result |
| `nimbus-runtime` daemon RSS at idle | **24.6 MiB** | `ps -o rss=` against a live daemon |
| gRPC `ListWorkloads` roundtrip, warm | **< 1 ms** | 4 consecutive calls after dial (Python timed) |
| gRPC `ListWorkloads` roundtrip, cold | **~320 ms** | First call includes process spawn + UDS dial |
| Debug build size (nimbus-runtime) | 33.8 MB | `target/debug/nimbus-runtime` |
| Debug build size (nimbusctl) | 15.2 MB | `target/debug/nimbusctl` |
| Full test suite runtime | **0.8 s** | `cargo test --workspace` from warm cache |
| `cargo test --workspace` (this repo) | **92 tests pass** | 83 lib + 9 vsock; + `go test ./...` = 9 Go tests passing |

What we **don't** claim (yet): high-throughput proxy throughput,
hot container spawn latency, eBPF fast-path numbers, and
cluster-scale performance. These are deferred until we have a
reason to optimize them (real load + a real eBPF implementation).

## Development status

**Working today (verified end-to-end on real hardware):**

- DAG store with rkyv + memmap2 + DashMap (zero-copy concurrent reads)
- OCI pull → DAG conversion → content-addressed deduplication (gzip fix for Docker Hub)
- OCI image index support (platform-aware child selection, skip attestations)
- **Push** — DAG-to-OCI layer reconstruction + registry upload (monolithic PUT)
- **Save/Load** — DAG-native tar export/import with content-addressed dedup
- Container execution via runc on Linux
- Rootless container execution (user namespace + pasta/slirp4netns, auto-detected)
- Firecracker VM boot from DAG-materialized ext4 rootfs (verified on real KVM host)
- Apple Virt VM boot + workload exec (verified both standalone and via `nimbusctl run --backend=vm`)
- VM networking: tap (`ioctl TUNSETIFF`, rootless) + bridge + userspace inbound proxy + iptables MASQUERADE outbound
- Container networking: veth pairs, bridge IP, default route, proxy port mapping
- Per-project bridge isolation (deterministic /24 from bridge name hash)
- `/init` wrapper injection for OCI images booted as VMs
- OCI kernel pull for Firecracker (via StagedKernel from OCI image)
- Policy engine: cosign signatures, CycloneDX SBOM, CVSS / license gates
- CRI shim + RuntimeClass mapping (`nimbus-container`, `nimbus-vm`)
- CRI `kubectl exec`, `kubectl attach`, `kubectl port-forward` (SPDY→TCP bridge)
- `nimbus compose up/down/ps/logs/build` with dependency ordering + per-project bridge
- Prometheus metrics + Grafana dashboard + 5 alert rules
- K8s deployment manifests (DaemonSet, ServiceMonitor, PrometheusRule)
- `nimbusctl inspect`, `nimbusctl events`, `nimbusctl workload run` (attach)
- `nimbusctl build` (native DAG-aware builder — no Docker needed)
- `nimbusctl login`, `nimbusctl logout` (registry auth stored in `~/.nimbus/auth.json`, 0600)
- `--kernel-image` and `--registry` flags for VM backends
- Control plane: gRPC API server, network-aware scheduling, file-backed persistence
- Cross-OS development (macOS + Linux workspaces)
- Linux build: `nimbus-vm` + `nimbus-runtime` compile on x86_64
- Architectural moat: per-VM kernel isolation eliminates overlayfs CVEs (CVE-2026-31431, CVE-2023-0386, CVE-2023-32629)
- DAG store advantages: file-level dedup across images, zero-copy mmap across N VMs, instant snapshots via 32-byte root digest, no decompress-on-pull overhead
- Rootless ext4 creation via `mkfs.ext4 -d` (no loop-mount, no root)
- Rootless TAP via `ioctl(TUNSETIFF)` (requires `setcap cap_net_admin=eip`)
- **Fully Docker-independent** — no Docker daemon, CLI, containerd, or overlayfs anywhere

**Stubs / partial:**

- Build layer caching (based on Dockerfile instruction hash for incremental builds)
- Cross-node service discovery (`.nimbus.local` DNS across cluster)
- NetworkPolicy K8s integration
- eBPF/XDP fast-path for the userspace proxy
- Windows WSL2 forwarding
- iptables NAT rules still require `CAP_NET_ADMIN` or root (TAP and ext4 are rootless)
- `nimbusctl login/logout` with `--password-stdin` for CI usage
- Volume / bind mounts (compose volumes field exists but not wired through executors)
- `docker cp` equivalent (no copy to/from container)
- `docker stats` equivalent (live CPU/mem reporting)
- Resource limits (CPU/memory cgroup constraints enforced via `nimbusctl run --cpu --memory`; live update via `nimbusctl update`)
- Restart policies / health checks

**Test coverage: 92 Rust tests pass** (83 lib + 9 vsock). **9 Go tests pass**
(`go test ./...` from `cli/nimbusctl`).

## Repository layout

```
proto/            protobuf service definitions (single source of truth)
proto-go/         generated Go protobuf code (shared module)
runtime/          Rust workspace: store, oci, net, policy, exec, vm, runtime
cli/nimbusctl/    Go CLI (cobra)
cri/nimbus-cri/   Go CRI shim
control-plane/    Go control-plane stub (API server + agent)
deploy/           Kubernetes manifests (DaemonSet, ServiceMonitor, etc.)
tools/            Standalone smoke-test binaries (e.g. vm-outbound-smoke)
docs/             Deeper architecture, operations, and policy docs
```

## How to contribute / what to read next

- **`docs/ARCHITECTURE.md`** — the zero-copy store, the executor trait, the network model
- **`docs/OPERATIONS.md`** — deploying, monitoring, troubleshooting
- **`docs/POLICY.md`** — how the policy engine composes (cosign, SBOM, CVSS, license)
- **`PROGRESS.md`** — what's been done, what's next (single source of truth for status)
- **`WARNINGS.md`** — the gotchas we hit; read this *before* opening a PR

## License

TBD. The codebase is currently unlicensed; please contact the
maintainers before redistributing.
