# Pullrun — Master Progress Tracker

**Last updated:** 2026-06-11  
**Current overall status:** v0.1.x — Docker CE feature parity complete. Beginning Phase 1 (Operational Hardening).  
**Current test count:** 126 Rust tests + 9 Go tests — all passing.

### Recent fixes

| Bug | Root cause | Fix | Status |
|-----|-----------|-----|--------|
| `runc exec -t` fails with "open /dev/tty: no such device or address" | Daemon has no terminal; `runc exec -t` requires a host PTY | Allocate host PTY via `posix_openpt`/`grantpt`/`unlockpt` and pass slave fd as stdin/stdout/stderr to `runc exec -t`. Replaced broken `--console-socket` approach (blocked on Ubuntu runc 1.3.4). | ✅ Fixed in `service.rs:allocate_pty()` |
| `pullrun list` shows all workloads as `exited` on macOS | Watcher marks backend=="vm" workload as exited after 5s | Skip VM workloads in watcher error handler (`service.rs:645`) and checkpoint recovery (`service.rs:490`) | ✅ Fixed |
| VM lifecycle has no state machine | `record_workload_state` sets `status:"running"` for un-booted VMs; status never transitions | Initial status = `"pending"`. `AttachWorkload` transitions `pending`→`running` on VM boot. `on_exit` callback transitions `running`→`exited` on VM exit. Checkpoint written at every transition. | ✅ Fixed in `service.rs`, `pullrun-vm/src/attach.rs` |
| `exec -t` after workload ID not parsed as flag | Cobra `SetInterspersed(false)` treats `-t` after ID as positional arg | Remove `SetInterspersed(false)` and manually scan command args for `-t`/`--tty`. Both `exec <id> -t -- <cmd>` and `exec -t <id> -- <cmd>` work. | ✅ Fixed in `commands.go` |
| `spawn_vm` has no exit notification mechanism | Runtime service cannot detect when VM background thread exits | Add `on_exit: Option<Box<dyn FnOnce() + Send>>` parameter to `spawn_vm`. Callback fires after background thread cleanup, updates status to `exited` and removes from `persistent_vms`. | ✅ Fixed in `pullrun-vm/src/attach.rs` |
| Daemon not signed with Virtualization entitlement | `cargo build` strips code signatures | Document `make apple-sign-daemon`. Run it after every build. | ✅ Documented in README.md + Makefile |

---

> **How to use this document**  
> This file is the single source of truth for what has been built and what must be built next. Each Phase section below contains numbered sub-items with one of these statuses:  
> -  `🔄 NOT STARTED` — we have not touched this yet.  
> -  `⏳ IN PROGRESS` — someone is working on it right now (add your name and date).  
> -  `✅ DONE` — code merged, tests passing, fully integrated.  
> -  `📝 TODO` — decreed as needed but not yet assigned or scheduled.  
>
> When a session ends, update every `⏳ IN PROGRESS` you touched to `✅ DONE` (or back to `🔄 NOT STARTED` if not finished), update the "Last updated" line, and update the test count.

---

## Phase 0 — COMPLETED (Pre-2026-06-09)

This section archives everything that existed *before* we started the phased roadmap. It is locked-read-only; do not edit. Every item listed here was completed and merged before this file was created.

### Functionality completed pre-Phase-1

