// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Immutable, content-addressed image bases.
//!
//! The OCI DAG is flattened **once per manifest digest** into a
//! read-only base tree. Every sandbox then gets a copy-on-write clone
//! of that base (reflink where available, full copy otherwise), which
//! fixes three problems at once:
//!
//! - sandboxes no longer share a writable rootfs,
//! - the Nth sandbox stops re-walking every layer and re-copying every
//!   file,
//! - on reflink filesystems the clones share physical blocks.
//!
//! Whiteouts are applied only while building the base, in the confined
//! extractor. The runtime never deletes host paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use pullrun_store::{Digest, MmapStore};

use crate::cow::{self, CloneStats, CowMode};
use crate::materializer::{MaterializedBundle, OciMaterializer};
use crate::puller::OciError;

/// One immutable base tree, built once per manifest digest.
#[derive(Debug, Clone)]
pub struct BaseTree {
    pub digest: Digest,
    pub path: PathBuf,
}

/// Builds and tracks immutable image bases under `<store>/bases`.
pub struct BaseStore {
    store: Arc<MmapStore>,
    root: PathBuf,
    inflight: Mutex<HashMap<Digest, Arc<Mutex<()>>>>,
    cow_mode: CowMode,
}

impl BaseStore {
    pub fn new(store: Arc<MmapStore>, root: PathBuf) -> Self {
        let cow_mode = cow::probe(&root);
        info!(
            root = %root.display(),
            cow = cow_mode.label(),
            "base store initialized"
        );
        Self {
            store,
            root,
            inflight: Mutex::new(HashMap::new()),
            cow_mode,
        }
    }

    /// Default location: `<store_root>/bases`.
    pub fn from_store(store: Arc<MmapStore>) -> Self {
        let root = store.root_dir().join("bases");
        Self::new(store, root)
    }

