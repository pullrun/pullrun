// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use memmap2::Mmap;
use rkyv::Deserialize;
use tracing::{debug, trace};

use crate::{node::ArchivedDagNode, DagNode, Digest};

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
pub struct ArchivedNodeGuard {
    _mmap: Arc<Mmap>,
    archived: *const ArchivedDagNode,
}

impl ArchivedNodeGuard {
    fn new(digest: &Digest, mmap: Arc<Mmap>) -> Result<Self, StoreError> {
        let ptr: *const ArchivedDagNode =
            rkyv::check_archived_root::<DagNode>(&mmap[..])
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
        assert!(hex.len() >= 4 && hex.is_char_boundary(4), "digest too short or at char boundary");
        let (a, b, rest) = (&hex[0..2], &hex[2..4], &hex[4..]);
        self.root.join(a).join(b).join(rest).join("node.rkyv")
    }

    fn path_for_blob(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_hex();
        assert!(hex.len() >= 4 && hex.is_char_boundary(4), "digest too short or at char boundary");
        let (a, b, rest) = (&hex[0..2], &hex[2..4], &hex[4..]);
        self.root.join(a).join(b).join(rest).join("blob.raw")
    }

    pub fn compute_digest(data: &[u8]) -> Digest {
        Digest::compute(data)
    }

    pub async fn put(&self, node: &DagNode) -> Result<Digest, StoreError> {
        let bytes = rkyv::to_bytes::<_, 256>(node).map_err(|e| {
            StoreError::Serialization(format!("rkyv serialization failed: {e}"))
        })?;

        let digest = Self::compute_digest(&bytes);
        let path = self.path_for(&digest);

        if tokio::fs::metadata(&path).await.is_ok() {
            debug!(%digest, "node already exists (deduplicated)");
            return Ok(digest);
        }

        let parent = path.parent().expect("path must have parent");
        tokio::fs::create_dir_all(parent).await?;

        tokio::fs::write(&path, &bytes).await?;
        trace!(%digest, path = %path.display(), "node stored");

        Ok(digest)
    }

    pub fn put_blocking(&self, node: &DagNode) -> Result<Digest, StoreError> {
        let bytes = rkyv::to_bytes::<_, 256>(node).map_err(|e| {
            StoreError::Serialization(format!("rkyv serialization failed: {e}"))
        })?;

        let digest = Self::compute_digest(&bytes);
        let path = self.path_for(&digest);

        if path.exists() {
            debug!(%digest, "node already exists (deduplicated)");
            return Ok(digest);
        }

        let parent = path.parent().expect("path must have parent");
        std::fs::create_dir_all(parent)?;
        std::fs::write(&path, &bytes)?;
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

        let parent = path.parent().expect("path must have parent");
        tokio::fs::create_dir_all(parent).await?;
        tokio::fs::write(&path, data).await?;

        Ok(())
    }

    pub fn put_blob_blocking(&self, digest: &Digest, data: &[u8]) -> Result<(), StoreError> {
        let path = self.path_for_blob(digest);

        if path.exists() {
            return Ok(());
        }

        let parent = path.parent().expect("path must have parent");
        std::fs::create_dir_all(parent)?;
        std::fs::write(&path, data)?;

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
    pub fn evict_cache_entry(&self, digest: &Digest) {
        if let Some((_, evicted)) = self.cache.remove(digest) {
            self.total_node_bytes
                .fetch_sub(evicted.len() as u64, Ordering::Relaxed);
            let mut lru = self.lru.lock().expect("lru lock poisoned");
            lru.retain(|d| d != digest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeKind;

    fn d(hex: &str) -> Digest {
        Digest::from_hex(hex).unwrap()
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
        assert!(bytes_after < 1024 * 1024, "store grew unexpectedly large: {bytes_after}");

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

        let node = DagNode::manifest(
            vec![d1, d2],
            b"image config for concurrent test".to_vec(),
        );
        let digest = store.put(&node).await.unwrap();

        let mut handles = vec![];
        for _ in 0..100 {
            let store = store.clone();
            let digest = digest.clone();
            handles.push(thread::spawn(move || {
                let mmap = store.get(&digest).unwrap();
                let archived = unsafe { rkyv::archived_root::<DagNode>(&mmap[..]) };
                assert!(archived.is_manifest());
                assert_eq!(archived.edges.len(), 2);
                assert_eq!(archived.inline_data.as_ref(), b"image config for concurrent test");
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
            let digest = digest.clone();
            handles.push(thread::spawn(move || {
                let archived_ref = store.get_archived(&digest).unwrap();
                assert!(archived_ref.is_manifest());
                assert_eq!(
                    archived_ref.inline_data.as_ref(),
                    b"manifest bytes"
                );
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
        assert_eq!(
            blob_node.inline_data.as_ref(),
            b"file content"
        );
    }
}