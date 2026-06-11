# Architecture

A deep dive into how Pullrun stores, addresses, and executes OCI
workloads. This is the long-form companion to the README's
high-level diagram.

## 1. The content-addressed store (`pullrun-store`)

### The on-disk format

Every Pullrun node is an OCI artifact represented in the DAG as a
`DagNode`:

```rust
// runtime/pullrun-store/src/node.rs
pub struct DagNode {
    pub kind: NodeKind,         // Blob | Tree | Layer | Manifest
    pub edges: Vec<Digest>,     // child digests
    pub inline_data: Vec<u8>,   // small payloads (the JSON manifest
                               // and tree, gzipped layer bytes if small)
}
```

`Digest` is a type alias for `String` — the hex-encoded SHA-256
of the node's serialized form.

A node lives on disk as a single file, named by its digest:

```
$ ls /var/lib/pullrun/
12a3...e4  <- root manifest
8b7f...9c  <- a tree node
...
```

The file *is* the rkyv-serialized `DagNode`. To read it, we
`mmap()` the file and ask rkyv for an `&ArchivedDagNode` — a
zero-copy view into the on-disk bytes. There is no `read → parse →
allocate` step; the page cache gives us pages, rkyv gives us
field access.

```rust
// runtime/pullrun-store/src/store.rs
pub fn get_archived(&self, digest: &Digest) -> Result<&ArchivedDagNode, StoreError> {
    let mmap = self.get(digest)?;          // DashMap<_, Arc<Mmap>>
    let archived = unsafe { rkyv::archived_root::<DagNode>(&mmap[..]) };
    Ok(archived)
}
```

(The `unsafe` is safe because rkyv's `#[archive(check_bytes)]` on
the type verifies the bytes are well-formed at write time, and
reads require no mutation. See `WARNINGS.md` for the pitfalls of
`check_bytes` in v0.)

### The in-memory cache

The store keeps a `DashMap<Digest, Arc<Mmap>>` of recently-touched
nodes. Concurrent reads are lock-free; the first reader pays for
`mmap()` + page faults, every subsequent reader is a single atomic
load.

```rust
cache: DashMap<Digest, Arc<Mmap>>
```

The cache has no eviction policy in v0. The on-disk file is the
source of truth; a `get_archived` miss falls through to `mmap` and
re-populates the cache. For workloads with thousands of unique
layers, the cache can grow large — eviction is on the v1 list
(LRU keyed by last-touch).

### Why content-addressing

Two consequences fall out of naming files by digest:

1. **Deduplication is automatic.** If you pull `alpine:3.18` on
   two nodes, the second pull is a no-op for any layer that
   already exists on disk. The store's `exists()` check is just a
   `path_for(digest).exists()`.

2. **The same digest runs the same thing, anywhere.** A node that
   has `sha256:abc...` will *always* run identically on every host
   that has it. This is the foundation of the
   "reproducible-artifact" claim and the reason the
   K8s `RuntimeClass` integration is trivial: a workload's
   `image_root` is its identity.

### The blob store

A separate, simpler store handles non-DAG bytes (large layer
blobs, signature payloads, SBOMs):

```rust
pub async fn put_blob(&self, digest: &Digest, data: &[u8]) -> Result<(), StoreError>
pub fn get_blob(&self, digest: &Digest) -> Result<Arc<Mmap>, StoreError>
```

The blob store is a thin wrapper around a content-addressed
filesystem. v0 has no separate namespace — blobs live under
`<store_root>/blobs/<digest>`.

## 2. The OCI pipeline (`pullrun-oci`)

```
HTTP GET /v2/<name>/manifests/<ref>  (with bearer token)
                │
                ▼
   OciPuller::pull(image_ref, registry) -> OciImage
                │
                ▼
   OciToDagConverter::convert(OciImage) -> Digest  (the root)
                │
                ▼
   MmapStore::put(manifest) -> Digest
   MmapStore::put(tree)     -> Digest
   MmapStore::put_blob(layer) per layer
```

The puller is a minimal OCI distribution v2 client: token auth,
manifest fetch, layer fetch, all streaming. The converter walks
the OCI image (which is a manifest → config + layers) and
produces a DAG of Pullrun nodes. The config blob becomes a `Blob`
node, each layer becomes a `Layer` node, and the manifest itself
becomes a `Manifest` node whose edges point at the config and the
layers.

If the converter encounters a digest that already exists in the
store, the `put()` is a no-op. This is what makes
`pullrun pull` idempotent and free for re-pulls.

## 3. The executor model (`pullrun-exec`, `pullrun-vm`)

The runtime defines an executor trait that abstracts over
isolation level:

```rust
#[async_trait]
pub trait Executor: Send + Sync {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError>;
    async fn start(&self, handle: &ProcessHandle) -> Result<(), ExecError>;
    async fn stop(&self, id: &str) -> Result<(), ExecError>;
    async fn wait(&self, id: &str) -> Result<ExitStatus, ExecError>;
    async fn status(&self, id: &str) -> Result<String, ExecError>;
}
```

