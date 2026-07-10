// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashSet, VecDeque};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use memmap2::Mmap;
use rkyv::Deserialize;
use tracing::{debug, info, trace, warn};

use crate::{node::ArchivedDagNode, DagNode, Digest};

/// Monotonically increasing counter used to generate unique temp file
/// suffixes. Combined with PID, this guarantees no two concurrent
/// `write_atomically` calls produce the same tmp name.
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("blob not found: {0}")]
    NotFound(Digest),
    #[error("corrupted archive for {0}: {1}")]
    Corrupted(Digest, String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(String),
    #[error("Archive validation: {0}")]
    ArchiveValidation(String),
    #[error("Digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },
}

const DEFAULT_MAX_NODE_CACHE_BYTES: u64 = 256 * 1024 * 1024; // 256 MB
const DEFAULT_MAX_BLOB_CACHE_BYTES: u64 = 256 * 1024 * 1024; // 256 MB

pub struct MmapStore {
    root: PathBuf,
    cache: DashMap<Digest, Arc<Mmap>>,
    blob_cache: DashMap<Digest, Arc<Mmap>>,
    max_node_cache_bytes: Option<u64>,
    max_blob_cache_bytes: Option<u64>,
    lru: Arc<Mutex<VecDeque<Digest>>>,
    blob_lru: Arc<Mutex<VecDeque<Digest>>>,
    total_node_bytes: Arc<AtomicU64>,
    total_blob_bytes: Arc<AtomicU64>,
}

/// A guard that keeps the underlying `Mmap` alive and provides
/// `Deref` access to the archived DAG node. Prevents use-after-free
/// if the LRU cache evicts the entry while a reference is held.
#[derive(Debug)]
pub struct ArchivedNodeGuard {
    _mmap: Arc<Mmap>,
    archived: *const ArchivedDagNode,
}

impl ArchivedNodeGuard {
    fn new(digest: &Digest, mmap: Arc<Mmap>) -> Result<Self, StoreError> {
        let ptr: *const ArchivedDagNode = rkyv::check_archived_root::<DagNode>(&mmap[..])
            .map_err(|e| StoreError::Corrupted(*digest, e.to_string()))?;
        Ok(Self {
            _mmap: mmap,
            archived: ptr,
        })
    }
}

impl Deref for ArchivedNodeGuard {
    type Target = ArchivedDagNode;
    fn deref(&self) -> &ArchivedDagNode {
        // SAFETY: `self.archived` points into the mmap owned by
        // `self._mmap` (the `Arc<Mmap>` field ensures the backing
        // memory stays alive). The pointer is valid, aligned, and
        // was produced by `rkyv::archived_root`.
        unsafe { &*self.archived }
    }
}

// SAFETY: ArchivedNodeGuard owns an Arc<Mmap> which is Send+Sync,
// and the raw pointer points into that mmap, so sending the guard
// across threads is safe.
unsafe impl Send for ArchivedNodeGuard {}
unsafe impl Sync for ArchivedNodeGuard {}

/// Write `bytes` to `path` atomically via write-then-rename.
///
/// 1. Write to `<path>.tmp.<pid>.<counter>` (unique per writer — no race
///    when two threads write the same digest concurrently).
/// 2. `fsync` the temp file (data + metadata).
/// 3. `rename` the temp file over the final path (atomic on POSIX).
/// 4. `fsync` the parent directory (required for durability on ext4/XFS/btrfs).
///
/// On failure the temp file is removed to avoid orphan accumulation.
///
/// **Known limitation:** newly-created shard directories
/// (e.g. `00/11/ab/` for a fresh digest) above the immediate parent are
/// not individually `fsync`ed. A crash after rename but before the shard
/// dir metadata reaches disk can lose the file. This window is accepted:
/// shard dirs are quickly reused, and `recover()` handles orphaned temps.
/// Full shard pre-creation (256² = 65k dirs) was judged too expensive.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .expect("write_atomically: no parent directory");
    std::fs::create_dir_all(parent)?;

    let pid = std::process::id();
    let seq = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        "{}.tmp.{}.{:016x}",
        path.file_name()
            .expect("write_atomically: no file name")
            .to_string_lossy(),
        pid,
        seq,
    );
    let tmp_path = parent.join(tmp_name);

    let result = (|| -> Result<(), StoreError> {
        std::fs::write(&tmp_path, bytes)?;

        let f = std::fs::File::open(&tmp_path)?;
        f.sync_all()?;
        drop(f);

        std::fs::rename(&tmp_path, path)?;

        let parent_f = std::fs::File::open(parent)?;
        parent_f.sync_all()?;

        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }

    result
}

