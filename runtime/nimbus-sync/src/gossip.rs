use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::block_sync::{BlockSyncClient, BlockSyncService};
use crate::bloom::BloomFilter;
use crate::discovery::Discovery;
use crate::proto::HaveBlobsRequest;

const GOSSIP_INTERVAL: Duration = Duration::from_secs(60);
const CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
pub struct PeerBloomInfo {
    pub node_id: String,
    pub sync_addr: String,
    pub bloom_bytes: Vec<u8>,
    pub bloom_k: u32,
    pub bloom_m: u64,
    pub last_updated: Instant,
}

#[derive(Clone, Debug)]
pub struct PeerBloomCache {
    cache: Arc<RwLock<HashMap<String, PeerBloomInfo>>>,
}

impl Default for PeerBloomCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerBloomCache {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn update(&self, node_id: String, sync_addr: String, info: PeerBloomInfo) {
        let mut cache = self.cache.write().await;
        if let Some(existing) = cache.get_mut(&node_id) {
            existing.bloom_bytes = info.bloom_bytes;
            existing.bloom_k = info.bloom_k;
            existing.bloom_m = info.bloom_m;
            existing.last_updated = info.last_updated;
            existing.sync_addr = sync_addr;
        } else {
            cache.insert(node_id, info);
        }
    }

    /// Find peers that likely have a given blob digest.
    /// Returns `(node_id, sync_addr)` for matching peers.
    pub async fn find_peers_with_blob(&self, digest: &str) -> Vec<(String, String)> {
        let cache = self.cache.read().await;
        let now = Instant::now();
        cache
            .values()
            .filter(|info| {
                if now.duration_since(info.last_updated) > CACHE_TTL {
                    return false;
                }
                if let Some((bf, _)) = BloomFilter::from_bytes(&info.bloom_bytes) {
                    bf.contains(digest)
                } else {
                    false
                }
            })
            .map(|info| (info.node_id.clone(), info.sync_addr.clone()))
            .collect()
    }

    pub async fn peer_count(&self) -> usize {
        let cache = self.cache.read().await;
        let now = Instant::now();
        cache
            .values()
            .filter(|info| now.duration_since(info.last_updated) <= CACHE_TTL)
            .count()
    }

    pub async fn remove_stale(&self) {
        let mut cache = self.cache.write().await;
        let now = Instant::now();
        cache.retain(|_, info| now.duration_since(info.last_updated) <= CACHE_TTL);
    }
}

pub struct BloomGossip {
    block_sync_client: BlockSyncClient,
    block_sync_service: BlockSyncService,
    discovery: Discovery,
    bloom_cache: PeerBloomCache,
}

impl BloomGossip {
    pub fn new(
        block_sync_client: BlockSyncClient,
        block_sync_service: BlockSyncService,
        discovery: Discovery,
        bloom_cache: PeerBloomCache,
    ) -> Self {
        Self {
            block_sync_client,
            block_sync_service,
            discovery,
            bloom_cache,
        }
    }

    pub async fn run(&self) {
        let mut interval = tokio::time::interval(GOSSIP_INTERVAL);
        // Skip first immediate tick
        interval.tick().await;

        info!("bloom filter gossip started (interval={}s)", GOSSIP_INTERVAL.as_secs());

        loop {
            interval.tick().await;

            let peers = self.discovery.get_peers().await;
            if peers.is_empty() {
                debug!("no peers for gossip exchange");
                self.bloom_cache.remove_stale().await;
                continue;
            }

            // Pick 2-3 random peers per round for faster convergence.
            // Use StdRng::from_entropy() instead of thread_rng() because
            // thread_rng() is !Send and cannot cross .await boundaries.
            let mut rng = StdRng::from_entropy();
            let peer_count = (peers.len() as u32).clamp(1, 3);
            let selected: Vec<_> = peers.as_slice().choose_multiple(&mut rng, peer_count as usize).cloned().collect();

            // Get our bloom filter once, reuse for all peers in this round.
            let (bf_bytes, bf_k, bf_m) = self.block_sync_service.bloom_filter_bytes().await;

            for peer in &selected {
                let request = tonic::Request::new(HaveBlobsRequest {
                    bloom_filter: bf_bytes.clone(),
                    bloom_k: bf_k as i32,
                    bloom_m: bf_m as i32,
                });

                match self.block_sync_client.clone().have_blobs(request).await {
                    Ok(response) => {
                        let resp = response.into_inner();
                        let peer_addr = peer.sync_addr.to_string();
                        // Cache the parsed bloom filter alongside the raw bytes.
                        let info = PeerBloomInfo {
                            node_id: peer.node_id.clone(),
                            sync_addr: peer_addr.clone(),
                            bloom_bytes: resp.bloom_filter,
                            bloom_k: resp.bloom_k as u32,
                            bloom_m: resp.bloom_m as u64,
                            last_updated: Instant::now(),
                        };
                        self.bloom_cache
                            .update(peer.node_id.clone(), peer_addr, info)
                            .await;
                        let count = self.bloom_cache.peer_count().await;
                        debug!(
                            peer = %peer.node_id,
                            bloom_peers = count,
                            "gossip exchange complete"
                        );
                    }
                    Err(e) => {
                        warn!(peer = %peer.node_id, error = %e, "gossip exchange failed");
                    }
                }
            }

            self.bloom_cache.remove_stale().await;
        }
    }
}
