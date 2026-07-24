# Changelog

## 0.6.7 — 2026-07-24

### Bug fixes
- **Rootless `runc` containers now launch correctly.** Fixed three intertwined
  bugs: `--root` flag was passed after the container ID (ignored by runc), spawn
  failures were not checked (daemon proceeded with a dead container), and uid/gid
  mappings + capabilities + mounts were not applied in the rootless config path.
  Rootless mode is now functional on Linux.
- **`pullrun save <tag>` resolves to digest before exporting.** Previously,
  saving by tag produced a silently empty archive. Now `ListImages` resolves the
  tag to a digest; unknown tags produce a clear error.
- **`pullrun rmi` accepts digest prefixes.** Truncated hex prefixes now work —
  the CLI walks the image list and matches the first digest starting with the
  given prefix.
- **`pullrun prune` properly removes exited workloads.** The workload map was not
  cleaned up after container exit, so `rmi` refused to remove the image. Prune
  now removes completed workloads from the map before cleaning up layers.
- **`pullrun build` catches directory-as-Dockerfile.** Passing a directory to
  `--file` now returns a clear error hinting at the correct argument order.
- **`walk_dag_collect` returns errors on invalid digests.** Instead of silently
  using a zero digest and producing a corrupt archive, the DAG export walk now
  propagates the error upward.

### Documentation
- **Binary sizes corrected in README.** Stripped Go binaries are ~14 MB (not
  20 MB); runtime is ~6 MB. Total install size ~20 MB.

## 0.6.6 — 2026-07-22

### Bug fixes
- **Support images with no base layer (`FROM scratch`).** The DAG layer walk now
  handles zero-layer images correctly instead of panicking on an empty iterator.
  `pullrun run scratch-image` works for static binaries and distroless images.
- **`pullrun load` re-roots GC roots.** Manifests loaded from a tar archive now
  register their root digest in the GC root set, preventing immediate garbage
  collection of freshly imported images.
- **OCI-compliant media type detection for Docker-schema manifests.** The
  manifest parser now accepts `application/vnd.docker.distribution.manifest.v1+json`
  (with and without `+prettyjws`) alongside the OCI types. Fixes pull failures
  from registries still serving the legacy Docker format.
- **Port-mapping errors propagate to the caller.** `ensure_mapping` now returns
  the actual error from `proxyd_client` instead of silently swallowing it.
- **Release profile corrected in `Cargo.toml`.** The workspace profile was using
  debug settings for release builds. Now `lto = "fat"` and `opt-level = 3` are
  applied consistently.

### Tests
- **E2E tests using `#[ignore]` for conditional skipping.** Tests that require
  external registries or root privileges are now marked `#[ignore]` with a
  clear reason string, so `cargo test` passes out of the box without special
  setup.

### Documentation
- **Pre-HN polish pass.** CVE severity claims matched to actual CVSS scores;
  benchmarks script published; sweeping claims softened to实事求语气.
- **Zenodo badge replaced.** Badge now uses shields.io (zenodo.org returns 403
  to GitHub's camo proxy).

## 0.6.5 — 2026-07-17

### Bug fixes
- **install.sh consistently uses suffixed binary names.** All three lookup paths
  (macOS/Linux CLI, Windows CLI, WSL2 runtime) now search for `$BIN-$OS-$ARCH`
  first before falling back to bare names. Previously the Windows path looked for
  bare `pullrun.exe` and the WSL2 path looked for bare `pullrun-runtime`, neither
  of which matched the actual tarball contents.

## 0.6.4 — 2026-07-16

### Features
- **CLI `-V` short flag.** Cobra's `Version` field is now registered before
  `InitDefaultVersionFlag()`, so `-V` works instead of the default `-v` (which
  would conflict with `--verbose`).
- **`pullrun-runtime --version`.** The runtime binary now supports
  `--version`/`-V` natively via clap's `#[command(version)]` attribute, instead
  of requiring a separate `version` subcommand.

### Bug fixes
- **install.sh version references are dynamic.** `CURRENT_VERSION` variable at
  the top of the script is referenced in rate-limit error messages instead of
  hardcoded strings, so users always see the correct suggested version.

## 0.6.3 — 2026-07-15

### Bug fixes
- **install.sh binary-selection glob no longer matches wrong binary.** The glob
  `$BIN-*` matched `pullrun-runtime-*` before `pullrun-*` when `BIN=pullrun`
  (alphabetical ordering). Replaced with exact `$BIN-$OS-$ARCH` matching with
  exact basename fallback, ensuring the CLI binary is always selected for
  installation.

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