impl MmapStore {
    pub fn new(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).ok();
        Self {
            root,
            cache: DashMap::new(),
            blob_cache: DashMap::new(),
            max_node_cache_bytes: Some(DEFAULT_MAX_NODE_CACHE_BYTES),
            max_blob_cache_bytes: Some(DEFAULT_MAX_BLOB_CACHE_BYTES),
            lru: Arc::new(Mutex::new(VecDeque::new())),
            blob_lru: Arc::new(Mutex::new(VecDeque::new())),
            total_node_bytes: Arc::new(AtomicU64::new(0)),
            total_blob_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create a store without cache size limit (in-memory cache
    /// grows unbounded). Use for small test workloads.
    pub fn new_unbounded(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).ok();
        Self {
            root,
            cache: DashMap::new(),
            blob_cache: DashMap::new(),
            max_node_cache_bytes: None,
            max_blob_cache_bytes: None,
            lru: Arc::new(Mutex::new(VecDeque::new())),
            blob_lru: Arc::new(Mutex::new(VecDeque::new())),
            total_node_bytes: Arc::new(AtomicU64::new(0)),
            total_blob_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Set the maximum node cache size in bytes.
    pub fn set_max_node_cache_bytes(&mut self, max: u64) {
        self.max_node_cache_bytes = Some(max);
    }

    /// Set the maximum blob cache size in bytes.
    pub fn set_max_blob_cache_bytes(&mut self, max: u64) {
        self.max_blob_cache_bytes = Some(max);
    }

    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_hex();
        assert!(
            hex.len() >= 4 && hex.is_char_boundary(4),
            "digest too short or at char boundary"
        );
        let (a, b, rest) = (&hex[0..2], &hex[2..4], &hex[4..]);
        self.root.join(a).join(b).join(rest).join("node.rkyv")
    }

    fn path_for_blob(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_hex();
        assert!(
            hex.len() >= 4 && hex.is_char_boundary(4),
            "digest too short or at char boundary"
        );
        let (a, b, rest) = (&hex[0..2], &hex[2..4], &hex[4..]);
        self.root.join(a).join(b).join(rest).join("blob.raw")
    }

    pub fn compute_digest(data: &[u8]) -> Digest {
        Digest::compute(data)
    }

    pub async fn put(&self, node: &DagNode) -> Result<Digest, StoreError> {
        let bytes = rkyv::to_bytes::<_, 256>(node)
            .map_err(|e| StoreError::Serialization(format!("rkyv serialization failed: {e}")))?;

        let digest = Self::compute_digest(&bytes);
        let path = self.path_for(&digest);

        // Lazy dedup: file existence is sufficient. Content is validated
        // on read via rkyv::check_archived_root in get_archived(). A
        // partial file left by a pre-WAL crash will be caught on first
        // read (Corrupted error), not silently served.
        if tokio::fs::metadata(&path).await.is_ok() {
            debug!(%digest, "node already exists (deduplicated)");
            return Ok(digest);
        }

        let path_c = path.clone();
        let bytes_c = bytes.to_vec();
        match tokio::task::spawn_blocking(move || write_atomically(&path_c, &bytes_c)).await {
            Ok(Ok(())) => {
                trace!(%digest, path = %path.display(), "node stored");
                Ok(digest)
            }
            Ok(Err(e)) => Err(e),
            Err(join_err) => Err(StoreError::Io(std::io::Error::other(join_err))),
        }
    }

    pub fn put_blocking(&self, node: &DagNode) -> Result<Digest, StoreError> {
        let bytes = rkyv::to_bytes::<_, 256>(node)
            .map_err(|e| StoreError::Serialization(format!("rkyv serialization failed: {e}")))?;

        let digest = Self::compute_digest(&bytes);
        let path = self.path_for(&digest);

        if path.exists() {
            debug!(%digest, "node already exists (deduplicated)");
            return Ok(digest);
        }

        write_atomically(&path, &bytes)?;
        trace!(%digest, path = %path.display(), "node stored");

        Ok(digest)
    }

    pub fn get(&self, digest: &Digest) -> Result<Arc<Mmap>, StoreError> {
        // Fast path: check if the entry is already cached.
        if let Some(entry) = self.cache.get(digest) {
            let result = entry.value().clone();
            drop(entry);
            // Mark as recently used and trigger eviction.
            let mut lru = self.lru.lock().expect("lru lock poisoned");
            if let Some(pos) = lru.iter().position(|d| d == digest) {
                lru.remove(pos);
            }
            lru.push_back(*digest);
            self.evict_lru_locked(&mut lru);
            return Ok(result);
        }

        // Slow path: load from disk, insert, then evict.
        let path = self.path_for(digest);
        if !path.exists() {
            return Err(StoreError::NotFound(*digest));
        }
        let file = std::fs::File::open(&path)?;
        // SAFETY: `file` is opened read-only and is not modified while
        // the mapping exists. `memmap2::Mmap::map` requires the caller
        // to ensure the underlying file doesn't change — the store's
        // write path only appends new blobs, never modifies existing ones.
        let mmap = unsafe { Mmap::map(&file)? };
        let len = mmap.len() as u64;

        // Evict before inserting so the new entry can't be evicted
        // in the same round.
        let mut lru = self.lru.lock().expect("lru lock poisoned");
        self.evict_lru_locked(&mut lru);
        lru.push_back(*digest);
        drop(lru);

        self.total_node_bytes.fetch_add(len, Ordering::Relaxed);
        let mmap = Arc::new(mmap);
        self.cache.insert(*digest, mmap.clone());
        Ok(mmap)
    }

    /// Evict the least-recently-used entries from the node cache until
    /// total bytes are within `max_node_cache_bytes`. Must hold the LRU
    /// lock. No-op when `max_node_cache_bytes` is `None` or zero.
    fn evict_lru_locked(&self, lru: &mut VecDeque<Digest>) {
        let max = match self.max_node_cache_bytes {
            Some(m) if m > 0 => m,
            _ => return,
        };
        while self.total_node_bytes.load(Ordering::Relaxed) > max {
            let oldest = match lru.front() {
                Some(d) => *d,
                None => break,
            };
            if let Some((_, evicted)) = self.cache.remove(&oldest) {
                self.total_node_bytes
                    .fetch_sub(evicted.len() as u64, Ordering::Relaxed);
            }
            lru.pop_front();
        }
    }

    pub fn get_archived(&self, digest: &Digest) -> Result<ArchivedNodeGuard, StoreError> {
        let mmap = self.get(digest)?;
        ArchivedNodeGuard::new(digest, mmap)
    }

    pub fn get_deserialized(&self, digest: &Digest) -> Result<DagNode, StoreError> {
        let mmap = self.get(digest)?;
        let bytes: &[u8] = &mmap[..];
        let archived = rkyv::check_archived_root::<DagNode>(bytes)
            .map_err(|e| StoreError::Corrupted(*digest, e.to_string()))?;
        let node: DagNode = archived
            .deserialize(&mut rkyv::Infallible)
            .map_err(|e| StoreError::Corrupted(*digest, e.to_string()))?;
        Ok(node)
    }

    pub fn exists(&self, digest: &Digest) -> bool {
        self.cache.contains_key(digest) || self.path_for(digest).exists()
    }

    pub async fn put_blob(&self, digest: &Digest, data: &[u8]) -> Result<(), StoreError> {
        let path = self.path_for_blob(digest);

        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(());
        }

        let path_c = path.clone();
        let data_c = data.to_vec();
        match tokio::task::spawn_blocking(move || write_atomically(&path_c, &data_c)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(join_err) => Err(StoreError::Io(std::io::Error::other(join_err))),
        }
    }

    pub fn put_blob_blocking(&self, digest: &Digest, data: &[u8]) -> Result<(), StoreError> {
        let path = self.path_for_blob(digest);

        if path.exists() {
            return Ok(());
        }

        write_atomically(&path, data)?;
        Ok(())
    }

    pub fn get_blob(&self, digest: &Digest) -> Result<Arc<Mmap>, StoreError> {
        // Fast path: check blob cache.
        if let Some(entry) = self.blob_cache.get(digest) {
            let result = entry.value().clone();
            drop(entry);
            let mut lru = self.blob_lru.lock().expect("blob_lru lock poisoned");
            if let Some(pos) = lru.iter().position(|d| d == digest) {
                lru.remove(pos);
            }
            lru.push_back(*digest);
            self.evict_blob_lru_locked(&mut lru);
            return Ok(result);
        }

        // Slow path: load from disk.
        let path = self.path_for_blob(digest);
        if !path.exists() {
            return Err(StoreError::NotFound(*digest));
        }
        let file = std::fs::File::open(&path)?;
        // SAFETY: Same invariants as `get_deserialized` — blob files
        // are write-once, read-many, so the mapping will not be mutated.
        let mmap = unsafe { Mmap::map(&file)? };
        // Note: we do NOT content-validate blobs here because the
        // policy layer stores blobs at digests derived from logical
        // keys, not from content. Corruption detection relies on the
        // atomic-write guarantee from write_atomically. For nodes,
        // rkyv::check_archived_root provides structural validation.
        let len = mmap.len() as u64;

        // Evict before inserting so the new entry survives this round.
        let mut lru = self.blob_lru.lock().expect("blob_lru lock poisoned");
        self.evict_blob_lru_locked(&mut lru);
        lru.push_back(*digest);
        drop(lru);

        self.total_blob_bytes.fetch_add(len, Ordering::Relaxed);
        let mmap = Arc::new(mmap);
        self.blob_cache.insert(*digest, mmap.clone());
        Ok(mmap)
    }

    /// Evict the least-recently-used blob cache entries until total
    /// bytes are within `max_blob_cache_bytes`. Must hold the blob LRU lock.
    fn evict_blob_lru_locked(&self, lru: &mut VecDeque<Digest>) {
        let max = match self.max_blob_cache_bytes {
            Some(m) if m > 0 => m,
            _ => return,
        };
        while self.total_blob_bytes.load(Ordering::Relaxed) > max {
            let oldest = match lru.front() {
                Some(d) => *d,
                None => break,
            };
            if let Some((_, evicted)) = self.blob_cache.remove(&oldest) {
                self.total_blob_bytes
                    .fetch_sub(evicted.len() as u64, Ordering::Relaxed);
            }
            lru.pop_front();
        }
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.path_for_blob(digest)
    }

    /// Path to the rkyv-serialized DagNode file for a given digest.
    pub fn node_path(&self, digest: &Digest) -> PathBuf {
        self.path_for(digest)
    }

    pub fn cached_node_count(&self) -> usize {
        self.cache.len()
    }

    /// Total bytes mmap'd across all node entries currently held in the
    /// in-process cache.
    pub fn total_node_bytes(&self) -> u64 {
        self.total_node_bytes.load(Ordering::Relaxed)
    }

    /// Total bytes mmap'd across all blob entries currently held in the
    /// in-process cache.
    pub fn total_blob_bytes(&self) -> u64 {
        self.total_blob_bytes.load(Ordering::Relaxed)
    }

    /// Total cached bytes across both node and blob caches.
    pub fn total_bytes(&self) -> u64 {
        self.total_node_bytes() + self.total_blob_bytes()
    }

    /// Evict a single entry from the in-memory node cache. Updates the
    /// LRU tracker and total byte count. This is a no-op if the
    /// digest is not currently cached.
    /// Recover from a prior crash: walk the store root and remove any
    /// orphaned `*.tmp.*` files left by interrupted `write_atomically`
    /// calls. Idempotent and safe to call on a healthy store (no-ops).
    ///
    /// Returns the number of orphaned files removed and bytes freed.
    pub fn recover(&self) -> Result<(u64, u64), StoreError> {
        let mut files_removed = 0u64;
        let mut bytes_freed = 0u64;
        self.recover_dir(&self.root, &mut files_removed, &mut bytes_freed)?;
        if files_removed > 0 {
            info!(
                files_removed,
                bytes_freed, "store recovery: removed orphaned temp files"
            );
        }
        Ok((files_removed, bytes_freed))
    }

    fn recover_dir(
        &self,
        dir: &Path,
        files_removed: &mut u64,
        bytes_freed: &mut u64,
    ) -> Result<(), StoreError> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                self.recover_dir(&path, files_removed, bytes_freed)?;
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.contains(".tmp.") {
                    if let Ok(meta) = path.metadata() {
                        *bytes_freed += meta.len();
                    }
                    match std::fs::remove_file(&path) {
                        Ok(()) => {
                            *files_removed += 1;
                            trace!("recovered: removed orphaned tmp file {:?}", path);
                        }
                        Err(e) => {
                            warn!("recover: failed to remove {:?}: {}", path, e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn evict_cache_entry(&self, digest: &Digest) {
        if let Some((_, evicted)) = self.cache.remove(digest) {
            self.total_node_bytes
                .fetch_sub(evicted.len() as u64, Ordering::Relaxed);
            let mut lru = self.lru.lock().expect("lru lock poisoned");
            lru.retain(|d| d != digest);
        }
    }
}

/// BFS walk from the given root digests, collecting all reachable node
/// digests and detecting which ones have separate blob files on disk.
///
/// Returns `(node_digests, blob_digests)` where `blob_digests` is a
/// subset of `node_digests`.
///
/// # Errors
///
/// Returns `StoreError::Corrupted` if a reachable node fails archive
/// validation. NotFound digests are silently skipped (concurrent deletion).
pub fn walk_reachable(
    store: &MmapStore,
    roots: &[Digest],
) -> Result<(Vec<Digest>, Vec<Digest>), StoreError> {
    let mut visited: HashSet<Digest> = HashSet::new();
    let mut queue: VecDeque<Digest> = VecDeque::new();
    let mut node_digests: Vec<Digest> = Vec::new();
    let mut blob_digests: Vec<Digest> = Vec::new();

    for root in roots {
        queue.push_back(*root);
    }

    while let Some(digest) = queue.pop_front() {
        if !visited.insert(digest) {
            continue;
        }
        if !store.exists(&digest) {
            continue;
        }
        node_digests.push(digest);
        if store.blob_path(&digest).exists() {
            blob_digests.push(digest);
        }
        match store.get_archived(&digest) {
            Ok(archived) => {
                for edge in archived.edges.iter() {
                    queue.push_back(Digest(*edge));
                }
            }
            Err(StoreError::NotFound(_)) => {
                // Node was deleted concurrently — skip.
                continue;
            }
            Err(StoreError::Corrupted(d, _)) => {
                return Err(StoreError::Corrupted(
                    d,
                    "corrupted node during BFS walk — aborting to prevent subtree deletion".into(),
                ));
            }
            Err(e) => {
                // Unexpected error — abort to be safe.
                return Err(e);
            }
        }
    }

    Ok((node_digests, blob_digests))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeKind;

    fn d(hex: &str) -> Digest {
        Digest::from_hex(hex).unwrap()
    }

    #[test]
    fn test_write_atomically_no_partial_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.node.rkyv");

        // Write normally, verify final file exists.
        write_atomically(&path, b"hello").unwrap();
        assert!(path.exists());

        // After success, no tmp files should remain.
        let tmp_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.contains(".tmp."))
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(tmp_count, 0, "no orphaned tmp files after success");
    }

    #[tokio::test]
    async fn test_corrupted_file_returns_error_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        // Write a valid node, then replace the file with garbage.
        let node = DagNode::blob(b"valid data".to_vec());
        let digest = store.put(&node).await.unwrap();
        let path = store.path_for(&digest);
        std::fs::write(&path, b"truncated garbage").unwrap();

        // get_archived() validates rkyv — must return Corrupted.
        match store.get_archived(&digest) {
            Err(StoreError::Corrupted(d, _)) if d == digest => {}
            other => panic!("expected Corrupted for truncated archive, got {other:?}"),
        }

        // get_deserialized() also validates.
        match store.get_deserialized(&digest) {
            Err(StoreError::Corrupted(d, _)) if d == digest => {}
            other => panic!("expected Corrupted for truncated archive, got {other:?}"),
        }

        // get_blob() does NOT content-validate (callers use logical-key
        // digests). get() for nodes validates via rkyv — tested above.
    }

    #[test]
    fn test_recover_removes_orphaned_tmps() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        // Create orphaned tmp files at various nesting depths.
        let file_1 = dir
            .path()
            .join("aa")
            .join("bb")
            .join("node.rkyv.tmp.1234.0000000000000001");
        let file_2 = dir
            .path()
            .join("aa")
            .join("bb")
            .join("blob.raw.tmp.1234.0000000000000002");
        let file_3 = dir
            .path()
            .join("zz")
            .join("orphan.tmp.9999.ffffffffffffffff");
        for f in [&file_1, &file_2, &file_3] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"orphaned").unwrap();
        }

