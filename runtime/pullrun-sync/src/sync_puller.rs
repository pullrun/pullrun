// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use futures::StreamExt;
use pullrun_oci::{OciAuth, OciImageConfig, OciManifest, OciPuller, PulledImage};
use pullrun_store::{Digest, MmapStore};
use tracing::{debug, info, warn};

use crate::block_sync::BlockSyncClient;
use crate::gossip::PeerBloomCache;
use crate::proto::GetBlobsRequest;

/// A puller that checks the local store and peer nodes before falling
/// back to the upstream OCI registry for each blob.
///
/// Uses `PeerBloomCache` (populated by `BloomGossip`) to identify
/// which peers likely have each blob, avoiding full peer queries.
pub struct SyncPuller {
    store: Arc<MmapStore>,
    oci_puller: OciPuller,
    bloom_cache: Option<PeerBloomCache>,
}

impl SyncPuller {
    pub fn new(
        store: Arc<MmapStore>,
        auth: Option<OciAuth>,
        insecure_registries: std::collections::HashSet<String>,
        bloom_cache: Option<PeerBloomCache>,
    ) -> Self {
        let oci_puller = OciPuller::with_insecure_registries(auth, insecure_registries);
        Self {
            store,
            oci_puller,
            bloom_cache,
        }
    }

    /// Pull an image using peer-to-peer block sync when possible.
    /// Falls back to OciPuller entirely if no peers are available.
    pub async fn pull(
        &self,
        image_ref: &str,
        explicit_registry: Option<&str>,
        platform: Option<&str>,
    ) -> Result<PulledImage, pullrun_oci::OciError> {
        let bloom_cache = match &self.bloom_cache {
            Some(c) => c,
            None => {
                info!("block sync disabled; using standard OCI pull");
                return self
                    .oci_puller
                    .pull_with_platform(image_ref, explicit_registry, platform)
                    .await;
            }
        };

        let (manifest, config, config_digest, registry, repository, token) = self
            .resolve_image(image_ref, explicit_registry, platform)
            .await?;

        let peer_count = bloom_cache.peer_count().await;
        info!(
            image_ref,
            layers = manifest.layers.len(),
            known_peers = peer_count,
            "resolved manifest; fetching blobs via sync"
        );

        let mut layer_blobs = Vec::with_capacity(manifest.layers.len());
        for layer in &manifest.layers {
            let blob_data = self
                .fetch_blob_with_sync(
                    bloom_cache,
                    &layer.digest,
                    &registry,
                    &repository,
                    token.as_deref(),
                )
                .await?;
            let d = Digest::from_hex(&layer.digest)
                .map_err(|e| pullrun_oci::OciError::Other(format!("invalid layer digest: {e}")))?;
            layer_blobs.push((d, blob_data, layer.media_type.clone()));
        }

        let cd = Digest::from_hex(&config_digest)
            .map_err(|e| pullrun_oci::OciError::Other(format!("invalid config digest: {e}")))?;

        info!(image_ref, layers = layer_blobs.len(), "all layers synced");
        Ok(PulledImage {
            manifest,
            config,
            config_digest: cd,
            layer_blobs,
        })
    }

    async fn resolve_image(
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
        pullrun_oci::OciError,
    > {
        self.oci_puller
            .resolve_image(image_ref, explicit_registry, platform)
            .await
    }

    async fn fetch_blob_with_sync(
        &self,
        bloom_cache: &PeerBloomCache,
        digest: &str,
        registry: &str,
        repository: &str,
        token: Option<&str>,
    ) -> Result<Vec<u8>, pullrun_oci::OciError> {
        // Parse the hex portion (Digest::from_hex strips "sha256:" prefix).
        let d = Digest::from_hex(digest)
            .map_err(|e| pullrun_oci::OciError::Other(format!("invalid digest {digest}: {e}")))?;

        // 1. Check local store.
        if self.store.exists(&d) {
            debug!(%digest, "blob found locally");
            match self.store.get_blob(&d) {
                Ok(mmap) => return Ok(mmap[..].to_vec()),
                Err(_) => debug!(%digest, "local blob read failed, trying peers"),
            }
        }

        // 2. Check bloom cache for candidate peers.
        let candidates = bloom_cache.find_peers_with_blob(digest).await;
        if !candidates.is_empty() {
            debug!(%digest, candidates = candidates.len(), "found peer candidates via bloom cache");
            for (_node_id, sync_addr) in &candidates {
                if let Some(data) = self.try_fetch_from_peer(sync_addr, digest).await {
                    debug!(%digest, addr = %sync_addr, size = data.len(), "blob fetched from peer");
                    let actual = Digest::compute(&data);
                    if actual != d {
                        warn!(%digest, addr = %sync_addr, "peer returned blob with wrong digest, discarding");
                        continue;
                    }
                    let _ = self.store.put_blob_blocking(&d, &data);
                    return Ok(data);
                }
            }
        }

        // 3. Fall back to upstream registry.
        debug!(%digest, "blob not on any peer; falling back to registry");
        let data = self
            .oci_puller
            .fetch_blob_by_digest(registry, repository, digest, token)
            .await?;
        let _ = self.store.put_blob_blocking(&d, &data);
        Ok(data)
    }

    async fn try_fetch_from_peer(&self, sync_addr: &str, digest: &str) -> Option<Vec<u8>> {
        const PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

        let endpoint = format!("http://{}", sync_addr);
        let mut client =
            match tokio::time::timeout(PEER_TIMEOUT, BlockSyncClient::connect(endpoint)).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    debug!(%sync_addr, error = %e, "failed to connect to peer");
                    return None;
                }
                Err(_) => {
                    debug!(%sync_addr, "peer connect timed out");
                    return None;
                }
            };

        let request = tonic::Request::new(GetBlobsRequest {
            digests: vec![digest.to_string()],
        });

        match tokio::time::timeout(PEER_TIMEOUT, client.get_blobs(request)).await {
            Ok(Ok(response)) => {
                let mut stream = response.into_inner();
                let mut collected = Vec::new();
                while let Some(chunk_result) = stream.next().await {
                    match chunk_result {
                        Ok(chunk) => {
                            collected.extend_from_slice(&chunk.data);
                            if chunk.is_final {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(%digest, %sync_addr, error = %e, "peer stream error");
                            return None;
                        }
                    }
                }
                if !collected.is_empty() {
                    Some(collected)
                } else {
                    None
                }
            }
            Ok(Err(e)) => {
                debug!(%digest, %sync_addr, error = %e, "peer get_blobs failed");
                None
            }
            Err(_) => {
                debug!(%digest, %sync_addr, "peer get_blobs timed out");
                None
            }
        }
    }
}
