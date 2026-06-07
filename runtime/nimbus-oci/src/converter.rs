use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tracing::{debug, info};

use nimbus_store::{DagNode, Digest, MmapStore, NodeKind};

use crate::puller::{OciError, PulledImage};

const SMALL_FILE_THRESHOLD: u64 = 4096;

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
            digest: self.digest.clone(),
            mode: self.mode,
            size: self.size,
            is_dir: self.is_dir,
            is_symlink: self.is_symlink,
            symlink_target: self.symlink_target.clone(),
        };
        let mut buf = serde_json::to_vec(&entry).unwrap_or_default();
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
    digest: String,
    mode: u32,
    size: u64,
    is_dir: bool,
    is_symlink: bool,
    symlink_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestData {
    pub config_json: String,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub working_dir: Option<String>,
    pub architecture: String,
    pub os: String,
}

/// Convert OCI images to Nimbus DAG nodes.
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

    async fn convert_layer(
        &self,
        layer_digest: &str,
        blob: &[u8],
    ) -> Result<Digest, OciError> {
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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Digest, OciError>> + Send + 'b>> {
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
                    let subtree_digest = self
                        .store_trees(dir_index, entries, &sub_path)
                        .await?;
                    child_digests.push(subtree_digest);
                } else {
                    child_digests.push(entry.digest.clone());
                }
                child_entries.push(entry.clone());
            }

            let inline: Vec<u8> = child_entries.iter().flat_map(|e| e.to_inline_bytes()).collect();

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
        let config_json = serde_json::to_string(config)
            .map_err(|e| OciError::InvalidManifest(format!("config serialize: {e}")))?;

        let config_bytes = config_json.as_bytes().to_vec();

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
            config_json: String::from_utf8_lossy(&config_bytes).to_string(),
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
    let decoder = GzDecoder::new(blob);
    let mut archive = Archive::new(decoder);
    let mut dir_index: HashMap<String, Vec<usize>> = HashMap::new();

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
        let mode = header.mode()?;
        let size = header.size()?;

        let symlink_target = if is_symlink {
            header
                .link_name()?
                .map(|p| p.to_string_lossy().to_string())
        } else {
            None
        };

        let (parent, name) = split_path(&path_str);

        let blob_digest = if is_dir || is_symlink {
            let blob_d = format!("{}:{}:dir", if is_symlink { "symlink" } else { "dir" }, path_str);
            MmapStore::compute_digest(blob_d.as_bytes())
        } else {
            let mut file_data = Vec::new();
            entry.read_to_end(&mut file_data)?;

            let blob_node = DagNode::blob(file_data.clone());
            let blob_digest = store.put_blocking(&blob_node)?;

            if size > SMALL_FILE_THRESHOLD {
                store.put_blob_blocking(&blob_digest, &file_data)?;
            }

            blob_digest
        };

        let idx = entries.len();
        entries.push(DirectoryEntry {
            name,
            digest: blob_digest,
            mode,
            size,
            is_dir,
            is_symlink,
            symlink_target,
        });

        dir_index.entry(parent).or_default().push(idx);

        if is_dir {
            let full_dir_path = if path_str.ends_with('/') {
                path_str.clone()
            } else {
                format!("{path_str}/")
            };
            dir_index.entry(path_str.clone()).or_default();
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
        assert_eq!(
            split_path("usr/"),
            ("".to_string(), "usr".to_string())
        );
        assert_eq!(
            split_path("foo"),
            ("".to_string(), "foo".to_string())
        );
    }
}