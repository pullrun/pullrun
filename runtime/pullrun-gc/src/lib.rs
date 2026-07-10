use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use pullrun_store::{list_fresh_op_locks, walk_reachable, Digest, MmapStore, StoreError};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Error, Debug)]
pub enum GcError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("active operation locks exist, GC deferred")]
    ActiveOpLocks,
    #[error("root set is empty — nothing to preserve")]
    EmptyRootSet,
    #[error("root digest {0} not found in store")]
    RootDigestMissing(Digest),
    #[error("corrupted node {0} during BFS walk — aborting to prevent subtree deletion")]
    CorruptedNodeDuringWalk(Digest),
    #[error("store error during GC: {0}")]
    Store(#[from] StoreError),
    #[error("would free {pct}% of {total_mb} MB store (max {max_pct}%); use --force to override")]
    WouldFreeTooMuch {
        pct: u64,
        total_mb: u64,
        max_pct: u64,
    },
}

#[derive(Debug, Default, Clone)]
pub struct GcReport {
    pub total_nodes: usize,
    pub reachable_nodes: usize,
    pub unreachable_nodes: usize,
    pub deleted_nodes: usize,
    pub deleted_blobs: usize,
    pub bytes_freed: u64,
    pub dry_run: bool,
}

/// Garbage collector for the content-addressed DAG store.
///
/// Usage:
/// ```
/// use pullrun_store::{Digest, MmapStore};
/// use pullrun_gc::GarbageCollector;
/// use std::sync::Arc;
///
/// let store = Arc::new(MmapStore::new("/tmp/gc-test".into()));
/// let gc = GarbageCollector::new(store, "/tmp/gc-test".into());
/// // In a real invocation, roots would come from image_tags + workloads.
/// // An empty root set is rejected by the safety check:
/// assert!(gc.collect(&[]).is_err());
/// ```
pub struct GarbageCollector {
    store: Arc<MmapStore>,
    store_root: PathBuf,
    dry_run: bool,
    force: bool,
}

impl GarbageCollector {
    pub fn new(store: Arc<MmapStore>, store_root: PathBuf) -> Self {
        Self {
            store,
            store_root,
            dry_run: true,
            force: false,
        }
    }

