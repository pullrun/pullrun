# Cross-Node DAG Block Sync

> **The last Docker CE gap closed by turning image distribution into a content-addressed peer-to-peer block sync — not by reimplementing Swarm or Kubernetes.**

## The insight

OCI images are already content-addressed. The DAG store stores every blob keyed by its SHA256 digest. When two nodes have pulled the same image — or different images that share layers — they have byte-identical files on disk at paths determined by their digests.

This means:

- **No trust problem.** A blob `sha256:abc` from node A is the same blob `sha256:abc` on node B. Verification is inherent.
- **No registry needed for peer transfers.** The digest identifies the content uniquely. You don't need a signed manifest; you just need the hash.
- **Delta sync falls out for free.** If node A has blobs `[a, b, c, d]` and node B has `[a, e, f]`, syncing image `x` whose blob set is `[a, b, c]` costs exactly 2 block transfers (b and c), not a full layer pull.

Docker/containerd distributes images per-node — each kubelet pulls independently from the registry. BuildKit needs a shared remote cache to get cross-node dedup. **Nimbus doesn't.** The DAG store already deduplicates across images on a single node; `BlockSync` extends that dedup horizon to the cluster.

## Architecture

```
┌─────────────────────┐                ┌─────────────────────┐
│   Node A (agent)     │                │   Node B (agent)    │
│                      │                │                     │
│  ┌─────────────────┐ │                │  ┌────────────────┐ │
│  │  DAG Store      │ │                │  │  DAG Store     │ │
│  │  [a, b, c, d]   │ │                │  │  [a, e, f]     │ │
│  └────────┬────────┘ │                │  └────────┬───────┘ │
│           │          │                │           │         │
│  ┌────────▼────────┐ │                │  ┌────────▼───────┐ │
│  │  BlockSync       │◄──── gRPC ──────►│  BlockSync       │ │
│  │  (server+client) │ │   bidi stream  │  (server+client) │ │
│  └────────┬────────┘ │                │  └────────┬───────┘ │
│           │          │                │           │         │
│  ┌────────▼────────┐ │                │  ┌────────▼───────┐ │
│  │  OciPuller      │ │                │  │  OciPuller     │ │
│  │  (fallback)     │ │                │  │  (fallback)    │ │
│  │  │              │ │                │  │  │             │ │
│  │  ▼              │ │                │  │  ▼             │ │
│  │  Registry       │ │                │  │  Registry      │ │
│  └─────────────────┘ │                │  └────────────────┘ │
└─────────────────────┘                └─────────────────────┘

                ▲                        ▲
                │                        │
                └──────────┬─────────────┘
                           │
                  ┌────────▼────────┐
                  │  Discovery      │
                  │  (mDNS / gossip │
                  │   / registrar)  │
                  └─────────────────┘
```

### Components

| Component | Responsibility | File |
|-----------|---------------|------|
| `BlockSync` | gRPC service: `HaveBlobs`, `GetBlobs`, `SyncBlobs` RPCs | `runtime/nimbus-sync/src/block_sync.rs` |
| `SyncPuller` | `OciPuller` wrapper that queries peers before registry | `runtime/nimbus-sync/src/sync_puller.rs` |
| `BloomFilter` | Compact block set advertisement (1% false positive, 1 KB per 10 K blobs) | `runtime/nimbus-sync/src/bloom.rs` |
| `Discovery` | mDNS (LAN) + gossip (WAN) + optional registrar fallback | `runtime/nimbus-sync/src/discovery.rs` |
| `DeltaSync` | Given two bloom filters, computes the minimal transfer set | `runtime/nimbus-sync/src/delta.rs` |

### gRPC protocol

