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
#[derive(Archive, Deserialize, Serialize)]
#[archive(check_bytes)]
pub enum DagNode {
    Blob(Digest),                     // raw layer blob (tar.gz)
    Tree(HashMap<String, Digest>),    // directory listing
    Executable(Digest),               // symlink to a blob marked as executable
    Config(ImageConfig),              // OCI image config
    Manifest(ManifestMetadata),       // OCI manifest (layers, config ref)
    ManifestList(Vec<ManifestRef>),   // multi-arch index
}
```

`Digest` wraps a `[u8; 32]` — the raw SHA-256 hash.

Nodes are stored in a 3-level sharded directory rooted at
`<store_root>/` (default `~/.local/share/pullrun/`):

```
store_root/
└── oci-store/            (or the configured store path)
    ├── 00/
    │   └── 11/
    │       └── ab/
    │           ├── node.rkyv        ← rkyv-archived DagNode
    │           └── blob.raw         ← raw blob (tar.gz, config, etc.)
    └── ...
```

Each digest `d = SHA256(data)` maps to path
`<store_root>/dd/dd/dd/...` where the first two hex bytes form the
first two directory levels and the remaining bytes form the third.
This keeps directory sizes manageable (at most 256 entries per
level).

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
reads require no mutation. See `docs/PULLRUN_GUIDE.md` for the pitfalls of
`check_bytes` in v0.)

### The in-memory cache and concurrency model

Two LRU caches (256 MiB each) sit in front of the store: one for
`DagNode` deserializations and one for blob data.

```rust
node_cache: LruCache<Digest, Arc<ArchivedDagNode>>
blob_cache: LruCache<Digest, Arc<Mmap>>
```

Cache entries are `Arc<Mmap>` references, ensuring that eviction
does not invalidate live references held by callers. On a cache
miss, the file is mmap'd and the `ArchivedDagNode` is
zero-copy viewed (rkyv) from the mapped bytes; on eviction, the
`Mmap` is dropped and the kernel reclaims the pages.

The primary store is backed by `DashMap<Digest, Arc<Mmap>>` — a
lock-free concurrent hash map (sharded by digest prefix). This
is a deliberate architectural choice over `RwLock<HashMap>`:

- **Reads are fully concurrent.** DashMap shards the key space
  across 64 internal locks; reads that hit different shards do
  not contend. A single `RwLock<HashMap>` serializes all readers
  on the same lock.
- **Writes do not block readers.** An insert or remove inside one
  shard does not block lookups in other shards.
- **The sharding strategy matches the store's own layout.** The
  same two-hex-byte prefix that determines the on-disk path
  determines the in-memory shard, so concurrent access patterns
  (multi-image pull, simultaneous list + prune) naturally
  distribute across shards.

Concurrent reads are lock-free at the `Arc` level; the first reader
pays for `mmap()` + page faults, every subsequent reader is a
single atomic load. In practice, the node cache achieves a >99%
hit rate for repeated `pull` and `list` operations, and the 256 MiB
budget is sufficient for several hundred images worth of metadata.

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

A separate namespace handles non-DAG bytes (large layer blobs,
signature payloads, SBOMs). Blobs live alongside their DAG nodes
in the same sharded directory as `blob.raw`:

```
<store_root>/dd/dd/dd/blob.raw
```

```rust
pub fn put_blob(&self, digest: &Digest, data: &[u8]) -> Result<(), StoreError>
pub fn get_blob(&self, digest: &Digest) -> Result<Arc<Mmap>, StoreError>
```

The blob store uses the same content-addressed, zero-copy model
as the DAG store. There is no separate namespace — the hex digest
determines the on-disk path, which is shared between `node.rkyv`
and `blob.raw`.

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

### Garbage collection

The store grows monotonically: every `pull`, `build`, `commit`,
and `save` adds nodes that are never removed. Pullrun provides a
`prune` command that performs reference-counted garbage collection:

- **Roots** are tagged images (stored in `<store>/tags.json`) and
  running workload manifests.
- **Reachability** is determined by BFS from all roots through
  the DAG.
- **Unreachable** nodes and blobs are deleted from disk and
  evicted from caches.
- A `--dry-run` flag reports what would be freed without deleting.

This mirrors Git's object store pruning and prevents unbounded
storage growth.

### Materialization

When a workload is executed, the DAG must be converted to a
runnable rootfs:

- **Containers:** The OCI pipeline produces a rootfs directory of
  hardlinks pointing at the mmap'd DAG node pages. Multiple
  containers on the same image share the same on-disk bytes.
- **Linux VMs (Firecracker):** Layer blobs are streamed through
  `mkfs.ext4` to produce a sparse ext4 rootfs image.
- **macOS VMs (Apple Virt):** The rootfs directory is shared
  directly via VirtioFS — no image build step.

The materialized rootfs lives at
`<store_root>/rootfs/<digest>/` — the store root is
`~/.local/share/pullrun/` by default, ensuring rootfs survives
host reboots (unlike `/tmp`).

#### `pullrun exec` and PTY handling

For `exec` sessions, the daemon opens a host PTY via
`posix_openpt`/`grantpt`/`unlockpt` and passes the slave fd to
the backend:

- **Containers:** The slave fd is passed to `runc exec -t`.
- **VMs:** The TTY is proxied over vsock to the guest agent
  (`pullrun-init`), which attaches it to the workload process
  inside the VM.

## 3. The executor model (`pullrun-exec`, `pullrun-vm`)

The runtime defines an executor trait that abstracts over
isolation level:

```rust
#[async_trait]
pub trait Executor: Send + Sync {
    async fn create(&self, spec: &WorkloadSpec) -> Result<ProcessHandle, ExecError>;
    async fn start(&self, handle: &ProcessHandle) -> Result<(), ExecError>;
    async fn stop(&self, id: &str) -> Result<(), ExecError>;
    async fn exec(&self, id: &str, cmd: &[String], env: &[(String, String)],
                  tty: bool) -> Result<ProcessHandle, ExecError>;
    async fn signal(&self, id: &str, signal: u32) -> Result<(), ExecError>;
    async fn stats(&self, id: &str) -> Result<WorkloadStats, ExecError>;
}
```

A `WorkloadSpec` is the language-neutral description of "what to
run": id, image root digest, backend choice, command, env, mounts
(volumes, secrets, configs), CPU and memory limits, network mode
and rules.

`ProcessHandle` is the language-neutral "what got created": backend
name, optional PID, optional internal IP.

### Warm VM Pool (`VmPool`)

For the Firecracker backend, the runtime maintains a pool of
pre-booted VMs to eliminate cold-boot latency. A background
`refill_loop` keeps `pool_size` VMs alive; each VM boots a minimal
32 MiB scratch rootfs containing only an idle `/init` that loops
on `sleep 3600`. When a workload arrives, `create()` first calls
`pool.acquire()`. If a pool entry is available, the fast path runs:

1. **Materialize** the workload's ext4 rootfs from the DAG store,
2. **Hot-swap** the rootfs into the running Firecracker VM via
   `PUT /drives/rootfs` over its UDS API socket,
3. **Reboot** the VM via `PUT /actions` with `SendCtrlAltDel` so it
   boots the workload's rootfs,
4. **Return** a `ProcessHandle` with the pool VM's pre-allocated IP
   and TAP device (no IPAM, bridge, or proxy setup needed).

This reduces perceived boot time from ~500 ms (cold `mkfs.ext4` +
`firecracker --api-sock`) to ~200 ms (rootfs materialization +
hot-swap + reboot), and reuses pre-warmed page cache and kernel
state. Pool health is checked every 30 seconds; dead VMs are
removed and replaced. The pool is configured via `--vm-warm-pool-size
N` (default 0, disabled).

```
 Firecracker Warm Pool Flow:

 ┌─ pullrun-runtime ──────────────────────────────────────────┐
 │                                                             │
 │  VmPool (background loops)                                  │
 │    ├─ refill_loop (every 5s)                                │
 │    │   └─ boot_pool_vm → push_back into pool                │
 │    └─ health_loop (every 30s)                               │
 │        └─ check_health → remove dead VMs                   │
 │                                                             │
 │  FirecrackerExecutor::create()                               │
 │    ├─ pool.acquire() → PooledVm                            │
 │    │   ├─ materialize_ext4_rootfs (workload rootfs)         │
 │    │   ├─ pool_entry.swap_rootfs (hot-swap via FC API)     │
 │    │   ├─ pool_entry.reboot (CtrlAltDel)                    │
 │    │   └─ return ProcessHandle (pre-allocated IP)           │
 │    └─ fallback: cold boot (if pool empty)                   │
 └─────────────────────────────────────────────────────────────┘