    pub fn cow_mode(&self) -> CowMode {
        self.cow_mode
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn version_root(&self) -> PathBuf {
        self.root.join("v1")
    }

    pub fn base_path(&self, digest: &Digest) -> PathBuf {
        self.version_root().join(digest.as_hex())
    }

    fn ready_marker(&self, digest: &Digest) -> PathBuf {
        self.version_root().join(format!("{}.ready", digest.as_hex()))
    }

    pub fn is_ready(&self, digest: &Digest) -> bool {
        self.ready_marker(digest).is_file() && self.base_path(digest).is_dir()
    }

    /// Ensure the base for `digest` exists, building it exactly once.
    pub async fn ensure_base(&self, digest: &Digest) -> Result<Arc<BaseTree>, OciError> {
        if self.is_ready(digest) {
            return Ok(Arc::new(BaseTree {
                digest: *digest,
                path: self.base_path(digest),
            }));
        }

        let lock = {
            let mut inflight = self.inflight.lock().await;
            inflight
                .entry(*digest)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        if self.is_ready(digest) {
            return Ok(Arc::new(BaseTree {
                digest: *digest,
                path: self.base_path(digest),
            }));
        }

        let final_path = self.base_path(digest);
        let tmp = self.version_root().join(format!(
            ".tmp-{}-{}-{}",
            digest.as_hex(),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));

        if tmp.exists() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        std::fs::create_dir_all(&tmp)?;

        info!(%digest, path = %final_path.display(), "building image base");
        let rootfs = tmp.join("rootfs");
        let materializer = OciMaterializer::new(&self.store);
        let result = materializer.materialize_into(digest, &rootfs).await;

        if let Err(err) = result {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(err);
        }

        // Replace a stale, unmarked directory from an interrupted build.
        if final_path.exists() && !self.ready_marker(digest).exists() {
            warn!(path = %final_path.display(), "removing stale incomplete base");
            let _ = std::fs::remove_dir_all(&final_path);
        }

        match std::fs::rename(&rootfs, &final_path) {
            Ok(()) => {}
            Err(err) if final_path.is_dir() => {
                // Another process won the race; keep its base.
                debug!(%digest, error = %err, "base appeared while building; reusing");
                let _ = std::fs::remove_dir_all(&tmp);
            }
            Err(err) => {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(OciError::Io(err));
            }
        }

        let marker = self.ready_marker(digest);
        cow::write_atomic(&marker, b"v1")?;
        let _ = std::fs::remove_dir_all(&tmp);

        info!(%digest, path = %final_path.display(), "image base ready");
        Ok(Arc::new(BaseTree {
            digest: *digest,
            path: final_path,
        }))
    }

    /// Remove bases that are not in `live` (used by GC/rmi).
    pub fn gc(&self, live: &std::collections::HashSet<Digest>) -> std::io::Result<u64> {
        let mut removed = 0u64;
        let version_root = self.version_root();
        let entries = match std::fs::read_dir(&version_root) {
            Ok(e) => e,
            Err(_) => return Ok(0),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(hex) = name.strip_suffix(".ready") {
                if let Ok(digest) = Digest::from_hex(hex) {
                    if !live.contains(&digest) {
                        let base = version_root.join(hex);
                        let _ = std::fs::remove_dir_all(&base);
                        let _ = std::fs::remove_file(&path);
                        removed += 1;
                    }
                }
            } else if name.starts_with(".tmp-") {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
        Ok(removed)
    }
}

/// Materialize a workload bundle from a prebuilt base.
///
/// The returned bundle has the same shape as
/// [`OciMaterializer::materialize_bundle`] but the rootfs is a clone of
/// the immutable base rather than a fresh extraction.
pub async fn prepare_bundle_from_base(
    store: &MmapStore,
    base: &BaseTree,
    manifest_digest: &Digest,
    bundle_dir: &Path,
) -> Result<(MaterializedBundle, CloneStats), OciError> {
    let materializer = OciMaterializer::new(store);
    let manifest_data = materializer.materialize_manifest(manifest_digest)?;

    std::fs::create_dir_all(bundle_dir)?;
    let rootfs_path = bundle_dir.join("rootfs");
    if rootfs_path.exists() {
        std::fs::remove_dir_all(&rootfs_path)?;
    }

    let base_path = base.path.clone();
    let rootfs_for_clone = rootfs_path.clone();
    let stats = tokio::task::spawn_blocking(move || {
        cow::clone_tree(&base_path, &rootfs_for_clone)
    })
    .await
    .map_err(|e| OciError::Other(format!("clone join: {e}")))??;

    Ok((
        MaterializedBundle {
            rootfs_path,
            config_path: bundle_dir.join("config.json"),
            entrypoint: manifest_data.entrypoint,
            cmd: manifest_data.cmd,
            env: manifest_data.env,
            working_dir: manifest_data.working_dir,
        },
        stats,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pullrun_store::DagNode;

    fn store_with_one_file() -> (Arc<MmapStore>, Digest, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(dir.path().join("store")));
        let blob = DagNode::blob(b"hello".to_vec());
        let blob_digest = store.put_blocking(&blob).unwrap();
        // Minimal directory entry: reuse the converter's encoder.
        let entry = crate::converter::DirectoryEntry {
            name: "hello.txt".to_string(),
            size: 5,
            mode: 0o644,
            uid: 0,
            gid: 0,
            mtime: 0,
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            digest: blob_digest,
            xattrs: Default::default(),
        };
        // A Tree node whose inline data lists its entries; the parent
        // Layer node carries the (empty) path prefix.
        let tree_dir = DagNode {
            kind: pullrun_store::NodeKind::Tree,
            edges: vec![blob_digest],
            inline_data: entry.to_inline_bytes().to_vec(),
        };
        let tree_digest = store.put_blocking(&tree_dir).unwrap();
        let layer = DagNode::layer(vec![tree_digest], Vec::new());
        let layer_digest = store.put_blocking(&layer).unwrap();
        let manifest_data = crate::converter::ManifestData {
            entrypoint: vec![],
            cmd: vec![],
            env: vec![],
            working_dir: None,
            architecture: "arm64".to_string(),
            os: "linux".to_string(),
            annotations: None,
            subject: None,
            user: None,
            stop_signal: None,
            exposed_ports: None,
            volumes: None,
            variant: None,
        };
        let manifest = DagNode::manifest(
            vec![layer_digest],
            serde_json::to_vec(&manifest_data).unwrap(),
        );
        let manifest_digest = store.put_blocking(&manifest).unwrap();
        (store, manifest_digest, dir)
    }

    #[tokio::test]
    async fn ensure_base_is_idempotent_and_cloneable() {
        let (store, digest, _dir) = store_with_one_file();
        let base_store = BaseStore::from_store(store.clone());

        let base = base_store.ensure_base(&digest).await.unwrap();
        assert!(base.path.join("hello.txt").is_file());

        // Second call returns the same base without rebuilding.
        let base2 = base_store.ensure_base(&digest).await.unwrap();
        assert_eq!(base.path, base2.path);

        // Two workloads get independent trees.
        let out_a = tempfile::tempdir().unwrap();
        let out_b = tempfile::tempdir().unwrap();
        let (a, _) = prepare_bundle_from_base(&store, &base, &digest, out_a.path())
            .await
            .unwrap();
        let (b, _) = prepare_bundle_from_base(&store, &base, &digest, out_b.path())
            .await
            .unwrap();
        std::fs::write(a.rootfs_path.join("hello.txt"), b"changed").unwrap();
        assert_eq!(
            std::fs::read(b.rootfs_path.join("hello.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(
            std::fs::read(base.path.join("hello.txt")).unwrap(),
            b"hello"
        );
    }
}
