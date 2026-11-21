// Review-only confinement probes (not part of the shipped suite).
//
// These exercise the *real* converter + materializer with crafted layer
// tars. Every outside path used as an attack target is a file or
// directory inside a `tempfile::tempdir()`, so the test cannot damage
// the host. The assertion is that materialization stays inside the
// bundle rootfs; it currently does not.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use pullrun_oci::puller::{OciImageConfig, OciManifest, OciRootFs};
use pullrun_oci::{OciDescriptor, OciMaterializer, OciToDagConverter, PulledImage};
use pullrun_store::{Digest, MmapStore};

const MT_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const MT_CONFIG: &str = "application/vnd.oci.image.config.v1+json";

fn gz(tar_bytes: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(tar_bytes).unwrap();
    gz.finish().unwrap()
}

fn tar_with<F: FnOnce(&mut tar::Builder<&mut Vec<u8>>)>(f: F) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut b = tar::Builder::new(&mut out);
        f(&mut b);
        b.finish().unwrap();
    }
    out
}

fn header(size: u64, mode: u32) -> tar::Header {
    let mut h = tar::Header::new_gnu();
    h.set_size(size);
    h.set_mode(mode);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h
}

fn descriptor(media_type: &str, digest: &Digest, size: u64) -> OciDescriptor {
    OciDescriptor {
        media_type: media_type.to_string(),
        digest: digest.to_string(),
        size,
        urls: None,
        annotations: None,
        data: None,
        platform: None,
        artifact_type: None,
    }
}

fn image_with_layers(layers: Vec<Vec<u8>>) -> (PulledImage, Vec<Digest>) {
    let mut layer_blobs = Vec::new();
    let mut layer_descriptors = Vec::new();
    for raw in layers {
        let d = Digest::compute(&raw);
        layer_descriptors.push(descriptor(MT_LAYER, &d, raw.len() as u64));
        layer_blobs.push((d, raw, MT_LAYER.to_string()));
    }

    let manifest = OciManifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        artifact_type: None,
        config: descriptor(MT_CONFIG, &Digest([1u8; 32]), 2),
        layers: layer_descriptors,
        subject: None,
        annotations: None,
    };
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
    let digests: Vec<Digest> = layer_blobs.iter().map(|(d, _, _)| *d).collect();
    (
        PulledImage {
            manifest,
            config,
            config_digest: Digest([1u8; 32]),
            layer_blobs,
        },
        digests,
    )
}

async fn convert(image: &PulledImage) -> (Arc<MmapStore>, tempfile::TempDir, Digest) {
    let store_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MmapStore::new(store_dir.path().to_path_buf()));
    let root = OciToDagConverter::new(store.clone())
        .convert(image)
        .await
        .expect("convert");
    (store, store_dir, root)
}

/// Symlink in layer 1 points outside; layer 2 whiteouts a path through it.
/// Confined resolution re-roots the absolute symlink target, so the
/// outside file must survive.
#[tokio::test]
async fn symlink_whiteout_stays_inside_rootfs() {
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel.txt");
    std::fs::write(&sentinel, b"must survive").unwrap();

    let outside_target = outside.path().to_string_lossy().to_string();

    let layer1 = tar_with(|b| {
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_link_name(&outside_target).unwrap();
        h.set_cksum();
        b.append_data(&mut h, "escape", &[][..]).unwrap();
    });
    let layer1 = gz(&layer1);

    let layer2 = tar_with(|b| {
        let mut h = header(0, 0o644);
        h.set_cksum();
        b.append_data(&mut h, "escape/.wh.sentinel.txt", &[][..])
            .unwrap();
    });
    let layer2 = gz(&layer2);

    let (image, _) = image_with_layers(vec![layer1, layer2]);
    let (store, _store_dir, root) = convert(&image).await;

    let bundle_dir = tempfile::tempdir().unwrap();
    let _bundle = OciMaterializer::new(&store)
        .materialize_bundle(&root, bundle_dir.path())
        .expect("materialize");

    assert!(
        sentinel.exists(),
        "symlink+whiteout deleted a file outside the rootfs ({})",
        sentinel.display()
    );
}

/// Sanity: the materializer does create the symlink as a symlink.
#[tokio::test]
async fn symlink_is_created() {
    let layer = tar_with(|b| {
        let mut h = header(0, 0o777);
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_link_name("/tmp").unwrap();
        h.set_cksum();
        b.append_data(&mut h, "link", &[][..]).unwrap();
    });
    let layer = gz(&layer);
    let (image, _) = image_with_layers(vec![layer]);
    let (store, _store_dir, root) = convert(&image).await;

    let bundle_dir = tempfile::tempdir().unwrap();
    let bundle = OciMaterializer::new(&store)
        .materialize_bundle(&root, bundle_dir.path())
        .expect("materialize");
    let meta = std::fs::symlink_metadata(bundle.rootfs_path.join("link")).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(
        std::fs::read_link(bundle.rootfs_path.join("link")).unwrap(),
        Path::new("/tmp")
    );
}

/// A DAG archive (`pullrun load`) can carry arbitrary whiteout targets,
/// including absolute paths. Materialization must reject them rather
/// than unlink a host path.
#[tokio::test]
async fn dag_imported_absolute_whiteout_is_rejected() {
    use pullrun_oci::ManifestData;
    use pullrun_store::DagNode;

    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel.txt");
    std::fs::write(&sentinel, b"must survive").unwrap();

    let store_dir = tempfile::tempdir().unwrap();
    let store = Arc::new(MmapStore::new(store_dir.path().to_path_buf()));

    let whiteout = DagNode::whiteout(sentinel.to_string_lossy().as_bytes().to_vec());
    let whiteout_digest = store.put(&whiteout).await.unwrap();

    let layer = DagNode::layer(vec![whiteout_digest], Vec::new());
    let layer_digest = store.put(&layer).await.unwrap();

    let manifest_data = ManifestData {
        entrypoint: vec![],
        cmd: vec![],
        env: vec![],
        working_dir: None,
        architecture: "arm64".to_string(),
        os: "linux".to_string(),
        annotations: None,
        subject: None,
        user: None,
        stop_signal: None,
        exposed_ports: None,
        volumes: None,
        variant: None,
    };
    let manifest = DagNode::manifest(
        vec![layer_digest],
        serde_json::to_vec(&manifest_data).unwrap(),
    );
    let manifest_digest = store.put(&manifest).await.unwrap();

    let bundle_dir = tempfile::tempdir().unwrap();
    let result = OciMaterializer::new(&store)
        .materialize_bundle(&manifest_digest, bundle_dir.path());

    assert!(
        result.is_err(),
        "absolute whiteout target must be rejected"
    );
    assert!(
        sentinel.exists(),
        "DAG whiteout deleted {} outside the rootfs",
        sentinel.display()
    );
}
