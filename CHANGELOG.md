# Changelog

## 0.5.0 — 2026-07-11

### Features
- **nftables support for bridge NAT** (`FirewallBackend` trait).
  Auto-detects the host's firewall backend at daemon startup — nftables
  (preferred, modern distros) or iptables (fallback, legacy). The
  `pullrun-net` crate now owns the firewall abstraction in
  `runtime/pullrun-net/src/firewall.rs`.
- **`IptablesBackend`** — ports the existing iptables logic (3 rules:
  MASQUERADE, 2×FORWARD) into the trait.
- **`NftablesBackend`** — equivalent rules via `nft -f -` atomic
  transactions (preferred). `inet` family table handles IPv4+IPv6.
  Table-based `delete table inet pullrun` for cleanup.
- **`detect_backend()`** — probes `nft --version` first, then
  `iptables --version`. Returns `None` on systems with neither.
- **`ensure_bridge_named` now uses the detected backend.** No behavior
  change for existing deployments — iptables is used where nft is
  unavailable. Backward compatible.

### Fixes
- **Sync tests: IPv6 loopback fallback.** ProtonVPN can remove all IPv4
  from `lo0`, not just reassign it. `loopback_ip()` now falls back to
  `::1` (IPv6 loopback) when no 127.0.0.x address is bindable.

## 0.4.2 — 2026-07-11

### Fixes
- **Sync tests fail on macOS with ProtonVPN.** VPN client reassigns the
  loopback interface from 127.0.0.1 to 127.0.0.2, causing
  `TcpListener::bind("127.0.0.1:0")` to return `EADDRNOTAVAIL`.
  `loopback_ip()` helper scans `127.0.0.1..=127.0.0.16` for the first
  bindable address. Applied to both `start_block_sync_server` and
  `start_registrar_server` test helpers.

## 0.4.1 — 2026-07-11

### Features
- **Async I/O for store reads** (`get_archived_async`, `get_blob_async`).  
  New async methods on `MmapStore` that check a non-blocking cache first,
  then fall back to `spawn_blocking`. Prevents tokio worker starvation
  during concurrent cold reads.
- **GC handler uses `spawn_blocking`.** Batch GC operations no longer
  run on the async runtime thread pool.
- **`inspect_workload` DAG walk uses `spawn_blocking`.** Large DAG walks
  in the inspect handler no longer block async workers.

### Tests
- **4 new tests** for async read methods:
  - `test_get_archived_async_matches_sync` — correctness vs synchronous path
  - `test_get_archived_async_cache_hit_is_fast` — sub-millisecond cache hits
  - `test_get_blob_async_matches_sync` — blob correctness
  - `test_concurrent_cold_reads_dont_starve_runtime` — 50 concurrent cold
    reads with timer probe (verifies no tokio starvation)

## 0.4.0 — 2026-07-10

### Features
- **DAG store garbage collection** (`pullrun gc`, `pullrun gc --apply`).  
  Mark-and-sweep GC with BFS reachability walk, 90% safety guard, op-lock
  protection, and CLI integration.
- **Crash-atomic writes** (write-then-rename with parent `fsync`).  
  All four `put` methods atomically write to a `.tmp` file then rename;
  daemon startup runs `recover()` to clean orphaned temp files.
- **Operation locks** protect in-flight pull/build/commit/import from GC.  
  RAII-based `OpLock` with `Drop` auto-cleanup; stale lock cleanup at
  daemon startup (1h TTL).
- **`kernel_image_digest` pinning** — stopped VMs' kernel layers are
  preserved by GC via `WorkloadState.kernel_image_digest`.
- **`walk_reachable` factored into pullrun-store** and shared by
  `dag_export.rs` and the GC crate.

### Fixes
- **`enumerate_gc_roots` data-loss bug fixed.**  
  `try_read()` silently returned an incomplete root set when a write lock
  was held. Changed to blocking `read().await` to guarantee correctness.
- **GC correctness fixes:**
  - BFS aborts on corrupted nodes (prevents silent subtree deletion).
  - Dry-run GC no longer mutates the filesystem (`list_fresh_op_locks`
    replaces `clean_stale_op_locks` in the dry-run path).
  - 90% safety guard measures unreachable bytes, not node count.
- **Pre-existing clippy warnings fixed** across the workspace.

### Tests
- **25 new tests** across PRs 1–5 (134 → 159 total):
  - 6 OpLock mechanics tests
  - 13 GarbageCollector unit tests (incl. 4 review-driven edge-case tests)
  - 3 root enumeration tests
  - 3 integration tests (shared-layer survival, concurrent put, concurrent get)

### Breaking changes
- None. All additions are backward-compatible.