| Feature | Status | Key files / notes |
|---------|--------|-------------------|
| Docker CE feature parity | ✅ DONE | All `docker` CLI commands replicated in `pullrun` |
| `pullrun pull` | ✅ DONE | OCI pull, multi-arch via `--platform` |
| `pullrun push` | ✅ DONE | DAG-to-OCI layer reconstruction |
| `pullrun build` | ✅ DONE | DAG-aware Dockerfile builder |
| `pullrun run/stop/exec/logs/attach` | ✅ DONE | Full workload lifecycle |
| `pullrun inspect/list` | ✅ DONE | DAG introspection |
| `pullrun stats` | ✅ DONE | Live cgroupfs CPU/memory |
| `pullrun update` | ✅ DONE | Live `runc update` for CPU/memory |
| `pullrun cp` | ✅ DONE | `CopyFile` RPC, host↔container |
| `pullrun commit` | ✅ DONE | Snapshot running container → DAG layer |
| `pullrun diff` | ✅ DONE | File-level diff vs original image tree |
| `pullrun save/load` | ✅ DONE | DAG-native tar export/import |
| `pullrun secret/config` | ✅ DONE | AES-256-GCM secrets, plain configs, `--secret` / `--config` on run |
| `pullrun network` | ✅ DONE | User-defined bridge networks, /24 allocation from 10.43.0.0/16 |
| `pullrun login/logout` | ✅ DONE | Registry auth in `~/.pullrun/auth.json` (0600) |
| `pullrun compose` | ✅ DONE | `up/down/ps/logs/build`, dependency ordering, per-project bridge isolation |
| `pullrun info/version` | ✅ DONE | Runtime version, uptime, store stats |
| Multi-arch pull | ✅ DONE | `--platform`, resolves multi-arch image indexes |
| Multi-arch build | ✅ DONE | `--platform` + `FROM --platform`, binfmt_misc auto-registration |
| Multi-arch run | ✅ DONE | Cross-arch via qemu-user-static, auto-registered on daemon start |
| Manifest list as DAG node | ✅ DONE | `NodeKind::ManifestList`, `build_multi()`, `push_manifest_list()` |
| Cross-arch push | ✅ DONE | `DagPusher::push()` auto-detects manifest vs manifest list |
| Health checks | ✅ DONE | `Executor::exec()` watcher, state machine, `--health-cmd` |
| Restart policies | ✅ DONE | `no/on-failure/always/unless-stopped`, exponential backoff |
| Volume/bind mounts | ✅ DONE | Proto `Mount`, CLI `--volume`/`-v`, compose volume translation |
| Bridge networking (single segment) | ✅ DONE | `pullrun-br0` 10.42.0.0/16, veth pairs, TAP for VMs |
| Userspace TCP/UDP proxy | ✅ DONE | Inbound port mapping, auto-promote to bridge mode |
| iptables MASQUERADE | ✅ DONE | VM outbound NAT, auto-detect outbound interface |
| IPAM | ✅ DONE | Atomic allocate/release, 10.42.0.0/16 |
| DNS proxy | ✅ DONE | `.pullrun.local` records, upstream forwarding |
| Rootless containers | ✅ DONE | runc + user namespace + pasta/slirp4netns |
| Firecracker VM backend | ✅ DONE | ext4 rootfs via `mkfs.ext4 -d`, TAP+bridge, `/init` shim |
| Apple Virt VM backend | ✅ DONE | macOS Virtualization.framework, vsock attach, 3-VM pool |
| Volume mounts (VirtioFS) | ✅ DONE | Host→VM directory sharing via `VZVirtioFileSystemDeviceConfiguration`. Persistent data across VM restarts. Read-only (`:ro`) support. |
| Zero-copy DAG store | ✅ DONE | rkyv + memmap2 + DashMap, lock-free reads |
| LRU in-memory cache (256 MB each) | ✅ DONE | Node cache + blob cache with `VecDeque` LRU |
| Content-addressed dedup | ✅ DONE | File-level dedup across images, automatic on write |
| Layer materialization | ✅ DONE | Hardlinks → shared on-disk bytes |
| P2P DAG block sync (Phases 1-3) | ✅ DONE | BlockSync gRPC, mDNS, gossip bloom exchange, SyncPuller, Registrar |
| Cosign signature verification | ✅ DONE | Ed25519, per-image, `--require-signature` |
| SBOM scanning | ✅ DONE | CycloneDX, CVSS gates, license deny list |
| Policy engine | ✅ DONE | `PolicyEngine::evaluate_for_image()`, signature + SBOM + seccomp |
| CRI shim | ✅ DONE | RuntimeService + ImageService, `pullrun-container` / `pullrun-vm` RuntimeClass |
| K8s integration | ✅ DONE | `kubectl exec/attach/port-forward`, PodSandbox + Container lifecycle |
| Prometheus metrics | ✅ DONE | `/metrics` endpoint, pull rate, workload latency/exit, store size |
| Grafana dashboard | ✅ DONE | 6 panels, 5 alert rules |
| K8s manifests | ✅ DONE | DaemonSet, ServiceMonitor, PrometheusRule |

---

## Phase 1 — Operational Hardening & Performance (Target: 8 weeks)

> **Goal:** Transform pullrun from a developer tool into a production-grade daemon that can run unattended for months without manual intervention.

### 1.1 DAG Store Garbage Collection & Pruning

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Implement reference-counted garbage collection for the DAG store to prevent unbounded storage growth. The store currently grows monotonically — every pull, build, commit, and load adds nodes that are never removed.

**Sub-tasks:**

- [ ] Create new crate `runtime/pullrun-gc` with the following core types:
  - `GcPolicy` enum with variants:
    - `ReachableOnly` — keep nodes reachable from any tagged image or running workload
    - `LruBytes(u64)` — evict least-recently-used nodes when total store size exceeds threshold
    - `TimeBased(Duration)` — remove unreferenced nodes older than TTL
    - `Hybrid { max_bytes: u64, protect_tags: bool }` — LRU + tag protection (default)
  - `GarbageCollector` struct with:
    - `store: Arc<MmapStore>`
    - `policy: GcPolicy`
    - `protected: RwLock<HashSet<Digest>>` — roots that must never be collected
    - `background_interval: Duration` — how often to run (default: 1 hour)

- [ ] Build reachability analysis:
  - BFS from all protected roots (tagged images → manifests → configs → layers → trees → blobs)
  - Track visited digests in a `HashSet<Digest>`
  - Any digest not in the visited set is a candidate for deletion

- [ ] Implement safe deletion:
  - Remove node files from disk (`<store_root>/aa/bb/.../node.rkyv`)
  - Remove blob files from disk (`<store_root>/aa/bb/.../blob.raw`)
  - Evict from in-memory caches (`DashMap::remove`)
  - Must be atomic: either fully deleted or not touched (no partial state)

