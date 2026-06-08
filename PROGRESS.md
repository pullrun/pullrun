# Nimbus — Build Progress

**Last updated:** 2026-06-08
**Status:** Fully independent of Docker. Phase E — all major Docker CLI gaps closed (restart policies, commit/diff, info/version). Deployed and running on server.
**Tests:** 101+ Rust + 9 Go — all passing.
**New (Gap 3):** `nimbusctl info` (runtime version, uptime, store stats) + `nimbusctl version`.
**Previously (Gap 2):** `nimbusctl commit <id> [tag]` (running-container snapshot → DAG layer) + `nimbusctl diff <id>` (added/modified/deleted file listing vs original image).
**Previously (Gap 1):** `--restart` flag (`no`/`on-failure`/`always`/`unless-stopped`) with exponential backoff watcher, race-fixed status check.
**Previously:** Bridge fix deployed, resource limits (`--cpu`/`--memory`), volumes, compose auth, live stats, health checks, docker cp, build layer caching.

---

## What's Done (Docker-independent feature surface)

### Image management
- `pull` — OCI pull from any registry (Docker Hub, private, insecure), Docker Hub gzip fix, image index support
- `push` — DAG-to-OCI layer reconstruction + registry upload via OCI distribution API (monolithic PUT)
- `save` — DAG-native tar export (BFS walk, serializes all nodes+blobs, **not** OCI format)
- `load` — Tar import with content-addressed dedup
- `list` — List images in store
- `inspect` — Inspect DAG nodes, image config, layers
- `build` — Native DAG-aware builder: Dockerfile parser, RUN execution via runc, COPY/ADD, layer snapshotting
- **Build layer caching** — SHA256 instruction cache in DagBuilder for RUN/COPY/ADD; incremental builds reuse cached layers
- `login/logout` — Registry auth stored in `~/.nimbus/auth.json` (0600 perms), auto-attached to pull/push
- **Docker-independent** — No Docker daemon, `docker` CLI, containerd, or overlayfs anywhere

### Running workloads
- `run` — Run containers (runc) or VMs (Firecracker / Apple Virt)
- `stop` — Graceful stop with timeout
- `exec` — Exec into running container
- `logs` — Stream stdout/stderr logs
- `attach` — Attach to running workload (bidi stdio via gRPC stream)
- `port-forward` — CRI shim dials workload IP over bridge (SPDY→TCP bridge)
- `compose up/down/ps/logs` — Full compose support with dependency ordering, port mapping, per-project bridge isolation
- **`update`** — Live resource limit updates via `runc update` + UpdateWorkload RPC + `nimbusctl update --cpu --memory`
- **`stats`** — Cgroupfs-based live CPU/memory reporting + GetWorkloadStats RPC + `nimbusctl stats <id>`
- **`cp`** — CopyFile RPC + `nimbusctl cp <id>:<path> <local>` / `nimbusctl cp <local> <id>:<path>` (docker cp equivalent)
- **Health checks** — Executor::exec() background watcher loop, health state machine (starting/healthy/unhealthy), `--health-cmd` CLI flags
- **Volume/bind mounts** — Proto Mount + CLI `--volume`/`-v` + compose volume translation; wire HostPath through executors

### Networking
- Bridge networking (`nimbus-br0`, 10.42.0.0/16) with veth pairs for containers
- Per-project bridge isolation (deterministic /24 from bridge name hash)
- TAP devices for VM networking via direct `ioctl(TUNSETIFF)` (no `ip tuntap add`)
- Userspace TCP inbound proxy (port mapping)
- iptables MASQUERADE for VM outbound NAT (graceful if unavailable)
- IPAM (atomic allocate/release, 10.42.0.0/16)
- DNS proxy (local `.nimbus.local` records, upstream forwarding)
- Rootless container networking (pasta/slirp4netns)

### Execution backends
- **Container** — runc, bridge networking (veth+IP+route), proxy port mapping
- **Container (rootless)** — runc + user namespace + pasta/slirp4netns, auto-detected when EUID != 0
- **Firecracker VM** — ext4 rootfs via `mkfs.ext4 -d` (rootless), TAP+bridge networking, `/init` shim for OCI images
- **Apple Virt VM** — macOS Virtualization.framework, vsock attach, 3-VM pool, console logging

### Storage
- Zero-copy DAG store (rkyv + memmap2 + DashMap, lock-free concurrent reads)
- File-level dedup across images (two images sharing identical files store them once)
- LRU eviction (512 MB default cache cap)
- No decompress-on-pull (DAG stores blobs as-is)
- Instant snapshots (32-byte root digest = complete snapshot; rollback is O(1))