```protobuf
service BlockSync {
  // Advertise which blobs this node has.
  rpc HaveBlobs(HaveBlobsRequest) returns (HaveBlobsResponse);

  // Request specific blobs by digest. Returns a stream of blob data.
  rpc GetBlobs(GetBlobsRequest) returns (stream BlobChunk);

  // Bidirectional: both sides send/receive missing blobs concurrently.
  rpc SyncBlobs(stream SyncBlob) returns (stream SyncBlob);
}

message HaveBlobsRequest {
  // Bloom filter of all digests this node stores locally.
  bytes bloom_filter = 1;
  int32 bloom_k = 2;        // number of hash functions
  int32 bloom_m = 3;        // filter size in bits
}

message HaveBlobsResponse {
  // The peer's bloom filter of its own blobs.
  bytes bloom_filter = 1;
  int32 bloom_k = 2;
  int32 bloom_m = 3;
}

message GetBlobsRequest {
  repeated string digests = 1;  // "sha256:..." list
}

message BlobChunk {
  string digest = 1;
  bytes data = 2;
  uint32 offset = 3;        // for streaming large blobs
  bool final = 4;
}

message SyncBlob {
  string digest = 1;
  bytes data = 2;
}
```

### Pull flow (modified `OciPuller::pull`)

```
kubelet → CRI shim → ImageService::PullImage
                          │
                          ▼
                   SyncPuller::pull(ref_name, platform)
                          │
                          ├──► Parse ref → get manifest digest from registry
                          │
                          ├──► For each blob in manifest:
                          │       │
                          │       ├──► has_blob_local(digest)? → skip
                          │       │
                          │       ├──► query peers via BlockSync::HaveBlobs
                          │       │     └── bloom filter check
                          │       │
                          │       ├──► peer has it? → BlockSync::GetBlobs
                          │       │     └── stream blob data, store locally
                          │       │
                          │       └──► nobody has it? → pull from upstream registry
                          │
                          └──► return image reference
```

### Delta sync flow (push to cluster)

```
nimbusctl push myimage:latest
        │
        ▼
  DagPusher::push(root_digest)
        │
        ├──► Walk DAG, collect all blob digests
        │
        ├──► For each peer:
        │       │
        │       ├──► Send my bloom filter → receive peer's bloom filter
        │       │
        │       ├──► Compute delta = my_blobs - peer_blobs
        │       │
        │       └──► Stream delta blobs via SyncBlobs
        │
        └──► push manifest list to upstream registry (if needed)
```

## Discovery

### Level 1: mDNS (local LAN, zero config)

- Nodes broadcast their presence on `_nimbus-sync._tcp.local`
- Each node maintains a peer table with bloom filters
- No central registrar needed
- Works across: same subnet, same wireguard network

### Level 2: Gossip (multi-subnet / WAN)

- Each node maintains a partial view of the cluster
- Periodic `HAVE` messages exchanged with random peers (gossip protocol, fanout = 3)
- Bloom filters piggyback on gossip heartbeats
- Converges in O(log N) rounds
- Tolerates network partitions gracefully

### Level 3: Registrar (optional, for managed clusters)

- Control plane node maintains a `Digest → [peer addresses]` index
- New nodes register on join, publish their bloom filter
- Puller queries registrar for "who has blob X?" before broadcasting
- Useful for large clusters (1000+ nodes) where gossip traffic is non-trivial

```
┌──────────────────────────────────────────────┐
│              Registrar (optional)              │
│                                                │
│  digest:sha256:abc ──► [10.0.0.1, 10.0.0.3]   │
│  digest:sha256:def ──► [10.0.0.2]             │
│  digest:sha256:xyz ──► [10.0.0.1, 10.0.0.4]   │
└──────────────────────────────────────────────┘
```

## Integration with Kubernetes

**Zero changes to Kubernetes.** The CRI shim (`cri/nimbus-cri`) already implements `RuntimeService` and `ImageService`. The only change is in `ImageService::PullImage`:

```rust
// Before (current):
let image = OciPuller::new(&config).pull(ref_name, platform)?;

// After:
let image = SyncPuller::new(&config, &block_sync_client)
    .pull(ref_name, platform)?;
```

The `SyncPuller` is a drop-in wrapper that implements the same `ImagePuller` trait but checks peers before the upstream registry. The scheduler still places pods as before — the only difference is that image pulls are faster on the 2nd through Nth nodes.

### RuntimeClass flow (unchanged)

```
Pod (runtimeClassName: nimbus-container)
  → kubelet → CRI → nimbus-cri
      → ImageService::PullImage (now via SyncPuller)
      → RuntimeService::RunPodSandbox
      → RuntimeService::CreateContainer
      → RuntimeService::StartContainer
```

