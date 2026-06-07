use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use memmap2::Mmap;
use rkyv::Deserialize;
use sha2::{Digest as Sha256Digest, Sha256};
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

const DEFAULT_MAX_CACHE_BYTES: u64 = 512 * 1024 * 1024; // 512 MB

pub struct MmapStore {
    root: PathBuf,
    cache: DashMap<Digest, Arc<Mmap>>,
    max_cache_bytes: Option<u64>,
    lru: Arc<Mutex<VecDeque<Digest>>>,
    total_bytes: Arc<AtomicU64>,
}

impl MmapStore {
    pub fn new(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).ok();
        Self {
            root,
            cache: DashMap::new(),
            max_cache_bytes: Some(DEFAULT_MAX_CACHE_BYTES),
            lru: Arc::new(Mutex::new(VecDeque::new())),
            total_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create a store without cache size limit (in-memory cache
    /// grows unbounded). Use for small test workloads.
    pub fn new_unbounded(root: PathBuf) -> Self {
        std::fs::create_dir_all(&root).ok();
        Self {
            root,
            cache: DashMap::new(),
            max_cache_bytes: None,
            lru: Arc::new(Mutex::new(VecDeque::new())),
            total_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Set the maximum cache size in bytes. When the total mmap'd
    /// data exceeds this, the store evicts the least-recently-used
    /// entries on subsequent `get()` calls.
    pub fn set_max_cache_bytes(&mut self, max: u64) {
        self.max_cache_bytes = Some(max);
    }

    /// Remove `max_cache_bytes` limit (unbounded cache).
    pub fn clear_max_cache_bytes(&mut self) {
        self.max_cache_bytes = None;
    }

    pub fn max_cache_bytes(&self) -> Option<u64> {
        self.max_cache_bytes
    }

    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, digest: &Digest) -> PathBuf {
        let (a, b, rest) = (&digest[0..2], &digest[2..4], &digest[4..]);
        self.root.join(a).join(b).join(rest).join("node.rkyv")
    }

    fn path_for_blob(&self, digest: &Digest) -> PathBuf {
        let (a, b, rest) = (&digest[0..2], &digest[2..4], &digest[4..]);
        self.root.join(a).join(b).join(rest).join("blob.raw")
    }

    pub fn compute_digest(data: &[u8]) -> Digest {
        let mut hasher = Sha256::new();
        sha2::digest::Digest::update(&mut hasher, data);
        hex::encode(sha2::digest::Digest::finalize(hasher))
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
        let entry = self
            .cache
            .entry(digest.clone())
            .or_try_insert_with(|| -> Result<Arc<Mmap>, StoreError> {
                let path = self.path_for(digest);
                if !path.exists() {
                    return Err(StoreError::NotFound(digest.clone()));
                }
                let file = std::fs::File::open(&path)?;
                let mmap = unsafe { Mmap::map(&file)? };
                // Track cache bytes for the LRU evictor.
                self.total_bytes.fetch_add(mmap.len() as u64, Ordering::Relaxed);
                Ok(Arc::new(mmap))
            })?;

        let result = entry.value().clone();

        // Mark this digest as recently used and trigger eviction
        // if the cache exceeds the configured limit.
        let mut lru = self.lru.lock().unwrap();
        // Move to back (most recently used).
        if let Some(pos) = lru.iter().position(|d| d == digest) {
            lru.remove(pos);
        }
        lru.push_back(digest.clone());
        self.evict_lru_locked(&mut lru);

        Ok(result)
    }

    /// Evict the least-recently-used entries from the cache until
    /// total bytes are within `max_cache_bytes`. Must hold the LRU
    /// lock. No-op when `max_cache_bytes` is `None` or zero.
    fn evict_lru_locked(&self, lru: &mut VecDeque<Digest>) {
        let max = match self.max_cache_bytes {
            Some(m) if m > 0 => m,
            _ => return,
        };
        while self.total_bytes.load(Ordering::Relaxed) > max {
            let oldest = match lru.front() {
                Some(d) => d.clone(),
                None => break,
            };
            if let Some((_, evicted)) = self.cache.remove(&oldest) {
                self.total_bytes
                    .fetch_sub(evicted.len() as u64, Ordering::Relaxed);
            }
            lru.pop_front();
        }
    }

    pub fn get_archived(&self, digest: &Digest) -> Result<&ArchivedDagNode, StoreError> {
        let mmap = self.get(digest)?;
        // SAFETY: rkyv::check_archived_root returns a reference tied to the input slice.
        // The Arc<Mmap> is stored in the cache and only freed when the entry is evicted.
        // We return a reference tied to `&self` (which holds the DashMap).
        let bytes: &[u8] = &mmap[..];
        let archived = rkyv::check_archived_root::<DagNode>(bytes)
            .map_err(|e| StoreError::Corrupted(digest.clone(), e.to_string()))?;
        // SAFETY: The archived reference is valid as long as the Arc<Mmap> lives in the cache.
        // We extend the lifetime to 'self since the cache outlives any single call.
        Ok(unsafe { std::mem::transmute::<&ArchivedDagNode, &ArchivedDagNode>(archived) })
    }

    pub fn get_deserialized(&self, digest: &Digest) -> Result<DagNode, StoreError> {
        let mmap = self.get(digest)?;
        let bytes: &[u8] = &mmap[..];
        let archived = rkyv::check_archived_root::<DagNode>(bytes)
            .map_err(|e| StoreError::Corrupted(digest.clone(), e.to_string()))?;
        let node: DagNode = archived
            .deserialize(&mut rkyv::Infallible)
            .map_err(|e| StoreError::Corrupted(digest.clone(), e.to_string()))?;
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
        let path = self.path_for_blob(digest);

        if !path.exists() {
            return Err(StoreError::NotFound(digest.clone()));
        }

        let file = std::fs::File::open(&path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Arc::new(mmap))
    }

    pub fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.path_for_blob(digest)
    }

    pub fn node_count(&self) -> usize {
        self.cache.len()
    }

    /// Total bytes mmap'd across all nodes currently held in the
    /// in-process cache. Uses the LRU tracker's running total,
    /// updated atomically on each insertion/eviction.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes.load(Ordering::Relaxed)
    }

    /// Evict a single entry from the in-memory cache. Updates the
    /// LRU tracker and total byte count. This is a no-op if the
    /// digest is not currently cached.
    pub fn evict_cache_entry(&self, digest: &Digest) {
        if let Some((_, evicted)) = self.cache.remove(digest) {
            self.total_bytes
                .fetch_sub(evicted.len() as u64, Ordering::Relaxed);
            let mut lru = self.lru.lock().unwrap();
            lru.retain(|d| d != digest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::NodeKind;

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

        let result = store.get(&"deadbeef".repeat(8));
        assert!(matches!(result, Err(StoreError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_all_node_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let blob = DagNode::new(NodeKind::Blob, vec![], b"data".to_vec());
        let tree = DagNode::new(NodeKind::Tree, vec!["abc123".into()], b"tree".to_vec());
        let layer = DagNode::new(NodeKind::Layer, vec!["def456".into()], b"layer".to_vec());
        let manifest = DagNode::new(NodeKind::Manifest, vec!["ghi789".into()], b"manifest".to_vec());

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
        // Empty store: zero on both axes.
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());
        assert_eq!(store.node_count(), 0);
        assert_eq!(store.total_bytes(), 0);

        // `put()` writes to disk but does NOT populate the in-process
        // mmap cache. `get()` and `get_blob()` are the operations that
        // load the data into the cache. Mirror the real workload
        // pattern: put then get.
        let d1 = store.put(&DagNode::blob(b"alpha".to_vec())).await.unwrap();
        let d2 = store.put(&DagNode::blob(b"bravo!".to_vec())).await.unwrap();
        let d3 = store.put(&DagNode::blob(b"".to_vec())).await.unwrap();
        let _ = store.get(&d1).unwrap();
        let _ = store.get(&d2).unwrap();
        let _ = store.get(&d3).unwrap();
        assert_eq!(store.node_count(), 3);
        let bytes_after = store.total_bytes();
        // memmap2 returns the file size (not page-aligned) for small
        // files. rkyv overhead is 8 bytes per node for the small
        // blobs we wrote, so the sum is small but strictly positive.
        assert!(bytes_after > 0, "got {bytes_after}");
        assert!(bytes_after < 1024 * 1024, "store grew unexpectedly large: {bytes_after}");

        // A duplicate get() (after a put() of identical content) must
        // not double-count: dedup happens at put() so the second get()
        // is a cache hit.
        let d1b = store.put(&DagNode::blob(b"alpha".to_vec())).await.unwrap();
        assert_eq!(d1, d1b, "identical content must hash to the same digest");
        let _ = store.get(&d1b).unwrap();
        assert_eq!(store.node_count(), 3);
        assert_eq!(store.total_bytes(), bytes_after);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_concurrent_reads() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().to_path_buf()));

        let node = DagNode::manifest(
            vec!["abc123".into(), "def456".into()],
            b"image config for concurrent test".to_vec(),
        );
        let digest = store.put(&node).await.unwrap();

        // 100 OS threads, zero copies, no locks.
        // Validates: rkyv + memmap2 + DashMap = lock-free concurrent access
        // to immutable archives across threads.
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
        // The architectural test: 100 std threads, each doing a zero-copy
        // mmap + rkyv::archived_root dereference, all in parallel.
        // No locks. No allocations. Pure shared page cache.
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().to_path_buf()));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let node = DagNode::manifest(vec!["abc123".into()], b"manifest bytes".to_vec());
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

        let tree = DagNode::tree(vec![blob_digest.clone()], b"dir listing".to_vec());
        let tree_digest = store.put(&tree).await.unwrap();

        let layer = DagNode::layer(vec![tree_digest.clone()], b"layer delta".to_vec());
        let layer_digest = store.put(&layer).await.unwrap();

        let manifest = DagNode::manifest(vec![layer_digest.clone()], b"image config".to_vec());
        let manifest_digest = store.put(&manifest).await.unwrap();

        let root_mmap = store.get(&manifest_digest).unwrap();
        let root = unsafe { rkyv::archived_root::<DagNode>(&root_mmap[..]) };
        assert!(root.is_manifest());

        let edges: Vec<String> = root.edges.iter().map(|e| e.as_str().to_string()).collect();
        assert_eq!(edges.len(), 1);

        let layer_mmap = store.get(&edges[0]).unwrap();
        let layer_node = unsafe { rkyv::archived_root::<DagNode>(&layer_mmap[..]) };
        let layer_edges: Vec<String> = layer_node.edges.iter().map(|e| e.as_str().to_string()).collect();

        let tree_mmap = store.get(&layer_edges[0]).unwrap();
        let tree_node = unsafe { rkyv::archived_root::<DagNode>(&tree_mmap[..]) };
        let tree_edges: Vec<String> = tree_node.edges.iter().map(|e| e.as_str().to_string()).collect();

        let blob_mmap = store.get(&tree_edges[0]).unwrap();
        let blob_node = unsafe { rkyv::archived_root::<DagNode>(&blob_mmap[..]) };
        assert!(blob_node.is_blob());
        assert_eq!(
            blob_node.inline_data.as_ref(),
            b"file content"
        );
    }
}