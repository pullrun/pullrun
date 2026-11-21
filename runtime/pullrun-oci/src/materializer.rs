// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use tracing::{debug, info};

use pullrun_store::{Digest, MmapStore};

use crate::confinement::{RelPath, RootDir};
use crate::converter::ManifestData;
use crate::puller::OciError;

pub struct MaterializedBundle {
    pub rootfs_path: PathBuf,
    pub config_path: PathBuf,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
}

pub struct OciMaterializer<'a> {
    store: &'a MmapStore,
}

impl<'a> OciMaterializer<'a> {
    pub fn new(store: &'a MmapStore) -> Self {
        Self { store }
    }

    pub fn materialize_manifest(&self, manifest_digest: &Digest) -> Result<ManifestData, OciError> {
        let manifest_node = self.store.get_archived(manifest_digest).map_err(|e| {
            OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}"))
        })?;

        if !manifest_node.is_manifest() {
            return Err(OciError::InvalidManifest(format!(
                "node {manifest_digest} is not a manifest"
            )));
        }

        let data: ManifestData = serde_json::from_slice(&manifest_node.inline_data)
            .map_err(|e| OciError::InvalidManifest(format!("manifest data parse: {e}")))?;

        Ok(data)
    }

    pub fn materialize_bundle(
        &self,
        manifest_digest: &Digest,
        output_dir: &Path,
    ) -> Result<MaterializedBundle, OciError> {
        let rootfs_path = output_dir.join("rootfs");
        let root = RootDir::open(&rootfs_path)?;

        let manifest_data = self.materialize_manifest(manifest_digest)?;

        info!(%manifest_digest, rootfs = %rootfs_path.display(), "materializing bundle");

        let manifest_node = self.store.get_archived(manifest_digest).map_err(|e| {
            OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}"))
        })?;

        let layer_digests: Vec<Digest> = manifest_node.edges.iter().map(|e| Digest(*e)).collect();

        for (i, layer_digest) in layer_digests.iter().enumerate() {
            debug!(layer = i, %layer_digest, "materializing layer");
            self.materialize_layer(layer_digest, &root)?;
        }

        let config_path = output_dir.join("config.json");
        self.write_oci_config(&config_path, &manifest_data, &rootfs_path)?;

        debug!(config = %config_path.display(), "bundle materialized");

        Ok(MaterializedBundle {
            rootfs_path,
            config_path,
            entrypoint: manifest_data.entrypoint,
            cmd: manifest_data.cmd,
            env: manifest_data.env,
            working_dir: manifest_data.working_dir,
        })
    }

    /// Materialize a DAG root directly into a target directory.
    /// Unlike `materialize_bundle`, this does NOT create a `rootfs/` subdir
    /// and does NOT write an OCI config.json. It is used for raw rootfs
    /// materialization (e.g., into a mounted ext4 image).
    pub async fn materialize_into(
        &self,
        manifest_digest: &Digest,
        target_dir: &Path,
    ) -> Result<(), OciError> {
        let root = RootDir::open(target_dir)?;

        info!(%manifest_digest, target = %target_dir.display(), "materializing DAG root into target dir");

        let manifest_node = self.store.get_archived(manifest_digest).map_err(|e| {
            OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}"))
        })?;

        let layer_digests: Vec<Digest> = manifest_node.edges.iter().map(|e| Digest(*e)).collect();

        for (i, layer_digest) in layer_digests.iter().enumerate() {
            debug!(layer = i, %layer_digest, "materializing layer");
            self.materialize_layer(layer_digest, &root)?;
        }

        Ok(())
    }

    fn materialize_layer(&self, layer_digest: &Digest, root: &RootDir) -> Result<(), OciError> {
        let layer_node = self
            .store
            .get_archived(layer_digest)
            .map_err(|e| OciError::Other(format!("corrupt layer node {layer_digest}: {e}")))?;

        if !layer_node.is_layer() {
            return Ok(());
        }

        let layer_path_raw = String::from_utf8_lossy(&layer_node.inline_data).to_string();
        let layer_path = RelPath::parse(&layer_path_raw).map_err(|e| {
            OciError::Other(format!("invalid layer prefix {layer_path_raw:?}: {e}"))
        })?;

        // Split edges into tree roots and whiteout nodes.
        let mut tree_edges = Vec::new();
        let mut whiteout_edges = Vec::new();
        for edge in layer_node.edges.iter() {
            let child_digest = Digest(*edge);
            match self.store.get_archived(&child_digest) {
                Ok(child) if child.is_whiteout() || child.is_opaque_dir() => {
                    whiteout_edges.push((child_digest, child.is_opaque_dir()));
                }
                _ => tree_edges.push(child_digest),
            }
        }

        // Apply whiteouts BEFORE extracting the layer's tree so that
        // lower-layer entries are removed before this layer's files land.
        for (whiteout_digest, is_opaque) in &whiteout_edges {
            let whiteout_node = self.store.get_archived(whiteout_digest).map_err(|e| {
                OciError::Other(format!("corrupt whiteout node {whiteout_digest}: {e}"))
            })?;
            let target_raw = String::from_utf8_lossy(&whiteout_node.inline_data).to_string();
            let target = RelPath::parse(&target_raw).map_err(|e| {
                OciError::Other(format!("invalid whiteout target {target_raw:?}: {e}"))
            })?;
            let effective = layer_path.join(&target);
            if effective.is_root() {
                // Replacing the whole rootfs: clear the bundle, never a
                // host path.
                debug!("applying root opaque whiteout");
                root.clear_dir(&RelPath::root())?;
            } else if *is_opaque {
                debug!(target = %effective.display(), "applying opaque whiteout");
                root.clear_dir(&effective)?;
            } else {
                debug!(target = %effective.display(), "applying whiteout");
                root.remove_path(&effective)?;
            }
        }

        // Now extract the tree contents (this layer's actual files).
        for child_digest in &tree_edges {
            self.materialize_tree(child_digest, root, &layer_path)?;
        }

        Ok(())
    }

    fn materialize_tree(
        &self,
        tree_digest: &Digest,
        root: &RootDir,
        base_path: &RelPath,
    ) -> Result<(), OciError> {
        let tree_node = self
            .store
            .get_archived(tree_digest)
            .map_err(|e| OciError::Other(format!("corrupt tree node {tree_digest}: {e}")))?;

        if tree_node.is_layer() {
            let raw = String::from_utf8_lossy(&tree_node.inline_data).to_string();
            let layer_path = RelPath::parse(&raw)
                .map_err(|e| OciError::Other(format!("invalid layer prefix {raw:?}: {e}")))?;
            for edge in tree_node.edges.iter() {
                let child_digest = Digest(*edge);
                self.materialize_tree(&child_digest, root, &layer_path)?;
            }
            return Ok(());
        }

        if !tree_node.is_tree() {
            return Ok(());
        }

        let inline_data = tree_node.inline_data.as_ref();

        if !inline_data.is_empty() {
            let entries = crate::converter::DirectoryEntry::from_inline_bytes(inline_data);
            for entry in entries {
                let rel = base_path.with(&entry.name).map_err(|e| {
                    OciError::Other(format!("invalid entry name {:?}: {e}", entry.name))
                })?;
                if entry.is_dir {
                    root.ensure_dir(&rel)?;
                } else if entry.is_symlink {
                    if let Some(target) = &entry.symlink_target {
                        root.symlink(&rel, std::ffi::OsStr::new(target))?;
                    }
                } else {
                    let blob_path = self.store.blob_path(&entry.digest);
                    if blob_path.exists() {
                        root.copy_file_from(&rel, &blob_path, 0o644)?;
                    } else if let Ok(blob_node) = self.store.get_archived(&entry.digest) {
                        root.write_file(&rel, &blob_node.inline_data[..], 0o644)?;
                    }
                }

                // Apply file attributes (mode, uid, gid, mtime, xattrs).
                apply_unix_attributes(root, &rel, &entry)?;
            }
        }

        for edge in tree_node.edges.iter() {
            let child_digest = Digest(*edge);
            if self.store.get(&child_digest).is_err() {
                debug!(child = %child_digest, "skipping edge (symlink marker, not a stored node)");
                continue;
            }
            self.materialize_tree(&child_digest, root, base_path)?;
        }

        Ok(())
    }

    fn write_oci_config(
        &self,
        config_path: &Path,
        manifest_data: &ManifestData,
        rootfs_path: &Path,
    ) -> Result<(), OciError> {
        // Parse User string per OCI conversion spec.
        // Formats: "user", "user:group", "uid", "uid:gid", "uid:group"
        let (uid, gid) = parse_user_string(&manifest_data.user);

        // Build runtime annotations from conversion metadata.
        let mut annotations = manifest_data.annotations.clone().unwrap_or_default();
        annotations
            .entry("org.opencontainers.image.os".to_string())
            .or_insert_with(|| manifest_data.os.clone());
        annotations
            .entry("org.opencontainers.image.architecture".to_string())
            .or_insert_with(|| manifest_data.architecture.clone());
        if let Some(variant) = &manifest_data.variant {
            annotations
                .entry("org.opencontainers.image.variant".to_string())
                .or_insert_with(|| variant.clone());
        }
        if let Some(signal) = &manifest_data.stop_signal {
            annotations
                .entry("org.opencontainers.image.stopSignal".to_string())
                .or_insert_with(|| signal.clone());
        }
        if let Some(ports) = &manifest_data.exposed_ports {
            if !ports.is_empty() {
                annotations
                    .entry("org.opencontainers.image.exposedPorts".to_string())
                    .or_insert_with(|| ports.join(","));
            }
        }

        // Build mounts for volumes.
        let mut mounts = Vec::new();
        if let Some(vols) = &manifest_data.volumes {
            for vol in vols {
                mounts.push(serde_json::json!({
                    "destination": vol,
                    "type": "bind",
                    "source": vol,
                    "options": ["rbind", "rw"]
                }));
            }
        }

        let oci_spec = serde_json::json!({
            "ociVersion": "1.1.0",
            "process": {
                "terminal": false,
                "user": {
                    "uid": uid,
                    "gid": gid
                },
                "args": if !manifest_data.entrypoint.is_empty() {
                    [manifest_data.entrypoint.clone(), manifest_data.cmd.clone()].concat()
                } else {
                    manifest_data.cmd.clone()
                },
                "env": manifest_data.env,
                "cwd": manifest_data.working_dir.clone().unwrap_or_else(|| "/".to_string()),
            },
            "root": {
                "path": rootfs_path.to_string_lossy(),
                "readonly": false
            },
            "linux": {
                "namespaces": [
                    {"type": "pid"},
                    {"type": "network"},
                    {"type": "ipc"},
                    {"type": "uts"},
                    {"type": "mount"}
                ]
            },
            "annotations": annotations,
            "mounts": mounts,
        });

        std::fs::write(
            config_path,
            serde_json::to_string_pretty(&oci_spec).expect("oci_spec serialization never fails"),
        )?;

        Ok(())
    }
}