## Performance characteristics

| Scenario | Docker/K8s (registry pull) | Nimbus (block sync) |
|----------|---------------------------|---------------------|
| 1 node, 1 image | 1 full pull | 1 full pull (same) |
| 10 nodes, 1 image | 10 full pulls | 1 full pull + 9 delta syncs |
| 10 nodes, same base image, different apps | 10 full pulls (partial layer cache per node) | 1 full pull (base) + N delta syncs (unique layers only) |
| Rolling update (new version) | N full pulls of new layers | 1 full pull + (N-1) delta syncs |
| Cold cluster, 100 nodes | 100 concurrent registry pulls → rate limited | 1 registry pull → peers distribute |

The key metric: **bytes transferred from registry**. Block sync reduces registry egress by O(N) in the cluster size.

## Implementation plan

### Phase 1: BlockSync protocol + mDNS discovery

- [ ] `runtime/nimbus-sync/` — new crate
- [ ] `BloomFilter` — compact set representation with configurable FP rate
- [ ] `BlockSyncService` — gRPC server (HaveBlobs, GetBlobs, SyncBlobs)
- [ ] `mDNS discovery` — zeroconf peer discovery on `_nimbus-sync._tcp.local`
- [ ] `SyncPuller` — OciPuller wrapper, queries peers before registry
- [ ] Integration test: two nimbus agents on same machine, pull on A, verify B delta-syncs

### Phase 2: CRI integration + gossip

- [ ] Wire `SyncPuller` into `cri/nimbus-cri` `ImageService::PullImage`
- [ ] Gossip-based bloom filter exchange (WAN-friendly)
- [ ] Fallback to registry when peer unavailable
- [ ] Bloom filter invalidation on blob GC/prune
- [ ] e2e test: 3 nodes, pull image on node 1, schedule pod on node 2, verify no registry pull

### Phase 3: Registrar + metrics

- [ ] Optional registrar service (digest → peer index)
- [ ] Prometheus metrics: bytes saved by block sync, peer transfer latency, bloom filter false positive rate
- [ ] Graceful degrade: if no peers respond, fall back to registry pull
- [ ] Cross-datacenter: bloom filter caching + WAN-optimized transfers

## Why not IPFS / BitTorrent / P2P Docker?

| Approach | Why not |
|----------|---------|
| **IPFS** | Adds a dependency on IPFS daemon + IPLD schema. DAG store is already content-addressed; wrapping IPFS around it adds latency and complexity with no benefit over a direct gRPC block sync. |
| **BitTorrent** | Built for swarms of seeders/leechers around single files. Image blobs are small (layers may be 100 MB+ but individual blobs are typically 4 KB–64 KB). Tracker overhead and piece selection aren't a good fit for our blob set. |
| **docker/registry** (pull-through cache) | Every node still pulls every blob from the cache. No delta sync, no peer-to-peer. Cache hit reduces registry load but doesn't reduce bytes on the wire per node. |
| **containerd remote snapshotter** (stargz, nydus) | Optimizes lazy pulling, not cross-node dedup. Still per-node pull. |

The BlockSync protocol is ~20–30 KB of Rust gRPC code with zero external runtime dependencies. It's the minimal layer that turns the existing DAG store into a peer-to-peer block distribution network.

## Open questions

1. **Authentication.** Should block sync be unauthenticated within a trusted network (e.g. wireguard mesh), or should peers verify each other via mTLS? Answer: mTLS for production, plain TCP for dev loopback. Default: wrap in existing TLS config.

2. **Bloom filter refresh frequency.** A node's blob set grows over time as images are pulled. How often should bloom filters be exchanged? Answer: on pull completion and every 30s during gossip heartbeats.

3. **Large blob streaming.** A single layer blob can be 1 GB+. `GetBlobs` streams chunks to avoid buffering. Should we use HTTP range requests or gRPC streaming? Answer: gRPC streaming (simple, same transport as other RPCs).

4. **Partial availability.** If some peers are down, we should still proceed with registry fallback. Answer: configurable peer timeout (default 2s), registry fallback on timeout or error.
