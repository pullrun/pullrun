use std::sync::Arc;

use nimbus_oci::{OciAuth, OciImageConfig, OciManifest, OciPuller, PulledImage};
use nimbus_store::MmapStore;
use tracing::{debug, info, warn};

use crate::block_sync::BlockSyncClient;
use crate::proto::GetBlobsRequest;

/// A puller that checks the local store and peer nodes before falling
/// back to the upstream OCI registry for each blob.
pub struct SyncPuller {
    store: Arc<MmapStore>,
    oci_puller: OciPuller,
    block_sync_client: Option<BlockSyncClient>,
}

impl SyncPuller {
    pub fn new(
        store: Arc<MmapStore>,
        auth: Option<OciAuth>,
        insecure_registries: std::collections::HashSet<String>,
        block_sync_client: Option<BlockSyncClient>,
    ) -> Self {
        let oci_puller = OciPuller::with_insecure_registries(auth, insecure_registries);
        Self {
            store,
            oci_puller,
            block_sync_client,
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
        let client = match &self.block_sync_client {
            Some(c) => c.clone(),
            None => {
                info!("block sync disabled; using standard OCI pull");
                return self
                    .oci_puller
                    .pull_with_platform(image_ref, explicit_registry, platform)
                    .await;
            }
        };

        // Resolve manifest and config via standard OCI (small HTTP requests).
        let (manifest, config, config_digest, registry, repository, token) = self
            .resolve_image(image_ref, explicit_registry, platform)
            .await?;

        info!(
            image_ref,
            layers = manifest.layers.len(),
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
                .fetch_blob_with_sync(
                    client.clone(),
                    digest,
                    &registry,
                    &repository,
                    token.as_deref(),
                )
                .await?;
            layer_blobs.push((digest.clone(), blob_data));
        }

        info!(image_ref, layers = layer_blobs.len(), "all layers synced");

        Ok(PulledImage {
            manifest,
            config,
            config_digest,
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
        nimbus_oci::OciError,
    > {
        self.oci_puller
            .resolve_image(image_ref, explicit_registry, platform)
            .await
    }

    async fn fetch_blob_with_sync(
        &self,
        client: BlockSyncClient,
        digest: &str,
        registry: &str,
        repository: &str,
        token: Option<&str>,
    ) -> Result<Vec<u8>, nimbus_oci::OciError> {
        let d = digest.to_string();

        if self.store.exists(&d) {
            debug!(%digest, "blob found locally");
            match self.store.get_blob(&d) {
                Ok(mmap) => return Ok(mmap[..].to_vec()),
                Err(_) => debug!(%digest, "local blob read failed, trying peers"),
            }
        }

        let blob = self.try_fetch_from_peers(&client, digest).await;
        if let Some(data) = blob {
            debug!(%digest, size = data.len(), "blob fetched from peer");
            let _ = self.store.put_blob_blocking(&d, &data);
            return Ok(data);
        }

        debug!(%digest, "blob not on any peer; falling back to registry");
        let d2 = digest.to_string();
        let data = self
            .oci_puller
            .fetch_blob_by_digest(registry, repository, digest, token)
            .await?;
        let _ = self.store.put_blob_blocking(&d2, &data);
        Ok(data)
    }

    async fn try_fetch_from_peers(
        &self,
        client: &BlockSyncClient,
        digest: &str,
    ) -> Option<Vec<u8>> {
        let request = tonic::Request::new(GetBlobsRequest {
            digests: vec![digest.to_string()],
        });

        match client.clone().get_blobs(request).await {
            Ok(response) => {
                let mut stream = response.into_inner();
                let mut collected = Vec::new();

                use futures::StreamExt;
                while let Some(chunk_result) = stream.next().await {
                    match chunk_result {
                        Ok(chunk) => {
                            collected.extend_from_slice(&chunk.data);
                            if chunk.is_final {
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(%digest, error = %e, "peer stream error");
                            return None;
                        }
                    }
                }

                if !collected.is_empty() {
                    return Some(collected);
                }
                None
            }
            Err(e) => {
                debug!(%digest, error = %e, "peer get_blobs failed");
                None
            }
        }
    }
}
