// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tracing::{debug, info};

use pullrun_store::{DagNode, Digest, MmapStore, NodeKind, SMALL_FILE_THRESHOLD};

use crate::puller::{OciError, PulledImage, PulledImageList};

#[derive(Debug, Clone)]
pub struct DirectoryEntry {
    pub name: String,
    pub digest: Digest,
    pub mode: u32,
    pub size: u64,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
}

impl DirectoryEntry {
    fn to_inline_bytes(&self) -> Vec<u8> {
        let entry: SerializedEntry = SerializedEntry {
            name: self.name.clone(),
            digest: self.digest,
            mode: self.mode,
            size: self.size,
            is_dir: self.is_dir,
            is_symlink: self.is_symlink,
            symlink_target: self.symlink_target.clone(),
        };
        let mut buf = serde_json::to_vec(&entry).expect("SerializedEntry must always serialize");
        buf.push(b'\n');
        buf
    }

    pub fn from_inline_bytes(data: &[u8]) -> Vec<Self> {
        let mut out = Vec::new();
        for line in data.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_slice::<SerializedEntry>(line) {
                out.push(DirectoryEntry {
                    name: e.name,
                    digest: e.digest,
                    mode: e.mode,
                    size: e.size,
                    is_dir: e.is_dir,
                    is_symlink: e.is_symlink,
                    symlink_target: e.symlink_target,
                });
            }
        }
        out
    }
}

#[derive(Serialize, Deserialize)]
struct SerializedEntry {
    name: String,
    digest: Digest,
    mode: u32,
    size: u64,
    is_dir: bool,
    is_symlink: bool,
    symlink_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestData {
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub architecture: String,
    pub os: String,
}

/// Convert OCI images to Pullrun DAG nodes.
pub struct OciToDagConverter {
    store: Arc<MmapStore>,
}

impl OciToDagConverter {
    pub fn new(store: Arc<MmapStore>) -> Self {
        Self { store }
    }

    pub async fn convert(&self, image: &PulledImage) -> Result<Digest, OciError> {
        info!("converting OCI image to DAG");

        let mut layer_digests = Vec::new();

        for (layer_digest, blob) in &image.layer_blobs {
            let dag_digest = self.convert_layer(layer_digest, blob).await?;
            layer_digests.push(dag_digest);
        }

        let manifest_digest = self
            .create_manifest_node(&image.config, &layer_digests)
            .await?;

        info!(%manifest_digest, layers = layer_digests.len(), "DAG conversion complete");
        Ok(manifest_digest)
    }

    /// Convert a multi-arch `PulledImageList` into DAG nodes.
    /// Each child image is converted individually, then a
    /// `ManifestList` node is created with edges to each
    /// platform's manifest digest.
    pub async fn convert_list(&self, list: &PulledImageList) -> Result<Digest, OciError> {
        info!("converting multi-arch image list to DAG");

        let mut manifest_digests = Vec::with_capacity(list.images.len());
        for image in &list.images {
            let digest = self.convert(image).await?;
            manifest_digests.push(digest);
        }

        let n = manifest_digests.len();
        let list_node = DagNode::manifest_list(manifest_digests, list.list_bytes.clone());
        let list_digest = self.store.put(&list_node).await?;

        info!(%list_digest, children = n, "manifest list DAG conversion complete");
        Ok(list_digest)
    }

    async fn convert_layer(&self, layer_digest: &Digest, blob: &[u8]) -> Result<Digest, OciError> {
        debug!(%layer_digest, size = blob.len(), "converting layer to DAG");

        let store = self.store.clone();
        let blob_owned = blob.to_vec();

        let (entries, dir_index) = tokio::task::spawn_blocking(move || -> Result<_, OciError> {
            let mut entries: Vec<DirectoryEntry> = Vec::new();
            let dir_index = extract_tar_entries_sync(&blob_owned, &store, &mut entries)?;
            Ok((entries, dir_index))
        })
        .await
        .map_err(|e| OciError::Other(format!("join error: {e}")))??;

        Box::pin(self.store_trees(&dir_index, &entries, "")).await
    }

