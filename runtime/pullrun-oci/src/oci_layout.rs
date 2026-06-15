use std::path::{Path, PathBuf};

use sha2::{Digest as Sha256Digest, Sha256};
use tracing::info;

use pullrun_store::{Digest, MmapStore};

use crate::converter::ManifestData;
use crate::puller::{OciDescriptor, OciError, OciImageIndex, OciManifest};
use crate::puller::{media_types, OciImageConfig, OciRootFs, OciRuntimeConfig};

/// Export a DAG image to an OCI Image Layout directory.
/// Produces:
///   output_dir/
///     oci-layout           → {"imageLayoutVersion": "1.0.0"}
///     index.json           → OCI image index
///     blobs/sha256/<hash>  → config, layer, manifest blobs
pub fn export_to_oci_layout(
    store: &MmapStore,
    root_digest: &Digest,
    output_dir: &Path,
    tags: &[String],
) -> Result<PathBuf, OciError> {
    let blobs_dir = output_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_dir)?;

    // Write oci-layout file.
    std::fs::write(
        output_dir.join("oci-layout"),
        "{\"imageLayoutVersion\": \"1.0.0\"}\n",
    )?;

    // Check if root is a manifest list or single manifest.
    let root_archived = store
        .get_archived(root_digest)
        .map_err(|e| OciError::Other(format!("read root node: {e}")))?;

    if root_archived.is_manifest_list() {
        export_manifest_list(store, root_digest, &blobs_dir, output_dir, tags)
    } else if root_archived.is_manifest() {
        export_single_manifest(store, root_digest, &blobs_dir, output_dir, tags)
    } else {
        Err(OciError::Other(format!(
            "root digest {root_digest} is neither a manifest nor manifest list"
        )))
    }
}

fn export_single_manifest(
    store: &MmapStore,
    manifest_digest: &Digest,
    blobs_dir: &Path,
    output_dir: &Path,
    tags: &[String],
) -> Result<PathBuf, OciError> {
    let archived = store
        .get_archived(manifest_digest)
        .map_err(|e| OciError::Other(format!("read manifest node: {e}")))?;

    let manifest_data: ManifestData =
        serde_json::from_slice(archived.inline_data.as_ref())
            .map_err(|e| OciError::InvalidManifest(format!("manifest data parse: {e}")))?;

    let layer_digests: Vec<Digest> = archived.edges.iter().map(|e| Digest(*e)).collect();

    // Reconstruct OCI config.
    let oci_config = OciImageConfig {
        created: None,
        author: None,
        architecture: manifest_data.architecture.clone(),
        os: manifest_data.os.clone(),
        os_version: None,
        os_features: None,
        variant: manifest_data.variant.clone(),
        config: Some(OciRuntimeConfig {
            user: manifest_data.user.clone(),
            exposed_ports: manifest_data
                .exposed_ports
                .as_ref()
                .map(|ports| {
                    ports
                        .iter()
                        .map(|p| (p.clone(), serde_json::Value::Object(Default::default())))
                        .collect()
                }),
            env: Some(manifest_data.env.clone()),
            entrypoint: Some(manifest_data.entrypoint.clone()),
            cmd: Some(manifest_data.cmd.clone()),
            volumes: manifest_data
                .volumes
                .as_ref()
                .map(|vols| {
                    vols.iter()
                        .map(|v| (v.clone(), serde_json::Value::Object(Default::default())))
                        .collect()
                }),
            working_dir: manifest_data.working_dir.clone(),
            labels: None,
            stop_signal: manifest_data.stop_signal.clone(),
            args_escaped: false,
        }),
        rootfs: OciRootFs {
            diff_ids: vec![],
            fs_type: "layers".to_string(),
        },
        history: None,
    };
    let config_json = serde_json::to_vec(&oci_config)?;
    let config_digest_hex = hex::encode(Sha256::digest(&config_json));
    std::fs::write(blobs_dir.join(&config_digest_hex), &config_json)?;

    // Reconstruct and write each layer.
    let mut oci_layers = Vec::new();
    for layer_digest in &layer_digests {
        let (layer_data, _oci_digest) = reconstruct_layer(store, layer_digest)?;
        let layer_digest_hex = hex::encode(Sha256::digest(&layer_data));
        std::fs::write(blobs_dir.join(&layer_digest_hex), &layer_data)?;

        oci_layers.push(OciDescriptor {
            media_type: media_types::LAYER_TAR_GZIP.to_string(),
            digest: format!("sha256:{layer_digest_hex}"),
            size: layer_data.len() as u64,
            urls: None,
            annotations: None,
            data: None,
            platform: None,
            artifact_type: None,
        });
    }

    // Write manifest blob.
    let oci_manifest = OciManifest {
        schema_version: 2,
        media_type: media_types::IMAGE_MANIFEST.to_string(),
        artifact_type: None,
        config: OciDescriptor {
            media_type: media_types::IMAGE_CONFIG.to_string(),
            digest: format!("sha256:{config_digest_hex}"),
            size: config_json.len() as u64,
            urls: None,
            annotations: None,
            data: None,
            platform: None,
            artifact_type: None,
        },
        layers: oci_layers,
        subject: manifest_data.subject,
        annotations: manifest_data.annotations,
    };
    let manifest_json = serde_json::to_vec(&oci_manifest)?;
    let manifest_digest_hex = hex::encode(Sha256::digest(&manifest_json));
    std::fs::write(blobs_dir.join(&manifest_digest_hex), &manifest_json)?;

    // Write index.json.
    let mut manifests = Vec::new();
    for tag in tags {
        manifests.push(OciDescriptor {
            media_type: media_types::IMAGE_MANIFEST.to_string(),
            digest: format!("sha256:{manifest_digest_hex}"),
            size: manifest_json.len() as u64,
            urls: None,
            annotations: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    crate::puller::annotations::REF_NAME.to_string(),
                    tag.clone(),
                );
                Some(m)
            },
            data: None,
            platform: None,
            artifact_type: None,
        });
    }

    // If no tags provided, add an untagged entry.
    if manifests.is_empty() {
        manifests.push(OciDescriptor {
            media_type: media_types::IMAGE_MANIFEST.to_string(),
            digest: format!("sha256:{manifest_digest_hex}"),
            size: manifest_json.len() as u64,
            urls: None,
            annotations: None,
            data: None,
            platform: None,
            artifact_type: None,
        });
    }

    let index = OciImageIndex {
        schema_version: 2,
        media_type: media_types::IMAGE_INDEX.to_string(),
        artifact_type: None,
        manifests,
        subject: None,
        annotations: None,
    };
    let index_json = serde_json::to_vec(&index)?;
    std::fs::write(output_dir.join("index.json"), &index_json)?;

    let digest_hex = hex::encode(manifest_digest.0.as_slice());
    info!(%digest_hex, dir = %output_dir.display(), "OCI layout exported");
    Ok(output_dir.join("index.json"))
}

