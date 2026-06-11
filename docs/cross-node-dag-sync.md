# Cross-Node DAG Block Sync

> **The last Docker CE gap closed by turning image distribution into a content-addressed peer-to-peer block sync — not by reimplementing Swarm or Kubernetes.**

## The insight

OCI images are already content-addressed. The DAG store stores every blob keyed by its SHA256 digest. When two nodes have pulled the same image — or different images that share layers — they have byte-identical files on disk at paths determined by their digests.

This means:

- **No trust problem.** A blob `sha256:abc` from node A is the same blob `sha256:abc` on node B. Verification is inherent.
- **No registry needed for peer transfers.** The digest identifies the content uniquely. You don't need a signed manifest; you just need the hash.
- **Delta sync falls out for free.** If node A has blobs `[a, b, c, d]` and node B has `[a, e, f]`, syncing image `x` whose blob set is `[a, b, c]` costs exactly 2 block transfers (b and c), not a full layer pull.

Docker/containerd distributes images per-node — each kubelet pulls independently from the registry. BuildKit needs a shared remote cache to get cross-node dedup. **Pullrun doesn't.** The DAG store already deduplicates across images on a single node; `BlockSync` extends that dedup horizon to the cluster.

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
| `BlockSync` | gRPC service: `HaveBlobs`, `GetBlobs`, `SyncBlobs` RPCs | `runtime/pullrun-sync/src/block_sync.rs` |
| `SyncPuller` | `OciPuller` wrapper that queries peers before registry | `runtime/pullrun-sync/src/sync_puller.rs` |
| `BloomFilter` | Compact block set advertisement (1% false positive, 1 KB per 10 K blobs) | `runtime/pullrun-sync/src/bloom.rs` |
| `Discovery` | mDNS (UDP multicast, 30s heartbeat, 90s timeout) | `runtime/pullrun-sync/src/discovery.rs` |
| `BloomGossip` | Periodic bloom filter exchange with random peers (60s interval) | `runtime/pullrun-sync/src/gossip.rs` |
| `PeerBloomCache` | Peer bloom filter cache with 5-minute TTL | `runtime/pullrun-sync/src/gossip.rs` |
| `Registrar` | Optional centralized gRPC registry (Register/Lookup/ListPeers/Heartbeat/Deregister) | `runtime/pullrun-sync/src/registrar.rs` |
| `DeltaSync` | Given two bloom filters, computes the minimal transfer set | `runtime/pullrun-sync/src/delta.rs` |

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

service Registrar {
  // Register this node with the registrar.
  rpc Register(RegisterRequest) returns (RegisterResponse);
  // Look up a single peer by node_id.
  rpc Lookup(LookupRequest) returns (LookupResponse);
  // List all active (non-expired) peers.
  rpc ListPeers(ListPeersRequest) returns (ListPeersResponse);
  // Refresh the TTL for this node.
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  // Best-effort removal on clean shutdown.
  rpc Deregister(DeregisterRequest) returns (DeregisterResponse);
}

message PeerRegistration {
  string node_id   = 1;
  string sync_addr = 2;
  int64  last_seen_unix_secs = 3;
}

message RegisterRequest  { string node_id = 1; string sync_addr = 2; }
message RegisterResponse { int32 peer_count = 1; }
message LookupRequest    { string node_id = 1; }
message LookupResponse   { PeerRegistration peer = 1; bool found = 2; }
message ListPeersRequest {}
message ListPeersResponse { repeated PeerRegistration peers = 1; }
message HeartbeatRequest  { string node_id = 1; }
message HeartbeatResponse { int32 peer_count = 1; }
message DeregisterRequest  { string node_id = 1; }
message DeregisterResponse {}
```

### Pull flow (modified via `SyncPuller`)

```
kubelet → CRI shim → ImageService::PullImage
                          │
                          ▼
              pullrun-runtime PullImage handler
              (auto-detects block sync availability)
                          │
                          ├──► SyncPuller::pull(ref_name, platform)
                          │       │
                          │       ├──► Parse ref → get manifest digest via registry
                          │       │
                          │       ├──► For each blob in manifest:
                          │       │       │
                          │       │       ├──► has_blob_local(digest)? → skip
                          │       │       │
                          │       │       ├──► query PeerBloomCache (bloom filter)
                          │       │       │     └── O(1) bloom membership test per peer
                          │       │       │
                          │       │       ├──► candidates found? → connect to peer
                          │       │       │     └── BlockSyncClient::GetBlobs stream
                          │       │       │     └── store blob locally
                          │       │       │
                          │       │       └──► no candidate (or peer failed)?
                          │       │             └── OciPuller::fetch_blob_by_digest
                          │       │
                          │       └──► convert manifest to DAG, return root digest
                          │
                          └──► Return PullImageResponse to CRI shim
```

### Delta sync flow (pull time, not push)

The current implementation applies delta sync at **pull time** — nodes pull missing blobs from peers. There's no push-side delta yet (future work). The pull-side flow is described above.

Future push-side delta would work as follows (not yet implemented):

```
pullrun push myimage:latest
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