- [ ] CLI integration:
  - `pullrun prune` — manual trigger, respects policy
  - `pullrun prune --dry-run` — report what would be deleted without deleting
  - `pullrun prune --force` — ignore policy, delete everything unreachable
  - Daemon flag `--gc-interval <seconds>` (default: 3600)

- [ ] Daemon background task:
  - Spawn `tokio::spawn` loop that runs GC at `gc_interval`
  - Skip if store size is below a threshold (e.g. < 100 MB — no point)
  - Emit Prometheus metric `pullrun_gc_runs_total` (counter) and `pullrun_gc_freed_bytes` (gauge)
  - Log at INFO level: freed bytes, nodes removed, duration

- [ ] Tests:
  - Unit: `GarbageCollector::collect()` on a synthetic store with known orphaned nodes
  - Unit: verify protected roots are never deleted
  - Integration: run `pullrun pull`, `pullrun prune --dry-run`, verify no false positives
  - Integration: run `pullrun pull`, `pullrun stop`, `pullrun prune`, verify orphaned layers are removed

**Tests to add:** 5 Rust unit + 2 integration  
**Estimated effort:** 2 weeks  

---

### 1.2 Write-Ahead Logging (WAL) for Crash Recovery

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Add a lightweight WAL to `MmapStore` to ensure crash-atomicity of writes. A crash during `put()` or `put_blob()` can leave partially-written files or stale in-memory cache state.

**Sub-tasks:**

- [ ] Create new module `runtime/pullrun-store/src/wal.rs`:
  - `WriteAheadLog` struct with:
    - `log_path: PathBuf` (e.g. `<store_root>/.pullrun/wal`)
    - `pending: Vec<WalEntry>` (buffered entries)
  - `WalEntry` enum:
    - `PutNode { digest: Digest, path: PathBuf }`
    - `PutBlob { digest: Digest, path: PathBuf }`
    - `Delete { digest: Digest }`

- [ ] WAL write protocol:
  - Before writing any node or blob: append serialized `WalEntry` to WAL file
  - `fsync()` the WAL (guarantees durability)
  - Perform the actual file write (node.rkyv or blob.raw)
  - After successful write: remove the WAL entry (truncate or mark as committed)
  - On failure: WAL entry remains, will be replayed on next startup

- [ ] WAL replay on startup:
  - In `MmapStore::new()`, check if WAL file exists
  - If yes: replay each uncommitted entry
    - For `PutNode` / `PutBlob`: re-attempt the write (idempotent — store is content-addressed)
    - For `Delete`: re-attempt the deletion
  - After replay: truncate WAL to empty
  - If replay fails: log FATAL and refuse to start (corruption detected)

- [ ] Performance considerations:
  - WAL is append-only, not a database log (no LSM tree needed)
  - Use `std::fs::OpenOptions::append(true)` + `std::os::unix::fs::OpenOptionsExt::sync_all(true)`
  - Batch frequent small writes: buffer entries for up to 10ms or 1MB, then fsync

- [ ] Tests:
  - Unit: simulate crash after WAL append but before file write → replay recovers
  - Unit: simulate crash during file write → replay re-attempts, dedup prevents duplicate
  - Integration: `SIGKILL` the daemon mid-pull → restart, verify store is consistent

**Tests to add:** 4 Rust unit + 1 integration  
**Estimated effort:** 1 week  

---

### 1.3 Async I/O & Parallelization for Store Operations

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Replace all blocking file I/O in `MmapStore` with async `tokio::fs` and add batch/parallel operations. Currently some paths (especially in `put_blocking`, `get`) use blocking syscalls inside async contexts, which starves the tokio runtime.

**Sub-tasks:**

- [ ] Audit all blocking I/O in `MmapStore`:
  - `put_blocking()` → convert to async `put()`
  - `put_blob_blocking()` → convert to async `put_blob()`
  - `get()` → the `mmap()` itself is inherently blocking; wrap in `tokio::task::spawn_blocking` or use `tokio::fs::read` for small entries
  - `get_blob()` → same as above

- [ ] Add batch operations:
  - `put_batch(nodes: &[DagNode]) -> Vec<Result<Digest, StoreError>>`
    - Spawns `tokio::spawn` per node for true concurrency
    - Use `futures::future::join_all` to await
    - Limit concurrency with `tokio::sync::Semaphore` (default: CPU count)
  - `get_batch(digests: &[Digest]) -> Vec<Result<Arc<Mmap>, StoreError>>`
    - Same pattern

- [ ] Replace callers:
  - `DagBuilder::build()` — parallelize RUN/COPY/ADD layer creation
  - `OciToDagConverter::convert()` — parallelize layer conversion
  - `pullrun-runtime/src/service.rs` — async gRPC handlers should not call blocking methods

- [ ] Cache eviction improvements:
  - Current LRU uses `VecDeque` + `Mutex`, which serializes all cache ops
  - Consider `dashmap` for LRU head tracking, or replace with `schnellru` / `lru` crate (tested, proven)
  - Goal: `get()` should be lock-free in the hot path ( DashMap hit )

