// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;

use serde::Deserialize;
use tracing::{debug, info};

use pullrun_store::{Digest, MmapStore};

use crate::puller::OciError;

#[derive(Deserialize)]
struct ExportManifest {
    root_digest: String,
    #[allow(dead_code)]
    format: String,
    #[allow(dead_code)]
    node_count: usize,
    #[allow(dead_code)]
    blob_count: usize,
}

/// Read a tar archive previously created by `export_dag_to_tar`
/// and populate the store with all nodes and blobs.
///
/// Returns the root digest from the manifest, along with storage
/// statistics (bytes stored and bytes deduplicated).
pub fn import_dag_from_tar<R: Read>(
    store: &MmapStore,
    reader: R,
) -> Result<(String, i64, i64), OciError> {
    let mut archive = tar::Archive::new(reader);
    let mut root_digest = String::new();
    let mut bytes_stored: i64 = 0;
    let mut bytes_dedup: i64 = 0;
    let mut found_manifest = false;

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let path = match entry.path().ok().map(|p| p.to_string_lossy().to_string()) {
            Some(p) => p,
            None => continue,
        };

        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;

        if path == "pullrun-manifest.json" {
            let data: ExportManifest = serde_json::from_slice(&buf)?;
            root_digest = data.root_digest;
            found_manifest = true;
            info!(root_digest = %root_digest, "importing DAG export");
            continue;
        }

        let entry_data = buf;

        if let Some(digest_str) = path.strip_prefix("pullrun-nodes/") {
            let digest = Digest::from_hex(digest_str)
                .map_err(|e| OciError::Other(format!("invalid digest in archive: {e}")))?;
            let computed = MmapStore::compute_digest(&entry_data);
            if computed != digest {
                return Err(OciError::Other(format!(
                    "node digest mismatch: expected {digest_str}, got {}",
                    computed.as_hex()
                )));
            }
            // Validate the rkyv data, then write directly to disk.
            let _validated =
                rkyv::check_archived_root::<pullrun_store::DagNode>(&entry_data).map_err(|e| {
                    OciError::Other(format!("invalid node {digest_str}: {e}"))
                })?;

            let already_exists = store.exists(&digest);
            if !already_exists {
                let node_path = store.node_path(&digest);
                if let Some(parent) = node_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&node_path, &entry_data)?;
                bytes_stored += entry_data.len() as i64;
            } else {
                bytes_dedup += entry_data.len() as i64;
            }
            debug!(
                %digest,
                size = entry_data.len(),
                dedup = already_exists,
                "imported node"
            );
        } else if let Some(digest_str) = path.strip_prefix("pullrun-blobs/") {
            let digest = Digest::from_hex(digest_str)
                .map_err(|e| OciError::Other(format!("invalid digest in archive: {e}")))?;
            // Verify content matches the claimed digest.
            let computed = MmapStore::compute_digest(&entry_data);
            if computed != digest {
                return Err(OciError::Other(format!(
                    "blob digest mismatch: expected {digest_str}, got {}",
                    computed.as_hex()
                )));
            }
            let already_exists = store.blob_path(&digest).exists();
            store.put_blob_blocking(&digest, &entry_data)?;
            if already_exists {
                bytes_dedup += entry_data.len() as i64;
            } else {
                bytes_stored += entry_data.len() as i64;
            }
            debug!(
                %digest,
                size = entry_data.len(),
                dedup = already_exists,
                "imported blob"
            );
        }
    }

    if !found_manifest {
        return Err(OciError::Other(
            "pullrun-manifest.json not found in archive".into(),
        ));
    }

    info!(
        %root_digest,
        bytes_stored,
        bytes_dedup,
        "import complete"
    );

    Ok((root_digest, bytes_stored, bytes_dedup))
}
