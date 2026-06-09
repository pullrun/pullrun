use std::sync::Arc;

use futures::StreamExt;
use nimbus_oci::{OciAuth, OciImageConfig, OciManifest, OciPuller, PulledImage};
use nimbus_store::MmapStore;
use tonic::transport::Endpoint;
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
    ) -> Result<PulledImage, nimbus_oci::OciError> {
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

        let needed_digests: Vec<String> = manifest
            .layers
            .iter()
            .map(|l| l.digest.clone())
            .collect();

        let mut layer_blobs = Vec::with_capacity(needed_digests.len());
        for digest in &needed_digests {
            let blob_data = self
                .fetch_blob_with_sync(bloom_cache, digest, &registry, &repository, token.as_deref())
                .await?;
            layer_blobs.push((digest.clone(), blob_data));
        }

        info!(image_ref, layers = layer_blobs.len(), "all layers synced");
        Ok(PulledImage { manifest, config, config_digest, layer_blobs })
    }

    async fn resolve_image(
        &self,
        image_ref: &str,
        explicit_registry: Option<&str>,
        platform: Option<&str>,
    ) -> Result<(OciManifest, OciImageConfig, String, String, String, Option<String>), nimbus_oci::OciError>
    {
        self.oci_puller.resolve_image(image_ref, explicit_registry, platform).await
    }

    async fn fetch_blob_with_sync(
        &self,
        bloom_cache: &PeerBloomCache,
        digest: &str,
        registry: &str,
        repository: &str,
        token: Option<&str>,
    ) -> Result<Vec<u8>, nimbus_oci::OciError> {
        let d = digest.to_string();

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
                    let _ = self.store.put_blob_blocking(&d, &data);
                    return Ok(data);
                }
            }
        }

        // 3. Fall back to upstream registry.
        debug!(%digest, "blob not on any peer; falling back to registry");
        let d2 = digest.to_string();
        let data = self.oci_puller.fetch_blob_by_digest(registry, repository, digest, token).await?;
        let _ = self.store.put_blob_blocking(&d2, &data);
        Ok(data)
    }

    async fn try_fetch_from_peer(&self, sync_addr: &str, digest: &str) -> Option<Vec<u8>> {
        let endpoint = format!("http://{}", sync_addr);
        let mut client = match BlockSyncClient::connect(endpoint).await {
            Ok(c) => c,
            Err(e) => {
                debug!(%sync_addr, error = %e, "failed to connect to peer");
                return None;
            }
        };

        let request = tonic::Request::new(GetBlobsRequest {
            digests: vec![digest.to_string()],
        });

        match client.get_blobs(request).await {
            Ok(response) => {
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
                if !collected.is_empty() { Some(collected) } else { None }
            }
            Err(e) => {
                debug!(%digest, %sync_addr, error = %e, "peer get_blobs failed");
                None
            }
        }
    }
}