- [ ] Tests:
  - Benchmark: `put_batch` vs serial `put` for 100 nodes (expect 3-5x speedup on SSD)
  - Benchmark: `get_batch` vs serial `get` for 100 digests
  - Stress test: 1000 concurrent `get()` calls on same digest (no deadlock, no cache corruption)

**Tests to add:** 3 Rust unit + 2 benchmark  
**Estimated effort:** 1 week  

---

### 1.4 NFTables/iptables Abstraction & Rootless Networking Completion

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

The current networking code hardcodes `iptables` for NAT and forwarding. This requires root for NAT and is incompatible with modern distros moving to `nftables` (Fedora, RHEL, Debian-testing). We need a pluggable `NetBackend` trait plus a true rootless fallback.

**Sub-tasks:**

- [ ] Create new module `runtime/pullrun-net/src/backend.rs`:
  - `NetBackend` trait (async_trait):
    - `setup_masquerade(subnet: &str, bridge: &str) -> Result<(), NetError>`
    - `add_forward_rule(from: &str, to: &str, port: u16) -> Result<String, NetError>` (returns rule ID)
    - `remove_rule(rule_id: &str) -> Result<(), NetError>`
    - `list_rules() -> Result<Vec<Rule>, NetError>`

- [ ] Implement backends:
  - `IptablesBackend` — current behavior, extracted from `container.rs`
  - `NftablesBackend` — uses `nft` CLI with equivalent rules (handle both `nftables` and `iptables-nft`)
  - `RootlessBackend` — no iptables/nftables at all; relies on `pasta` / `slirp4netns` for everything (NAT, port forwarding resolved via socket activation)

- [ ] Auto-detection on daemon startup:
  - Check `which iptables` → if present and works, use `IptablesBackend`
  - Else check `which nft` → if present, use `NftablesBackend`
  - Else if EUID != 0, try `RootlessBackend` (requires `pasta` or `slirp4netns`)
  - If none available: log warning, run in loopback-only mode (no outbound NAT, no port forwarding)

- [ ] Rootless TAP creation:
  - Already partially working: `ioctl(TUNSETIFF)` on `/dev/net/tun` with `cap_net_admin`
  - Ensure `setcap cap_net_admin=eip` is documented and tested in CI
  - Bridge creation still needs `CAP_NET_ADMIN`; document that `pasta` can replace bridge for rootless

- [ ] Tests:
  - Unit: each backend produces equivalent iptables/nftables rules for same input
  - Integration: start daemon with each backend, verify VM can reach internet (outbound NAT)
  - Integration: rootless mode — start daemon as non-root, verify `pullrun run` works

**Tests to add:** 4 Rust unit + 3 integration  
**Estimated effort:** 2 weeks  

### Phase 1 Summary

| Sub-task | Status | Effort | Tests | Entry point |
|----------|--------|--------|-------|-------------|
| 1.1 DAG GC | `🔄 NOT STARTED` | 2 wks | 5 + 2 | `runtime/pullrun-gc` |
| 1.2 WAL | `🔄 NOT STARTED` | 1 wk | 4 + 1 | `runtime/pullrun-store/src/wal.rs` |
| 1.3 Async I/O | `🔄 NOT STARTED` | 1 wk | 3 + 2 | `runtime/pullrun-store/src/store.rs` |
| 1.4 Net backends | `🔄 NOT STARTED` | 2 wks | 4 + 3 | `runtime/pullrun-net/src/backend.rs` |
| **Total** | | **6 wks** | **16 + 8 = 24** | |

**Definition of done for Phase 1:**
- `pullrun prune` works and is documented
- Daemon survives simulated `kill -9` and restarts cleanly (WAL replay)
- `cargo test --workspace` passes with new tests added
- Rootless mode works on Linux without root (documented, not experimental)

---

## Phase 2 — Enterprise Readiness (Target: 10 weeks)

> **Goal:** Make pullrun deployable in regulated, multi-team Kubernetes environments with standard operational tooling.

### 2.1 CNI Plugin Mode

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Package the pullrun network stack as a CNI plugin binary (`pullrun-cni`) that can be dropped into any Kubernetes cluster. Currently pullrun networking works for direct-mode workloads but is not a CNI plugin — kubelet cannot delegate to it.

**Sub-tasks:**

- [ ] Create new crate `runtime/pullrun-cni`:
  - Binary target: `pullrun-cni` (installed to `/opt/cni/bin/`)
  - Reads CNI config from stdin (JSON per CNI spec)
  - Config file location: `/etc/cni/net.d/10-pullrun.conf`
  - Communicates with `pullrun-runtime` gRPC over UDS at `/var/run/pullrun/pullrun-cni.sock`

- [ ] CNI ADD flow:
  1. `pullrun-cni` invoked by kubelet/cni with env vars `CNI_COMMAND=ADD`, `CNI_CONTAINERID`, `CNI_NETNS`
  2. Parse CNI config JSON, extract `name` (network name), `runtimeSocket` (optional)
  3. gRPC `CniAddRequest { container_id, netns_path, network_name }` to runtime
  4. Runtime: IPAM allocation for this network namespace → create veth pair → move one end into netns → attach other to bridge → return `CniAddResult`
  5. `pullrun-cni` returns standard CNI result JSON: `{ "cniVersion": "1.0.0", "ips": [...], "routes": [...] }`

