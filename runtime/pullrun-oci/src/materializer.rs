// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use tracing::{debug, info};

use pullrun_store::{Digest, MmapStore};

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

    pub fn materialize_manifest(
        &self,
        manifest_digest: &Digest,
    ) -> Result<ManifestData, OciError> {
        let manifest_node = self.store.get_archived(manifest_digest)
            .map_err(|e| OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}")))?;

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

        let manifest_node = self.store.get_archived(manifest_digest)
            .map_err(|e| OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}")))?;

        let layer_digests: Vec<Digest> = manifest_node
            .edges
            .iter()
            .map(|e| Digest(*e))
            .collect();

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

        let manifest_node = self.store.get_archived(manifest_digest)
            .map_err(|e| OciError::Other(format!("corrupt manifest node {manifest_digest}: {e}")))?;

        let layer_digests: Vec<Digest> = manifest_node
            .edges
            .iter()
            .map(|e| Digest(*e))
            .collect();

        for (i, layer_digest) in layer_digests.iter().enumerate() {
            debug!(layer = i, %layer_digest, "materializing layer");
            self.materialize_layer(layer_digest, target_dir)?;
        }

        Ok(())
    }

    fn materialize_layer(
        &self,
        layer_digest: &Digest,
        rootfs_path: &Path,
    ) -> Result<(), OciError> {
        let layer_node = self.store.get_archived(layer_digest)
            .map_err(|e| OciError::Other(format!("corrupt layer node {layer_digest}: {e}")))?;

        if layer_node.is_layer() {
            let layer_path = String::from_utf8_lossy(&layer_node.inline_data).to_string();
            for edge in layer_node.edges.iter() {
                let child_digest = Digest(*edge);
                self.materialize_tree(&child_digest, rootfs_path, &layer_path)?;
            }
        }

        Ok(())
    }

    fn materialize_tree(
        &self,
        tree_digest: &Digest,
        rootfs_path: &Path,
        base_path: &str,
    ) -> Result<(), OciError> {
        let tree_node = self.store.get_archived(tree_digest)
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

                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mode = if entry.mode == 0 { 0o644 } else { entry.mode };
                        let _ = std::fs::set_permissions(
                            &full_path,
                            std::fs::Permissions::from_mode(mode),
                        );
                    }
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
        let oci_spec = serde_json::json!({
            "ociVersion": "1.1.0",
            "process": {
                "terminal": false,
                "user": {
                    "uid": 0,
                    "gid": 0
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
            }
        });

        std::fs::write(
            config_path,
            serde_json::to_string_pretty(&oci_spec)
                .expect("oci_spec serialization never fails"),
        )?;

        Ok(())
    }
}