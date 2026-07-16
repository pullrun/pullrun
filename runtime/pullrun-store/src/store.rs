// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashSet, VecDeque};
use std::io::Read;
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

    pub fn path_for(&self, digest: &Digest) -> PathBuf {
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

    fn refcount_path(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_hex();
        let (a, b, rest) = (&hex[0..2], &hex[2..4], &hex[4..]);
        self.root.join(a).join(b).join(rest).join("node.refcount")
    }

    /// Compute the total on-disk size of an image by BFS-walking from the
    /// root digest and summing all blob file sizes. Node files (node.rkyv)
    /// are excluded — they are metadata, not image data.
    pub fn compute_image_size(&self, root: &Digest) -> u64 {
        let mut total = 0u64;
        let mut visited: HashSet<Digest> = HashSet::new();
        let mut queue: VecDeque<Digest> = VecDeque::new();
        queue.push_back(*root);
        while let Some(d) = queue.pop_front() {
            if !visited.insert(d) {
                continue;
            }
            // Count the DAG node file.
            let node_path = self.node_path(&d);
            if node_path.exists() {
                total += std::fs::metadata(&node_path).map(|m| m.len()).unwrap_or(0);
            }
            // Count the blob file, if present (large files stored externally).
            let blob_path = self.path_for_blob(&d);
            if blob_path.exists() {
                total += std::fs::metadata(&blob_path).map(|m| m.len()).unwrap_or(0);
            }
            if let Ok(archived) = self.get_archived(&d) {
                for edge in archived.edges.iter() {
                    queue.push_back(Digest(*edge));
                }
            }
        }
        total
    }

    /// Read the reference count for a digest. Returns 0 if no refcount
    /// file exists (node was created before refcounting, or never referenced).
    pub fn get_refcount(&self, digest: &Digest) -> Result<u64, StoreError> {
        let path = self.refcount_path(digest);
        if !path.exists() {
            return Ok(0);
        }
        let mut buf = [0u8; 8];
        let mut f = std::fs::File::open(&path)?;
        f.read_exact(&mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    /// Set the reference count for a digest, persisted atomically.
    pub fn set_refcount(&self, digest: &Digest, count: u64) -> Result<(), StoreError> {
        if count == 0 {
            let path = self.refcount_path(digest);
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        write_atomically(&self.refcount_path(digest), &count.to_le_bytes())
    }

    /// Increment the reference count for a digest. Returns the new count.
    pub fn increment_refcount(&self, digest: &Digest) -> Result<u64, StoreError> {
        let count = self.get_refcount(digest)? + 1;
        self.set_refcount(digest, count)?;
        Ok(count)
    }

    /// Decrement the reference count for a digest. Returns the new count.
    pub fn decrement_refcount(&self, digest: &Digest) -> Result<u64, StoreError> {
        let count = self.get_refcount(digest)?;
        let new = count.saturating_sub(1);
        self.set_refcount(digest, new)?;
        Ok(new)
    }

    /// Rebuild all refcounts from scratch by BFS-walking from the given roots.
    /// Called on startup for crash recovery. Idempotent.
    ///
    /// Accumulates counts in a HashMap before writing any file, so stale
    /// `node.refcount` files from a partially-completed previous run are
    /// completely overwritten.
    pub fn recompute_all_refcounts(&self, roots: &[Digest]) -> Result<(), StoreError> {
        use std::collections::HashMap;
        let mut counts: HashMap<Digest, u64> = HashMap::new();
        let mut visited: HashSet<Digest> = HashSet::new();
        let mut queue: VecDeque<Digest> = VecDeque::new();

        for root in roots {
            *counts.entry(*root).or_default() += 1;
            queue.push_back(*root);
        }

        while let Some(node) = queue.pop_front() {
            if !visited.insert(node) {
                continue;
            }
            if !self.path_for(&node).exists() {
                continue;
            }
            let archived = match self.get_archived(&node) {
                Ok(a) => a,
                Err(StoreError::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            for edge in archived.edges.iter() {
                let child = Digest(*edge);
                if !self.path_for(&child).exists() {
                    continue;
                }
                *counts.entry(child).or_default() += 1;
                queue.push_back(child);
            }
        }

        // Write all final counts in one pass, completely overwriting any
        // stale node.refcount files from a previously crashed run.
        for (digest, count) in &counts {
            self.set_refcount(digest, *count)?;
        }

        info!(
            "refcounts rebuilt: {} nodes from {} roots",
            counts.len(),
            roots.len()
        );
        Ok(())
    }

    /// Remove a tag root: decrement its refcount and cascade-delete the
    /// entire subtree when refcount reaches zero. Returns bytes freed.
    pub fn remove_tag(&self, root: &Digest) -> Result<u64, StoreError> {
        let remaining = self.decrement_refcount(root)?;
        if remaining > 0 {
            debug!(%root, remaining, "root still referenced, not deleting");
            return Ok(0);
        }

        let mut queue: VecDeque<Digest> = VecDeque::new();
        let mut deleted: HashSet<Digest> = HashSet::new();
        let mut bytes_freed = 0u64;
        queue.push_back(*root);

        while let Some(node) = queue.pop_front() {
            if !deleted.insert(node) {
                continue;
            }
            let node_path = self.path_for(&node);
            if !node_path.exists() {
                continue;
            }

            // Read edges before deleting the node file.
            let edges: Vec<Digest> = match self.get_archived(&node) {
                Ok(archived) => archived.edges.iter().map(|e| Digest(*e)).collect(),
                Err(StoreError::NotFound(_)) => vec![],
                Err(e) => return Err(e),
            };

            for child in &edges {
                if self.decrement_refcount(child)? == 0 {
                    queue.push_back(*child);
                }
            }

            // Evict from in-memory cache.
            self.evict_cache_entry(&node);

            // Delete node file.
            if let Ok(meta) = std::fs::metadata(&node_path) {
                bytes_freed += meta.len();
            }
            let _ = std::fs::remove_file(&node_path);

            // Delete blob file if present.
            let blob_path = self.path_for_blob(&node);
            if blob_path.exists() {
                if let Ok(meta) = std::fs::metadata(&blob_path) {
                    bytes_freed += meta.len();
                }
                let _ = std::fs::remove_file(&blob_path);
            }

            // Delete refcount file.
            let rc_path = self.refcount_path(&node);
            let _ = std::fs::remove_file(&rc_path);

            // Remove empty shard dirs.
            self.remove_empty_dirs(&node_path);
        }

        info!(%root, bytes_freed, "rmi complete");
        Ok(bytes_freed)
    }

    /// Walk up from a deleted node's parent directory, removing empty
    /// shard directories until reaching the store root or a non-empty dir.
    fn remove_empty_dirs(&self, path: &Path) {
        let store_root = &self.root;
        let mut dir = path.parent().unwrap_or(path);
        loop {
            if !dir.starts_with(store_root) || dir == store_root.as_path() {
                break;
            }
            let is_empty = std::fs::read_dir(dir)
                .map(|mut it| it.next().is_none())
                .unwrap_or(false);
            if is_empty {
                let _ = std::fs::remove_dir(dir);
                dir = dir.parent().unwrap_or(dir);
            } else {
                break;
            }
        }
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
                // Increment refcounts for all child edges (new parent references).
                // On failure the node is stored but refcount is wrong; the
                // startup recompute_all_refcounts will correct it next boot.
                for edge in &node.edges {
                    if let Err(e) = self.increment_refcount(edge) {
                        warn!(%digest, edge = %edge, error = %e, "failed to increment child refcount");
                    }
                }
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
        for edge in &node.edges {
            if let Err(e) = self.increment_refcount(edge) {
                warn!(%digest, edge = %edge, error = %e, "failed to increment child refcount");
            }
        }

        Ok(digest)
    }

    pub fn get(&self, digest: &Digest) -> Result<Arc<Mmap>, StoreError> {
        // Fast path: check if the entry is already cached.
        if let Some(entry) = self.cache.get(digest) {
            let result = entry.value().clone();
            drop(entry);
            let mut lru = self.lru.lock().expect("lru lock poisoned");
            if let Some(pos) = lru.iter().position(|d| d == digest) {
                lru.remove(pos);
            }
            lru.push_back(*digest);
            self.evict_lru_locked(&mut lru);
            return Ok(result);
        }

        // Slow path: load from disk via shared helper.
        self.load_node_from_disk(digest)
    }

    /// Load a node from disk, insert into cache, return mmap.
    /// Shared by sync `get()` and async `get_archived_async()`.
    fn load_node_from_disk(&self, digest: &Digest) -> Result<Arc<Mmap>, StoreError> {
        let path = self.path_for(digest);
        if !path.exists() {
            return Err(StoreError::NotFound(*digest));
        }
        let file = std::fs::File::open(&path)?;
        // SAFETY: file is read-only, never modified after creation.
        // write-then-rename ensures no partial files reach the final path.
        let mmap = unsafe { Mmap::map(&file)? };
        let len = mmap.len() as u64;

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

    /// Async get_archived — cache hit is non-blocking, cache miss
    /// runs in spawn_blocking to avoid starving tokio workers.
    pub async fn get_archived_async(
        self: &Arc<Self>,
        digest: &Digest,
    ) -> Result<ArchivedNodeGuard, StoreError> {
        if let Some(mmap) = self.cache.get(digest) {
            return ArchivedNodeGuard::new(digest, mmap.clone());
        }
        let store = Arc::clone(self);
        let d = *digest;
        let inner = tokio::task::spawn_blocking(move || {
            if let Some(mmap) = store.cache.get(&d) {
                return Ok(mmap.clone());
            }
            store.load_node_from_disk(&d)
        })
        .await
        .map_err(|join_err| StoreError::Io(std::io::Error::other(join_err)))?;
        let mmap = inner?;
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

        // Slow path: load from disk via shared helper.
        self.load_blob_from_disk(digest)
    }

    /// Load a blob from disk, insert into blob cache, return mmap.
    /// Shared by sync `get_blob()` and async `get_blob_async()`.
    fn load_blob_from_disk(&self, digest: &Digest) -> Result<Arc<Mmap>, StoreError> {
        let path = self.path_for_blob(digest);
        if !path.exists() {
            return Err(StoreError::NotFound(*digest));
        }
        let file = std::fs::File::open(&path)?;
        // SAFETY: Same invariants as get_deserialized — blob files
        // are write-once, read-many, so the mapping will not be mutated.
        let mmap = unsafe { Mmap::map(&file)? };
        let len = mmap.len() as u64;

        let mut lru = self.blob_lru.lock().expect("blob_lru lock poisoned");
        self.evict_blob_lru_locked(&mut lru);
        lru.push_back(*digest);
        drop(lru);

        self.total_blob_bytes.fetch_add(len, Ordering::Relaxed);
        let mmap = Arc::new(mmap);
        self.blob_cache.insert(*digest, mmap.clone());
        Ok(mmap)
    }

    /// Async get_blob — cache hit is non-blocking, cache miss
    /// runs in spawn_blocking to avoid starving tokio workers.
    pub async fn get_blob_async(
        self: &Arc<Self>,
        digest: &Digest,
    ) -> Result<Arc<Mmap>, StoreError> {
        if let Some(blob) = self.blob_cache.get(digest) {
            return Ok(blob.clone());
        }
        let store = Arc::clone(self);
        let d = *digest;
        let inner = tokio::task::spawn_blocking(move || {
            if let Some(blob) = store.blob_cache.get(&d) {
                return Ok(blob.clone());
            }
            store.load_blob_from_disk(&d)
        })
        .await
        .map_err(|join_err| StoreError::Io(std::io::Error::other(join_err)))?;
        inner
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

    // ── Async read method tests ─────────────────────────────────

    #[tokio::test]
    async fn test_get_archived_async_matches_sync() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().join("store")));
        let digest = store
            .put_blocking(&DagNode::blob(b"test data".to_vec()))
            .unwrap();

        // Clear cache to force disk read.
        store.evict_cache_entry(&digest);

        let async_result = store.get_archived_async(&digest).await.unwrap();
        let sync_result = store.get_archived(&digest).unwrap();

        assert_eq!(async_result.edges, sync_result.edges);
        assert_eq!(async_result.kind, sync_result.kind);
    }

    #[tokio::test]
    async fn test_get_blob_async_matches_sync() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().join("store")));
        let digest = Digest::compute(b"blob data");
        store.put_blob_blocking(&digest, b"blob data").unwrap();

        // Clear cache to force disk read.
        store.evict_cache_entry(&digest);

        let async_result = store.get_blob_async(&digest).await.unwrap();
        let sync_result = store.get_blob(&digest).unwrap();

        assert_eq!(async_result.len(), 9);
        assert_eq!(&async_result[..], b"blob data");
        assert_eq!(&sync_result[..], b"blob data");
    }

    #[tokio::test]
    async fn test_get_archived_async_cache_hit_is_fast() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().join("store")));
        let digest = store
            .put_blocking(&DagNode::blob(b"cache hit".to_vec()))
            .unwrap();

        // Warm the cache.
        let _ = store.get_archived_async(&digest).await.unwrap();

        // Second read must be a cache hit — sub-millisecond.
        let start = std::time::Instant::now();
        let _ = store.get_archived_async(&digest).await.unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_millis(1),
            "cache hit should be sub-millisecond"
        );
    }

    #[tokio::test]
    async fn test_concurrent_cold_reads_dont_starve_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().join("store")));

        // Insert 50 nodes of meaningful size.
        let digests: Vec<_> = (0..50)
            .map(|i| {
                store
                    .put_blocking(&DagNode::blob(vec![i as u8; 4096]))
                    .unwrap()
            })
            .collect();

        // Clear cache so every read hits disk.
        for d in &digests {
            store.evict_cache_entry(d);
        }

        // Spawn a timer that should complete in ~10ms.
        let timer_start = std::time::Instant::now();
        let timer_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            timer_start.elapsed()
        });

        // Spawn 50 concurrent cold reads.
        let mut handles = vec![];
        for d in &digests {
            let store = Arc::clone(&store);
            let d = *d;
            handles.push(tokio::spawn(async move {
                store.get_archived_async(&d).await.unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let elapsed = timer_task.await.unwrap();
        // If the timer was delayed significantly, tokio workers
        // were starved by blocking reads.
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "timer was delayed by {elapsed:?} — tokio thread starvation detected"
        );
    }

    // ─── Refcount tests ───────────────────────────────────────────

    #[test]
    fn test_refcount_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());
        let blob = DagNode::blob(b"hello".to_vec());
        let d = store.put_blocking(&blob).unwrap();

        // Fresh node has refcount 0 (no tags, no parents).
        assert_eq!(store.get_refcount(&d).unwrap(), 0);

        // Increment.
        assert_eq!(store.increment_refcount(&d).unwrap(), 1);
        assert_eq!(store.get_refcount(&d).unwrap(), 1);

        // Increment again.
        assert_eq!(store.increment_refcount(&d).unwrap(), 2);

        // Decrement.
        assert_eq!(store.decrement_refcount(&d).unwrap(), 1);
        assert_eq!(store.get_refcount(&d).unwrap(), 1);

        // Decrement to zero.
        assert_eq!(store.decrement_refcount(&d).unwrap(), 0);
        assert_eq!(store.get_refcount(&d).unwrap(), 0);

        // Decrement below zero is saturated.
        assert_eq!(store.decrement_refcount(&d).unwrap(), 0);

        // set_refcount to specific value.
        store.set_refcount(&d, 5).unwrap();
        assert_eq!(store.get_refcount(&d).unwrap(), 5);

        // set_refcount to 0 removes the file.
        store.set_refcount(&d, 0).unwrap();
        assert!(!store.refcount_path(&d).exists());
        assert_eq!(store.get_refcount(&d).unwrap(), 0);
    }

    #[test]
    fn test_put_increments_child_refcounts() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        // Create two leaf blobs.
        let b1 = store
            .put_blocking(&DagNode::blob(b"leaf1".to_vec()))
            .unwrap();
        let b2 = store
            .put_blocking(&DagNode::blob(b"leaf2".to_vec()))
            .unwrap();
        assert_eq!(store.get_refcount(&b1).unwrap(), 0);
        assert_eq!(store.get_refcount(&b2).unwrap(), 0);

        // Put a tree referencing both leaves.
        let tree = DagNode::tree(vec![b1, b2], vec![]);
        let t = store.put_blocking(&tree).unwrap();
        assert_eq!(store.get_refcount(&t).unwrap(), 0);
        // Each child got an increment when the tree was stored.
        assert_eq!(store.get_refcount(&b1).unwrap(), 1);
        assert_eq!(store.get_refcount(&b2).unwrap(), 1);

        // Put the same tree again — dedup, refcounts stay unchanged.
        let _dup = store.put_blocking(&tree).unwrap();
        assert_eq!(store.get_refcount(&b1).unwrap(), 1);
        assert_eq!(store.get_refcount(&b2).unwrap(), 1);

        // Put a second tree referencing b1 again — only b1 increments.
        let tree2 = DagNode::tree(vec![b1], b"other".to_vec());
        let t2 = store.put_blocking(&tree2).unwrap();
        assert_eq!(store.get_refcount(&t2).unwrap(), 0);
        assert_eq!(store.get_refcount(&b1).unwrap(), 2);
        assert_eq!(store.get_refcount(&b2).unwrap(), 1);
    }

    #[test]
    fn test_remove_tag_shared_layers() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        // Build: tag_a → manifest_a → layer_shared → blob_shared
        //        tag_b → manifest_b → layer_shared → blob_shared
        let blob_shared = store
            .put_blocking(&DagNode::blob(b"shared data".to_vec()))
            .unwrap();
        let layer_shared = store
            .put_blocking(&DagNode::layer(vec![blob_shared], vec![]))
            .unwrap();
        let manifest_a = store
            .put_blocking(&DagNode::manifest(vec![layer_shared], b"config_a".to_vec()))
            .unwrap();
        let manifest_b = store
            .put_blocking(&DagNode::manifest(vec![layer_shared], b"config_b".to_vec()))
            .unwrap();

        // Simulate two tags pointing to the roots.
        store.increment_refcount(&manifest_a).unwrap();
        store.increment_refcount(&manifest_b).unwrap();

        // layer_shared has refcount 2 (both manifests' edges point to it).
        // blob_shared has refcount 1 (only layer_shared's edge points to it).
        assert_eq!(store.get_refcount(&layer_shared).unwrap(), 2);
        assert_eq!(store.get_refcount(&blob_shared).unwrap(), 1);

        // Remove tag_a. layer_shared goes from 2→1 (still ref'd by b).
        // blob_shared stays at 1 (still ref'd via layer_shared).
        let freed = store.remove_tag(&manifest_a).unwrap();
        assert!(freed > 0, "tag_a removal should free bytes");
        assert!(!store.path_for(&manifest_a).exists());
        assert!(store.path_for(&layer_shared).exists());
        assert!(store.path_for(&blob_shared).exists());
        assert_eq!(store.get_refcount(&layer_shared).unwrap(), 1);
        assert_eq!(store.get_refcount(&blob_shared).unwrap(), 1);

        // Remove tag_b. layer_shared goes from 1→0 → cascade to blob_shared.
        let freed = store.remove_tag(&manifest_b).unwrap();
        assert!(freed > 0, "tag_b removal should free bytes");
        assert!(!store.path_for(&layer_shared).exists());
        assert!(!store.path_for(&blob_shared).exists());
        assert!(!store.refcount_path(&layer_shared).exists());
        assert!(!store.refcount_path(&blob_shared).exists());
    }

    #[test]
    fn test_remove_tag_mulitple_tags_same_root() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        // Single blob with two tags (simulated by refcount 2).
        let blob = store
            .put_blocking(&DagNode::blob(b"data".to_vec()))
            .unwrap();
        store.set_refcount(&blob, 2).unwrap();

        // First remove_tag decrements to 1 — no deletion.
        let freed = store.remove_tag(&blob).unwrap();
        assert_eq!(freed, 0);
        assert!(store.path_for(&blob).exists());
        assert_eq!(store.get_refcount(&blob).unwrap(), 1);

        // Second remove_tag decrements to 0 — deletion happens.
        let freed = store.remove_tag(&blob).unwrap();
        assert!(freed > 0);
        assert!(!store.path_for(&blob).exists());
        assert_eq!(store.get_refcount(&blob).unwrap(), 0);
    }

    #[test]
    fn test_recompute_refcounts_after_stale_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        // Build: root → middle → leaf
        let leaf = store
            .put_blocking(&DagNode::blob(b"leaf".to_vec()))
            .unwrap();
        let middle = store
            .put_blocking(&DagNode::tree(vec![leaf], b"middle".to_vec()))
            .unwrap();
        let root = store
            .put_blocking(&DagNode::manifest(vec![middle], b"root".to_vec()))
            .unwrap();

        // Manually corrupt refcounts: leaf has count 999, middle has count 42.
        store.set_refcount(&leaf, 999).unwrap();
        store.set_refcount(&middle, 42).unwrap();
        // Root has no refcount file (count = 0).

        // Recompute from the root.
        store.recompute_all_refcounts(&[root]).unwrap();

        // After recompute:
        //   root = 1 (root reference)
        //   middle = 1 (referenced by root's edge)
        //   leaf = 1 (referenced by middle's edge)
        assert_eq!(
            store.get_refcount(&root).unwrap(),
            1,
            "root should have count 1"
        );
        assert_eq!(
            store.get_refcount(&middle).unwrap(),
            1,
            "middle should have count 1"
        );
        assert_eq!(
            store.get_refcount(&leaf).unwrap(),
            1,
            "leaf should have count 1"
        );

        // Recompute with two copies of the same root (simulating 2 tags).
        store.recompute_all_refcounts(&[root, root]).unwrap();
        assert_eq!(
            store.get_refcount(&root).unwrap(),
            2,
            "root should have count 2 for two tags"
        );
        // middle and leaf each have one parent edge, so refcount is 1.
        // The root's count of 2 acts as a gate: only the second remove_tag
        // will trigger the cascade (refcount 1→0 on middle and leaf).
        assert_eq!(
            store.get_refcount(&middle).unwrap(),
            1,
            "middle should have count 1 (one parent edge)"
        );
        assert_eq!(
            store.get_refcount(&leaf).unwrap(),
            1,
            "leaf should have count 1 (one parent edge)"
        );
    }

    #[test]
    fn test_recompute_refcounts_skips_missing_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let leaf = store
            .put_blocking(&DagNode::blob(b"leaf".to_vec()))
            .unwrap();
        let root = store
            .put_blocking(&DagNode::manifest(vec![leaf], b"root".to_vec()))
            .unwrap();

        // Delete the leaf node to simulate a partial deletion.
        std::fs::remove_file(store.path_for(&leaf)).unwrap();

        // Recompute from root should handle the missing leaf gracefully.
        store.recompute_all_refcounts(&[root]).unwrap();
        assert_eq!(store.get_refcount(&root).unwrap(), 1);
        // Leaf was already deleted; its old refcount file stays on disk
        // (harmless — the node itself is gone, so the stale count is never
        // consulted in any meaningful path). recompute must not crash.
        assert!(
            !store.path_for(&leaf).exists(),
            "leaf node file remains deleted"
        );
    }

    #[test]
    fn test_delete_empty_dirs_after_remove_tag() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        let blob = store
            .put_blocking(&DagNode::blob(b"data".to_vec()))
            .unwrap();
        store.set_refcount(&blob, 1).unwrap();

        let node_dir = store.path_for(&blob).parent().unwrap().to_path_buf();
        assert!(node_dir.exists(), "shard directory should exist before rmi");

        store.remove_tag(&blob).unwrap();

        // The shard directory should be removed (empty after deletion).
        assert!(
            !node_dir.exists(),
            "empty shard directory should be cleaned up"
        );
    }

    #[test]
    fn test_compute_image_size() {
        let dir = tempfile::tempdir().unwrap();
        let store = MmapStore::new(dir.path().to_path_buf());

        // Two blobs with different sizes.
        let small_data = b"12 bytes here!";
        let big_data = b"this is exactly 32 bytes of data!!!";
        let medium_data = b"twenty bytes";

        let small = store
            .put_blocking(&DagNode::blob(small_data.to_vec()))
            .unwrap();
        let big = store
            .put_blocking(&DagNode::blob(big_data.to_vec()))
            .unwrap();
        let medium = store
            .put_blocking(&DagNode::blob(medium_data.to_vec()))
            .unwrap();

        let small_len = small_data.len() as u64;
        let big_len = big_data.len() as u64;
        let medium_len = medium_data.len() as u64;

        // Size of each node.rkyv file (present for every stored node).
        // This accounts for the DAG structure overhead.
        let node_size = |d: &Digest| -> u64 {
            std::fs::metadata(store.node_path(d))
                .map(|m| m.len())
                .unwrap_or(0)
        };

        // Inline blob — no blob.raw file, only the node file.
        assert_eq!(
            store.compute_image_size(&small),
            node_size(&small),
            "inline blob: only node.rkyv on disk"
        );

        // Manually set up blob.raw files by writing them.
        std::fs::write(store.path_for_blob(&small), small_data).unwrap();
        std::fs::write(store.path_for_blob(&big), big_data).unwrap();

        // Root referencing small and big.
        let root = store
            .put_blocking(&DagNode::manifest(vec![small, big], b"config".to_vec()))
            .unwrap();
        assert_eq!(
            store.compute_image_size(&root),
            node_size(&root) + node_size(&small) + small_len + node_size(&big) + big_len,
            "leaf blob sizes + node.rkyv files"
        );

        // Add medium as a blob.raw under a second level.
        std::fs::write(store.path_for_blob(&medium), medium_data).unwrap();
        let intermediate = store
            .put_blocking(&DagNode::tree(vec![medium], b"".to_vec()))
            .unwrap();
        let root2 = store
            .put_blocking(&DagNode::manifest(
                vec![big, intermediate],
                b"config2".to_vec(),
            ))
            .unwrap();
        assert_eq!(
            store.compute_image_size(&root2),
            node_size(&root2)
                + node_size(&big)
                + big_len
                + node_size(&intermediate)
                + node_size(&medium)
                + medium_len,
            "nested blob sizes + node.rkyv files sum"
        );
    }
}