- [ ] CNI DEL flow:
  1. `CNI_COMMAND=DEL` → gRPC `CniDelRequest { container_id }`
  2. Runtime: delete veth pair, release IPAM, clean up any iptables/nftables rules
  3. Return empty success JSON

- [ ] CNI CHECK flow (optional, CNI spec v1.0):
  - Verify the container's network state matches what was configured
  - If mismatch, return error so kubelet can restart the pod

- [ ] Packaging:
  - Default CNI config template in `deploy/cni/10-pullrun.conf`
  - Helm chart / DaemonSet that installs `pullrun-cni` and the config
  - Tested with `kind` (Kubernetes in Docker) and `k3s`

- [ ] Tests:
  - Unit: parse CNI input JSON correctly
  - Integration: `kind` cluster with pullrun CNI, verify pod-to-pod communication
  - Integration: `kubectl exec` into pod, verify network is functional

**Tests to add:** 3 Rust unit + 3 integration (kind cluster)  
**Estimated effort:** 3 weeks  

---

### 2.2 Multi-Tenant Workload Namespaces

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Add a `Namespace` concept to isolate workloads, networks, and resources per team/project. Currently everything shares the global `pullrun-br0` and single IPAM pool — no multi-tenancy.

**Sub-tasks:**

- [ ] Proto additions (`proto/pullrun/runtime.proto`):
  - `message Namespace { string name; map<string, string> labels; NetworkConfig network; bytes resource_quota; }`
  - Add `string namespace = 1;` to `RunRequest`, `ListWorkloadsRequest`, etc.
  - New RPCs: `CreateNamespace`, `DeleteNamespace`, `ListNamespaces`

- [ ] Store schema:
  - Namespaces are lightweight metadata, not DAG nodes
  - Store in JSON files: `<store_root>/namespaces/<name>.json`
  - Fields: `name`, `created_at`, `subnet`, `labels`, `resource_quota`

- [ ] Network isolation:
  - Each namespace gets its own bridge (e.g. `pullrun-br-<namespace>`) or VLAN-tagged veths on a shared bridge
  - IPAM per namespace: allocate a /24 from a configurable supernet (default 10.43.0.0/16)
  - Cross-namespace traffic: denied by default, can be allowed via `NetworkPolicy`-like rules (v2.3)

- [ ] Resource quotas:
  - `ResourceQuota` message: max_cpu_millicores, max_memory_bytes, max_workloads, max_storage_bytes
  - Enforced at `RunWorkload` time: reject if quota would be exceeded
  - Periodically enforced (e.g. kill lowest-priority workload if over quota)

- [ ] CLI integration:
  - `pullrun namespace create <name> [--subnet <CIDR>] [--cpu <mcores>] [--memory <bytes>]`
  - `pullrun namespace rm <name>` (fails if workloads still running)
  - `pullrun namespace ls`
  - `--namespace <name>` flag on all workload commands (default: `default`)

- [ ] Tests:
  - Unit: namespace creation / deletion
  - Unit: quota enforcement (reject over-quota workload)
  - Integration: two namespaces, verify workloads cannot communicate (unless allowed)
  - Integration: delete namespace with running workloads → error

**Tests to add:** 5 Rust unit + 3 integration  
**Estimated effort:** 2 weeks  

---

### 2.3 Workload Quotas & Resource Accounting (Cgroups v2)

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Replace the current `runc` resource limit integration with direct cgroups v2 control for finer-grained and more reliable enforcement. Currently limits are passed to `runc` which writes cgroups; we want to own the cgroup hierarchy.

**Sub-tasks:**

- [ ] Create new module `runtime/pullrun-exec/src/cgroups.rs`:
  - `CgroupManager` struct with methods:
    - `create_cgroup(path: &Path, spec: &ResourceQuota) -> Result<(), ExecError>`
    - `apply_limits(path: &Path, spec: &ResourceQuota) -> Result<(), ExecError>`
    - `read_stats(path: &Path) -> Result<WorkloadStats, ExecError>`
    - `destroy_cgroup(path: &Path) -> Result<(), ExecError>`

- [ ] Cgroups v2 files to manage:
  - `cpu.max` — CPU quota/period (e.g. "500000 1000000" = 0.5 cores)
  - `cpu.weight` — CPU weight (1-10000)
  - `memory.max` — hard memory limit
  - `memory.high` — throttling threshold (soft limit)
  - `memory.swap.max` — swap limit
  - `pids.max` — max processes
  - `io.weight` — I/O weight

- [ ] Integration with `LinuxContainerExecutor`:
  - Before `runc create`: create cgroup at `/sys/fs/cgroup/pullrun/<namespace>/<workload_id>`
  - Pass `--cgroup-path` to runc so it does not try to create its own
  - After `runc delete`: destroy the cgroup

- [ ] Integration with `pullrun-exec/src/types.rs`:
  - Extend `WorkloadSpec` with `ResourceQuota` field
  - `pullrun run --cpu-shares`, `--cpu-quota`, `--io-weight`, `--pids-limit`