fn export_manifest_list(
    store: &MmapStore,
    list_digest: &Digest,
    blobs_dir: &Path,
    output_dir: &Path,
    _tags: &[String],
) -> Result<PathBuf, OciError> {
    let archived = store
        .get_archived(list_digest)
        .map_err(|e| OciError::Other(format!("read list node: {e}")))?;

    let mut child_descriptors = Vec::new();
    for edge in archived.edges.iter() {
        let child = Digest(*edge);
        // Export each child as a single manifest blob.
        export_single_manifest(store, &child, blobs_dir, output_dir, &[])?;

        let child_node = store
            .get_archived(&child)
            .map_err(|e| OciError::Other(format!("read child: {e}")))?;
        let child_data: ManifestData =
            serde_json::from_slice(child_node.inline_data.as_ref())
                .map_err(|e| OciError::InvalidManifest(format!("child data: {e}")))?;

        // Reconstruct the OCI manifest to get its digest.
        let oci_config = OciImageConfig {
            created: None,
            author: None,
            architecture: child_data.architecture.clone(),
            os: child_data.os.clone(),
            os_version: None,
            os_features: None,
        variant: child_data.variant.clone(),
        config: Some(OciRuntimeConfig {
            user: child_data.user.clone(),
            exposed_ports: child_data
                .exposed_ports
                .as_ref()
                .map(|ports| {
                    ports
                        .iter()
                        .map(|p| (p.clone(), serde_json::Value::Object(Default::default())))
                        .collect()
                }),
            env: Some(child_data.env.clone()),
            entrypoint: Some(child_data.entrypoint.clone()),
            cmd: Some(child_data.cmd.clone()),
            volumes: child_data
                .volumes
                .as_ref()
                .map(|vols| {
                    vols.iter()
                        .map(|v| (v.clone(), serde_json::Value::Object(Default::default())))
                        .collect()
                }),
            working_dir: child_data.working_dir.clone(),
            labels: None,
            stop_signal: child_data.stop_signal.clone(),
            args_escaped: false,
        }),
            rootfs: OciRootFs {
                diff_ids: vec![],
                fs_type: "layers".to_string(),
            },
            history: None,
        };
        let config_json = serde_json::to_vec(&oci_config)?;
        let child_layer_digests: Vec<Digest> =
            child_node.edges.iter().map(|e| Digest(*e)).collect();
        let mut oci_layers = Vec::new();
        for ld in &child_layer_digests {
            let (layer_data, _) = reconstruct_layer(store, ld)?;
            let layer_hex = hex::encode(Sha256::digest(&layer_data));
            oci_layers.push(OciDescriptor {
                media_type: media_types::LAYER_TAR_GZIP.to_string(),
                digest: format!("sha256:{layer_hex}"),
                size: layer_data.len() as u64,
                urls: None,
                annotations: None,
                data: None,
                platform: None,
                artifact_type: None,
            });
        }
        let config_hex = hex::encode(Sha256::digest(&config_json));
        let child_manifest = OciManifest {
            schema_version: 2,
            media_type: media_types::IMAGE_MANIFEST.to_string(),
            artifact_type: None,
            config: OciDescriptor {
                media_type: media_types::IMAGE_CONFIG.to_string(),
                digest: format!("sha256:{config_hex}"),
                size: config_json.len() as u64,
                urls: None,
                annotations: None,
                data: None,
                platform: None,
                artifact_type: None,
            },
            layers: oci_layers,
            subject: child_data.subject,
            annotations: child_data.annotations,
        };
        let manifest_json = serde_json::to_vec(&child_manifest)?;
        let manifest_hex = hex::encode(Sha256::digest(&manifest_json));
        std::fs::write(blobs_dir.join(&manifest_hex), &manifest_json)?;

        child_descriptors.push(OciDescriptor {
            media_type: media_types::IMAGE_MANIFEST.to_string(),
            digest: format!("sha256:{manifest_hex}"),
            size: manifest_json.len() as u64,
            urls: None,
            annotations: None,
            data: None,
            platform: None,
            artifact_type: None,
        });
    }

    let index = OciImageIndex {
        schema_version: 2,
        media_type: media_types::IMAGE_INDEX.to_string(),
        artifact_type: None,
        manifests: child_descriptors,
        subject: None,
        annotations: None,
    };
    let index_json = serde_json::to_vec(&index)?;
    std::fs::write(output_dir.join("index.json"), &index_json)?;

    let root_hex = hex::encode(list_digest.0.as_slice());
    info!(%root_hex, dir = %output_dir.display(), "OCI manifest list layout exported");
    Ok(output_dir.join("index.json"))
}

