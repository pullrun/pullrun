# Changelog

## 0.6.2 — 2026-07-14

### Bug fixes
- **`save`/`load` round-trip digest mismatch.** Removed incorrect blob content
  verification in `dag_import.rs` — the blob path uses the parent node's digest
  (SHA-256 of rkyv-serialized DagNode), not a hash of the raw blob data, so
  re-hashing the blob bytes always failed. Blob integrity is transitively
  guaranteed by its parent node's edges. Air-gapped export/import now works.
- **`secret ls` / `config ls` CREATED timestamp shows epoch zero.** Added
  `timestamp_from_meta()` helper that falls back to `meta.modified()` when
  `meta.created()` is unavailable (Linux ext4/xfs/btrfs). Fixes four
  occurrences in `list_secrets`, `inspect_secret`, `list_configs`,
  `inspect_config`. Also fixes `list_images` to read the node file's mtime.
- **`pullrun push` error message for unauthenticated pushes.** The CLI now
  detects 401/Unauthorized/`no Location header` errors and prints
  `not authenticated — run 'pullrun login <registry>'` instead of leaking
  HTTP-level details.
- **`pullrun --version` / `-V` flags.** Added cobra `Version: "0.6.2"` to the
  root command. Also added `pullrun-runtime version` subcommand.
- **`install.sh` GitHub API rate-limit handling.** When the API returns
  non-release JSON (rate-limited), `TAG` is now detected as empty and the
  script exits with a clear error suggesting `VERSION=v0.6.2`.
- **`install.sh` non-sudo install path.** When `sudo` is unavailable or the
  user has no password-less sudo, binaries are installed to
  `~/.local/bin/` instead of failing silently.

### Documentation
- README: removed non-functional `pullrun diff <tag1> <tag2>` example
  (diff only works on workload IDs). Changed `--network my-net` example
  to use the correct `--net bridge` flag.
- `pullrun workload` help text fixed from `(run, exec, list, ...)` to
  `(attach via workflow run)`.

## 0.6.1 — 2026-07-13

### Features
- **`pullrun images`** — new CLI command to list pulled images in the local
  DAG store, with table and `--json` output. Reuses the existing `ListImages`
  gRPC RPC (which was previously only exposed via the MCP server).
- **`pullrun compose` subcommand** — `pullrun compose` now delegates to the
  `pullrun-compose` binary, making compose workflows discoverable from the main
  CLI. Global flags (`--direct`, `--socket`, `--server`) are automatically
  stripped before delegation.

### Fixes
- **`pullrun exec` returns proper NotFound for missing IDs.** Added a workload
  existence pre-check in `exec_in_workload` before dispatching to the executor.
  Previously, missing workload IDs caused a misleading `runc exec failed`
  internal error; now they return `NotFound` consistently with other lifecycle
  commands.
- **`pullrun login` validates credentials before saving.** The login command
  now pings the registry's `/v2/` endpoint with provided credentials and
  rejects 401/403 responses before writing to `~/.pullrun/auth.json`. Also
  added an interactive prompt when no flags are supplied.
- **`pullrun build` resolves paths client-side.** Dockerfile and context
  directory paths are now resolved to absolute paths before being sent to the
  daemon, fixing `No such file or directory` errors when running from a
  different working directory.
- **All Dockerfile and flag errors fixed in docs.** `build`, `push`, `save`/`load`,
  `attach`, `compose`, `config create`, `secret create`, and `prune` now
  document the correct syntax in both `README.md` and `docs/PULLRUN_GUIDE.md`.

### Housekeeping
- Bumped version to 0.6.1 (Go CLI, Rust workspace, MCP server).

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