- [ ] Prometheus metrics:
  - `pullrun_cgroup_cpu_usage_seconds_total` — per workload
  - `pullrun_cgroup_memory_usage_bytes` — per workload
  - `pullrun_cgroup_oom_events_total` — counter of OOM kills

- [ ] Tests:
  - Unit: write/read cgroups v2 files, verify parsing
  - Integration: run `stress` workload with `--memory 100M`, verify killed at ~100M
  - Integration: run CPU-bound workload with `--cpu-quota 50%`, verify throttling in cgroup stats

**Tests to add:** 4 Rust unit + 2 integration  
**Estimated effort:** 2 weeks  

---

### 2.4 External Secret Vault Integration

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Replace the monolithic `SecretStore` with a pluggable `SecretProvider` trait so secrets can live in external vaults (HashiCorp Vault, AWS Secrets Manager, Kubernetes secrets) rather than the local AES-256-GCM store.

**Sub-tasks:**

- [ ] Create new trait in `runtime/pullrun-policy/src/secrets.rs`:
  ```rust
  #[async_trait]
  pub trait SecretProvider: Send + Sync {
      async fn get(&self, name: &str) -> Result<Vec<u8>, SecretError>;
      async fn put(&self, name: &str, value: &[u8]) -> Result<(), SecretError>;
      async fn delete(&self, name: &str) -> Result<(), SecretError>;
      async fn list(&self) -> Result<Vec<String>, SecretError>;
  }
  ```

- [ ] Implement providers:
  - `FileSecretProvider` — current AES-256-GCM implementation (default, backward compatible)
  - `HashicorpVaultProvider` — HTTP API to Vault KV v2
  - `KubernetesSecretProvider` — K8s API (`kubectl`-less, uses in-cluster service account)
  - `AwsSecretsManagerProvider` — AWS SDK v2 (optional feature flag)

- [ ] Mount semantics (unchanged):
  - At workload creation: `SecretProvider::get(name)` → write to tmpfs at `/run/secrets/<name>`
  - On workload stop: unmount and delete tmpfs
  - No change to container interface — only the source changes

- [ ] Configuration:
  - Runtime flag: `--secret-provider <type> [--secret-provider-url <url>] [--secret-provider-auth <method>]`
  - Per-namespace override: namespace config can specify its own vault provider provider per provider

- [ ] Tests:
  - Unit: `FileSecretProvider` roundtrip (encrypt/decrypt, list, delete)
  - Integration with `vault` dev server (HashiCorp Vault in dev mode)
  - Mock provider for unit tests of `SecretProvider` trait

**Tests to add:** 4 Rust unit + 2 integration  
**Estimated effort:** 2 weeks  

### Phase 2 Summary

| Sub-task | Status | Effort | Tests | Entry point |
|----------|--------|--------|-------|-------------|
| 2.1 CNI plugin | `🔄 NOT STARTED` | 3 wks | 3 + 3 | `runtime/pullrun-cni` |
| 2.2 Namespaces | `🔄 NOT STARTED` | 2 wks | 5 + 3 | `runtime/pullrun-runtime/src/namespace.rs` |
| 2.3 Cgroups v2 | `🔄 NOT STARTED` | 2 wks | 4 + 2 | `runtime/pullrun-exec/src/cgroups.rs` |
| 2.4 Secret vaults | `🔄 NOT STARTED` | 2 wks | 4 + 2 | `runtime/pullrun-policy/src/secrets.rs` |
| **Total** | | **9 wks** | **16 + 10 = 26** | |

**Definition of done for Phase 2:**
- `kind` cluster with pullrun CNI, pods can communicate
- Two namespaces cannot see each other's workloads (unless explicitly allowed)
- Workload killed at cgroup memory limit (no silent OOM)
- Secrets can be stored in HashiCorp Vault and read at runtime

---

## Phase 3 — Cloud-Native Differentiation (Target: 14 weeks)

> **Goal:** Build features that no existing container platform (Docker, containerd, K8s) offers, making pullrun the clear choice for specific high-value use cases.

### 3.1 Live Migration for VM Workloads

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Expose Firecracker's snapshot/restore capability through pullrun to enable zero-downtime workload migration across nodes. This is only possible with the VM backend (containers cannot be live-migrated).

**Sub-tasks:**

- [ ] Extend `Executor` trait:
  ```rust
  #[async_trait]
  pub trait VmExecutor: Executor {
      async fn snapshot(&self, id: &str, path: &Path) -> Result<(), ExecError>;
      async fn restore(&self, path: &Path) -> Result<ProcessHandle, ExecError>;
      async fn migrate(&self, id: &str, target: &str) -> Result<(), ExecError>;
  }
  ```

- [ ] Snapshot implementation:
  - Use Firecracker's `PUT /snapshot/create` API
  - Output: `snapshot.mem` + `snapshot.vmstate` files
  - These are just blobs — store them in the DAG store (!) as a single `Snapshot` node
  - Content-addressed snapshot = can be block-synced to target node automatically

- [ ] Restore implementation:
  - Target node: download snapshot via P2P sync (same blob sync already built)
  - Use Firecracker's `PUT /snapshot/load` API
  - VM resumes from exact memory state and disk state

