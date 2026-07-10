use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

use pullrun_store::{clean_stale_op_locks, walk_reachable, Digest, MmapStore};
use thiserror::Error;
use tracing::{debug, info};

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
/// use pullrun_store::{Digest, MmapStore, walk_reachable};
/// use pullrun_gc::GarbageCollector;
/// use std::sync::Arc;
///
/// let store = Arc::new(MmapStore::new("/tmp/gc-test".into()));
/// let gc = GarbageCollector::new(store, "/tmp/gc-test".into());
/// // In a real invocation, roots would come from image_tags + workloads.
/// let roots = [];
/// match gc.collect(&roots) {
///     Ok(report) => println!("GC report: {report:?}"),
///     Err(e) => eprintln!("GC failed: {e}"),
/// }
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
        let fresh = clean_stale_op_locks(&self.store_root)?;
        if !fresh.is_empty() && !self.force {
            debug!(count = fresh.len(), "active op locks found, deferring GC");
            return Err(GcError::ActiveOpLocks);
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
        let (reachable_nodes, _reachable_blobs) = walk_reachable(&self.store, roots);
        let reachable: HashSet<Digest> = reachable_nodes.iter().copied().collect();
        let reachable_count = reachable.len();

        // 6. Compute unreachable set.
        let unreachable: Vec<Digest> = all_digests
            .into_iter()
            .filter(|d| !reachable.contains(d))
            .collect();
        let unreachable_count = unreachable.len();

        // 7. Safety: abort if >90% of the store would be freed and the
        //    store is larger than 100 MB (likely indicates a root-set bug
        //    rather than genuine garbage). Override with --force.
        if !self.force && total_nodes > 0 {
            let pct = (unreachable_count as u64 * 100) / total_nodes as u64;
            if pct >= 90 {
                let total_bytes = self.store.total_bytes();
                if total_bytes > 100 * 1024 * 1024 {
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
                if let Some(digest) = self.digest_from_path(&path) {
                    digests.push(digest);
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
        // blob_a <- tree_a <- layer_a <- manifest_a
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
        // All nodes in the root graph should still exist.
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
        // With force, GC runs despite the lock. Since there are no orphans,
        // nothing is deleted.
        assert_eq!(report.deleted_nodes, 0);
    }

    #[test]
    fn test_gc_report_counts() {
        let (store, _dir) = store_and_dir();
        let root = insert_test_graph(&store);
        insert_orphan(&store);
        let gc = GarbageCollector::new(store, _dir.path().join("store"));
        let report = gc.collect(&[root]).unwrap();
        // 4 reachable nodes: blob_a, tree_a, layer_a, manifest_a
        assert_eq!(report.reachable_nodes, 4);
        // 5 total = 4 reachable + 1 orphan
        assert_eq!(report.total_nodes, 5);
        assert_eq!(report.unreachable_nodes, 1);
    }

    #[test]
    fn test_multiple_roots() {
        let (store, _dir) = store_and_dir();
        let root_a = insert_test_graph(&store);
        // Create a second independent graph.
        let blob_b = store
            .put_blocking(&DagNode::blob(b"blob b".to_vec()))
            .unwrap();
        let manifest_b = store
            .put_blocking(&DagNode::manifest(vec![blob_b], b"config b".to_vec()))
            .unwrap();
        // Orphan that should be deleted.
        let orphan = insert_orphan(&store);

        let gc = GarbageCollector::new(store.clone(), _dir.path().join("store"))
            .with_dry_run(false);
        let report = gc.collect(&[root_a, manifest_b]).unwrap();
        assert_eq!(report.deleted_nodes, 1);
        assert!(store.exists(&root_a));
        assert!(store.exists(&manifest_b));
        assert!(!store.exists(&orphan));
    }
}