```

### Container backend (`LinuxContainerExecutor`)

A thin wrapper around `runc`. The work of pulling the OCI image is
already done by Pullrun, so runc only sees a Pullrun-formatted OCI
bundle on disk. Calls `runc create` → `runc start` → `runc
delete` through the standard lifecycle.

The container's rootfs is *materialized* from the DAG on demand:
the converter produces a directory of hardlinks pointing at the
mmap'd DAG nodes. Multiple containers on the same image share the
same on-disk bytes; the hardlinks are free.

**Rootless mode:** When EUID != 0, Pullrun uses a
`RootlessContainerExecutor` that patches the OCI spec with a user
namespace mapping, NET_NS isolation via `pasta` (from Passt), and
rootless cgroup delegation. This mode requires no setuid binaries.

### VM backend (`AppleVirtExecutor` — macOS)

On Apple Silicon, Pullrun uses the `Virtualization.framework`
(macOS 13+ Ventura) to run OCI images as lightweight VMs. The
executor runs in-process with the daemon, which must be signed
with the `com.apple.security.virtualization` entitlement.

#### Two-thread dispatch model

Apple Virtualization FFI requires that all framework calls happen
on the main thread (specifically, the thread that calls
`dispatch_main()`). This creates an unusual constraint for a Rust
async runtime. The executor uses a two-thread architecture:

- **Main thread** — calls `dispatch2::dispatch_main()` to pump
  the main dispatch queue. The Apple Virtualization framework
  submits all its async completion handlers (VM boot, vsock
  events, VirtioFS I/O) to this queue. This thread never returns.
- **Body thread** — does all actual work: kernel staging, VM
  pool operations, vsock transport, workload exec. When done, it
  calls `libc::_exit(code)` because the main thread is blocked
  in `dispatch_main()` and cannot observe a normal return.

A panic hook catches body-thread panics and calls `_exit(1)`;
without this, a panic would leave the process stuck forever in
`dispatch_main()` with no way to signal failure. This design is
used by both `apple-virt-smoke` and `apple-virt-exec` standalone
tools as well as the daemon's AppleVirtExecutor.

```
 ┌─ pullrun-runtime (signed process) ────────────────────┐
 │                                                        │
 │  Thread 1 (main): dispatch_main()                      │
 │    └─ Apple Virtualization completion handlers         │
 │                                                        │
 │  Thread 2 (body): all actual work                      │
 │    ├─ VM creation (lazy, not on boot)                  │
 │    ├─ VirtioFS rootfs share (the DAG rootfs directory  │
 │    │   is shared directly — no ext4 image build)       │
 │    ├─ VZVirtioSocketListener (vsock port 42)           │
 │    └─ on_exit callback → status update + event         │
 │                                                        │
 │  On completion: body thread calls _exit(code)          │
 └────────────────────────────────────────────────────────┘