- Nodes broadcast `NodeAnnouncement { node_id, sync_addr, version }` as JSON over UDP multicast `239.255.0.100:54321`
- Each node maintains a peer table (`PeerInfo`) with instant-based timeout
- 30s broadcast interval, 90s peer eviction
- No central registrar needed
- Works across: same subnet, same wireguard network

### Level 2: Gossip (multi-subnet / WAN)

- `BloomGossip` background task picks a random peer from the peer table every 60s
- Sends local bloom filter via `HaveBlobs` RPC → receives peer's bloom filter
- Updates `PeerBloomCache` with peer's filter (5-minute TTL)
- Converges in O(log N) rounds; tolerates network partitions
- `SyncPuller` consults `PeerBloomCache` before pulling: O(1) bloom lookup per blob digest → connects to candidate peers via `BlockSyncClient` → registry fallback

### Level 3: Registrar (optional, for managed clusters)

- Control plane node hosts `Registrar` gRPC service on a configurable address (`--registrar-addr`)
- Worker nodes register via `--registrar-connect <addr>`: `Register` on startup, `Heartbeat` every 30s, `Deregister` on shutdown (best-effort)
- Registrar maintains peer registry with 120s TTL; background eviction every 30s
- Lookup by `node_id` returns peer's sync address; `ListPeers` returns all active peers
- Useful for large clusters (1000+ nodes) where mDNS is impractical (cross-subnet, cloud)
- Registrar is independent of block sync: can be hosted on nodes without `--sync-addr`

```
┌──────────────────────────────────────────────┐
│              Registrar (optional)              │
│                                                │
│  digest:sha256:abc ──► [10.0.0.1, 10.0.0.3]   │
│  digest:sha256:def ──► [10.0.0.2]             │
│  digest:sha256:xyz ──► [10.0.0.1, 10.0.0.4]   │
└──────────────────────────────────────────────┘
```

## Integration with pullrun-runtime daemon

**Zero changes to Kubernetes.** The CRI shim (`cri/pullrun-cri`) already implements `RuntimeService` and `ImageService`. The block sync integration is entirely in the pullrun-runtime daemon:

```rust
// In main.rs (simplified):
// 1. Start BlockSync gRPC server (--sync-addr)
let block_sync_service = BlockSyncService::new(store.clone());
let bss = BlockSyncServer::new(block_sync_service.clone());
tokio::spawn(async move { tonic::Server::builder().add_service(bss).serve(addr).await });

// 2. Start mDNS discovery
let discovery = Discovery::new(node_id, sync_addr);
tokio::spawn(async move { discovery.run().await });

// 3. Start bloom gossip
let cache = PeerBloomCache::new();
let gossip = BloomGossip::new(client, block_sync_service, discovery, cache.clone());
tokio::spawn(async move { gossip.run().await });

// 4. Optionally host Registrar (--registrar-addr)
let reg_svc = RegistrarService::new();
let reg_svr = RegistrarServer::new(reg_svc.clone());
tokio::spawn(async move { tonic::Server::builder().add_service(reg_svr).serve(addr).await });

// 5. Optionally register with remote Registrar (--registrar-connect)
let client = RegistrarClient::connect(addr).await?;
tokio::spawn(async move { run_registrar_client(client, node_id, sync_addr).await });

// 6. Wire PeerBloomCache into ServiceConfig → PullImage uses SyncPuller
config = config.with_bloom_cache(cache);
```

The `SyncPuller` is a drop-in wrapper that implements the same `ImagePuller` trait but checks peers via `PeerBloomCache` before the upstream registry. The scheduler still places pods as before — the only difference is that image pulls are faster on the 2nd through Nth nodes.

### RuntimeClass flow (unchanged)

```
Pod (runtimeClassName: pullrun-container)
  → kubelet → CRI → pullrun-cri
      → ImageService::PullImage (now via SyncPuller)
      → RuntimeService::RunPodSandbox
      → RuntimeService::CreateContainer
      → RuntimeService::StartContainer
```

## Performance characteristics

| Scenario | Docker/K8s (registry pull) | Pullrun (block sync) |
|----------|---------------------------|---------------------|
| 1 node, 1 image | 1 full pull | 1 full pull (same) |
| 10 nodes, 1 image | 10 full pulls | 1 full pull + 9 delta syncs |
| 10 nodes, same base image, different apps | 10 full pulls (partial layer cache per node) | 1 full pull (base) + N delta syncs (unique layers only) |
| Rolling update (new version) | N full pulls of new layers | 1 full pull + (N-1) delta syncs |
| Cold cluster, 100 nodes | 100 concurrent registry pulls → rate limited | 1 registry pull → peers distribute |

The key metric: **bytes transferred from registry**. Block sync reduces registry egress by O(N) in the cluster size.

## Implementation plan

### ✅ Phase 1: BlockSync protocol + mDNS discovery *(complete)*