- [ ] Migration orchestration:
  - `pullrun migrate <id> --to <target_node>`
  - Source: `snapshot()` → upload to target via sync
  - Target: `restore()` → start VM
  - Update network endpoint: new IP (or same IP if using shared storage for MAC persistence)
  - Gracefully redirect traffic (via proxy or DNS update)
  - Source: `stop()` and cleanup

- [ ] Caveats and constraints:
  - Guest kernel must have `CONFIG_KVM_GUEST` and migration-aware drivers
  - Source and target must have same CPU architecture (x86→x86, arm64→arm64)
  - Network state (TCP connections) is NOT preserved — clients must reconnect (document this)
  - Best for stateless or checkpoint-friendly workloads

- [ ] Tests:
  - Integration: create VM workload, take snapshot, restore on same node, verify state preserved
  - Integration: two-node test, migrate workload, verify it runs on target
  - Stress: rapid snapshot/restore cycles (100x), verify no memory leaks

**Tests to add:** 2 Rust unit + 4 integration  
**Estimated effort:** 4 weeks  

---

### 3.2 WebAssembly (WASM) Executor Backend

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

Add a WASM executor as a third backend alongside containers and VMs. WASM modules start in <10ms, have near-native performance for compute, and provide stronger sandboxing than containers (memory-safe, no shared kernel).

**Sub-tasks:**

- [ ] Create new crate `runtime/pullrun-wasm`:
  - Depends on `wasmtime` (JIT compiler, not interpreter — performance)
  - `WasmExecutor` struct implementing `Executor`
  - `WasiContext` for system interface (filesystem, network, clocks)

- [ ] OCI image → WASM module conversion:
  - Detect when an image contains a `.wasm` file (or an OCI artifact with `application/wasm` media type)
  - Materialize WASM module from DAG blob directly into `wasmtime` memory (no disk write needed)

- [ ] WASI integration:
  - Map workload `command` to WASM module entry point
  - Environment variables → WASI env
  - Volume mounts → WASI preopened directories
  - Network → WASI sockets (TCP/UDP via wasmtime-wasi)
  - stdin/stdout/stderr → WASI stdio (already mapped to gRPC streams)

- [ ] Resource limits:
  - `wasmtime` fuel metering for CPU limiting
  - Memory limit via `wasmtime::Config::memory_limit`
  - No cgroup needed (WASM is already sandboxed)

- [ ] Security:
  - Capabilities model: explicitly allow filesystem, network, env access per workload
  - Disallow `wasi-nn`, `wasi-crypto` unless explicitly enabled (supply-chain risk)

- [ ] CLI integration:
  - `pullrun run <image> --backend wasm` (auto-detects if image contains `.wasm`)
  - `pullrun build --target wasm32-wasi` (if/when we add WASM build targets)

- [ ] Tests:
  - Unit: `WasmExecutor::create()` with a simple "hello world" WASM module
  - Integration: run `wasmtime` hello-world, verify stdout captured
  - Integration: run WASI HTTP server in WASM, verify port mapping works
  - Performance: benchmark startup time vs container vs VM

**Tests to add:** 4 Rust unit + 3 integration  
**Estimated effort:** 3 weeks  

---

### 3.3 Greenland Mode (Event-Driven Serverless)

**Status:** `🔄 NOT STARTED`  
**Owner:** —  
**Started:** —  
**Completed:** —  

**What to build:**

A serverless function platform on top of pullrun. Workloads are started on-demand in response to events (HTTP, message queue, cron) and scaled to zero when idle. This is a new layer above the existing runtime, not a replacement.

**Sub-tasks:**

- [ ] Create new crate `runtime/pullrun-greenland` (or module in `pullrun-runtime`):
  - `FunctionController` struct:
    - `triggers: Vec<Trigger>`
    - `scaling: ScalingPolicy`
    - `warm_pool: HashMap<String, Vec<ProcessHandle>>` — idle instances kept alive
  - `Trigger` enum:
    - `Http { path: String, method: String, port: u16 }`
    - `Queue { topic: String, broker: String, consumer_group: String }`
    - `Cron { schedule: String, timezone: String }`
    - `Webhook { url: String, secret: String }`

- [ ] Event ingestion:
  - HTTP: embed `axum` router in `pullrun-greenland`, route by path prefix to function
  - Queue: use `rdkafka` or equivalent to consume from Kafka, NATS, etc.
  - Cron: use `tokio::time::interval` + `cron-parser` crate
  - Webhook: `axum` route with HMAC verification

- [ ] Scaling logic:
  - `ColdStart` — no idle instances, create from DAG (slow, 400ms-4s depending on backend)
  - `WarmStart` — reuse idle instance from warm pool (fast, <50ms for WASM, <200ms for container)
  - `ScaleToZero` — after `idle_timeout` (default 5 min), stop instance, keep only DAG root
  - `ConcurrencyLimit` — max concurrent invocations per function (default: 100)

- [ ] Request routing:
  - Event arrives → find matching `Trigger` → lookup function image (DAG root digest)
  - Check warm pool for that function:
    - Match available → route request, mark instance as "busy"
    - No match → cold-start from DAG materialization
  - Return response (for HTTP) or ack (for queue)

