use std::collections::{HashSet, VecDeque};
use std::io::Write;

use serde::Serialize;
use tracing::debug;

use nimbus_store::MmapStore;

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
///   nimbus-manifest.json  — metadata (root digest, counts)
///   nimbus-nodes/<digest> — rkyv-serialized DagNode for each node
///   nimbus-blobs/<digest> — raw blob data (only when stored separately)
pub fn export_dag_to_tar<W: Write>(
    store: &MmapStore,
    root_digest: &str,
    writer: W,
) -> Result<(), OciError> {
    let mut tar = tar::Builder::new(writer);

    // BFS walk to collect all reachable digests.
    let (node_digests, blob_digests) = walk_dag_collect(store, root_digest);

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
    tar.append_data(&mut header, "nimbus-manifest.json", manifest_json.as_slice())?;

    // Write each node (read via the cache API).
    for digest in &node_digests {
        let mmap = store.get(digest).map_err(|e| {
            OciError::Other(format!("read node {digest}: {e}"))
        })?;
        let data: &[u8] = &mmap[..];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_mtime(0);
        let tar_path = format!("nimbus-nodes/{digest}");
        tar.append_data(&mut header, &tar_path, data)?;
        debug!(%digest, size = data.len(), "exported node");
    }

    // Write each blob (read via the store API).
    for digest in &blob_digests {
        let mmap = store.get_blob(digest).map_err(|e| {
            OciError::Other(format!("read blob {digest}: {e}"))
        })?;
        let data: &[u8] = &mmap[..];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_mtime(0);
        let tar_path = format!("nimbus-blobs/{digest}");
        tar.append_data(&mut header, &tar_path, data)?;
        debug!(%digest, size = data.len(), "exported blob");
    }

    tar.finish()?;
    Ok(())
}

/// BFS walk from `root_digest`, collecting all node digests and
/// which ones have separate blob files on disk.
fn walk_dag_collect(store: &MmapStore, root_digest: &str) -> (Vec<String>, Vec<String>) {
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut node_digests: Vec<String> = Vec::new();
    let mut blob_digests: Vec<String> = Vec::new();

    if !root_digest.is_empty() {
        queue.push_back(root_digest.to_string());
    }

    while let Some(digest) = queue.pop_front() {
        if !visited.insert(digest.clone()) {
            continue;
        }

        // Check node exists.
        if !store.exists(&digest) {
            continue;
        }

        node_digests.push(digest.clone());

        // Check if this node has a separate blob file.
        if store.blob_path(&digest).exists() {
            blob_digests.push(digest.clone());
        }

        // Enqueue edges.
        if let Ok(archived) = store.get_archived(&digest) {
            for edge in archived.edges.iter() {
                let edge_str: String = edge.as_str().to_string();
                if !edge_str.is_empty() {
                    queue.push_back(edge_str);
                }
            }
        }
    }

    (node_digests, blob_digests)
}
