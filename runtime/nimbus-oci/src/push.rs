use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine;
use sha2::{Digest as Sha256Digest, Sha256};
use tracing::{debug, info};

use nimbus_store::MmapStore;

use crate::converter::{DirectoryEntry, ManifestData};
use crate::puller::{OciAuth, OciDescriptor, OciError, OciImageConfig, OciManifest};

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
        layer_digest: &str,
    ) -> Result<(Vec<u8>, String), OciError> {
        let mut tar_buf = Vec::new();
        {
            // Scope ensures `tar` (which borrows `gz`) is dropped before
            // we try_finish `gz`.
            let mut gz =
                flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::default());
            {
                let mut tar = tar::Builder::new(&mut gz);
                self.walk_tree_for_layer(layer_digest, "", &mut tar)?;
                tar.finish()?;
            }
            gz.try_finish()?;
        }

        let digest = format!("sha256:{}", hex::encode(Sha256::digest(&tar_buf)));
        debug!(%digest, size = tar_buf.len(), "reconstructed OCI layer");
        Ok((tar_buf, digest))
    }

    fn walk_tree_for_layer(
        &self,
        node_digest: &str,
        base_path: &str,
        tar: &mut tar::Builder<&mut flate2::write::GzEncoder<&mut Vec<u8>>>,
    ) -> Result<(), OciError> {
        let d: String = node_digest.to_string();
        let archived = self.store.get_archived(&d).map_err(|e| {
            OciError::Other(format!("read node {node_digest}: {e}"))
        })?;

        if archived.is_layer() {
            let layer_path = String::from_utf8_lossy(&archived.inline_data).to_string();
            for edge in archived.edges.iter() {
                let child = edge.as_str().to_string();
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
                        drop(header);
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
                        OciError::Other(format!("read blob {}: {e}", entry.digest))
                    })?;
                    let blob_node = unsafe {
                        rkyv::archived_root::<nimbus_store::DagNode>(&blob_mmap[..])
                    };
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
            let child: String = edge.as_str().to_string();
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
        let check_resp = self.authorized_get(&check_url, token.clone()).send().await?;
        if check_resp.status().is_success() {
            info!(%digest, "blob already exists in registry, skipping");
            return Ok(());
        }

        // Start a monolithic upload session.
        let session_resp = self.client.post(&base_url).header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token.unwrap_or("")),
        ).send().await?;

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
        let put_url = format!("{upload_url}?digest={digest}");
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
    pub async fn push(
        &self,
        root_digest: &str,
        target_ref: &str,
    ) -> Result<(String, i64), OciError> {
        let (registry, repository, tag) = self.registry_for(target_ref);
        info!(
            root_digest = %root_digest,
            %registry,
            %repository,
            %tag,
            "pushing DAG image"
        );

        // Get auth token with push scope.
        let token = self.get_token(&registry, &repository, "push,pull").await?;

        // Read the manifest node.
        let rd = root_digest.to_string();
        let manifest_archived = self.store.get_archived(&rd).map_err(|e| {
            OciError::Other(format!("read manifest node {root_digest}: {e}"))
        })?;
        if !manifest_archived.is_manifest() {
            return Err(OciError::InvalidManifest(format!(
                "node {root_digest} is not a manifest node"
            )));
        }

        // Parse ManifestData from inline_data.
        let manifest_data: ManifestData =
            serde_json::from_slice(manifest_archived.inline_data.as_ref())?;

        // Parse the OCI image config from the stored JSON.
        let oci_config: OciImageConfig =
            serde_json::from_str(&manifest_data.config_json)?;

        // Reconstruct each layer as OCI tar.gz, tracking digests and sizes.
        let layer_digests: Vec<String> = manifest_archived
            .edges
            .iter()
            .map(|e| e.as_str().to_string())
            .collect();

        let mut oci_layers: Vec<OciDescriptor> = Vec::new();
        let mut total_pushed: i64 = 0;
        let mut oci_layer_digests: Vec<String> = Vec::new();

        for layer_digest in &layer_digests {
            info!(%layer_digest, "reconstructing OCI layer from DAG");
            let (layer_data, oci_digest) = self.reconstruct_layer(layer_digest)?;

            // Upload the layer blob.
            self.upload_blob(
                &registry,
                &repository,
                &oci_digest,
                &layer_data,
                token.as_deref(),
            )
            .await?;

            total_pushed += layer_data.len() as i64;
            oci_layer_digests.push(oci_digest.clone());

            oci_layers.push(OciDescriptor {
                media_type: "application/vnd.oci.image.layer.v1.tar+gzip".to_string(),
                digest: oci_digest,
                size: layer_data.len() as u64,
                annotations: None,
                platform: None,
            });
        }

        // Build and upload the config blob.
        let config_json = serde_json::to_vec(&oci_config)?;
        let config_digest = format!("sha256:{}", hex::encode(Sha256::digest(&config_json)));
        self.upload_blob(
            &registry,
            &repository,
            &config_digest,
            &config_json,
            token.as_deref(),
        )
        .await?;
        total_pushed += config_json.len() as i64;
        let n_layers = oci_layers.len() as u64;

        // Build and upload the manifest.
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

        let manifest_json = serde_json::to_vec(&oci_manifest)?;
        let manifest_digest = format!("sha256:{}", hex::encode(Sha256::digest(&manifest_json)));

        // Upload manifest by tag reference.
        let scheme = self.scheme(&registry);
        let manifest_url = format!(
            "{scheme}//{registry}/v2/{repository}/manifests/{tag}",
        );
        let manifest_resp = self
            .authorized_put(&manifest_url, token.as_deref())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/vnd.oci.image.manifest.v1+json",
            )
            .body(manifest_json)
            .send()
            .await?;

        if !manifest_resp.status().is_success() {
            let body = manifest_resp.text().await.unwrap_or_default();
            return Err(OciError::Other(format!(
                "manifest upload failed for {tag}: {body}"
            )));
        }

        info!(
            %manifest_digest,
            layers = n_layers,
            bytes = total_pushed,
            "image pushed successfully"
        );

        Ok((manifest_digest, total_pushed))
    }
}