/// Parse `Config.User` string per OCI conversion spec.
/// Returns (uid, gid). Defaults to (0, 0) on parse failure.
fn parse_user_string(user: &Option<String>) -> (u32, u32) {
    let Some(user_str) = user.as_ref() else {
        return (0, 0);
    };
    let user_str = user_str.trim();
    if user_str.is_empty() {
        return (0, 0);
    }

    // Try parsing as `uid[:gid]` (all numeric).
    if let Some((uid_str, gid_str)) = user_str.split_once(':') {
        let uid: u32 = uid_str.parse().unwrap_or(0);
        let gid: u32 = gid_str.parse().unwrap_or(0);
        return (uid, gid);
    }

    // Single value: could be numeric uid or named user.
    if let Ok(uid) = user_str.parse::<u32>() {
        return (uid, 0);
    }

    // Named user — fall back to root (0, 0).
    (0, 0)
}

#[cfg(unix)]
fn apply_unix_attributes(
    root: &RootDir,
    rel: &RelPath,
    entry: &crate::converter::DirectoryEntry,
) -> Result<(), OciError> {
    // Set permissions (mode).
    let mode = if entry.is_dir {
        if entry.mode == 0 {
            0o755
        } else {
            entry.mode
        }
    } else if entry.is_symlink {
        0o777
    } else if entry.mode == 0 {
        0o644
    } else {
        entry.mode
    };
    let _ = root.set_mode(rel, mode);

    // Set ownership (best-effort — may fail as non-root).
    let _ = root.set_owner(rel, entry.uid, entry.gid);

    // Set mtime (best-effort).
    let _ = root.set_mtime(rel, entry.mtime);

    // Apply extended attributes (best-effort).
    for (name, value) in &entry.xattrs {
        let _ = root.set_xattr(rel, name.as_bytes(), value);
    }

    Ok(())
}

#[cfg(not(unix))]
fn apply_unix_attributes(
    _root: &RootDir,
    _rel: &RelPath,
    _entry: &crate::converter::DirectoryEntry,
) -> Result<(), OciError> {
    Ok(())
}