- [ ] Billing / metering (optional but valuable):
  - Track per-function: invocation count, cold-start count, total CPU ms, total memory GB-seconds
  - Export as Prometheus metrics or to external billing system

- [ ] CLI integration:
  - `pullrun function create <name> --image <ref> --trigger http --path /api/hello`
  - `pullrun function ls`
  - `pullrun function invoke <name> --data '{"key":"value"}'`
  - `pullrun function scale <name> --min 2 --max 10`

- [ ] Tests:
  - Integration: create function, invoke via HTTP, verify response
  - Integration: invoke 1000 times rapidly, verify warm pool sizing
  - Integration: wait for idle timeout, verify scale-to-zero, next call is cold start
  - Integration: queue trigger — publish to Kafka topic, verify function executes

**Tests to add:** 3 Rust unit + 4 integration  
**Estimated effort:** 6 weeks  

### Phase 3 Summary

| Sub-task | Status | Effort | Tests | Entry point |
|----------|--------|--------|-------|-------------|
| 3.1 Live migration | `🔄 NOT STARTED` | 4 wks | 2 + 4 | `runtime/pullrun-vm/src/snapshot.rs` |
| 3.2 WASM executor | `🔄 NOT STARTED` | 3 wks | 4 + 3 | `runtime/pullrun-wasm` |
| 3.3 Serverless mode | `🔄 NOT STARTED` | 6 wks | 3 + 4 | `runtime/pullrun-greenland` |
| **Total** | | **13 wks** | **9 + 11 = 20** | |

**Definition of done for Phase 3:**
- VM workload can be migrated between two nodes with `pullrun migrate`
- WASM workload runs with `--backend wasm`, starts in <50ms
- HTTP function deployed via `pullrun function create`, auto-scales to zero, scales back on request

---

## Global Test Ledger

Track the test count here. Update after every merge.

| Date | Rust unit | Rust integ | Go unit | Total |
|------|-----------|------------|---------|-------|
| 2026-06-09 (baseline) | 118 | 8 | 9 | 135 |
| After Phase 1.1 | +5 | +2 | — | +7 |
| After Phase 1.2 | +4 | +1 | — | +5 |
| After Phase 1.3 | +3 | +2 (bench) | — | +5 |
| After Phase 1.4 | +4 | +3 | — | +7 |
| After Phase 2.1 | +3 | +3 | — | +6 |
| After Phase 2.2 | +5 | +3 | — | +8 |
| After Phase 2.3 | +4 | +2 | — | +6 |
| After Phase 2.4 | +4 | +2 | — | +6 |
| After Phase 3.1 | +2 | +4 | — | +6 |
| After Phase 3.2 | +4 | +3 | — | +7 |
| After Phase 3.3 | +3 | +4 | — | +7 |
| **Projected total** | **148** | **31** | **9** | **188** |

---

## Session-to-Session Handoff Checklist

When a session ends and you need to resume later, ensure:

1. [ ] Every `⏳ IN PROGRESS` section is updated to `✅ DONE` (if completed) or back to `🔄 NOT STARTED` (if incomplete)
2. [ ] The "Last updated" field at the top of this file is set to today's date
3. [ ] The test count in the **Global Test Ledger** is updated
4. [ ] Any new findings, blockers, or changes in scope are noted in the affected sub-tasks as blockquotes:
   > **Blocker discovered:** `rkyv` v0.8 breaks archive format; must pin v0.7 or upgrade before Phase 3.2
5. [ ] Commit this file: `git add PROGRESS.md && git commit -m "docs: update progress after <work-done>"`

---

## Appendix: Quick Reference

### Running the project
```bash
# Full workspace test (baseline: 126 Rust + 9 Go)
cargo test --workspace
go test ./cli/pullrun/...

# Build everything
make build

# Start the daemon
make run-runtime
# or
cargo run -p pullrun-runtime -- daemon --socket /tmp/pullrun.sock

# Build the CLI
make build-go
```

### Useful file paths
```
Runtime entry:          runtime/pullrun-runtime/src/main.rs
Store core:             runtime/pullrun-store/src/store.rs
Executor trait:         runtime/pullrun-exec/src/types.rs
Networking:             runtime/pullrun-net/src/
Sync (P2P):             runtime/pullrun-sync/src/
Policy:                 runtime/pullrun-policy/src/
CLI:                    cli/pullrun/
Protos:                 proto/pullrun/runtime.proto
Deploy manifests:       deploy/
```

### Key design decisions to preserve
1. **Zero-copy store**: All reads via `rkyv::archived_root()` + `memmap2::Mmap`; never deserialize on hot path
2. **Content-addressing**: File names are SHA256 digests; dedup is automatic
3. **No Docker/containerd dependency**: All build/pull/run/network/exec are self-contained
4. **Language split**: Rust = data plane (performance, zero-copy, async); Go = control plane (CLI, gRPC stubs, CRI)
5. **Bridge networking for all backends**: Containers (veth), VMs (TAP), same L2 segment, same IPAM pool