    pub fn with_dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    pub fn with_force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    /// Run garbage collection.
    ///
    /// `roots` is the set of root digests to preserve (from image_tags,
    /// workload checkpoints, etc.).
    pub fn collect(&self, roots: &[Digest]) -> Result<GcReport, GcError> {
        // 1. Check for active op locks — GC must not race with in-flight
        //    operations that are still writing to the store.
        //    Uses list_fresh_op_locks (read-only) to avoid mutating the
        //    filesystem during a dry run. Stale lock cleanup is handled
        //    by the daemon at startup.
        let fresh = list_fresh_op_locks(&self.store_root)?;
        if !fresh.is_empty() && !self.force {
            debug!(count = fresh.len(), "active op locks found, deferring GC");
            return Err(GcError::ActiveOpLocks);
        }
        // Only clean stale locks on a real (non-dry) run.
        if !self.dry_run {
            let _ = pullrun_store::clean_stale_op_locks(&self.store_root)?;
        }

        // 2. Safety: root set must not be empty.
        if roots.is_empty() {
            return Err(GcError::EmptyRootSet);
        }

        // 3. Pre-check: all root digests must exist in the store.
        for root in roots {
            if !self.store.exists(root) {
                return Err(GcError::RootDigestMissing(*root));
            }
        }

        // 4. Enumerate all stored node digests by walking the filesystem.
        let all_digests = self.list_all_node_digests()?;
        let total_nodes = all_digests.len();

        // 5. BFS walk from roots to find reachable digests.
        //    Aborts on corrupted nodes to prevent subtree deletion.
        let (reachable_nodes, _reachable_blobs) = walk_reachable(&self.store, roots)
            .map_err(|e| match e {
                StoreError::Corrupted(d, _) => GcError::CorruptedNodeDuringWalk(d),
                other => GcError::Store(other),
            })?;
        let reachable: HashSet<Digest> = reachable_nodes.iter().copied().collect();
        let reachable_count = reachable.len();

        // 6. Compute unreachable set.
        let unreachable: Vec<Digest> = all_digests
            .into_iter()
            .filter(|d| !reachable.contains(d))
            .collect();
        let unreachable_count = unreachable.len();

        // 7. Safety: compute unreachable bytes and abort if >90% of the
        //    store would be freed and the store is larger than 100 MB.
        //    (Likely indicates a root-set bug rather than genuine garbage.)
        //    Override with --force.
        if !self.force && total_nodes > 0 {
            let unreachable_bytes: u64 = unreachable
                .iter()
                .filter_map(|d| {
                    let p = self.store.node_path(d);
                    fs::metadata(&p).map(|m| m.len()).ok()
                })
                .sum();
            let total_bytes = self.store.total_bytes();
            if total_bytes > 100 * 1024 * 1024 {
                let pct = unreachable_bytes * 100 / total_bytes;
                if pct >= 90 {
                    return Err(GcError::WouldFreeTooMuch {
                        pct,
                        total_mb: total_bytes / 1024 / 1024,
                        max_pct: 90,
                    });
                }
            }
        }

        // 8. Sweep phase.
        let mut report = GcReport {
            total_nodes,
            reachable_nodes: reachable_count,
            unreachable_nodes: unreachable_count,
            ..Default::default()
        };

        if self.dry_run {
            report.dry_run = true;
            return Ok(report);
        }

        let mut deleted_nodes = 0usize;
        let mut deleted_blobs = 0usize;
        let mut bytes_freed = 0u64;

        for digest in &unreachable {
            // Evict from in-memory cache before deleting from disk.
            self.store.evict_cache_entry(digest);

            // Delete node file.
            let node_path = self.store.node_path(digest);
            if node_path.exists() {
                let size = fs::metadata(&node_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                fs::remove_file(&node_path)?;
                bytes_freed += size;
                deleted_nodes += 1;
            }

            // Delete associated blob file if it exists.
            let blob_path = self.store.blob_path(digest);
            if blob_path.exists() {
                let size = fs::metadata(&blob_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                fs::remove_file(&blob_path)?;
                bytes_freed += size;
                deleted_blobs += 1;
            }

            // Remove empty shard directories.
            self.clean_empty_dirs(&node_path);
        }

        info!(
            deleted_nodes,
            deleted_blobs,
            bytes_freed,
            reachable_nodes = reachable_count,
            "GC sweep complete"
        );

        report.deleted_nodes = deleted_nodes;
        report.deleted_blobs = deleted_blobs;
        report.bytes_freed = bytes_freed;

        Ok(report)
    }

    /// Walk the store directory tree and collect all `node.rkyv` file
    /// paths, reconstructing each `Digest` from the sharded path structure.
    fn list_all_node_digests(&self) -> Result<Vec<Digest>, GcError> {
        let mut digests = Vec::new();
        let root = self.store.root_dir().to_path_buf();
        self.walk_store_dir(&root, &mut digests)?;
        Ok(digests)
    }

    fn walk_store_dir(&self, dir: &Path, digests: &mut Vec<Digest>) -> Result<(), GcError> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if name == "ops" || name.starts_with('.') {
                    continue;
                }
                self.walk_store_dir(&path, digests)?;
            } else if path.file_name().and_then(|n| n.to_str()) == Some("node.rkyv") {
                match self.digest_from_path(&path) {
                    Some(digest) => digests.push(digest),
                    None => {
                        warn!(path = %path.display(), "failed to reconstruct digest from path — node invisible to GC");
                    }
                }
            }
        }
        Ok(())
    }

    /// Reconstruct a `Digest` from a `node.rkyv` file path.
    ///
    /// Path format: `<root>/<hex[0..2]>/<hex[2..4]>/<hex[4..]>/node.rkyv`
    /// Hex digest = `<hex[0..2]><hex[2..4]><hex[4..]>` (64 chars).
    fn digest_from_path(&self, path: &Path) -> Option<Digest> {
        let parent = path.parent()?;
        let tail = parent.file_name()?;
        let grandparent = parent.parent()?;
        let mid = grandparent.file_name()?;
        let great = grandparent.parent()?;
        let head = great.file_name()?;

        let hex = format!(
            "{}{}{}",
            head.to_str()?,
            mid.to_str()?,
            tail.to_str()?
        );
        Digest::from_hex(&hex).ok()
    }

    /// Walk up from a deleted node's parent directory, removing empty
    /// shard directories until reaching the store root or a non-empty dir.
    fn clean_empty_dirs(&self, path: &Path) {
        let store_root = self.store.root_dir();
        let mut dir = path.parent().unwrap_or(path);
        loop {
            if !dir.starts_with(store_root) || dir == store_root {
                break;
            }
            let is_empty = fs::read_dir(dir)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = fs::remove_dir(dir);
                dir = dir.parent().unwrap_or(dir);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pullrun_store::DagNode;
    use tempfile::TempDir;

    fn store_and_dir() -> (Arc<MmapStore>, TempDir) {
        let dir = TempDir::new().expect("tempdir");
        let store = Arc::new(MmapStore::new(dir.path().join("store")));
        (store, dir)
    }

    fn insert_test_graph(store: &MmapStore) -> Digest {
        let blob_a = store
            .put_blocking(&DagNode::blob(b"blob a".to_vec()))
            .unwrap();
        let tree_a = store
            .put_blocking(&DagNode::tree(vec![blob_a], b"tree a".to_vec()))
            .unwrap();
        let layer_a = store
            .put_blocking(&DagNode::layer(vec![tree_a], b"layer a".to_vec()))
            .unwrap();
        store
            .put_blocking(&DagNode::manifest(vec![layer_a], b"config a".to_vec()))
            .unwrap()
    }

    fn insert_orphan(store: &MmapStore) -> Digest {
        store
            .put_blocking(&DagNode::blob(b"orphan blob".to_vec()))
            .unwrap()
    }

    #[test]
    fn test_dry_run_returns_no_deletions() {
        let (store, _dir) = store_and_dir();
        let root = insert_test_graph(&store);
        let _orphan = insert_orphan(&store);
        let gc = GarbageCollector::new(store, _dir.path().join("store"));
        let report = gc.collect(&[root]).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.deleted_nodes, 0);
        assert_eq!(report.deleted_blobs, 0);
        assert!(report.unreachable_nodes > 0);
    }

    #[test]
    fn test_orphan_is_deleted() {
        let (store, _dir) = store_and_dir();
        let root = insert_test_graph(&store);
        let orphan = insert_orphan(&store);
        let gc = GarbageCollector::new(store.clone(), _dir.path().join("store"))
            .with_dry_run(false);
        let report = gc.collect(&[root]).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.deleted_nodes, 1);
        assert!(store.exists(&root), "root should still exist");
        assert!(!store.exists(&orphan), "orphan should be deleted");
    }

    #[test]
    fn test_garbage_collector_preserves_reachable() {
        let (store, _dir) = store_and_dir();
        let root = insert_test_graph(&store);
        insert_orphan(&store);
        let gc = GarbageCollector::new(store.clone(), _dir.path().join("store"))
            .with_dry_run(false);
        let report = gc.collect(&[root]).unwrap();
        assert_eq!(report.deleted_nodes, 1);
        assert!(store.exists(&root));
        let archived = store.get_archived(&root).unwrap();
        for edge in archived.edges.iter() {
            assert!(store.exists(&Digest(*edge)));
        }
    }

    #[test]
    fn test_empty_root_set_returns_error() {
        let (store, _dir) = store_and_dir();
        let gc = GarbageCollector::new(store, _dir.path().join("store"));
        let err = gc.collect(&[]).unwrap_err();
        assert!(matches!(err, GcError::EmptyRootSet));
    }

    #[test]
    fn test_missing_root_returns_error() {
        let (store, _dir) = store_and_dir();
        let missing = Digest::compute(b"nonexistent");
        let gc = GarbageCollector::new(store, _dir.path().join("store"));
        let err = gc.collect(&[missing]).unwrap_err();
        assert!(matches!(err, GcError::RootDigestMissing(d) if d == missing));
    }

    #[test]
    fn test_op_lock_blocks_gc() {
        let (store, dir) = store_and_dir();
        let _lock = pullrun_store::OpLock::new(&dir.path().join("store")).unwrap();
        let gc = GarbageCollector::new(store, dir.path().join("store"));
        let err = gc.collect(&[Digest::compute(b"dummy")]).unwrap_err();
        assert!(matches!(err, GcError::ActiveOpLocks));
    }

    #[test]
    fn test_force_overrides_op_lock() {
        let (store, dir) = store_and_dir();
        let root = insert_test_graph(&store);
        let _lock = pullrun_store::OpLock::new(&dir.path().join("store")).unwrap();
        let gc = GarbageCollector::new(store.clone(), dir.path().join("store"))
            .with_force(true)
            .with_dry_run(false);
        let report = gc.collect(&[root]).unwrap();
        assert_eq!(report.deleted_nodes, 0);
    }

    #[test]
    fn test_gc_report_counts() {
        let (store, _dir) = store_and_dir();
        let root = insert_test_graph(&store);
        insert_orphan(&store);
        let gc = GarbageCollector::new(store, _dir.path().join("store"));
        let report = gc.collect(&[root]).unwrap();
        assert_eq!(report.reachable_nodes, 4);
        assert_eq!(report.total_nodes, 5);
        assert_eq!(report.unreachable_nodes, 1);
    }

    #[test]
    fn test_multiple_roots() {
        let (store, _dir) = store_and_dir();
        let root_a = insert_test_graph(&store);
        let blob_b = store
            .put_blocking(&DagNode::blob(b"blob b".to_vec()))
            .unwrap();
        let manifest_b = store
            .put_blocking(&DagNode::manifest(vec![blob_b], b"config b".to_vec()))
            .unwrap();
        let orphan = insert_orphan(&store);

        let gc = GarbageCollector::new(store.clone(), _dir.path().join("store"))
            .with_dry_run(false);
        let report = gc.collect(&[root_a, manifest_b]).unwrap();
        assert_eq!(report.deleted_nodes, 1);
        assert!(store.exists(&root_a));
        assert!(store.exists(&manifest_b));
        assert!(!store.exists(&orphan));
    }

    #[test]
    fn test_gc_aborts_on_corrupted_reachable_node() {
        let (store, _dir) = store_and_dir();
        let root = insert_test_graph(&store);
        // Graph: manifest_a -> layer_a -> tree_a -> blob_a.
        // Corrupt the tree node (middle of the graph) so BFS fails.
        let archived = store.get_archived(&root).unwrap();
        let layer_a = Digest(*archived.edges.first().unwrap());
        let layer_archived = store.get_archived(&layer_a).unwrap();
        let tree_a = Digest(*layer_archived.edges.first().unwrap());
        // Capture blob digest before corruption.
        let tree_archived = store.get_archived(&tree_a).unwrap();
        let blob_digest = Digest(*tree_archived.edges.first().unwrap());

        let tree_path = store.node_path(&tree_a);
        std::fs::write(&tree_path, b"corrupted garbage data").unwrap();
        store.evict_cache_entry(&tree_a);

        let gc = GarbageCollector::new(store.clone(), _dir.path().join("store"))
            .with_dry_run(false);
        let err = gc.collect(&[root]).unwrap_err();
        assert!(
            matches!(err, GcError::CorruptedNodeDuringWalk(_)),
            "expected CorruptedNodeDuringWalk, got {err:?}"
        );
        // Blob below the corrupted tree must survive because GC aborted.
        assert!(
            store.exists(&blob_digest),
            "blob below corrupted node must survive"
        );
    }

    #[test]
    fn test_gc_manifest_list_breadth() {
        let (store, _dir) = store_and_dir();
        // Build: manifest_list -> [manifest_a, manifest_b] -> layers -> trees -> blobs
        let blob_a = store
            .put_blocking(&DagNode::blob(b"blob a".to_vec()))
            .unwrap();
        let blob_b = store
            .put_blocking(&DagNode::blob(b"blob b".to_vec()))
            .unwrap();
        let tree_a = store
            .put_blocking(&DagNode::tree(vec![blob_a], b"tree a".to_vec()))
            .unwrap();
        let tree_b = store
            .put_blocking(&DagNode::tree(vec![blob_b], b"tree b".to_vec()))
            .unwrap();
        let layer_a = store
            .put_blocking(&DagNode::layer(vec![tree_a], b"layer a".to_vec()))
            .unwrap();
        let layer_b = store
            .put_blocking(&DagNode::layer(vec![tree_b], b"layer b".to_vec()))
            .unwrap();
        let manifest_a = store
            .put_blocking(&DagNode::manifest(vec![layer_a], b"config a".to_vec()))
            .unwrap();
        let manifest_b = store
            .put_blocking(&DagNode::manifest(vec![layer_b], b"config b".to_vec()))
            .unwrap();
        let manifest_list = store
            .put_blocking(&DagNode::manifest_list(
                vec![manifest_a, manifest_b],
                b"list config".to_vec(),
            ))
            .unwrap();
        let orphan = insert_orphan(&store);

        let gc = GarbageCollector::new(store.clone(), _dir.path().join("store"))
            .with_dry_run(false);
        let report = gc.collect(&[manifest_list]).unwrap();
        assert_eq!(report.deleted_nodes, 1);
        assert!(store.exists(&manifest_list));
        assert!(store.exists(&manifest_a));
        assert!(store.exists(&manifest_b));
        assert!(!store.exists(&orphan));
    }

    #[test]
    fn test_gc_idempotent() {
        let (store, _dir) = store_and_dir();
        let root = insert_test_graph(&store);
        let orphan = insert_orphan(&store);

        let gc = GarbageCollector::new(store.clone(), _dir.path().join("store"))
            .with_dry_run(false);
        let first = gc.collect(&[root]).unwrap();
        assert_eq!(first.deleted_nodes, 1);
        assert!(!store.exists(&orphan));

        // Second run should collect nothing.
        let second = gc.collect(&[root]).unwrap();
        assert_eq!(
            second.deleted_nodes, 0,
            "idempotent GC should collect 0 on second run"
        );
        assert_eq!(second.unreachable_nodes, 0);
    }

    #[test]
    fn test_dry_run_does_not_mutate_filesystem() {
        let (store, dir) = store_and_dir();
        let root = insert_test_graph(&store);
        let _lock = pullrun_store::OpLock::new(&dir.path().join("store")).unwrap();
        let lock_path = dir.path().join("store").join("ops");

        // Count lock files before dry-run GC.
        let before: Vec<_> = std::fs::read_dir(&lock_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let before_count = before.len();

        // Dry-run GC should NOT remove stale locks.
        let gc = GarbageCollector::new(store.clone(), dir.path().join("store"));
        let err = gc.collect(&[root]).unwrap_err();
        assert!(matches!(err, GcError::ActiveOpLocks));

        // Lock files should still exist.
        let after: Vec<_> = std::fs::read_dir(&lock_path)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(
            after.len(),
            before_count,
            "dry-run GC must not remove lock files"
        );
    }
}
