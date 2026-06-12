// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::io::Read;

use base64::Engine;
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info};

use pullrun_store::Digest;

type FetchResult<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(Vec<u8>, Vec<OciDescriptor>), OciError>>
            + Send
            + 'a,
    >,
>;

#[derive(Debug, Error)]
pub enum OciError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Store error: {0}")]
    Store(#[from] pullrun_store::StoreError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Invalid manifest: {0}")]
    InvalidManifest(String),
    #[error("Layer not found: {0}")]
    LayerNotFound(String),
    #[error("Unsupported media type: {0}")]
    UnsupportedMediaType(String),
    #[error("Other: {0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciManifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub config: OciDescriptor,
    pub layers: Vec<OciDescriptor>,
    pub annotations: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciImageConfig {
    pub created: Option<String>,
    pub author: Option<String>,
    pub architecture: String,
    pub os: String,
    pub config: Option<OciRuntimeConfig>,
    pub rootfs: OciRootFs,
    pub history: Option<Vec<OciHistoryEntry>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciDescriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    pub annotations: Option<HashMap<String, String>>,
    /// OCI image indexes put the platform on each child
    /// descriptor. `None` for layer / config descriptors in
    /// a flat manifest.
    pub platform: Option<OciPlatform>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciPlatform {
    pub architecture: String,
    pub os: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciRuntimeConfig {
    #[serde(rename = "User")]
    pub user: Option<String>,
    #[serde(rename = "ExposedPorts")]
    pub exposed_ports: Option<HashMap<String, serde_json::Value>>,
    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,
    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,
    #[serde(rename = "Volumes")]
    pub volumes: Option<HashMap<String, serde_json::Value>>,
    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,
    #[serde(rename = "Labels")]
    pub labels: Option<HashMap<String, String>>,
    #[serde(rename = "StopSignal")]
    pub stop_signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciRootFs {
    #[serde(rename = "type")]
    pub fs_type: String,
    #[serde(rename = "diff_ids")]
    pub diff_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciHistoryEntry {
    pub created: Option<String>,
    #[serde(rename = "created_by")]
    pub created_by: Option<String>,
    pub comment: Option<String>,
    #[serde(rename = "empty_layer")]
    pub empty_layer: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct OciAuth {
    pub username: Option<String>,
    pub password: Option<String>,
    pub registry_token: Option<String>,
}

#[derive(Debug)]
pub struct PulledImage {
    pub manifest: OciManifest,
    pub config: OciImageConfig,
    pub config_digest: Digest,
    pub layer_blobs: Vec<(Digest, Vec<u8>)>,
}

/// The result of pulling all platforms from a multi-arch image index.
/// Each `images` entry corresponds to one child manifest in the list.
#[derive(Debug)]
pub struct PulledImageList {
    pub images: Vec<PulledImage>,
    pub list_bytes: Vec<u8>,
    pub child_descriptors: Vec<OciDescriptor>,
}

pub struct OciPuller {
    client: reqwest::Client,
    auth: Option<OciAuth>,
    /// Registries that should be reached over plain HTTP
    /// (no TLS). Defaults to empty. Useful for local
    /// testing against `registry:2` containers or
    /// self-hosted registries without a TLS cert.
    insecure_registries: HashSet<String>,
}

impl OciPuller {
    pub fn new(auth: Option<OciAuth>) -> Self {
        Self::with_insecure_registries(auth, HashSet::new())
    }

    pub fn with_insecure_registries(
        auth: Option<OciAuth>,
        insecure_registries: HashSet<String>,
    ) -> Self {
        // Build the reqwest client with a few defaults that
        // matter for OCI registries on macOS:
        //   - NO `.gzip(true)`: Docker Hub's CDN double-encodes
        //     gzip, causing "error decoding response body".
        //     We disable gzip(true) and instead manually check
        //     `Content-Encoding: gzip` via `decode_body()`.
        //   - `Accept-Encoding: identity`: tell servers not to
        //     gzip-encode; the manual gzip fallback handles
        //     registries that ignore this header.
        //   - `.redirect(reqwest::redirect::Policy::limited(5))`:
        //     follow up to 5 redirects (some registries
        //     redirect to S3 for blob downloads).
        //   - `.timeout(Duration::from_secs(60))`: don't hang
        //     forever on a misbehaving registry.
        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(
            reqwest::header::ACCEPT_ENCODING,
            reqwest::header::HeaderValue::from_static("identity"),
        );
        let client = reqwest::Client::builder()
            .default_headers(default_headers)
            .redirect(reqwest::redirect::Policy::limited(5))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            auth,
            insecure_registries,
        }
    }

    pub fn with_client(client: reqwest::Client, auth: Option<OciAuth>) -> Self {
        Self {
            client,
            auth,
            insecure_registries: HashSet::new(),
        }
    }

    /// `https://` for normal registries; `http://` for
    /// registries in the `insecure_registries` allowlist.
    /// The OCI distribution spec requires HTTPS, but
    /// local development against `registry:2` or
    /// self-hosted registries is significantly easier
    /// over plain HTTP.
    fn scheme(&self, registry: &str) -> &'static str {
        if self.insecure_registries.contains(registry) {
            "http:"
        } else {
            "https:"
        }
    }

    fn registry_for(&self, image_ref: &str, explicit_registry: Option<&str>) -> String {
        if let Some(reg) = explicit_registry {
            return reg.to_string();
        }
        if image_ref.contains('/') {
            let parts: Vec<&str> = image_ref.split('/').collect();
            let first = parts[0];
            if first.contains('.') || first.contains(':') || first == "localhost" {
                return first.to_string();
            }
        }
        "registry-1.docker.io".to_string()
    }

    fn image_parts(&self, image_ref: &str) -> (String, String) {
        let parts: Vec<&str> = image_ref.split('/').collect();
        let after_registry = if parts.len() > 1
            && (parts[0].contains('.') || parts[0].contains(':') || parts[0] == "localhost")
        {
            parts[1..].join("/")
        } else {
            image_ref.to_string()
        };

        let segments: Vec<&str> = after_registry.splitn(2, ':').collect();
        let name = if segments[0].contains('/') {
            segments[0].to_string()
        } else {
            format!("library/{}", segments[0])
        };
        let tag = segments.get(1).unwrap_or(&"latest").to_string();
        (name, tag)
    }

    async fn get_token(
        &self,
        registry: &str,
        repository: &str,
    ) -> Result<Option<String>, OciError> {
        let url = if registry == "registry-1.docker.io" || registry == "docker.io" {
            format!("https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repository}:pull")
        } else {
            format!(
                "{}//{registry}/token?scope=repository:{repository}:pull",
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
            #[derive(Deserialize)]
            struct TokenResponse {
                token: Option<String>,
                access_token: Option<String>,
            }
            let t: TokenResponse = resp.json().await?;
            Ok(t.token.or(t.access_token))
        } else if resp.status().as_u16() == 404 {
            // Registry has no auth endpoint — proceed without a token.
            Ok(None)
        } else {
            let status = resp.status();
            Err(OciError::Other(format!(
                "registry authentication failed for {registry}/{repository}: HTTP {status}"
            )))
        }
    }

    pub async fn pull(
        &self,
        image_ref: &str,
        explicit_registry: Option<&str>,
    ) -> Result<PulledImage, OciError> {
        self.pull_with_platform(image_ref, explicit_registry, None)
            .await
    }

    /// Pull an OCI image, optionally selecting a specific platform.
    ///
    /// `platform` is a string like `"linux/amd64"` or `"linux/arm64"`.
    /// When `None`, the host's native architecture is used (equivalent
    /// to calling `pull()`).
    pub async fn pull_with_platform(
        &self,
        image_ref: &str,
        explicit_registry: Option<&str>,
        platform: Option<&str>,
    ) -> Result<PulledImage, OciError> {
        let registry = self.registry_for(image_ref, explicit_registry);
        let (repository, tag) = self.image_parts(image_ref);
        let token = self.get_token(&registry, &repository).await?;

        info!(image_ref, %registry, %repository, %tag, platform = ?platform, "pulling OCI image");

        let manifest = self
            .fetch_manifest_with_platform(&registry, &repository, &tag, token.as_deref(), platform)
            .await?;
        let config = self
            .fetch_config(
                &registry,
                &repository,
                &manifest.config.digest,
                token.as_deref(),
            )
            .await?;

        let config_digest = Digest::from_hex(&manifest.config.digest)
            .map_err(|e| OciError::Other(format!("invalid config digest: {e}")))?;
        let _config_data = serde_json::to_vec(&config)?;

        let mut layer_blobs = Vec::new();
        for layer in &manifest.layers {
            let blob = self
                .fetch_blob(&registry, &repository, &layer.digest, token.as_deref())
                .await?;
            debug!(digest = %layer.digest, size = blob.len(), "layer downloaded");
            let layer_d = Digest::from_hex(&layer.digest)
                .map_err(|e| OciError::Other(format!("invalid layer digest: {e}")))?;
            layer_blobs.push((layer_d, blob));
        }

        info!(
            %config_digest,
            layers = layer_blobs.len(),
            "image pulled successfully"
        );

        Ok(PulledImage {
            manifest,
            config,
            config_digest,
            layer_blobs,
        })
    }

    /// Pull all platforms from a multi-arch image index.
    /// Fetches every child manifest, its config, and its layer blobs.
    pub async fn pull_all(
        &self,
        image_ref: &str,
        explicit_registry: Option<&str>,
    ) -> Result<PulledImageList, OciError> {
        let registry = self.registry_for(image_ref, explicit_registry);
        let (repository, tag) = self.image_parts(image_ref);
        let token = self.get_token(&registry, &repository).await?;

        info!(image_ref, %registry, %repository, %tag, "pulling all platforms");

        let (list_bytes, children) = self
            .fetch_raw_list(&registry, &repository, &tag, token.as_deref())
            .await?;

        let mut images = Vec::with_capacity(children.len());
        for child in &children {
            info!(
                child_digest = %child.digest,
                platform = ?child.platform,
                "pulling child manifest"
            );
            let manifest = self
                .fetch_manifest_with_platform(
                    &registry,
                    &repository,
                    &child.digest,
                    token.as_deref(),
                    None,
                )
                .await?;
            let config = self
                .fetch_config(
                    &registry,
                    &repository,
                    &manifest.config.digest,
                    token.as_deref(),
                )
                .await?;
            let config_digest = Digest::from_hex(&manifest.config.digest)
                .map_err(|e| OciError::Other(format!("invalid config digest: {e}")))?;

            let mut layer_blobs = Vec::new();
            for layer in &manifest.layers {
                let blob = self
                    .fetch_blob(&registry, &repository, &layer.digest, token.as_deref())
                    .await?;
                let layer_d = Digest::from_hex(&layer.digest)
                    .map_err(|e| OciError::Other(format!("invalid layer digest: {e}")))?;
                layer_blobs.push((layer_d, blob));
            }

            images.push(PulledImage {
                manifest,
                config,
                config_digest,
                layer_blobs,
            });
        }

        info!(
            platform_count = images.len(),
            "all platforms pulled successfully"
        );

        Ok(PulledImageList {
            images,
            list_bytes,
            child_descriptors: children,
        })
    }

    /// Fetch the raw manifest list for a reference, returning the
    /// raw bytes and the child descriptors. Used by `pull_all()`.
    fn fetch_raw_list<'a>(
        &'a self,
        registry: &'a str,
        repository: &'a str,
        reference: &'a str,
        token: Option<&'a str>,
    ) -> FetchResult<'a> {
        Box::pin(async move {
            let url = manifest_url(self, registry, repository, reference);
            let mut req = self
                .client
                .get(&url)
                .header("Accept", "application/vnd.oci.image.index.v1+json")
                .header(
                    "Accept",
                    "application/vnd.docker.distribution.manifest.list.v2+json",
                )
                .header("Accept", "application/vnd.oci.image.manifest.v1+json")
                .header(
                    "Accept",
                    "application/vnd.docker.distribution.manifest.v2+json",
                );

            if let Some(t) = token {
                req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
            }

            let resp = req.send().await?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(OciError::InvalidManifest(format!(
                    "manifest list fetch failed: {body}"
                )));
            }

            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let content_encoding = resp
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let raw = resp.bytes().await?.to_vec();
            let bytes = decode_body(&content_encoding, raw)?;

            let is_list = content_type.contains("manifest.list")
                || content_type.contains("image.index")
                || (serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map(|v| v.get("manifests").is_some())
                    .unwrap_or(false)
                    && serde_json::from_slice::<OciManifest>(&bytes).is_err());

            if !is_list {
                return Err(OciError::InvalidManifest(
                    "reference does not point to a manifest list".into(),
                ));
            }

            #[derive(Deserialize)]
            struct ManifestList {
                #[serde(rename = "mediaType")]
                _media_type: String,
                manifests: Vec<OciDescriptor>,
            }

            let list: ManifestList = serde_json::from_slice(&bytes)
                .map_err(|e| OciError::InvalidManifest(format!("manifest list parse: {e}")))?;

            Ok((bytes, list.manifests))
        })
    }

    fn fetch_manifest_with_platform<'a>(
        &'a self,
        registry: &'a str,
        repository: &'a str,
        reference: &'a str,
        token: Option<&'a str>,
        platform: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OciManifest, OciError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let url = manifest_url(self, registry, repository, reference);
            let mut req = self
                .client
                .get(&url)
                .header("Accept", "application/vnd.oci.image.manifest.v1+json")
                .header("Accept", "application/vnd.oci.image.index.v1+json")
                .header(
                    "Accept",
                    "application/vnd.docker.distribution.manifest.v2+json",
                )
                .header(
                    "Accept",
                    "application/vnd.docker.distribution.manifest.list.v2+json",
                );

            if let Some(t) = token {
                req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
            }

            let resp = req.send().await?;
            if !resp.status().is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(OciError::InvalidManifest(format!(
                    "manifest fetch failed: {body}"
                )));
            }

            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let content_encoding = resp
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let raw = resp.bytes().await?.to_vec();
            let bytes = decode_body(&content_encoding, raw)?;

            let is_manifest_list = content_type.contains("manifest.list")
                || content_type.contains("image.index")
                || (serde_json::from_slice::<serde_json::Value>(&bytes)
                    .map(|v| v.get("manifests").is_some())
                    .unwrap_or(false)
                    && serde_json::from_slice::<OciManifest>(&bytes).is_err());

            if is_manifest_list {
                #[derive(Deserialize)]
                struct ManifestList {
                    #[serde(rename = "mediaType")]
                    _media_type: String,
                    manifests: Vec<OciDescriptor>,
                }

                let list: ManifestList = serde_json::from_slice(&bytes)
                    .map_err(|e| OciError::InvalidManifest(format!("manifest list parse: {e}")))?;

                // Resolve the target platform: explicit override, or host native.
                let (target_arch, target_os) = if let Some(p) = platform {
                    parse_platform(p)
                } else {
                    (current_arch(), "linux")
                };

                // Pick the child manifest for our platform.
                // Multi-arch indexes put a `platform` field
                // on each child descriptor; the old code
                // looked for a `platform` annotation key
                // (which Docker's old v2s2 list emitted
                // serialized into annotations). The OCI v1
                // image index format puts `platform` as a
                // top-level field on the descriptor, so we
                // have to read it from there.
                let child = list
                    .manifests
                    .iter()
                    .find(|m| {
                        m.platform
                            .as_ref()
                            .map(|p| {
                                p.os.eq_ignore_ascii_case(target_os)
                                    && p.architecture.eq_ignore_ascii_case(target_arch)
                            })
                            .unwrap_or(false)
                    })
                    .or_else(|| {
                        // Fall back to the first non-attestation
                        // child (attestations are co-published
                        // with the image in Docker buildx and
                        // have `unknown`/`unknown` platform).
                        list.manifests.iter().find(|m| {
                            m.platform
                                .as_ref()
                                .map(|p| p.os != "unknown" && p.architecture != "unknown")
                                .unwrap_or(true)
                        })
                    })
                    .or_else(|| list.manifests.first())
                    .ok_or_else(|| {
                        OciError::InvalidManifest("no child manifest for current platform".into())
                    })?;

                info!(
                    child_digest = %child.digest,
                    "following manifest list child for platform {target_arch}/{target_os}"
                );

                return self
                    .fetch_manifest_with_platform(
                        registry,
                        repository,
                        &child.digest,
                        token,
                        platform,
                    )
                    .await;
            }

            let manifest: OciManifest = serde_json::from_slice(&bytes)
                .map_err(|e| OciError::InvalidManifest(format!("manifest parse: {e}")))?;
            Ok(manifest)
        })
    }

    async fn fetch_config(
        &self,
        registry: &str,
        repository: &str,
        digest: &str,
        token: Option<&str>,
    ) -> Result<OciImageConfig, OciError> {
        let url = blob_url(self, registry, repository, digest);
        let mut req = self.client.get(&url);

        if let Some(t) = token {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
        }

        let resp = req.send().await?;
        let content_encoding = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let raw = resp.bytes().await?.to_vec();
        let data = decode_body(&content_encoding, raw)?;
        let config: OciImageConfig = serde_json::from_slice(&data)
            .map_err(|e| OciError::InvalidManifest(format!("config parse: {e}")))?;
        Ok(config)
    }

    async fn fetch_blob(
        &self,
        registry: &str,
        repository: &str,
        digest: &str,
        token: Option<&str>,
    ) -> Result<Vec<u8>, OciError> {
        let url = blob_url(self, registry, repository, digest);
        let mut req = self.client.get(&url);

        if let Some(t) = token {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
        }

        let resp = req.send().await?;
        let content_encoding = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let raw = resp.bytes().await?.to_vec();
        let data = decode_body(&content_encoding, raw)?;
        Ok(data)
    }

    /// Resolve image reference and return manifest + config + registry
    /// metadata, without fetching layer blobs. Returns:
    ///   (manifest, config, config_digest, registry, repository, token)
    pub async fn resolve_image(
        &self,
        image_ref: &str,
        explicit_registry: Option<&str>,
        platform: Option<&str>,
    ) -> Result<
        (
            OciManifest,
            OciImageConfig,
            String,
            String,
            String,
            Option<String>,
        ),
        OciError,
    > {
        let registry = self.registry_for(image_ref, explicit_registry);
        let (repository, tag) = self.image_parts(image_ref);
        let token = self.get_token(&registry, &repository).await?;

        let manifest = self
            .fetch_manifest_with_platform(&registry, &repository, &tag, token.as_deref(), platform)
            .await?;
        let config = self
            .fetch_config(
                &registry,
                &repository,
                &manifest.config.digest,
                token.as_deref(),
            )
            .await?;
        let config_digest = manifest.config.digest.clone();

        Ok((manifest, config, config_digest, registry, repository, token))
    }

    /// Fetch a single blob from the registry by digest.
    pub async fn fetch_blob_by_digest(
        &self,
        registry: &str,
        repository: &str,
        digest: &str,
        token: Option<&str>,
    ) -> Result<Vec<u8>, OciError> {
        self.fetch_blob(registry, repository, digest, token).await
    }
}

