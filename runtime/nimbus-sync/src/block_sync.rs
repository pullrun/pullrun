use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::Stream;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, warn};

use nimbus_store::MmapStore;

use crate::bloom::BloomFilter;
use crate::proto::block_sync_server::BlockSync;
use crate::proto::{BlobChunk, HaveBlobsRequest, HaveBlobsResponse, GetBlobsRequest, SyncBlob};

pub use crate::proto::block_sync_client::BlockSyncClient as BlockSyncClientGen;
pub use crate::proto::block_sync_server::BlockSyncServer;

/// Default BlockSync client using tonic Channel transport.
pub type BlockSyncClient = BlockSyncClientGen<tonic::transport::Channel>;

const CHUNK_SIZE: usize = 1 * 1024 * 1024;

struct BlockSyncInner {
    store: Arc<MmapStore>,
    bloom_filter: RwLock<BloomFilter>,
    blob_count: AtomicUsize,
}

#[derive(Clone)]
pub struct BlockSyncService {
    inner: Arc<BlockSyncInner>,
}

impl BlockSyncService {
    pub fn new(store: Arc<MmapStore>) -> Self {
        Self {
            inner: Arc::new(BlockSyncInner {
                store,
                bloom_filter: RwLock::new(BloomFilter::optimal(10000)),
                blob_count: AtomicUsize::new(0),
            }),
        }
    }

    pub async fn rebuild_bloom_filter(&self) {
        let mut bf = BloomFilter::optimal(10000);
        let digests = self.collect_blob_digests().await;
        for d in &digests {
            bf.insert(d);
        }
        *self.inner.bloom_filter.write().await = bf;
        self.inner.blob_count.store(digests.len(), Ordering::Relaxed);
    }

    async fn collect_blob_digests(&self) -> Vec<String> {
        let root = self.inner.store.root_dir().to_owned();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            if root.exists() {
                let _ = walk_store_for_blobs(&root, &mut out);
            }
            out.sort();
            out.dedup();
            out
        })
        .await
        .unwrap_or_default()
    }

    pub fn blob_count(&self) -> usize {
        self.inner.blob_count.load(Ordering::Relaxed)
    }

    /// Returns (serialized_bloom_filter, k, m) for gossip exchange.
    pub async fn bloom_filter_bytes(&self) -> (Vec<u8>, u32, u64) {
        let bf = self.inner.bloom_filter.read().await;
        (bf.to_bytes(), bf.k(), bf.m())
    }

    /// Insert a digest into the local bloom filter (incremental update).
    pub async fn insert_bloom_digest(&self, digest: &str) {
        let mut bf = self.inner.bloom_filter.write().await;
        bf.insert(digest);
        self.inner.blob_count.fetch_add(1, Ordering::Relaxed);
    }
}

fn walk_store_for_blobs(dir: &std::path::Path, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_store_for_blobs(&path, out)?;
        } else if path.file_name().map_or(false, |n| n == "blob.raw") {
            if let Some(parent) = path.parent() {
                if let Some(rest) = parent.file_name() {
                    if let Some(gp) = parent.parent() {
                        if let Some(b) = gp.file_name() {
                            if let Some(ggp) = gp.parent() {
                                if let Some(a) = ggp.file_name() {
                                    let digest = format!(
                                        "{}{}{}",
                                        a.to_string_lossy(),
                                        b.to_string_lossy(),
                                        rest.to_string_lossy()
                                    );
                                    out.push(digest);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[tonic::async_trait]
impl BlockSync for BlockSyncService {
    type GetBlobsStream = Pin<Box<dyn Stream<Item = Result<BlobChunk, Status>> + Send>>;
    type SyncBlobsStream = Pin<Box<dyn Stream<Item = Result<SyncBlob, Status>> + Send>>;

    async fn have_blobs(
        &self,
        _request: Request<HaveBlobsRequest>,
    ) -> Result<Response<HaveBlobsResponse>, Status> {
        let bf = self.inner.bloom_filter.read().await;
        let bytes = bf.to_bytes();
        Ok(Response::new(HaveBlobsResponse {
            bloom_filter: bytes,
            bloom_k: bf.k() as i32,
            bloom_m: bf.m() as i32,
        }))
    }

    async fn get_blobs(
        &self,
        request: Request<GetBlobsRequest>,
    ) -> Result<Response<Self::GetBlobsStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = mpsc::channel::<Result<BlobChunk, Status>>(16);

        let store = self.inner.store.clone();
        tokio::spawn(async move {
            for digest in &req.digests {
                match store.get_blob(digest) {
                    Ok(mmap) => {
                        let data = mmap[..].to_vec();
                        let total = data.len();
                        let mut offset = 0usize;
                        while offset < total {
                            let end = (offset + CHUNK_SIZE).min(total);
                            let chunk = data[offset..end].to_vec();
                            let is_final = end >= total;
                            if tx
                                .send(Ok(BlobChunk {
                                    digest: digest.clone(),
                                    data: chunk,
                                    offset: offset as u32,
                                    is_final,
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            offset = end;
                        }
                    }
                    Err(e) => {
                        warn!(%digest, error = %e, "blob not found for peer");
                        let _ = tx
                            .send(Ok(BlobChunk {
                                digest: digest.clone(),
                                data: vec![],
                                offset: 0,
                                is_final: true,
                            }))
                            .await;
                    }
                }
            }
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream) as Self::GetBlobsStream))
    }

    async fn sync_blobs(
        &self,
        request: Request<Streaming<SyncBlob>>,
    ) -> Result<Response<Self::SyncBlobsStream>, Status> {
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel::<Result<SyncBlob, Status>>(16);

        let store = self.inner.store.clone();
        tokio::spawn(async move {
            while let Ok(Some(msg)) = inbound.message().await {
                let digest = msg.digest.clone();
                if !msg.data.is_empty() {
                    if let Err(e) = store.put_blob_blocking(&digest, &msg.data) {
                        warn!(%digest, error = %e, "failed to store synced blob");
                    }
                }
                debug!(%digest, size = msg.data.len(), "synced blob from peer");
            }
            let _ = tx
                .send(Ok(SyncBlob {
                    digest: String::new(),
                    data: vec![],
                }))
                .await;
        });

        let stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream) as Self::SyncBlobsStream))
    }
}