A `WorkloadSpec` is the language-neutral description of "what to
run": id, image root digest, backend choice, command, env, CPU
and memory limits, network mode and rules.

`ProcessHandle` is the language-neutral "what got created": backend
name, optional PID, optional internal IP.

### Container backend (`LinuxContainerExecutor`)

A thin wrapper around `runc`. v0 calls `runc create` + `runc
start` + `runc delete`; the work of pulling the OCI image is
already done by Pullrun, so runc only sees a Pullrun-formatted OCI
bundle on disk.

The container's rootfs is *materialized* from the DAG on demand:
the converter produces a directory of hardlinks pointing at the
mmap'd DAG nodes. Multiple containers on the same image share the
same on-disk bytes; the hardlinks are free.

### VM backend (`FirecrackerExecutor`)

A wrapper around `firecracker --api-socket` for each workload.
The DAG is materialized as an ext4 rootfs image (a sparse file
built by streaming layer blobs through `mkfs.ext4`), and the
Firecracker process boots from it. The kernel and the
initrd/rootfs are passed via Firecracker's block-device config.

Networking for the VM uses a tap device attached to the shared
`pullrun-br0` bridge — same L2 segment as the containers. The
guest gets a static IP via the kernel's `ip=` boot arg, with a
deterministic MAC derived from the IP (so two VMs can't collide).

### `ExecutorRouter`

A small dispatcher that holds both executors and routes by
`spec.backend`. From the gRPC handler's perspective, it's just
`Executor`. The router also handles the case where the request
names a VM but the VM backend isn't configured (returns
`BackendNotAvailable`).

## 4. The shared network model (`pullrun-net`)

This is the most opinionated design decision in Pullrun: **all
workloads, regardless of backend, live on the same L2 segment**
behind a single userspace proxy.

```
                  ┌──────── pullrun-br0 (Linux bridge, 10.42.0.0/16) ────────┐
                  │                                                          │
   container A    │   container B     VM C         VM D                      │
   10.42.0.5      │   10.42.0.6      10.42.0.7    10.42.0.8                │
   │              │   │               │            │                         │
   └─ veth pair ──┘   └─ veth pair ───┘  └─ tap ────┘  └─ tap ───────────────┘
                                                  │
                                                  ▼
                                     ┌─────────────────────────┐
                                     │  ProxyNetwork (10.42.0.1)│
                                     │  • IPAM (AtomicU32)      │
                                     │  • userspace TCP proxy   │
                                     │  • userspace UDP proxy   │
                                     │  • in-process DNS server │
                                     └─────────────────────────┘
                                                  │
                                       iptables MASQUERADE
                                       (for VM outbound only;
                                        containers use host net
                                        by default in v0)
                                                  │
                                                  ▼
                                              the internet
```

### IPAM

A single `Arc<Ipam>` is held by the runtime and shared between
the container and VM executors. Allocation is an atomic increment
on a `u32`; the IP space is 10.42.0.0/16, which gives us 65k
addresses. In v0 there's no garbage collection; an IP is held
for the lifetime of the workload.

### Inbound: userspace proxy

The proxy listens on `10.42.0.1:<port>` for each declared
`NetworkRule { direction: Inbound, port }`. When a packet arrives
at the host on that port, the proxy DNATs it to the workload's
internal IP, opens a TCP connection (or UDP association)
through the bridge, and bi-directionally shuttles bytes. This
is the `pullrun_net::proxy` module.

The proxy is *not* a full implementation of `iptables` / nft
semantics in v0 — it's a list of explicit port mappings. See
[docs/POLICY.md](./POLICY.md) for how this composes with
NetworkPolicy in v1.

### Outbound: iptables MASQUERADE for VMs

Containers in v0 share the host's network namespace for outbound
traffic (or use `NetworkMode::Host` directly). VMs need NAT
because their tap devices only see the bridge.

Pullrun writes three iptables rules at boot (and on every
`ensure_bridge` call, idempotently):

```bash
iptables -t nat -A POSTROUTING \
    -s 10.42.0.0/16 ! -d 10.42.0.0/16 \
    -j MASQUERADE
iptables -A FORWARD -i pullrun-br0 -o <outbound_iface> -j ACCEPT
iptables -A FORWARD -i <outbound_iface> -o pullrun-br0 \
    -m state --state RELATED,ESTABLISHED -j ACCEPT
```

The outbound interface is auto-detected by parsing
`ip route show default`; this works on cloud VMs, bare metal,
and most home routers.

### Why a single shared segment?

The alternative — one bridge per workload, with cross-workload
traffic going through a router — adds latency and operational
complexity for no real security win in the common case. Pullrun
workloads are *not* multi-tenant; they're all owned by the same
operator. The isolation boundary is the executor (container vs
VM), not the network.