/// Reconstruct a gzip-compressed OCI layer from a DAG layer node.
fn reconstruct_layer(store: &MmapStore, layer_digest: &Digest) -> Result<(Vec<u8>, String), OciError> {
    let mut tar_buf = Vec::new();
    let mut gz = flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::default());
    let mut tar = tar::Builder::new(&mut gz);
    walk_tree_for_layer(store, layer_digest, "", &mut tar)?;
    tar.finish()?;
    drop(tar);
    let _ = gz
        .finish()
        .map_err(|e| OciError::Other(format!("gzip finish: {e}")))?;

    let digest = format!("sha256:{}", hex::encode(Sha256::digest(&tar_buf)));
    Ok((tar_buf, digest))
}

fn walk_tree_for_layer(
    store: &MmapStore,
    node_digest: &Digest,
    base_path: &str,
    tar: &mut tar::Builder<&mut flate2::write::GzEncoder<&mut Vec<u8>>>,
) -> Result<(), OciError> {
    let archived = store
        .get_archived(node_digest)
        .map_err(|e| OciError::Other(format!("read node {node_digest}: {e}")))?;

    if archived.is_layer() {
        let layer_path = String::from_utf8_lossy(&archived.inline_data).to_string();
        for edge in archived.edges.iter() {
            let child = Digest(*edge);
            walk_tree_for_layer(store, &child, &layer_path, tar)?;
        }
        return Ok(());
    }

    if !archived.is_tree() {
        return Ok(());
    }

    let inline_data = archived.inline_data.as_ref();
    if !inline_data.is_empty() {
        let entries = crate::converter::DirectoryEntry::from_inline_bytes(inline_data);
        for entry in &entries {
            let entry_path = if base_path.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", base_path, entry.name)
            };

            if entry.is_dir {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(entry.mode);
                header.set_mtime(entry.mtime as u64);
                header.set_uid(entry.uid.into());
                header.set_gid(entry.gid.into());
                tar.append_data(&mut header, &entry_path, std::io::empty())?;
            } else if entry.is_symlink {
                if let Some(target) = &entry.symlink_target {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_mode(0o777);
                    tar.append_link(&mut header, &entry_path, target)?;
                }
            } else {
                let blob_path = store.blob_path(&entry.digest);
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(entry.mode);
                header.set_size(entry.size);
                header.set_mtime(entry.mtime as u64);
                header.set_uid(entry.uid.into());
                header.set_gid(entry.gid.into());

                let data: Vec<u8> = if blob_path.exists() {
                    std::fs::read(&blob_path)?
                } else if let Ok(blob_node) = store.get_archived(&entry.digest) {
                    blob_node.inline_data.to_vec()
                } else {
                    Vec::new()
                };

                tar.append_data(&mut header, &entry_path, std::io::Cursor::new(&data))?;
            }
        }
    }

    for edge in archived.edges.iter() {
        let child_digest = Digest(*edge);
        if store.get(&child_digest).is_err() {
            continue;
        }
        walk_tree_for_layer(store, &child_digest, base_path, tar)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pullrun_store::MmapStore;
    use std::sync::Arc;

    #[test]
    fn test_export_to_oci_layout() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let store = Arc::new(MmapStore::new(store_path));

        // Create a minimal DAG: a single blob node wrapped in a tree and layer.
        let blob = b"hello world";
        let blob_node = pullrun_store::DagNode::blob(blob.to_vec());
        let blob_digest = store.put_blocking(&blob_node).unwrap();

        let entry = crate::converter::DirectoryEntry {
            name: "hello.txt".to_string(),
            digest: blob_digest,
            mode: 0o644,
            size: blob.len() as u64,
            uid: 1000,
            gid: 1000,
            mtime: 0,
            is_dir: false,
            is_symlink: false,
            symlink_target: None,
            xattrs: std::collections::HashMap::new(),
        };
        let inline = entry.to_inline_bytes();
        let tree_node = pullrun_store::DagNode {
            kind: pullrun_store::NodeKind::Tree,
            edges: vec![blob_digest],
            inline_data: inline,
        };
        let tree_digest = store.put_blocking(&tree_node).unwrap();

        let layer_node = pullrun_store::DagNode {
            kind: pullrun_store::NodeKind::Layer,
            edges: vec![tree_digest],
            inline_data: b"".to_vec(),
        };
        let layer_digest = store.put_blocking(&layer_node).unwrap();

        let manifest_data = ManifestData {
            entrypoint: vec![],
            cmd: vec!["/bin/sh".to_string()],
            env: vec![],
            working_dir: None,
            architecture: "amd64".to_string(),
            os: "linux".to_string(),
            annotations: None,
            subject: None,
            user: None,
            stop_signal: None,
            exposed_ports: None,
            volumes: None,
            variant: None,
        };
        let manifest_node = pullrun_store::DagNode {
            kind: pullrun_store::NodeKind::Manifest,
            edges: vec![layer_digest],
            inline_data: serde_json::to_vec(&manifest_data).unwrap(),
        };
        let manifest_digest = store.put_blocking(&manifest_node).unwrap();

        let export_dir = dir.path().join("export");
        let result = export_to_oci_layout(&store, &manifest_digest, &export_dir, &[]);
        assert!(result.is_ok(), "export failed: {:?}", result.err());

        // Verify oci-layout file.
        let layout_path = export_dir.join("oci-layout");
        assert!(layout_path.exists(), "oci-layout missing");
        let layout_content = std::fs::read_to_string(&layout_path).unwrap();
        assert_eq!(layout_content.trim(), r#"{"imageLayoutVersion": "1.0.0"}"#);

        // Verify index.json exists and is valid JSON.
        let index_path = export_dir.join("index.json");
        assert!(index_path.exists(), "index.json missing");
        let index: serde_json::Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        assert_eq!(index["schemaVersion"], 2);
        assert!(index["manifests"].is_array());
        assert!(!index["manifests"].as_array().unwrap().is_empty());

        // Verify blobs directory exists and has content.
        let blobs_dir = export_dir.join("blobs").join("sha256");
        assert!(blobs_dir.exists(), "blobs/sha256 missing");
        let entries: Vec<_> = std::fs::read_dir(&blobs_dir).unwrap().collect();
        assert!(!entries.is_empty(), "blobs/sha256 is empty");

        // Verify the manifest blob is valid.
        let manifest_descriptor = &index["manifests"][0];
        let digest = manifest_descriptor["digest"].as_str().unwrap();
        let hex_digest = digest.strip_prefix("sha256:").unwrap();
        let blob_path = blobs_dir.join(hex_digest);
        assert!(blob_path.exists(), "manifest blob missing");
        let _manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&blob_path).unwrap()).unwrap();
    }
}
