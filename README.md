# Nimbus

> **A content-addressed workload execution system. Containers or VMs from the same OCI image. No Docker required.**

Nimbus pulls OCI images, deduplicates them into a zero-copy on-disk DAG
(rkyv + memmap2), and runs them in **whichever execution backend the
operator chooses** — Linux containers (runc), Firecracker microVMs, or
Apple Virtualization on macOS. The same image content can be
booted as a container or as a VM; the only thing that changes is the
backend.

**All Docker CE features covered** — multi-arch, secrets/configs, health
checks, restart policies, user-defined networks, resource limits, live
stats, compose, commit, cp, diff, and cross-node P2P image distribution
via DAG block sync. See [PROGRESS.md](PROGRESS.md) for the gap analysis.

## Quick start

```bash
# Build nimbusctl (once)
make build-go
# or: cd cli/nimbusctl && go build -o ../../bin/nimbusctl .

# 1. Pull an image (deduplicates into the on-disk DAG store).
nimbusctl pull alpine:3.18

# 2. Pull for a different architecture (resolves multi-arch image indexes):
nimbusctl pull alpine:3.18 --platform linux/arm64

# 3. Run it as a container (Linux) or VM (macOS):
nimbusctl run alpine:3.18 --backend container --cmd echo hello   # Linux only
nimbusctl run alpine:3.18 --backend vm       --cmd echo hello    # Linux+KVM

# 4. Native DAG-aware build (--platform overrides FROM --platform):
nimbusctl build -t myapp:latest --platform linux/arm64

# 5. Secrets and configs (Docker --secret/--config equivalent):
nimbusctl secret create db_password secret data
nimbusctl run myapp:latest --secret db_password

# 6. Enable peer-to-peer block sync for multi-node image distribution:
nimbus-runtime daemon --sync-addr 0.0.0.0:9500

# 7. (macOS) Apple Virt VM standalone tools — see docs/NIMBUS_GUIDE.md
```

The CLI uses `--direct` mode by default: it spawns the runtime as a
child process over a Unix domain socket. Use `--server host:port` to
talk to a long-lived runtime. Cross-platform: Go CLI runs everywhere.

## Architecture

```
                               ┌─────────────────────────────────┐
                               │       nimbusctl (Go)            │
                               │  pull · run · inspect · build   │
                               │  compose · stats · cp · update  │
                               │  secret · config · network      │
                               └────────────┬────────────────────┘
                                            │ gRPC (UDS or TCP)
                                            ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │                          nimbus-runtime                              │
   │                                                                      │
   │   ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌──────────┐  │
   │   │  pull_image │  │ run_workload│  │UpdateWorkload│  │CopyFile  │  │
   │   │  + policy   │─▶│  + policy   │  │  + stats     │  │ RPC      │  │
   │   └──────┬──────┘  └──────┬──────┘  └──────┬──────┘  └────┬─────┘  │
   │          │                │                │              │         │
   │          ▼                ▼                ▼              ▼         │
   │   ┌─────────────────────────────────────────────────────────────┐   │
   │   │                  MmapStore (rkyv + memmap2)                 │   │
   │   │  ┌──────────┐  ┌──────────┐  ┌──────────────────────────┐  │   │
   │   │  │ Manifest │─▶│   Tree   │─▶│ Layer / Blob DAG + Cache │  │   │
   │   │  └──────────┘  └──────────┘  └──────────────────────────┘  │   │
   │   │     content-addressed, lock-free DashMap cache              │   │
   │   └─────────────────────────────────────────────────────────────┘   │
   │          │                │                │                        │
   │          ▼                ▼                ▼                        │
   │   ┌──────────────┐  ┌────────────────┐  ┌──────────────────────┐  │
   │   │  LinuxContai-│  │ Firecracker    │  │  Health check        │  │
   │   │  nerExecutor │  │ Executor       │  │  watcher loop        │  │
   │   │   (runc)     │  │   (KVM)        │  │  (exec + state M/C)  │  │
   │   └──────┬───────┘  └──────┬─────────┘  └──────────────────────┘  │
   │          │                 │                                       │
   │          ▼                 ▼                                       │
   │   ┌──────────────────────────────────────────────────────────────┐ │
   │   │               ProxyNetwork (10.42.0.0/16)                    │ │
   │   │   userspace TCP/UDP proxy + DNS + IPAM + iptables            │ │
   │   │   cgroupfs stats reader + resource limit updater             │ │
   │   └──────────────────────────────────────────────────────────────┘ │
   └─────────────────────────────────────────────────────────────────────┘
                                            │
                                            ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │      Kubernetes integration (CRI shim, RuntimeClass)                 │
   │      Prometheus metrics (axum /metrics, ServiceMonitor)              │
   │      Grafana dashboard (6 panels, 5 alerts)                         │
   └─────────────────────────────────────────────────────────────────────┘
```

**The single most important property**: a `sha256:` digest of an OCI
image is the *same* on every node that has pulled it. The DAG nodes
are content-addressed; the on-disk file names are the digests. Two
nodes that have pulled `alpine:3.18` will have byte-identical files
on disk. This is what makes a workload reproducible across the
cluster — and it's what makes **cross-node block sync** trivial:
content-addressed blocks can be verified without trust, transferred
delta-only without a registry, and deduplicated across all images
and all nodes automatically.

```
   Node A: [a, b, c, d]          Node B: [a, e, f]
              │                         │
              ▼                         ▼
      ┌─────────────────┐      ┌─────────────────┐
      │  BlockSync       │◄────►│  BlockSync       │  gRPC bidirectional
      │  + Bloom filter  │      │  + Bloom filter  │  (have_blobs, get_blobs)
      └─────────────────┘      └─────────────────┘
              │                         │
              ▼                         ▼
         Upstream Registry         (fallback only)
```