- [x] `runtime/pullrun-sync/` — new crate
- [x] `BloomFilter` — compact set representation with configurable FP rate
- [x] `BlockSyncService` — gRPC server (HaveBlobs, GetBlobs, SyncBlobs)
- [x] `mDNS discovery` — UPD multicast (`239.255.0.100:54321`, 30s heartbeat, 90s timeout)
- [x] `SyncPuller` — OciPuller wrapper, queries peers before registry
- [x] `OciPuller::resolve_image()` + `fetch_blob_by_digest()` — non-breaking peer API support

### ✅ Phase 2: CRI integration + gossip *(complete)*

- [x] Wire `SyncPuller` into daemon `PullImage` handler (auto-detects block sync)
- [x] Gossip-based bloom filter exchange (`BloomGossip`, 60s interval)
- [x] `PeerBloomCache` with 5-minute TTL
- [x] Fallback to registry when peer unavailable (SyncPuller fallback chain: local → peers → registry)
- [x] e2e test: 4 P2P blob transfer tests (GetBlobs, HaveBlobs, bloom cache, multi-node)

### ✅ Phase 3: Registrar + metrics *(complete)*

- [x] Optional `Registrar` gRPC service (Register/Lookup/ListPeers/Heartbeat/Deregister)
- [x] `--registrar-addr` flag to host registrar, `--registrar-connect` to register remotely
- [x] Background TTL-based peer eviction (120s default)
- [x] Prometheus metrics: `bytes_sent`, `bytes_received`, `blob_requests` counters, `pullrun_sync_peer_count` gauge
- [x] Graceful degrade: registry fallback when peers unavailable
- [x] 4 registrar e2e tests (register+list, lookup, heartbeat, deregister)
- [x] 126 Rust + 9 Go tests — all passing

### Future

- [ ] Cross-datacenter: bloom filter caching + WAN-optimized transfers
- [ ] 3-node e2e: full daemons, pull image on node 1, delta sync to nodes 2+3
- [ ] Registrar discovery integration: Poll registrar for peers → feed into `PeerBloomCache`

## Why not IPFS / BitTorrent / P2P Docker?

| Approach | Why not |
|----------|---------|
| **IPFS** | Adds a dependency on IPFS daemon + IPLD schema. DAG store is already content-addressed; wrapping IPFS around it adds latency and complexity with no benefit over a direct gRPC block sync. |
| **BitTorrent** | Built for swarms of seeders/leechers around single files. Image blobs are small (layers may be 100 MB+ but individual blobs are typically 4 KB–64 KB). Tracker overhead and piece selection aren't a good fit for our blob set. |
| **docker/registry** (pull-through cache) | Every node still pulls every blob from the cache. No delta sync, no peer-to-peer. Cache hit reduces registry load but doesn't reduce bytes on the wire per node. |
| **Singularity / Apptainer** | Single-node container runtime designed for HPC; no multi-node image distribution protocol. |
| **containerd remote snapshotter** (stargz, nydus) | Optimizes lazy pulling, not cross-node dedup. Still per-node pull. |

The BlockSync protocol is ~20–30 KB of Rust gRPC code with zero external runtime dependencies. It's the minimal layer that turns the existing DAG store into a peer-to-peer block distribution network.

## Status

**All 126 Rust + 9 Go tests passing.** Phase 1-3 complete:

| Phase | Feature | Status | Key files |
|-------|---------|--------|-----------|
| 1 | BlockSync protocol + mDNS | ✅ | `block_sync.rs`, `bloom.rs`, `discovery.rs`, `sync_puller.rs` |
| 2 | Gossip bloom exchange + PeerBloomCache | ✅ | `gossip.rs` |
| 3a | Prometheus metrics | ✅ | `block_sync.rs` (`BlockSyncMetrics`), `metrics.rs` (`pullrun_sync_peer_count`) |
| 3b | Optional Registrar service | ✅ | `registrar.rs` |

## Open questions

1. **Authentication.** Should block sync be unauthenticated within a trusted network (e.g. wireguard mesh), or should peers verify each other via mTLS? Answer: mTLS for production, plain TCP for dev loopback. Default: wrap in existing TLS config. *Not yet implemented.*

2. **Bloom filter refresh frequency.** A node's blob set grows over time as images are pulled. How often should bloom filters be exchanged? Answer: on pull completion and every 60s during gossip heartbeats. Initial rebuild 5s after start, then every 300s. *Implemented.*

3. **Large blob streaming.** A single layer blob can be 1 GB+. `GetBlobs` streams chunks (1 MB) to avoid buffering. *Implemented.*

4. **Partial availability.** If some peers are down, we should still proceed with registry fallback. Answer: configurable peer timeout (default 2s), registry fallback on timeout or error. *Implemented in `SyncPuller`.*

5. **Registrar ↔ gossip integration.** Should the registrar feed into `PeerBloomCache` for cluster-wide discovery? Answer: future work — currently registrar is purely registration/heartbeat; peers listed via `ListPeers` can be added to `Discovery` for gRPC connection.