```

Key differences from the Firecracker backend:

- **No image build.** The rootfs is a host directory shared via
  VirtioFS; the guest sees changes instantly without rebundling
  an ext4 image.
- **VM persistence on detach.** The VM stays alive after an
  `exec` session ends; re-attaching opens a new vsock session
  to the same guest agent.
- **Thread naming.** Each VM runs on a dedicated thread named
  `vm-{workload_id}` for debuggability.

### VM backend (`FirecrackerExecutor`)

A wrapper around `firecracker --api-socket` for each workload.
The DAG is materialized as an ext4 rootfs image (a sparse file
built by streaming layer blobs through `mkfs.ext4`), and the
Firecracker process boots from it.

The guest runs `pullrun-init` (PID 1), a minimalist init process
that mounts the rootfs, wires up networking, and opens a vsock
connection back to the host daemon:

```
 ┌─ Host ──────────────────────────┐   ┌─ Firecracker VM ──────────────┐
 │                                  │   │                                │
 │  FirecrackerExecutor             │   │  pullrun-init (PID 1)          │
 │    ├── firecracker --api-socket  │   │    ├── mount rootfs            │
 │    ├── vsock listener (port 42)  │   │    ├── bring up eth0           │
 │    └── on_exit callback          │   │    ├── connect vsock → host    │
 │                                  │   │    └── exec workload           │
 └──────────────────────────────────┘   └────────────────────────────────┘