When a kubelet requests an image pull via CRI, the puller first
queries peer nodes via `BlockSync` for missing blobs, then falls
back to the upstream registry. One node pulls the full image; the
rest of the cluster delta-syncs. See `docs/cross-node-dag-sync.md`.

## CLI features

| Command | What it does |
|---------|-------------|
| `nimbusctl pull` | Pull OCI image from any registry (Docker Hub, private, insecure) |
| `nimbusctl push` | Push DAG to OCI registry |
| `nimbusctl build` | Native DAG-aware Dockerfile builder (no Docker needed) |
| `nimbusctl run` | Run workload in container or VM (`--attach` for single-step run+attach) |
| `nimbusctl stop` | Graceful stop with timeout |
| `nimbusctl exec` | Exec into running container |
| `nimbusctl logs` | Stream stdout/stderr |
| `nimbusctl workload run` | Bidi stdio attach to running workload (vm backend) |
| `nimbusctl inspect` | Inspect image or workload details |
| `nimbusctl list` | List images and workloads |
| `nimbusctl stats` | Live CPU/memory cgroup stats |
| `nimbusctl update` | Live resource limit updates |
| `nimbusctl cp` | Copy files between host and container |
| `nimbusctl commit` | Snapshot running container as DAG layer |
| `nimbusctl diff` | Show changed files vs original image |
| `nimbusctl save` / `load` | DAG-native tar export/import |
| `nimbusctl secret` | Create/list/inspect/remove secrets (AES-256-GCM encrypted) |
| `nimbusctl config` | Create/list/inspect/remove configs (plain text) |
| `nimbusctl network` | Create/remove/list user-defined bridge networks |
| `nimbusctl login` / `logout` | Registry auth storage |
| `nimbusctl compose` | Compose up/down/ps/logs/build |
| `nimbusctl info` / `version` | System info and version |

## Kubernetes integration

Nimbus registers as a CRI runtime. Pods can request a `RuntimeClass`
to choose the backend:

```yaml
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
`runtimeClassName: nimbus-vm`. **No code change to the pod; no
custom admission webhook.**

Prometheus metrics (`/metrics`), a Grafana dashboard (6 panels, 5
alert rules), and K8s deployment manifests are in `deploy/`.

## What lives where

| Crate / module | Purpose |
|---|---|
| `runtime/nimbus-store` | rkyv-encoded DAG nodes in mmap'd files; DashMap cache |
| `runtime/nimbus-oci` | OCI registry client + DAG converter |
| `runtime/nimbus-exec` | Execution backends: LinuxContainerExecutor (runc) + trait |
| `runtime/nimbus-vm` | Firecracker (KVM) + Apple Virt (macOS) executors |
| `runtime/nimbus-net` | IPAM, userspace TCP/UDP proxy, DNS, iptables |
| `runtime/nimbus-sync` | P2P DAG block sync: BloomFilter, BlockSync gRPC, mDNS, SyncPuller, gossip, registrar |
| `runtime/nimbus-policy` | Cosign signatures, CycloneDX SBOM, CVSS/license gates, seccomp |
| `runtime/nimbus-runtime` | The gRPC daemon: pulls, runs, inspect, events, metrics, health, stats, secrets, configs |
| `cli/nimbusctl` | Go CLI; thin wrapper over the gRPC API |
| `cri/nimbus-cri` | Kubernetes CRI shim with RuntimeClass support |
| `proto-go/` | Shared Go proto module (`nimbus/protoapi`) |
| `deploy/` | K8s manifests: DaemonSet, ServiceMonitor, PrometheusRule, Grafana |
| `proto/` | Protobuf service definitions (single source of truth) |
| `control-plane/` | Node registry (being superseded by P2P block sync) |

## Performance highlights

| Metric | Value |
|---|---|
| Firecracker VM boot (kernel + exit) | **4.9 s** |
| `alpine:3.18` pull (first time) | **968 ms** — ~2x faster than Docker |
| Container run latency | **~400 ms** |
| gRPC ListWorkloads (warm) | **< 1 ms** |
| Daemon RSS at idle | **24.6 MiB** |
| Release binary size | **12 MB** |
| Test suite | **126 Rust + 9 Go — all passing** |

**Architectural moat** — per-VM kernel isolation eliminates overlayfs
CVEs (CVE-2026-31431, CVE-2023-0386, CVE-2023-32629) that Docker
cannot fix.

## Repository layout

```
proto/            Protobuf definitions (single source of truth)
proto-go/         Generated Go protobuf code
runtime/          Rust workspace: store, oci, net, policy, exec, vm, sync, runtime
cli/nimbusctl/    Go CLI (cobra)
cri/nimbus-cri/   Go CRI shim
control-plane/    Node registry stub
deploy/           Kubernetes manifests, Grafana dashboard
tools/            Standalone smoke-test binaries
docs/             Architecture, operations, policy docs
```

## How to contribute / what to read next

- **`docs/ARCHITECTURE.md`** — the zero-copy store, executor trait, network model
- **`docs/OPERATIONS.md`** — deploying, monitoring, troubleshooting
- **`docs/POLICY.md`** — policy engine (cosign, SBOM, CVSS, license)
- **`docs/cross-node-dag-sync.md`** — P2P block sync architecture
- **`PROGRESS.md`** — gap analysis and what's next
- **`WARNINGS.md`** — known pitfalls

## License

TBD. The codebase is currently unlicensed; please contact the
maintainers before redistributing.
