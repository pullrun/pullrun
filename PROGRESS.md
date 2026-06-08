# Nimbus — Build Progress

**Last updated:** 2026-06-08
**Status:** Fully independent of Docker. Phase D/E — resource limits, volumes, health checks, live stats, docker cp, build layer caching all verified. Deployed and running on server.
**Tests:** 101 Rust + 9 Go — all passing.
**New:** Resource limits (`--cpu`/`--memory` + `nimbusctl update`), volume/bind mounts, compose auth, live stats (`nimbusctl stats`), health checks (`--health-cmd`), docker cp (`nimbusctl cp`), build layer caching, container compatibility (/tmp, /dev/shm, /etc/hosts, /etc/resolv.conf).

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
| **Commit** | `docker commit` | ❌ | No running-container snapshot yet |
| **Diff** | `docker diff` | ❌ | No filesystem diff |
| **Volume** | `docker volume` | ✅ | Bind mounts via `--volume`/`-v` + compose volumes translation |
| **Network create** | `docker network` | ❌ | Single bridge; user-defined networks not supported |
| **Login** | `docker login` | ✅ | `nimbusctl login` stores in `~/.nimbus/auth.json`, auto-used by pull/push |
| **CP** | `docker cp` | ✅ | `nimbusctl cp` via CopyFile RPC |
| **Stats** | `docker stats` | ✅ | `nimbusctl stats` via GetWorkloadStats RPC + cgroupfs |
| **Export/Import** | `docker export/import` | Different | Nimbus has `save`/`load` in DAG-native format |
| **Info / Version** | `docker info` | ❌ | No system info command |
| **Secret / Config** | `docker secret` | ❌ | |
| **Healthcheck** | HEALTHCHECK | ✅ | Executor::exec() watcher loop + health state machine + `--health-cmd` |
| **Restart policy** | `--restart always` | ❌ | No auto-restart on exit |
| **Resource limits** | `--memory --cpus` | ✅ | CPU/memory limits + live update via `runc update` + `nimbusctl update --cpu --memory` |
| **Native build** | Dockerfile → layer cache | ✅ | SHA256 instruction cache in DagBuilder for RUN/COPY/ADD |
| **Multi-node** | Swarm / Compose | ❌ | Control plane stub only; no cross-node orchestration |

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

## Next steps (in priority order)

1. **DAG directory scan optimization** — `nimbusctl build` scanning large rootfses is very slow; needs parallel/streaming walk or incremental scanning.
2. **Proxy TCP reset fix** — Port forwarding to containers has TCP reset issues; investigate SPDY→TCP bridge or switch to raw TCP proxy.
3. **Disk management** — Server disk at 78% (8 GB total); add GC / prune policies, monitor store growth.
4. **Restart policies** — Auto-restart workloads on unexpected exit (Docker `--restart always` equivalent).
5. **Multi-node DNS** — `.nimbus.local` resolution across nodes via control plane.
6. **Push auth test** — Deploy local `registry:2` container on server, verify `push` + `pull` round-trip with auth.
7. **Commit / diff** — `docker commit` equivalent (running-container snapshot) and `docker diff` equivalent.
8. **Network create** — User-defined bridge networks; currently only a single flat bridge.
9. **Info / Version** — `nimbusctl info` command for system status reporting.
