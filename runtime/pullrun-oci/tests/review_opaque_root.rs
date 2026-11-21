// Review-only regression probe (not part of the shipped suite).
//
// Goal: prove that a layer tar containing a *root* opaque whiteout
// (`.wh..wh..opq` with no parent directory) is converted into a DAG
// node whose inline data is the literal string "/".
//
// This test deliberately stops after conversion. It never calls the
// materializer, because the materializer would resolve `rootfs.join("/")`
// to the host root and attempt to delete its entries.

use std::io::Write;
use std::sync::Arc;

use pullrun_oci::puller::{OciImageConfig, OciManifest, OciRootFs};
use pullrun_oci::{OciDescriptor, OciToDagConverter, PulledImage};
use pullrun_store::{Digest, MmapStore};

fn gzip(tar_bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(tar_bytes).unwrap();
    gz.finish().unwrap()
}

fn header(size: u64, mode: u32) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_size(size);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h.set_cksum();
    h
}

fn layer_with_file() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut bytes);
        let mut h = header(5, 0o644);
        b.append_data(&mut h, "hello.txt", &b"hello"[..]).unwrap();
        b.finish().unwrap();
    }
    gzip(&bytes)
}

fn layer_with_root_opaque() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut b = tar::Builder::new(&mut bytes);
        let mut h = header(0, 0o644);
        b.append_data(&mut h, ".wh..wh..opq", &[][..]).unwrap();
        b.finish().unwrap();
    }
    gzip(&bytes)
}

#[tokio::test]
async fn root_opaque_whiteout_is_confined_to_bundle() {
    let store_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MmapStore::new(store_dir.path().to_path_buf()));

    let lower = layer_with_file();
    let upper = layer_with_root_opaque();
    let lower_digest = Digest::compute(&lower);
    let upper_digest = Digest::compute(&upper);

    let config = OciImageConfig {
        created: None,
        author: None,
        architecture: "arm64".to_string(),
        os: "linux".to_string(),
        os_version: None,
        os_features: None,
        variant: None,
        config: None,
        rootfs: OciRootFs {
            fs_type: "layers".to_string(),
            diff_ids: vec![],
        },
        history: None,
    };

    let descriptor = |media_type: &str, digest: &Digest, size: u64| OciDescriptor {
        media_type: media_type.to_string(),
        digest: digest.to_string(),
        size,
        urls: None,
        annotations: None,
        data: None,
        platform: None,
        artifact_type: None,
    };

    let manifest = OciManifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        artifact_type: None,
        config: descriptor(
            "application/vnd.oci.image.config.v1+json",
            &Digest([1u8; 32]),
            2,
        ),
        layers: vec![
            descriptor(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                &lower_digest,
                lower.len() as u64,
            ),
            descriptor(
                "application/vnd.oci.image.layer.v1.tar+gzip",
                &upper_digest,
                upper.len() as u64,
            ),
        ],
        subject: None,
        annotations: None,
    };

    let image = PulledImage {
        manifest,
        config,
        config_digest: Digest([1u8; 32]),
        layer_blobs: vec![
            (
                lower_digest,
                lower,
                "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            ),
            (
                upper_digest,
                upper,
                "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
            ),
        ],
    };

    let converter = OciToDagConverter::new(store.clone());
    let root = converter.convert(&image).await.expect("conversion");

    let manifest_node = store.get_archived(&root).unwrap();
    let edges: Vec<Digest> = manifest_node.edges.iter().map(|e| Digest(*e)).collect();
    drop(manifest_node);

    let mut opaque_paths = Vec::new();
    for layer_digest in &edges {
        let layer = store.get_archived(layer_digest).unwrap();
        for edge in layer.edges.iter() {
            let child_digest = Digest(*edge);
            if let Ok(child) = store.get_archived(&child_digest) {
                if child.is_opaque_dir() {
                    opaque_paths.push(String::from_utf8_lossy(&child.inline_data).to_string());
                }
            }
        }
    }

    // Root opacity is recorded as the empty relative path. It must
    // never be encoded as "/" (which `Path::join` would resolve to the
    // host root).
    assert_eq!(
        opaque_paths,
        vec![String::new()],
        "root opaque whiteout must be recorded as the empty relative path"
    );

    // Clearing the bundle root is confined to the materialized rootfs.
    let materializer = pullrun_oci::OciMaterializer::new(&store);
    let bundle_dir = tempfile::tempdir().unwrap();
    let bundle = materializer
        .materialize_bundle(&root, bundle_dir.path())
        .expect("materialize root opaque layer");
    assert!(bundle.rootfs_path.exists());
    assert!(
        !bundle.rootfs_path.join("hello.txt").exists(),
        "root opacity must clear the bundle root"
    );
}