Cross-workload isolation in v0 is enforced by the proxy (a
workload can't talk to another workload's declared ports unless
the operator added a `NetworkRule` for it). In v1 we'll add
per-workload nftables sets to enforce this at the kernel level.

## 5. The policy engine (`pullrun-policy`)

See [docs/POLICY.md](./POLICY.md) for the full picture. Briefly:

- `pull_image` runs the policy *after* the image is in the store.
  A failed policy check returns `permission_denied` and the
  image is not made runnable (though its bytes stay on disk —
  re-pulling is cheap).
- `run_workload` re-evaluates the policy as defense in depth.
  The signature check uses the image_ref the operator passed;
  the SBOM check uses the manifest digest. A workload that was
  pull-allowed but later denied (e.g. policy was tightened) will
  fail to start.

The engine is synchronous (`tokio::task::spawn_blocking`)
because signature verification and SBOM parsing are CPU-bound.

## 6. The runtime service (`pullrun-runtime`)

The runtime is a `tonic` gRPC server. The service is
`RuntimeService`; the gRPC trait is generated from
`proto/pullrun/runtime.proto` at compile time.

### gRPC methods (v0)

| Method | Purpose |
|---|---|
| `PullImage` | Fetch + convert + policy-gate an OCI image |
| `RunWorkload` | Create + start + record state for a workload |
| `StopWorkload` | Stop a running workload |
| `GetWorkload` | Snapshot of one workload's state |
| `ListWorkloads` | Snapshot of all workloads |
| `InspectWorkload` | Deep snapshot: state + DAG path + network rules + policy decisions |
| `StreamLogs` | (stub) log stream from a workload |
| `StreamEvents` | Real-time event stream from the bus |
| `ExecInWorkload` | Run a command inside a running container (runc exec) |

### The event bus

`EventBus` is a thin wrapper over `tokio::sync::broadcast`
(capacity 1024). Emitters call `.emit()`; subscribers call
`.subscribe()` for a fresh receiver. Semantics:

- **At-most-once per receiver.** A slow consumer sees
  `RecvError::Lagged(n)` when the channel rolls over. We log
  and continue — the consumer is the CLI or a future audit
  daemon, both of which are happy with recent events.
- **Multi-consumer.** Each subscriber gets its own receiver; an
  event is cloned into each.
- **Backpressure-free.** `emit()` is non-blocking and never
  fails for "no receivers" (the event is silently dropped).

A background watcher task polls `Executor::status` every 5s and
emits `WorkloadExited` for workloads that have transitioned out
of "running" on their own (process crash, OOM kill). The
operator-initiated stop path emits `WorkloadStopped` and is
mutually exclusive with the watcher (the watcher's HashSet of
"already announced" ids prevents double-emit).

### Observability

The runtime exposes a Prometheus `/metrics` HTTP endpoint via
`axum`, separate from the gRPC UDS. The recorder is a
`metrics::install_recorder()` singleton (guarded by `OnceLock`).
Histogram buckets are tuned for workload latencies
(0.05..60s for pulls, 0.01..10s for starts), not the
exporter's default exponential range.

`/healthz` is exposed alongside `/metrics` for K8s liveness /
readiness probes.

## 7. The control plane (stub)

The `control-plane/` Go module has a gRPC API server and an
agent, but the scheduler and the registry proxy are in-memory
only. The full picture is:

```
                            ┌──────────────────┐
                            │  pullrun-controller │
                            │  (control plane)   │
                            └──────────┬────────┘
                                       │ gRPC
                            ┌──────────▼────────┐
                            │   pullrun-agent    │   one per node
                            │   (deploys,       │
                            │    introspects)   │
                            └──────────┬────────┘
                                       │ gRPC
                            ┌──────────▼────────┐
                            │  pullrun-runtime   │   on each node
                            │  (gRPC daemon)    │
                            └───────────────────┘
```

Persistence (etcd), cross-node service discovery
(`.pullrun.local` DNS), and admission control are all v1 work.
The wire protocol is stable; only the server-side state
management changes.

## 8. Why Rust + Go

A deliberate split:

- **Rust owns the data plane.** The store, the executors, the
  proxy, the OCI pipeline, the policy engine. All of these
  benefit from `mmap`, zero-copy deserialization (rkyv), and
  fine-grained control over async runtime behavior. The Rust
  code is one Cargo workspace with seven crates.
- **Go owns the control plane and the CLI.** gRPC stubs are
  trivial in Go; the CLI is a 5-line `cobra` command. We
  deliberately don't share code between the CLI and the
  runtime — the CLI talks to the runtime over gRPC just like
  any other client.
- **Single source of truth for the wire format:**
  `proto/pullrun/runtime.proto` (and `control.proto` for the
  multi-node API). Rust generates from it at compile time
  (`tonic-build`); Go generates from it at `make proto` time
  (the shared `proto-go/` module).

This split keeps each side's tooling honest: you can't paper
over a Rust bug with a Go workaround, and vice versa.