    fn store_trees<'b>(
        &'b self,
        dir_index: &'b HashMap<String, Vec<usize>>,
        entries: &'b [DirectoryEntry],
        current_path: &'b str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Digest, OciError>> + Send + 'b>>
    {
        Box::pin(async move {
            let children = dir_index.get(current_path).cloned().unwrap_or_default();
            let mut child_entries = Vec::new();
            let mut child_digests = Vec::new();

            for idx in children {
                let entry = &entries[idx];
                if entry.is_dir {
                    let sub_path = if current_path.is_empty() {
                        entry.name.clone()
                    } else {
                        format!("{}/{}", current_path, entry.name)
                    };
                    let subtree_digest = self.store_trees(dir_index, entries, &sub_path).await?;
                    child_digests.push(subtree_digest);
                } else {
                    child_digests.push(entry.digest);
                }
                child_entries.push(entry.clone());
            }

            let inline: Vec<u8> = child_entries
                .iter()
                .flat_map(|e| e.to_inline_bytes())
                .collect();

            let tree_node = DagNode {
                kind: NodeKind::Tree,
                edges: child_digests,
                inline_data: inline,
            };

            let tree_digest = self.store.put(&tree_node).await?;

            let layer_node = DagNode {
                kind: NodeKind::Layer,
                edges: vec![tree_digest],
                inline_data: current_path.as_bytes().to_vec(),
            };

            let digest = self.store.put(&layer_node).await?;
            Ok(digest)
        })
    }

    async fn create_manifest_node(
        &self,
        config: &crate::puller::OciImageConfig,
        layer_digests: &[Digest],
    ) -> Result<Digest, OciError> {
        let _config_json = serde_json::to_string(config)
            .map_err(|e| OciError::InvalidManifest(format!("config serialize: {e}")))?;

        let entrypoint: Vec<String> = config
            .config
            .as_ref()
            .and_then(|c| c.entrypoint.clone())
            .unwrap_or_default();

        let cmd: Vec<String> = config
            .config
            .as_ref()
            .and_then(|c| c.cmd.clone())
            .unwrap_or_default();

        let env: Vec<String> = config
            .config
            .as_ref()
            .and_then(|c| c.env.clone())
            .unwrap_or_default();

        let manifest_data = ManifestData {
            entrypoint,
            cmd,
            env,
            working_dir: config.config.as_ref().and_then(|c| c.working_dir.clone()),
            architecture: config.architecture.clone(),
            os: config.os.clone(),
        };

        let inline = serde_json::to_vec(&manifest_data).unwrap_or_default();

        let manifest_node = DagNode {
            kind: NodeKind::Manifest,
            edges: layer_digests.to_vec(),
            inline_data: inline,
        };

        let digest = self.store.put(&manifest_node).await?;
        Ok(digest)
    }
}

fn extract_tar_entries_sync(
    blob: &[u8],
    store: &MmapStore,
    entries: &mut Vec<DirectoryEntry>,
) -> Result<HashMap<String, Vec<usize>>, OciError> {
    struct RawEntry {
        path: String,
        parent: String,
        name: String,
        mode: u32,
        size: u64,
        is_dir: bool,
        is_symlink: bool,
        symlink_target: Option<String>,
        is_hardlink: bool,
        hardlink_target: Option<String>,
        blob_digest: Digest,
    }

    let decoder = GzDecoder::new(blob);
    let mut archive = Archive::new(decoder);
    let mut dir_index: HashMap<String, Vec<usize>> = HashMap::new();
    let mut raw_entries: Vec<RawEntry> = Vec::new();
    // Map from path to digest for regular files (used to resolve hardlinks).
    let mut path_to_digest: HashMap<String, Digest> = HashMap::new();

    // First pass: collect all entries.
    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = entry.path()?;
        let path_str = path.to_string_lossy().to_string();

        if path_str == "." || path_str.is_empty() || path_str.contains("..") {
            continue;
        }

        let header = entry.header();
        let is_dir = header.entry_type().is_dir();
        let is_symlink = header.entry_type().is_symlink();
        let is_hardlink = header.entry_type().is_hard_link();
        let mode = header.mode()?;
        let size = header.size()?;

        let symlink_target = if is_symlink {
            header.link_name()?.map(|p| p.to_string_lossy().to_string())
        } else {
            None
        };

        let hardlink_target = if is_hardlink {
            header.link_name()?.map(|p| p.to_string_lossy().to_string())
        } else {
            None
        };

        let (parent, name) = split_path(&path_str);

        let blob_digest = if is_dir || is_symlink {
            let blob_d = format!(
                "{}:{}:dir",
                if is_symlink { "symlink" } else { "dir" },
                path_str
            );
            MmapStore::compute_digest(blob_d.as_bytes())
        } else if is_hardlink {
            // Hardlinks have no data; resolve digest from target in second pass.
            Digest([0u8; 32])
        } else {
            let mut file_data = Vec::new();
            entry.read_to_end(&mut file_data)?;

            let blob_node = DagNode::blob(file_data.clone());
            let blob_digest = store.put_blocking(&blob_node)?;

            if size > SMALL_FILE_THRESHOLD {
                store.put_blob_blocking(&blob_digest, &file_data)?;
            }

            path_to_digest.insert(path_str.clone(), blob_digest);
            blob_digest
        };

        raw_entries.push(RawEntry {
            path: path_str,
            parent,
            name,
            mode,
            size,
            is_dir,
            is_symlink,
            symlink_target,
            is_hardlink,
            hardlink_target,
            blob_digest,
        });
    }