```

#### pullrun-init: the guest agent

The guest-side agent is a static binary (`aarch64-unknown-linux-musl`)
that acts as PID 1 inside the VM. Its responsibilities differ by
backend:

**Firecracker (no initramfs):** The runtime generates a `/init`
shell script from the OCI image's `ENTRYPOINT`/`CMD` and embeds
it directly in the ext4 rootfs before boot. No separate initramfs,
busybox, or agent binary is needed — the kernel's `init=/init`
runs the generated script, which `exec`s the workload command.
On exit, the script calls `poweroff -f` and the Firecracker VM
shuts down.

```
OCI image ENTRYPOINT/CMD
        │
        ▼
Runtime writes /init: "exec <entrypoint> <cmd>"
        │
        ▼
mkfs.ext4 -d <rootfs_dir> <image>
        │
        ▼
firecracker --kernel vmlinux --rootfs ext4.img
     (init=/init)
```

**Apple Virt (initramfs):** A full initramfs (`cpio.gz`) bundles
`pullrun-init` (static aarch64 musl binary), busybox applets,
and device nodes. `pullrun-init` mounts VirtioFS shares
(rootfs + volumes), wires the vsock connection to the host daemon,
and `exec`s the workload. Unlike Firecracker, the VM stays alive
after workload exit — a new vsock session can re-attach to a
fresh shell.

```
Initramfs layout:
  /init                  → shell script (exec /sbin/pullrun-init)
  /sbin/pullrun-init      → static binary (aarch64 musl)
  /bin/busybox           → applets: sh, cat, ls, mount, ...
  /dev/{console,null,tty}→ device nodes
  /proc, /sys, /etc, /mnt, /tmp → mount targets