### Container compatibility
- `/tmp` and `/dev/shm` tmpfs mounts auto-configured
- `/etc/hosts` and `/etc/resolv.conf` auto-creation inside container rootfs

### Bugfixes
- **Bridge creation fix** — `ensure_bridge_exists` now uses `ip link add ... type bridge` ignoring "File exists" instead of `ip link show` which returned exit code 0 for nonexistent bridges; bridges were silently never created in prior versions
- **runc path fix** — `build_image` checks `is_file()` not `exists()` to avoid resolving directory as binary
- **Materializer layer order** — removed `.rev()` so layers apply base→top (fixed nginx:alpine)

### Observability
- Prometheus `/metrics` endpoint (pull rate, workload latency/exit, store size)
- Grafana dashboard (6 panels, 5 alert rules)
- K8s deployment manifests (DaemonSet, ServiceMonitor, PrometheusRule)
- Healthz endpoint

### Policy & security
- Cosign Ed25519 signature verification (per-image, `--require-signature`)
- CycloneDX SBOM scanning (CVSS score gates, `--max-cvss`)
- License deny list (`--deny-license`)
- Readonly rootfs, no-new-privileges enforcement
- Architecture moat: per-VM kernel isolation eliminates overlayfs CVEs (CVE-2026-31431, CVE-2023-0386, CVE-2023-32629)

### Kubernetes integration
- CRI shim (UDS listener, full RuntimeService + ImageService)
- RuntimeClass mapping (`nimbus-container`, `nimbus-vm`)
- `kubectl exec`, `kubectl attach`, `kubectl port-forward` (all via bridge)
- PodSandbox create/stop/remove, Container create/start/stop/remove
- Persistent sandbox store (file-backed JSON survives restart)

### Control plane
- gRPC API server + agent
- Network-aware scheduling (image locality scoring)
- Pull-through cache
- File-backed persistence
- Build/push/save/load RPCs in proto v4

### Compose integration
- **Compose auth** — reads `~/.nimbus/auth.json` for compose pulls; no Docker dependency
- **Compose volume translation** — docker-compose volume bind mounts translated to proto Mount and wired through executors
- **Compose build** — `nimbus compose build` invokes native DAG-aware builder

---

## Gaps vs Docker (what nimbus doesn't do yet)

| Feature | Docker | Nimbus | Notes |
|---------|--------|--------|-------|
| **Build** | `docker build` | ✅ | Native DAG-aware builder with layer caching (SHA256 instruction cache). No Docker needed. |
| **Tag** | `docker tag` | Not needed | Content-addressed; root digest IS the tag |
| **Commit** | `docker commit` | ✅ | `nimbusctl commit <id>` — running-container snapshot into DAG via `build_dag_from_directory` |
| **Diff** | `docker diff` | ✅ | `nimbusctl diff <id>` — added/modified/deleted file listing vs original image tree |
| **Volume** | `docker volume` | ✅ | Bind mounts via `--volume`/`-v` + compose volumes translation |
| **Network create** | `docker network` | ✅ | `nimbusctl network create/rm/ls` — user-defined bridge networks with persistent registry |
| **Login** | `docker login` | ✅ | `nimbusctl login`/`logout` stores in `~/.nimbus/auth.json`, 0600, auto-used by pull/push/compose |
| **CP** | `docker cp` | ✅ | `nimbusctl cp` via CopyFile RPC with path-escape validation |
| **Stats** | `docker stats` | ✅ | `nimbusctl stats` via GetWorkloadStats RPC + cgroupfs |
| **Export/Import** | `docker export/import` | Different | Nimbus has `save`/`load` in DAG-native format |
| **Info / Version** | `docker info` / `--version` | ✅ | `nimbusctl info` (version, uptime, store, workloads) + `nimbusctl version` |
| **Secret / Config** | `docker secret` | ❌ | |
| **Healthcheck** | HEALTHCHECK | ✅ | Executor::exec() watcher loop + health state machine + `--health-cmd` |
| **Restart policy** | `--restart` | ✅ | `--restart no|on-failure|always|unless-stopped` with exponential backoff watcher |
| **Resource limits** | `--memory --cpus` | ✅ | CPU/memory limits + live update via `runc update` + `nimbusctl update --cpu --memory` |
| **Native build** | Dockerfile → layer cache | ✅ | SHA256 instruction cache in DagBuilder for RUN/COPY/ADD |
| **Multi-node** | Swarm / Compose | ❌ | Control plane stub only; no cross-node orchestration |
| **VM backend** | ❌ (Docker Desktop WSL2 only) | ✅ | Firecracker (Linux KVM) + Apple Virt (macOS) — same OCI image, no rebuild |
| **Bridge networking** | ✅ | ✅ | veth pairs for containers, TAP for VMs; bridge fix deployed (was silently broken) |