fn decode_body(encoding: &str, data: Vec<u8>) -> Result<Vec<u8>, OciError> {
    if encoding.to_lowercase().contains("gzip") {
        let mut decoder = GzDecoder::new(&data[..]);
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded)?;
        Ok(decoded)
    } else {
        Ok(data)
    }
}

fn manifest_url(puller: &OciPuller, registry: &str, repository: &str, reference: &str) -> String {
    if registry == "registry-1.docker.io" || registry == "docker.io" {
        format!("https://index.docker.io/v2/{repository}/manifests/{reference}")
    } else {
        format!(
            "{}//{registry}/v2/{repository}/manifests/{reference}",
            puller.scheme(registry)
        )
    }
}

fn blob_url(puller: &OciPuller, registry: &str, repository: &str, digest: &str) -> String {
    if registry == "registry-1.docker.io" || registry == "docker.io" {
        format!("https://index.docker.io/v2/{repository}/blobs/{digest}")
    } else {
        format!(
            "{}//{registry}/v2/{repository}/blobs/{digest}",
            puller.scheme(registry)
        )
    }
}

pub fn current_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "arm",
        "powerpc64" => "ppc64le",
        "s390x" => "s390x",
        other => other,
    }
}

/// Parse a platform string like `"linux/amd64"` or `"linux/arm64"` into
/// `(architecture, os)`. Defaults to `("amd64", "linux")` on parse failure.
pub fn parse_platform(platform: &str) -> (&str, &str) {
    let mut parts = platform.splitn(2, '/');
    let arch = parts.next().unwrap_or("amd64");
    let os = parts.next().unwrap_or("linux");
    (arch, os)
}