```

The `pullrun-init` source lives in `runtime/pullrun-init/` and
is deliberately small (~500 lines of Rust) — it avoids an
init system, udev, or getty. The vsock protocol is a simple
framed message stream (length-prefixed JSON for control,
raw bytes for stdio).

Networking for the VM uses either:

- **Bridge mode** (default with `--net bridge`): a tap device
  attached to the shared `pullrun-br0` bridge — same L2 segment
  as the containers. The guest gets a static IP, with a
  deterministic MAC derived from the IP.
- **Slirp mode** (default with `--net slirp` or `--backend vm`):
  a tap device created by `slirp4netns` with a pure-userspace NAT
  stack. No bridge, no iptables, no kernel IP forwarding. Each VM
  gets its own private subnet; VMs cannot communicate with each
  other or with containers on the bridge. The only capability
  required is `CAP_NET_ADMIN` (for TAP creation), settable via
  `setcap cap_net_admin+ep` on the slirp4netns binary.

Pullrun uses a generic 6.x Linux kernel configured for minimal
boot time (no ACPI, no SCSI, no DRM, no sound). The kernel is
staged from an OCI kernel image (via `StagedKernel::from_image`)
or from pre-built paths on disk.

### Backend selection

The `Backend` enum determines which executor to use:

```rust
pub enum Backend {
    Container,       // LinuxContainerExecutor (runc)
    Vm,              // FirecrackerExecutor  (Linux) / AppleVirtExecutor (macOS)
    Wasm,            // planned: wasmtime / wasm-micro-runtime
}
```

### `ExecutorRouter`

A small dispatcher that holds all executor implementations and
routes by `spec.backend`. From the gRPC handler's perspective,
it's just `Executor`. The router also handles the case where the
request names a VM but the VM backend isn't configured (returns
`BackendNotAvailable`).

## 4. The shared network model (`pullrun-net`)

Pullrun supports two network models:

### Bridge model (default for containers)

**All workloads, regardless of backend, live on the same L2
segment** behind a single userspace proxy. This enables
inter-workload communication and shared IPAM.

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

### Slirp model (default for VMs, `--net slirp`)

Slirp mode replaces the shared bridge with per-VM userspace NAT,
eliminating the bridge and iptables entirely. Each VM gets its own
TAP device created by `slirp4netns`, which provides a lightweight
TCP/UDP/ICMP stack in userspace.

```
   VM C (10.42.1.100)      VM D (10.42.2.100)
   │                       │
   └─ tap-vm-c (slirp) ────┘  └─ tap-vm-d (slirp) ─────
         │                              │
   slirp4netns (userspace NAT)    slirp4netns (userspace NAT)
         │                              │
         └────────────── the internet ──┘
```

VMs in slirp mode cannot communicate with each other or with
bridge-mode workloads. This is the right choice for rootless
setups (no `iptables` or bridge kernel module required) and for
single-VM workloads that only need outbound internet access.

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

### Outbound: iptables MASQUERADE for VMs (bridge mode)

Containers in v0 share the host's network namespace for outbound
traffic (or use `NetworkMode::Host` directly). VMs need NAT
because their tap devices only see the bridge.

In bridge mode, Pullrun writes three iptables rules at boot
(and on every `ensure_bridge` call, idempotently):

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

### Outbound: slirp4netns userspace NAT (slirp mode)

In slirp mode, outbound NAT is provided entirely in userspace by
`slirp4netns`, which runs as a child process of `pullrun-runtime`.
It reads raw ethernet frames from the TAP device via its
`/dev/net/tun` fd, implements a minimal TCP/UDP/ICMP stack (based
on QEMU's libslirp), and forwards traffic through the host's
network stack as a regular userspace process. No iptables rules,
no kernel IP forwarding, no bridge.

**Requirements:**
- `slirp4netns` on PATH (install: `apt install slirp4netns` or
  `brew install slirp4netns`)
- `CAP_NET_ADMIN` on the slirp4netns binary (set once via
  `sudo setcap cap_net_admin+ep $(which slirp4netns)`)
- No bridge kernel module, no iptables, no root access at runtime

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

## 5. P2P block distribution (`pullrun-sync`)

Pullrun includes a peer-to-peer block synchronization layer that
enables cluster-wide image dissemination without registry
bottlenecks. Each node runs a gRPC `BlockSync` service alongside
the runtime daemon.

### Protocol (three-phase)

```
 Node A                              Node B
   │                                    │
   │── Phase 1: Bloom filter ──────────►│  "I have these digests"
   │◄── Phase 2: Delta request ────────│  "Send me the 12 I'm missing"
   │── Phase 3: Block stream ─────────►│  (bidirectional gRPC stream)
   │                                    │
