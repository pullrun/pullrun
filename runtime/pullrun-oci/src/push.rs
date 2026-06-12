// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine;
use serde::Serialize;
use sha2::{Digest as Sha256Digest, Sha256};
use tracing::{debug, info};

use pullrun_store::{Digest, MmapStore};

use crate::converter::{DirectoryEntry, ManifestData};
use crate::puller::{OciAuth, OciDescriptor, OciError, OciImageConfig, OciManifest, OciRuntimeConfig, OciRootFs};

/// Push a DAG image to an OCI-compatible registry.
pub struct DagPusher {
    store: Arc<MmapStore>,
    client: reqwest::Client,
    auth: Option<OciAuth>,
    insecure_registries: HashSet<String>,
}

impl DagPusher {
    pub fn new(
        store: Arc<MmapStore>,
        auth: Option<OciAuth>,
        insecure_registries: HashSet<String>,
    ) -> Self {
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(
            reqwest::header::ACCEPT_ENCODING,
            reqwest::header::HeaderValue::from_static("identity"),
        );
        let client = reqwest::Client::builder()
            .default_headers(default_headers)
            .redirect(reqwest::redirect::Policy::limited(5))
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            store,
            client,
            auth,
            insecure_registries,
        }
    }

    fn scheme(&self, registry: &str) -> &'static str {
        if self.insecure_registries.contains(registry) {
            "http:"
        } else {
            "https:"
        }
    }

    fn registry_for(&self, target_ref: &str) -> (String, String, String) {
        // Parse "registry.example.com/repo:tag" or "repo:tag"
        let parts: Vec<&str> = target_ref.split('/').collect();
        let (registry, rest) = if parts.len() > 1
            && (parts[0].contains('.') || parts[0].contains(':') || parts[0] == "localhost")
        {
            (parts[0].to_string(), parts[1..].join("/"))
        } else {
            ("registry-1.docker.io".to_string(), target_ref.to_string())
        };

        let segments: Vec<&str> = rest.splitn(2, ':').collect();
        let tag = segments.get(1).unwrap_or(&"latest").to_string();
        let repository = segments[0].to_string();

        (registry, repository, tag)
    }

    async fn get_token(
        &self,
        registry: &str,
        repository: &str,
        scope: &str, // e.g. "push,pull"
    ) -> Result<Option<String>, OciError> {
        let url = if registry == "registry-1.docker.io" || registry == "docker.io" {
            format!(
                "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repository}:{scope}"
            )
        } else {
            format!(
                "{}//{registry}/token?scope=repository:{repository}:{scope}",
                self.scheme(registry)
            )
        };

        let mut req = self.client.get(&url);
        if let Some(auth) = &self.auth {
            if let (Some(user), Some(pass)) = (&auth.username, &auth.password) {
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
                req = req.header(reqwest::header::AUTHORIZATION, format!("Basic {encoded}"));
            }
        }

        let resp = req.send().await?;
        if resp.status().is_success() {
            #[derive(serde::Deserialize)]
            struct TokenResponse {
                token: Option<String>,
                access_token: Option<String>,
            }
            let t: TokenResponse = resp.json().await?;
            Ok(t.token.or(t.access_token))
        } else {
            Ok(None)
        }
    }

    fn authorized_get(
        &self,
        url: &str,
        token: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut req = self.client.get(url);
        if let Some(t) = token {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        req
    }

    fn authorized_put(
        &self,
        url: &str,
        token: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut req = self.client.put(url);
        if let Some(t) = token {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        req
    }

    /// Reconstruct an OCI layer tar.gz from a DAG layer node.
    fn reconstruct_layer(
        &self,
        layer_digest: &Digest,
    ) -> Result<(Vec<u8>, String), OciError> {
        let mut tar_buf = Vec::new();
        let mut gz =
            flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(&mut gz);
        self.walk_tree_for_layer(layer_digest, "", &mut tar)?;
        tar.finish()?;
        drop(tar);
        let _ = gz.finish()
            .map_err(|e| OciError::Other(format!("gzip finish: {e}")))?;

        let digest = format!("sha256:{}", hex::encode(Sha256::digest(&tar_buf)));
        debug!(%digest, size = tar_buf.len(), "reconstructed OCI layer");
        Ok((tar_buf, digest))
    }

    fn walk_tree_for_layer(
        &self,
        node_digest: &Digest,
        base_path: &str,
        tar: &mut tar::Builder<&mut flate2::write::GzEncoder<&mut Vec<u8>>>,
    ) -> Result<(), OciError> {
        let archived = self.store.get_archived(node_digest).map_err(|e| {
            OciError::Other(format!("read node {node_digest}: {e}"))
        })?;

        if archived.is_layer() {
            let layer_path = String::from_utf8_lossy(&archived.inline_data).to_string();
            for edge in archived.edges.iter() {
                let child = Digest(*edge);
                self.walk_tree_for_layer(&child, &layer_path, tar)?;
            }
            return Ok(());
        }

        if !archived.is_tree() {
            return Ok(());
        }

        let inline_data = archived.inline_data.as_ref();
        if !inline_data.is_empty() {
            let entries = DirectoryEntry::from_inline_bytes(inline_data);
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
                    header.set_mtime(0);
                    tar.append_data(&mut header, &entry_path, std::io::empty())?;
                } else if entry.is_symlink {
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_mode(0o777);
                    header.set_mtime(0);
                    if let Some(target) = &entry.symlink_target {
                        header.set_size(0);
                        tar.append_data(&mut header, &entry_path, std::io::empty())?;
                        // Symlink target is stored in the link name by tar::Builder.
                        // We need to use append_link instead.
                        // Re-add with correct link name
                        let mut h = tar::Header::new_gnu();
                        h.set_entry_type(tar::EntryType::Symlink);
                        h.set_mode(0o777);
                        h.set_mtime(0);
                        tar.append_link(&mut h, &entry_path, target)?;
                    }
                } else {
                    // Regular file: read blob content.
                    let blob_mmap = self.store.get(&entry.digest).map_err(|e| {
                        OciError::Other(format!("read blob {}: {e}", entry.digest.as_hex()))
                    })?;
                    let blob_node = rkyv::check_archived_root::<pullrun_store::DagNode>(&blob_mmap[..])
                        .map_err(|e| OciError::Other(format!("corrupt blob node {}: {e}", entry.digest.as_hex())))?;
                    let file_data = blob_node.inline_data.as_ref();

                    let mut header = tar::Header::new_gnu();
                    header.set_size(file_data.len() as u64);
                    header.set_entry_type(tar::EntryType::Regular);
                    let mode = if entry.mode == 0 { 0o644 } else { entry.mode };
                    header.set_mode(mode);
                    header.set_mtime(0);
                    tar.append_data(&mut header, &entry_path, file_data)?;
                }
            }
        }

        // Recurse into child trees.
        for edge in archived.edges.iter() {
            let child = Digest(*edge);
            if self.store.exists(&child) {
                // Check if child is a tree node, not a blob.
                if let Ok(child_archived) = self.store.get_archived(&child) {
                    if child_archived.is_tree() {
                        self.walk_tree_for_layer(&child, base_path, tar)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Push a single blob to the registry.
    async fn upload_blob(
        &self,
        registry: &str,
        repository: &str,
        digest: &str,
        data: &[u8],
        token: Option<&str>,
    ) -> Result<(), OciError> {
        // Monolithic upload: HEAD to check existence (optional), then PUT.
        let scheme = self.scheme(registry);
        let base_url = format!("{scheme}//{registry}/v2/{repository}/blobs/uploads/");

        // Check if blob already exists.
        let check_url = format!("{scheme}//{registry}/v2/{repository}/blobs/{digest}");
        let check_resp = self.authorized_get(&check_url, token).send().await?;
        if check_resp.status().is_success() {
            info!(%digest, "blob already exists in registry, skipping");
            return Ok(());
        }

        // Start a monolithic upload session.
        let mut session_req = self.client.post(&base_url);
        if let Some(t) = token {
            session_req = session_req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let session_resp = session_req.send().await?;

        let location = session_resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| OciError::Other("no Location header in upload session".into()))?;

        let upload_url = if location.starts_with("http") {
            location.clone()
        } else {
            // Relative URL
            format!(
                "{}//{}{}",
                self.scheme(registry),
                registry,
                location
            )
        };

        // PUT the blob data with digest query param.
        // Some registries return upload URLs with ?_state=... query
        // parameters, so we must use & instead of ? to append.
        let sep = if upload_url.contains('?') { '&' } else { '?' };
        let put_url = format!("{upload_url}{sep}digest={digest}");
        let put_resp = self
            .authorized_put(&put_url, token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(data.to_vec())
            .send()
            .await?;

        if !put_resp.status().is_success() {
            let body = put_resp.text().await.unwrap_or_default();
            return Err(OciError::Other(format!(
                "blob upload failed for {digest}: {body}"
            )));
        }

        info!(%digest, size = data.len(), "blob uploaded");
        Ok(())
    }

    /// Push a complete OCI image (config + layers + manifest).
    /// If the root node is a `ManifestList`, delegates to
    /// `push_manifest_list`.
    pub async fn push(
        &self,
        root_digest: &str,
        target_ref: &str,
    ) -> Result<(String, i64), OciError> {
        let (registry, repository, tag) = self.registry_for(target_ref);
        info!(%root_digest, %registry, %repository, %tag, "pushing DAG image");

        let token = self.get_token(&registry, &repository, "push,pull").await?;

        // Check if the root is a manifest list.
        let rd = Digest::from_hex(root_digest)
            .map_err(|e| OciError::Other(format!("invalid root digest: {e}")))?;
        let root_archived = self.store.get_archived(&rd).map_err(|e| {
            OciError::Other(format!("read node {root_digest}: {e}"))
        })?;
        if root_archived.is_manifest_list() {
            return self.push_manifest_list(&rd, target_ref).await;
        }

        if !root_archived.is_manifest() {
            return Err(OciError::InvalidManifest(format!(
                "node {root_digest} is neither a manifest nor a manifest list"
            )));
        }

        // Single-arch: push the OCI manifest and tag it.
        let (oci_manifest, total_pushed) =
            self.push_oci_manifest(&rd, &registry, &repository, token.as_deref())
                .await?;

        let manifest_json = serde_json::to_vec(&oci_manifest)?;
        let manifest_digest = format!("sha256:{}", hex::encode(Sha256::digest(&manifest_json)));
        let n_layers = oci_manifest.layers.len() as u64;

        let scheme = self.scheme(&registry);
        let manifest_url = format!("{scheme}//{registry}/v2/{repository}/manifests/{tag}");
        let resp = self
            .authorized_put(&manifest_url, token.as_deref())
            .header(reqwest::header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
            .body(manifest_json)
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OciError::Other(format!("manifest upload failed for {tag}: {body}")));
        }

        info!(%manifest_digest, layers = n_layers, bytes = total_pushed, "image pushed successfully");
        Ok((manifest_digest, total_pushed))
    }

    /// Push a single OCI manifest (layers + config + manifest blob) to
    /// the registry by its content digest.  Used internally by both
    /// `push()` and `push_manifest_list()`.
    async fn push_oci_manifest(
        &self,
        manifest_digest: &Digest,
        registry: &str,
        repository: &str,
        token: Option<&str>,
    ) -> Result<(OciManifest, i64), OciError> {
        let archived = self.store.get_archived(manifest_digest).map_err(|e| {
            OciError::Other(format!("read manifest node {manifest_digest}: {e}"))
        })?;
        if !archived.is_manifest() {
            return Err(OciError::InvalidManifest(format!(
                "node {manifest_digest} is not a manifest node"
            )));
        }

        let manifest_data: ManifestData =
            serde_json::from_slice(archived.inline_data.as_ref())?;
        // Reconstruct the OCI image config from the manifest data fields.
        let oci_config = OciImageConfig {
            created: None,
            author: None,
            architecture: manifest_data.architecture.clone(),
            os: manifest_data.os.clone(),
            config: Some(OciRuntimeConfig {
                user: None,
                exposed_ports: None,
                env: Some(manifest_data.env.clone()),
                entrypoint: Some(manifest_data.entrypoint.clone()),
                cmd: Some(manifest_data.cmd.clone()),
                volumes: None,
                working_dir: manifest_data.working_dir.clone(),
                labels: None,
                stop_signal: None,
            }),
            rootfs: OciRootFs {
                diff_ids: vec![],
                fs_type: "layers".to_string(),
            },
            history: None,
        };

        let layer_digests: Vec<Digest> = archived
            .edges
            .iter()
            .map(|e| Digest(*e))
            .collect();

        let mut oci_layers = Vec::new();
        let mut total_pushed: i64 = 0;

        for layer_digest in &layer_digests {
            info!(%layer_digest, "reconstructing OCI layer from DAG");
            let (layer_data, oci_digest) = self.reconstruct_layer(layer_digest)?;

            self.upload_blob(registry, repository, &oci_digest, &layer_data, token)
                .await?;

            total_pushed += layer_data.len() as i64;

            oci_layers.push(OciDescriptor {
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
                digest: oci_digest,
                size: layer_data.len() as u64,
                annotations: None,
                platform: None,
            });
        }

        let config_json = serde_json::to_vec(&oci_config)?;
        let config_digest = format!("sha256:{}", hex::encode(Sha256::digest(&config_json)));
        self.upload_blob(registry, repository, &config_digest, &config_json, token)
            .await?;
        total_pushed += config_json.len() as i64;

        let oci_manifest = OciManifest {
            schema_version: 2,
            media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
            config: OciDescriptor {
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                digest: config_digest,
                size: config_json.len() as u64,
                annotations: None,
                platform: None,
            },
            layers: oci_layers,
            annotations: None,
        };

        Ok((oci_manifest, total_pushed))
    }

    /// Push a manifest list and all its child manfests to the registry.
    /// Walks the DAG from the manifest list node, pushes every child
    /// platform's layers/config/manifest, then pushes the image index
    /// (manifest list) at the requested tag.
    pub async fn push_manifest_list(
        &self,
        list_digest: &Digest,
        target_ref: &str,
    ) -> Result<(String, i64), OciError> {
        let (registry, repository, tag) = self.registry_for(target_ref);

        let list_archived = self.store.get_archived(list_digest).map_err(|e| {
            OciError::Other(format!("read manifest list node {list_digest}: {e}"))
        })?;
        if !list_archived.is_manifest_list() {
            return Err(OciError::InvalidManifest(format!(
                "node {list_digest} is not a manifest list node"
            )));
        }

        let token = self.get_token(&registry, &repository, "push,pull").await?;
        let child_digests: Vec<Digest> = list_archived
            .edges
            .iter()
            .map(|e| Digest(*e))
            .collect();

        let mut total_pushed: i64 = 0;
        let mut manifests = Vec::with_capacity(child_digests.len());

        for child in &child_digests {
            // Read architecture/os from the child manifest node.
            let child_archived = self.store.get_archived(child).map_err(|e| {
                OciError::Other(format!("read child manifest {child}: {e}"))
            })?;
            let manifest_data: ManifestData =
                serde_json::from_slice(child_archived.inline_data.as_ref())?;

            // Push layers + config + manifest blob.
            let (oci_manifest, manifest_bytes) = self
                .push_oci_manifest(child, &registry, &repository, token.as_deref())
                .await?;
            total_pushed += manifest_bytes;

            // Push the platform manifest by digest.
            let manifest_json = serde_json::to_vec(&oci_manifest)?;
            let child_digest = format!("sha256:{}", hex::encode(Sha256::digest(&manifest_json)));
            let manifest_size = manifest_json.len() as i64;
            let scheme = self.scheme(&registry);
            let manifest_url = format!(
                "{scheme}//{registry}/v2/{repository}/manifests/{child_digest}"
            );
            let resp = self
                .authorized_put(&manifest_url, token.as_deref())
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/vnd.oci.image.manifest.v1+json",
                )
                .body(manifest_json)
                .send()
                .await?;

            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(OciError::Other(format!(
                    "child manifest upload failed for {child_digest}: {body}"
                )));
            }
            total_pushed += manifest_size;

            manifests.push(OciDescriptor {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                digest: child_digest,
                size: manifest_size as u64,
                annotations: None,
                platform: Some(crate::puller::OciPlatform {
                    architecture: manifest_data.architecture,
                    os: manifest_data.os,
                }),
            });
        }

        // Build and push the image index (manifest list).
        #[derive(Serialize)]
        struct ImageIndex {
            #[serde(rename = "schemaVersion")]
            schema_version: u32,
            #[serde(rename = "mediaType")]
            media_type: String,
            manifests: Vec<OciDescriptor>,
        }

        let index = ImageIndex {
            schema_version: 2,
            media_type: "application/vnd.oci.image.index.v1+json".to_string(),
            manifests,
        };

        let index_json = serde_json::to_vec(&index)?;
        let index_digest = format!("sha256:{}", hex::encode(Sha256::digest(&index_json)));

        let scheme = self.scheme(&registry);
        let index_url = format!("{scheme}//{registry}/v2/{repository}/manifests/{tag}");
        let resp = self
            .authorized_put(&index_url, token.as_deref())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/vnd.oci.image.index.v1+json",
            )
            .body(index_json)
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(OciError::Other(format!(
                "manifest list upload failed for {tag}: {body}"
            )));
        }

        info!(
            %index_digest,
            children = child_digests.len(),
            bytes = total_pushed,
            "manifest list pushed successfully"
        );

        Ok((index_digest, total_pushed))
    }
}
