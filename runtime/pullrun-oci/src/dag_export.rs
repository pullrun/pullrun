// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::io::Write;

use serde::Serialize;
use tracing::debug;

use pullrun_store::{walk_reachable, Digest, MmapStore};

use crate::puller::OciError;

#[derive(Serialize)]
struct ExportManifest {
    root_digest: String,
    format: String,
    node_count: usize,
    blob_count: usize,
}

/// Walk the DAG starting at `root_digest` and serialize every
/// reachable node + blob into a tar archive written to `writer`.
///
/// The tar layout is:
///   pullrun-manifest.json  — metadata (root digest, counts)
///   `pullrun-nodes/<digest>` — rkyv-serialized DagNode for each node
///   `pullrun-blobs/<digest>` — raw blob data (only when stored separately)
pub fn export_dag_to_tar<W: Write>(
    store: &MmapStore,
    root_digest: &str,
    writer: W,
) -> Result<(), OciError> {
    let mut tar = tar::Builder::new(writer);

    // BFS walk to collect all reachable digests.
    let (node_digests, blob_digests) = walk_dag_collect(store, root_digest)
        .map_err(|e| OciError::Other(format!("BFS walk failed: {e}")))?;

    // Write manifest.
    let manifest = ExportManifest {
        root_digest: root_digest.to_string(),
        format: "dag".to_string(),
        node_count: node_digests.len(),
        blob_count: blob_digests.len(),
    };
    let manifest_json = serde_json::to_vec(&manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_json.len() as u64);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_mtime(0);
    tar.append_data(
        &mut header,
        "pullrun-manifest.json",
        manifest_json.as_slice(),
    )?;

    // Write each node (read via the cache API).
    for digest in &node_digests {
        let mmap = store
            .get(digest)
            .map_err(|e| OciError::Other(format!("read node {}: {e}", digest.as_hex())))?;
        let data: &[u8] = &mmap[..];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_mtime(0);
        let tar_path = format!("pullrun-nodes/{}", digest.as_hex());
        tar.append_data(&mut header, &tar_path, data)?;
        debug!(%digest, size = data.len(), "exported node");
    }

    // Write each blob (read via the store API).
    for digest in &blob_digests {
        let mmap = store
            .get_blob(digest)
            .map_err(|e| OciError::Other(format!("read blob {}: {e}", digest.as_hex())))?;
        let data: &[u8] = &mmap[..];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_mtime(0);
        let tar_path = format!("pullrun-blobs/{}", digest.as_hex());
        tar.append_data(&mut header, &tar_path, data)?;
        debug!(%digest, size = data.len(), "exported blob");
    }

    tar.finish()?;
    Ok(())
}

/// BFS walk from `root_digest`, collecting all node digests and
/// which ones have separate blob files on disk.
fn walk_dag_collect(
    store: &MmapStore,
    root_digest: &str,
) -> Result<(Vec<Digest>, Vec<Digest>), pullrun_store::StoreError> {
    let root = Digest::from_hex(root_digest).unwrap_or(Digest([0u8; 32]));
    walk_reachable(store, &[root])
}