    // Second pass: resolve hardlink digests and build final entries.
    // Deferred hardlinks whose target hasn't been seen yet.
    let mut deferred: Vec<usize> = Vec::new();

    for (idx, raw_entry) in raw_entries
        .iter_mut()
        .enumerate()
        .filter(|(_, e)| e.is_hardlink)
    {
        let target = raw_entry.hardlink_target.clone();
        if let Some(ref target_path) = target {
            if let Some(target_digest) = path_to_digest.get(target_path) {
                let d = *target_digest;
                path_to_digest.insert(raw_entry.path.clone(), d);
                raw_entry.blob_digest = d;
            } else {
                deferred.push(idx);
            }
        }
    }

    // Third pass: resolve deferred hardlinks (target may have been added in pass 2).
    let max_retries = deferred.len();
    let mut retries = 0;
    while retries < max_retries && !deferred.is_empty() {
        retries += 1;
        let mut still_deferred: Vec<usize> = Vec::new();
        for idx in deferred.drain(..) {
            let target = raw_entries[idx].hardlink_target.clone();
            if let Some(ref target_path) = target {
                if let Some(target_digest) = path_to_digest.get(target_path) {
                    raw_entries[idx].blob_digest = *target_digest;
                } else {
                    still_deferred.push(idx);
                }
            }
        }
        deferred = still_deferred;
    }

    // Build final entries.
    for raw in &raw_entries {
        let blob_digest = if raw.blob_digest == Digest([0u8; 32]) {
            // Unresolved hardlink: create empty blob as fallback.
            let empty = DagNode::blob(Vec::new());
            store.put_blocking(&empty)?
        } else {
            raw.blob_digest
        };

        let idx = entries.len();
        entries.push(DirectoryEntry {
            name: raw.name.clone(),
            digest: blob_digest,
            mode: raw.mode,
            size: raw.size,
            is_dir: raw.is_dir,
            is_symlink: raw.is_symlink,
            symlink_target: raw.symlink_target.clone(),
        });

        dir_index.entry(raw.parent.clone()).or_default().push(idx);

        if raw.is_dir {
            let full_dir_path = if raw.path.ends_with('/') {
                raw.path.clone()
            } else {
                format!("{}/", raw.path)
            };
            dir_index.entry(raw.path.clone()).or_default();
            dir_index.entry(full_dir_path).or_default();
        }
    }

    Ok(dir_index)
}

fn split_path(path: &str) -> (String, String) {
    let path = path.trim_end_matches('/');
    match path.rsplit_once('/') {
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (String::new(), path.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_path() {
        assert_eq!(
            split_path("usr/bin/bash"),
            ("usr/bin".to_string(), "bash".to_string())
        );
        assert_eq!(split_path("usr/"), ("".to_string(), "usr".to_string()));
        assert_eq!(split_path("foo"), ("".to_string(), "foo".to_string()));
    }
}