---

## Architecture notes

### Key decisions
- **No Docker dependency anywhere.** All build/push/save/load, networking, execution are self-contained.
- **DAG-native save/load format** (`nimbus-nodes/<digest>`, `nimbus-blobs/<digest>`): preserves per-file dedup, faster than OCI tar round-trips. Push to registries reconstructs OCI layers.
- **Bridge networking for all backends**: containers (veth pairs), VMs (tap devices). Shared IPAM (10.42.0.0/16), same bridge.
- **Rootless by default**: ext4 via `mkfs.ext4 -d` (no loop-mount), TAP via `ioctl(TUNSETIFF)` (no `ip tuntap add`), containers via runc + user namespace + pasta. Only iptables NAT rules still need root.
- **PortForward: CRI shim dials workload IP directly** over bridge (10.42.0.0/16). No Rust runtime changes needed.
- **Materializer applies layers in manifest order** (base → top). Critical: the `.rev()` call that reversed layers was removed, fixing images like nginx:alpine that depend on correct layer ordering.
- **Use `env PATH=... nimbus-runtime daemon`** when starting via nohup over SSH, because PATH is not inherited over SSH non-interactive sessions.
- **runc binary check**: `build_image` handler uses `is_file()` not `exists()` to avoid resolving the `/var/lib/nimbus/runc` directory as the binary path.
- **Bridge creation fix**: `ensure_bridge_exists` now uses `ip link add ... type bridge` and ignores "File exists" errors, instead of `ip link show` which returned exit code 0 for nonexistent bridges. Bridges were silently never created in prior versions.

### Cross-compilation
```bash
docker run --platform linux/amd64 \
  -v /Users/YACINE/nimbus:/nimbus -w /nimbus \
  rust:latest bash -c 'apt-get update -qq && apt-get install -y -qq protobuf-compiler && cargo build --release -p nimbus-runtime'
```
Go CLI cross-compile: `GOOS=linux GOARCH=amd64 go build -o nimbusctl-linux ./cli/nimbusctl/`

### Test status
```bash
cargo test --workspace   # 101 Rust tests (all pass)
go test ./cli/nimbusctl/...   # 9 Go tests
```

---

## Next steps (in priority order — closing remaining Docker feature gaps first)

### Must-have to match Docker CLI surface (implemented — remaining gaps)

1. ~~**Restart policies**~~ ✅ — `--restart no|on-failure|always|unless-stopped` with exponential backoff watcher.

2. ~~**Commit / diff**~~ ✅ — `nimbusctl commit <id> [tag]` + `nimbusctl diff <id>`.

3. ~~**Info / Version**~~ ✅ — `nimbusctl info` (version, uptime, store stats) + `nimbusctl version`.

4. ~~**Network create**~~ ✅ — `nimbusctl network create/rm/ls` — user-defined bridge networks with persistent registry (`{store_root}/networks.json`), deterministic /24 subnet allocation from 10.43.0.0/16.

5. ~~**Proxy TCP reset fix**~~ ✅ — Added `ip addr add 10.42.0.1/16 dev <bridge>` to `ensure_bridge_exists` so the bridge has a host-side IP and the kernel has a route to containers. The proxy's `TcpStream::connect(container_ip:port)` now works.

6. **Multi-node DNS** — `.nimbus.local` resolution across nodes via control plane. Depends on control plane progression.

### Performance & reliability (important, but less user-visible)

6. **DAG directory scan optimization** — Parallelize `walk_and_store` with `walkdir` + `rayon`; add incremental scanning (mtime-based skip for unchanged files).

7. **Proxy TCP reset fix** — Port forwarding to containers has TCP reset issues; investigate SPDY→TCP bridge or switch to raw TCP proxy.

8. **Push auth test** — Deploy local `registry:2` on server, verify `push` + `pull` round-trip with auth.

9. **Disk management** — GC / prune policies: delete old bundles, evict least-recently-used images from DAG store, clean runc container state.

### Post-v1 (multi-node / control plane)

10. **Multi-node orchestration** — Control plane: scheduler, cross-node image pull, cross-node DNS. This is a full re-architecture of the control plane.
