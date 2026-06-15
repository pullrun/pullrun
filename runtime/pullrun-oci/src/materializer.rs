// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use tracing::{debug, info};

use pullrun_store::{Digest, MmapStore};

use crate::converter::ManifestData;
use crate::puller::OciError;

// Platform-specific helpers for file attributes.
#[cfg(unix)]
mod imp {
    use std::path::Path;

    pub fn set_owner(path: &Path, uid: u32, gid: u32) {
        use std::os::unix::fs::MetadataExt;
        // Only chown if the current owner/gid differ from target.
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.uid() != uid || meta.gid() != gid {
                let _ = std::os::unix::fs::chown(path, Some(uid), Some(gid));
            }
        }
    }

    pub fn set_mtime(path: &Path, mtime: i64) {
        if mtime < 0 {
            return;
        }
        let ft = filetime::FileTime::from_unix_time(mtime, 0);
        let _ = filetime::set_file_mtime(path, ft);
    }

    pub fn set_xattr(path: &Path, name: &[u8], value: &[u8]) {
        use std::os::unix::ffi::OsStrExt;
        let name_os = std::ffi::OsStr::from_bytes(name);
        let _ = xattr::set(path, name_os, value);
    }
}

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
        std::fs::create_dir_all(&rootfs_path)?;

        let manifest_data = self.materialize_manifest(manifest_digest)?;

        info!(%manifest_digest, rootfs = %rootfs_path.display(), "materializing bundle");

        let manifest_node = self.store.get_archived(manifest_digest).map_err(|e| {
            OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}"))
        })?;

        let layer_digests: Vec<Digest> = manifest_node.edges.iter().map(|e| Digest(*e)).collect();

        for (i, layer_digest) in layer_digests.iter().enumerate() {
            debug!(layer = i, %layer_digest, "materializing layer");
            self.materialize_layer(layer_digest, &rootfs_path)?;
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
        std::fs::create_dir_all(target_dir)?;

        info!(%manifest_digest, target = %target_dir.display(), "materializing DAG root into target dir");

        let manifest_node = self.store.get_archived(manifest_digest).map_err(|e| {
            OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}"))
        })?;

        let layer_digests: Vec<Digest> = manifest_node.edges.iter().map(|e| Digest(*e)).collect();

        for (i, layer_digest) in layer_digests.iter().enumerate() {
            debug!(layer = i, %layer_digest, "materializing layer");
            self.materialize_layer(layer_digest, target_dir)?;
        }

        Ok(())
    }

    fn materialize_layer(&self, layer_digest: &Digest, rootfs_path: &Path) -> Result<(), OciError> {
        let layer_node = self
            .store
            .get_archived(layer_digest)
            .map_err(|e| OciError::Other(format!("corrupt layer node {layer_digest}: {e}")))?;

        if !layer_node.is_layer() {
            return Ok(());
        }

        let layer_path = String::from_utf8_lossy(&layer_node.inline_data).to_string();

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
            let target_path_str =
                String::from_utf8_lossy(&whiteout_node.inline_data).to_string();

            if *is_opaque {
                // Opaque whiteout: remove ALL children of the target directory.
                let dir_path = if layer_path.is_empty() {
                    rootfs_path.join(&target_path_str)
                } else {
                    rootfs_path.join(&layer_path).join(&target_path_str)
                };
                if dir_path.is_dir() {
                    debug!(target = %dir_path.display(), "applying opaque whiteout");
                    for entry in std::fs::read_dir(&dir_path)? {
                        let entry = entry?;
                        let path = entry.path();
                        if path.is_dir() {
                            std::fs::remove_dir_all(&path)?;
                        } else {
                            std::fs::remove_file(&path)?;
                        }
                    }
                }
            } else {
                // Explicit whiteout: delete a single path.
                let full_path = if layer_path.is_empty() {
                    rootfs_path.join(&target_path_str)
                } else {
                    rootfs_path.join(&layer_path).join(&target_path_str)
                };
                debug!(target = %full_path.display(), "applying whiteout");
                let _ = std::fs::remove_file(&full_path);
                let _ = std::fs::remove_dir(&full_path);
            }
        }

        // Now extract the tree contents (this layer's actual files).
        for child_digest in &tree_edges {
            self.materialize_tree(child_digest, rootfs_path, &layer_path)?;
        }

        Ok(())
    }

    fn materialize_tree(
        &self,
        tree_digest: &Digest,
        rootfs_path: &Path,
        base_path: &str,
    ) -> Result<(), OciError> {
        let tree_node = self
            .store
            .get_archived(tree_digest)
            .map_err(|e| OciError::Other(format!("corrupt tree node {tree_digest}: {e}")))?;

        if tree_node.is_layer() {
            let layer_path = String::from_utf8_lossy(&tree_node.inline_data).to_string();
            for edge in tree_node.edges.iter() {
                let child_digest = Digest(*edge);
                self.materialize_tree(&child_digest, rootfs_path, &layer_path)?;
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
                let entry_path = if base_path.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", base_path, entry.name)
                };

                let full_path = rootfs_path.join(&entry_path);

                if entry.is_dir {
                    std::fs::create_dir_all(&full_path)?;
                } else if entry.is_symlink {
                    if let Some(target) = &entry.symlink_target {
                        let _ = std::fs::remove_file(&full_path);
                        let _ = std::os::unix::fs::symlink(target, &full_path);
                    }
                } else {
                    if let Some(parent) = full_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    let blob_path = self.store.blob_path(&entry.digest);
                    if blob_path.exists() {
                        std::fs::copy(&blob_path, &full_path)?;
                    } else if let Ok(blob_node) = self.store.get_archived(&entry.digest) {
                        std::fs::write(&full_path, &blob_node.inline_data[..])?;
                    }
                }

                // Apply file attributes (mode, uid, gid, mtime, xattrs).
                #[cfg(unix)]
                {
                    apply_unix_attributes(&full_path, &entry);
                }
            }
        }

        for edge in tree_node.edges.iter() {
            let child_digest = Digest(*edge);
            if self.store.get(&child_digest).is_err() {
                debug!(child = %child_digest, "skipping edge (symlink marker, not a stored node)");
                continue;
            }
            self.materialize_tree(&child_digest, rootfs_path, base_path)?;
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
fn apply_unix_attributes(full_path: &Path, entry: &crate::converter::DirectoryEntry) {
    use std::os::unix::fs::PermissionsExt;

    // Set permissions (mode).
    let mode = if entry.is_dir {
        if entry.mode == 0 { 0o755 } else { entry.mode }
    } else if entry.is_symlink {
        0o777
    } else {
        if entry.mode == 0 { 0o644 } else { entry.mode }
    };
    let _ = std::fs::set_permissions(
        full_path,
        std::fs::Permissions::from_mode(mode),
    );

    // Set ownership (best-effort — may fail as non-root).
    imp::set_owner(full_path, entry.uid, entry.gid);

    // Set mtime (best-effort).
    imp::set_mtime(full_path, entry.mtime);

    // Apply extended attributes (best-effort).
    if !entry.xattrs.is_empty() {
        for (name, value) in &entry.xattrs {
            imp::set_xattr(full_path, name.as_bytes(), value);
        }
    }
}
