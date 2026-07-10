# Changelog

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