        // Also place a valid-looking node.rkyv (no .tmp suffix) — must survive.
        let valid_path = dir.path().join("00").join("00").join("node.rkyv");
        std::fs::create_dir_all(valid_path.parent().unwrap()).unwrap();
        std::fs::write(&valid_path, b"real data").unwrap();

        let (count, bytes) = store.recover().unwrap();
        assert_eq!(count, 3, "all three orphaned tmp files removed");
        assert!(bytes > 0, "should report freed bytes");

        // Valid files untouched.
        assert!(valid_path.exists(), "valid node.rkyv must survive recovery");

        // Orphans gone.
        assert!(!file_1.exists(), "file_1 must be removed");
        assert!(!file_2.exists(), "file_2 must be removed");
        assert!(!file_3.exists(), "file_3 must be removed");
    }

    #[tokio::test]
    async fn test_old_store_readable() {
        // Simulate a store written by the old code (direct write, no tmp/rename).
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let bytes = rkyv::to_bytes::<_, 256>(&DagNode::blob(b"old data".to_vec())).unwrap();
        // Old write path: direct write to final path, no fsync, no rename.
        let digest = MmapStore::compute_digest(&bytes);
        let path = store.path_for(&digest);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        // Backward compat: the new read path must still load it.
        let mmap = store.get(&digest).unwrap();
        let archived = unsafe { rkyv::archived_root::<DagNode>(&mmap[..]) };
        assert_eq!(archived.inline_data.as_ref(), b"old data");

        // New writes must also coexist using the new code path.
        let d2 = store
            .put(&DagNode::blob(b"new data".to_vec()))
            .await
            .unwrap();
        let mmap2 = store.get(&d2).unwrap();
        let archived2 = unsafe { rkyv::archived_root::<DagNode>(&mmap2[..]) };
        assert_eq!(archived2.inline_data.as_ref(), b"new data");
    }

    #[tokio::test]
    async fn test_put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let node = DagNode::blob(b"hello world".to_vec());
        let digest = store.put(&node).await.unwrap();

        assert!(store.exists(&digest));

        let mmap = store.get(&digest).unwrap();
        let archived = unsafe { rkyv::archived_root::<DagNode>(&mmap[..]) };
        assert!(archived.is_blob());
        assert_eq!(archived.inline_data.as_ref(), b"hello world");

        let deserialized = store.get_deserialized(&digest).unwrap();
        assert_eq!(deserialized.inline_data, b"hello world");
    }

    #[tokio::test]
    async fn test_deduplication() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let node = DagNode::blob(b"dedup test".to_vec());
        let d1 = store.put(&node).await.unwrap();
        let d2 = store.put(&node).await.unwrap();

        assert_eq!(d1, d2);
    }

    #[tokio::test]
    async fn test_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let result = store.get(&d(&"deadbeef".repeat(8)));
        assert!(matches!(result, Err(StoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_all_node_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let a = d(&"aa".repeat(32));
        let b = d(&"bb".repeat(32));
        let c = d(&"cc".repeat(32));

        let blob = DagNode::new(NodeKind::Blob, vec![], b"data".to_vec());
        let tree = DagNode::new(NodeKind::Tree, vec![a], b"tree".to_vec());
        let layer = DagNode::new(NodeKind::Layer, vec![b], b"layer".to_vec());
        let manifest = DagNode::new(NodeKind::Manifest, vec![c], b"manifest".to_vec());

        let b = store.put(&blob).await.unwrap();
        let t = store.put(&tree).await.unwrap();
        let l = store.put(&layer).await.unwrap();
        let m = store.put(&manifest).await.unwrap();

        for digest in [&b, &t, &l, &m] {
            let mmap = store.get(digest).unwrap();
            let archived = unsafe { rkyv::archived_root::<DagNode>(&mmap[..]) };
            match digest {
                d if d == &b => assert!(archived.is_blob()),
                d if d == &t => assert!(archived.is_tree()),
                d if d == &l => assert!(archived.is_layer()),
                d if d == &m => assert!(archived.is_manifest()),
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn test_total_bytes_and_node_count() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());
        assert_eq!(store.cached_node_count(), 0);
        assert_eq!(store.total_bytes(), 0);

        let d1 = store.put(&DagNode::blob(b"alpha".to_vec())).await.unwrap();
        let d2 = store.put(&DagNode::blob(b"bravo!".to_vec())).await.unwrap();
        let d3 = store.put(&DagNode::blob(b"".to_vec())).await.unwrap();
        let _ = store.get(&d1).unwrap();
        let _ = store.get(&d2).unwrap();
        let _ = store.get(&d3).unwrap();
        assert_eq!(store.cached_node_count(), 3);
        let bytes_after = store.total_bytes();
        assert!(bytes_after > 0, "got {bytes_after}");
        assert!(
            bytes_after < 1024 * 1024,
            "store grew unexpectedly large: {bytes_after}"
        );

        let d1b = store.put(&DagNode::blob(b"alpha".to_vec())).await.unwrap();
        assert_eq!(d1, d1b, "identical content must hash to the same digest");
        let _ = store.get(&d1b).unwrap();
        assert_eq!(store.cached_node_count(), 3);
        assert_eq!(store.total_bytes(), bytes_after);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().to_path_buf()));

        let d1 = d(&"aa".repeat(32));
        let d2 = d(&"bb".repeat(32));

        let node = DagNode::manifest(vec![d1, d2], b"image config for concurrent test".to_vec());
        let digest = store.put(&node).await.unwrap();

        let mut handles = vec![];
        for _ in 0..100 {
            let store = store.clone();
            handles.push(thread::spawn(move || {
                let mmap = store.get(&digest).unwrap();
                let archived = unsafe { rkyv::archived_root::<DagNode>(&mmap[..]) };
                assert!(archived.is_manifest());
                assert_eq!(archived.edges.len(), 2);
                assert_eq!(
                    archived.inline_data.as_ref(),
                    b"image config for concurrent test"
                );
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn concurrent_mmap_reads_100_threads() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().to_path_buf()));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let node = DagNode::manifest(vec![d(&"aa".repeat(32))], b"manifest bytes".to_vec());
        let digest = rt.block_on(store.put(&node)).unwrap();

        let mut handles = vec![];
        for _ in 0..100 {
            let store = store.clone();
            handles.push(thread::spawn(move || {
                let archived_ref = store.get_archived(&digest).unwrap();
                assert!(archived_ref.is_manifest());
                assert_eq!(archived_ref.inline_data.as_ref(), b"manifest bytes");
            }));
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[tokio::test]
    async fn test_blob_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let data = b"large blob data that goes to separate file".to_vec();
        let digest = MmapStore::compute_digest(&data);

        store.put_blob(&digest, &data).await.unwrap();

        let mmap = store.get_blob(&digest).unwrap();
        assert_eq!(&mmap[..], &data[..]);
    }

    #[tokio::test]
    async fn test_complex_dag() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let blob_data = DagNode::blob(b"file content".to_vec());
        let blob_digest = store.put(&blob_data).await.unwrap();

        let tree = DagNode::tree(vec![blob_digest], b"dir listing".to_vec());
        let tree_digest = store.put(&tree).await.unwrap();

        let layer = DagNode::layer(vec![tree_digest], b"layer delta".to_vec());
        let layer_digest = store.put(&layer).await.unwrap();

        let manifest = DagNode::manifest(vec![layer_digest], b"image config".to_vec());
        let manifest_digest = store.put(&manifest).await.unwrap();

        let root_mmap = store.get(&manifest_digest).unwrap();
        let root = unsafe { rkyv::archived_root::<DagNode>(&root_mmap[..]) };
        assert!(root.is_manifest());

        let edges: Vec<Digest> = root.edges.iter().map(|e| Digest(*e)).collect();
        assert_eq!(edges.len(), 1);

        let layer_mmap = store.get(&edges[0]).unwrap();
        let layer_node = unsafe { rkyv::archived_root::<DagNode>(&layer_mmap[..]) };
        let layer_edges: Vec<Digest> = layer_node.edges.iter().map(|e| Digest(*e)).collect();

        let tree_mmap = store.get(&layer_edges[0]).unwrap();
        let tree_node = unsafe { rkyv::archived_root::<DagNode>(&tree_mmap[..]) };
        let tree_edges: Vec<Digest> = tree_node.edges.iter().map(|e| Digest(*e)).collect();

        let blob_mmap = store.get(&tree_edges[0]).unwrap();
        let blob_node = unsafe { rkyv::archived_root::<DagNode>(&blob_mmap[..]) };
        assert!(blob_node.is_blob());
        assert_eq!(blob_node.inline_data.as_ref(), b"file content");
    }
}