```

**Phase 1 — Bloom filter exchange:** Each node maintains a Bloom
filter of every digest in its store. Filters are built with 10
bits per entry and 4 SHA-256-based hash functions (via the
`bitvec` crate). A filter is ~4 KiB per 10,000 entries.

**Phase 2 — Delta request:** The receiving node checks each filter
entry against its local store. Missing digests are batched into a
delta request.

**Phase 3 — Block transfer:** Missing nodes and blobs are
streamed over a bidirectional gRPC stream. Blocks are sent
uncompressed (the OCI registry already served gzip'd layers;
re-compressing is wasted CPU).

### Peer discovery

| Method | Scope | Configuration |
|--------|-------|---------------|
| Static | Fixed peers | `--sync-peers host:port` flags |
| mDNS   | Local subnet (RFC 6762) | Zero-conf, no config needed |
| Gossip | Multi-hop | SWIM-style epidemic protocol (planned) |

### Evaluation

For a 4-node cluster pulling `ubuntu:22.04` (7 layers, 183 MB
uncompressed):

| Method | Time |
|--------|------|
| Registry pull (single node) | 8.3 s |
| P2P sync (Bloom + delta) | 1.8 s |

The 4.6× improvement comes from reusing a node that already has
the image in its local store, avoiding WAN registry round-trips.

## 6. The policy engine (`pullrun-policy`)

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

## 7. The runtime service (`pullrun-runtime`)

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
| `ExecInWorkload` | Run a command inside a running workload (PTY allocated for TTY mode) |

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

### MCP server

The pullrun CLI includes an `mcp` subcommand that starts a Model Context Protocol
server, exposing 15 tools and 4 resources for AI agent integration. See
[docs/ALL_MCP.md](ALL_MCP.md) for the complete reference.

## 8. The control plane (stub)

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

## 9. Standalone smoke-test tooling (`tools/`)

The `tools/` directory contains six independent Cargo workspaces,
each a focused binary that exercises one subsystem in isolation.
They are not unit tests — they are standalone executables useful
for CI, debugging, and hardware validation.

| Tool | Platform | Purpose |
|------|----------|---------|
| `apple-virt-smoke` | macOS | FFI pool test — creates and destroys N Apple VMs concurrently to validate the Virtualization.framework FFI bindings under load. Exits after all VMs complete. |
| `apple-virt-exec` | macOS | Full VM workload runner without the daemon. Takes a kernel, initramfs, and rootfs directory, boots a VM, runs a command via vsock, and reports exit code + wall time. Useful for CI pipelines that need VM isolation without the gRPC stack. |
| `build-initramfs` | Cross-platform | Initramfs builder for Apple Virt guest images. Produces `pullrun-initramfs.cpio.gz` containing `pullrun-init` (static aarch64 musl), busybox, and device nodes. |
| `build-kernel-image` | Cross-platform | Packs a vmlinux binary into an OCI image layer for registry distribution. Enables `--kernel-image ghcr.io/...` workflows. |
| `firecracker-smoke` | Linux | Boots a real Firecracker microVM with a minimal Alpine rootfs, runs a single command, and validates the exit code. Requires `PULLRUN_FC_BIN` and `PULLRUN_FC_VMLINUX` env vars. |
| `vm-network-smoke` | Linux | Tests VM outbound connectivity by booting a microVM, running `wget` against a host-bound HTTP server, and checking the serial console for `pullrun-vm-outbound OK`. |

The standalone design means each tool has its own `Cargo.toml`
and `target/` directory — no shared build state, no dependency on
the runtime's crate graph. This is intentional: a CI job that
only needs `apple-virt-exec` should not compile the entire OCI
puller or policy engine.

## 10. Why Rust + Go

A deliberate split:

- **Rust owns the data plane.** The store, the executors, the
  proxy, the OCI pipeline, the policy engine. All of these
  benefit from `mmap`, zero-copy deserialization (rkyv), and
  fine-grained control over async runtime behavior. The Rust
  code is one Cargo workspace with eleven crates (pullrun-store,
  pullrun-oci, pullrun-exec, pullrun-vm, pullrun-net, pullrun-dns,
  pullrun-vsock, pullrun-sync, pullrun-policy, pullrun-runtime,
  pullrun-init).
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
