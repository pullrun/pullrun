#![allow(unused_variables)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nimbus_store::{Digest, MmapStore};
use nimbus_sync::{
    BlockSyncClient, BlockSyncServer, BlockSyncService, PeerBloomCache, PeerBloomInfo,
};
use nimbus_sync::proto::{GetBlobsRequest, HaveBlobsRequest};
use tempfile::TempDir;

fn seed_blob(store: &MmapStore, data: &[u8]) -> Digest {
    let digest = MmapStore::compute_digest(data);
    store.put_blob_blocking(&digest, data).unwrap();
    digest
}

async fn start_block_sync_server(store: Arc<MmapStore>) -> (SocketAddr, BlockSyncService) {
    let svc = BlockSyncService::new(store);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let server = BlockSyncServer::new(svc.clone());
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(server)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .ok();
    });
    // Brief pause to let the server start
    tokio::time::sleep(Duration::from_millis(100)).await;
    (addr, svc)
}

#[tokio::test]
async fn test_block_sync_get_blobs() {
    let dir1 = TempDir::new().unwrap();
    let dir2 = TempDir::new().unwrap();
    let store1 = Arc::new(MmapStore::new(dir1.path().join("store")));
    let store2 = Arc::new(MmapStore::new(dir2.path().join("store")));

    // Seed blob in store1
    let data = b"hello-from-block-sync";
    let digest = seed_blob(&store1, data);
    let digest_hex = digest.as_hex();

    // Start BlockSync server on store1
    let (addr, svc1) = start_block_sync_server(store1.clone()).await;
    svc1.insert_bloom_digest(&digest_hex).await;

    // Connect as client and fetch blob
    let mut client = BlockSyncClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    let request = tonic::Request::new(GetBlobsRequest {
        digests: vec![digest_hex.clone()],
    });

    let response = client.get_blobs(request).await.unwrap();
    let mut stream = response.into_inner();
    let mut result = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        result.extend_from_slice(&chunk.data);
        if chunk.is_final {
            break;
        }
    }

    assert!(!result.is_empty(), "should receive blob data");
    assert_eq!(result, data, "blob data should match");
}

#[tokio::test]
async fn test_block_sync_have_blobs() {
    let dir1 = TempDir::new().unwrap();
    let store1 = Arc::new(MmapStore::new(dir1.path().join("store")));

    let data = b"test-data-for-bloom";
    let digest = seed_blob(&store1, data);

    let (addr, svc1) = start_block_sync_server(store1).await;
    svc1.insert_bloom_digest(&digest.as_hex()).await;

    let mut client = BlockSyncClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    // Send our (empty) bloom filter and receive the server's
    let request = tonic::Request::new(HaveBlobsRequest {
        bloom_filter: vec![],
        bloom_k: 0,
        bloom_m: 0,
    });

    let response = client.have_blobs(request).await.unwrap();
    let resp = response.into_inner();

    assert!(!resp.bloom_filter.is_empty(), "should receive bloom filter");
    assert!(resp.bloom_k > 0, "k should be positive");
    assert!(resp.bloom_m > 0, "m should be positive");
}

#[tokio::test]
async fn test_peer_bloom_cache() {
    let cache = PeerBloomCache::new();

    let info = PeerBloomInfo {
        node_id: "node1".into(),
        sync_addr: "127.0.0.1:9501".into(),
        bloom_bytes: vec![], // empty bloom = no blobs
        bloom_k: 4,
        bloom_m: 640,
        last_updated: std::time::Instant::now(),
    };

    cache.update("node1".into(), "127.0.0.1:9501".into(), info).await;
    assert_eq!(cache.peer_count().await, 1);

    let peers = cache.find_peers_with_blob("sha256:nonexistent").await;
    assert!(peers.is_empty(), "empty bloom should not match any digest");
}

#[tokio::test]
async fn test_block_sync_multi_node_blob_transfer() {
    // Simulates: node1 has blob, node2 fetches it via P2P block sync
    let dir1 = TempDir::new().unwrap();
    let dir2 = TempDir::new().unwrap();
    let store1 = Arc::new(MmapStore::new(dir1.path().join("store")));
    let store2 = Arc::new(MmapStore::new(dir2.path().join("store")));

    let data = b"multi-node-transfer-data";
    let digest = seed_blob(&store1, data);
    let digest_hex = digest.as_hex();

    let (addr, svc1) = start_block_sync_server(store1).await;
    svc1.insert_bloom_digest(&digest_hex).await;

    // Store2 connects to store1 and fetches the blob
    let mut client = BlockSyncClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    let request = tonic::Request::new(GetBlobsRequest {
        digests: vec![digest_hex.clone()],
    });

    let response = client.get_blobs(request).await.unwrap();
    let mut stream = response.into_inner();
    let mut received = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        received.extend_from_slice(&chunk.data);
        if chunk.is_final {
            break;
        }
    }

    assert_eq!(received, data, "transferred blob data should match original");

    // Store the blob in store2
    store2.put_blob_blocking(&digest, &received).unwrap();
    let stored = store2.get_blob(&digest).unwrap();
    assert_eq!(&stored[..], data, "stored blob should match original");
}

// ─── Registrar tests ────────────────────────────────────────────

use nimbus_sync::{RegistrarClient, RegistrarServer, RegistrarService};
use nimbus_sync::proto::{
    DeregisterRequest, HeartbeatRequest, ListPeersRequest, LookupRequest, RegisterRequest,
};

async fn start_registrar_server() -> (SocketAddr, RegistrarService) {
    let svc = RegistrarService::new();
    let server = RegistrarServer::new(svc.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(server)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .ok();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    (addr, svc)
}

#[tokio::test]
async fn test_registrar_register_and_list() {
    let (addr, _svc) = start_registrar_server().await;
    let mut client = RegistrarClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    let resp = client
        .register(RegisterRequest {
            node_id: "node-a".into(),
            sync_addr: "127.0.0.1:9500".into(),
        })
        .await
        .unwrap();
    assert_eq!(resp.into_inner().peer_count, 1);

    let resp = client
        .register(RegisterRequest {
            node_id: "node-b".into(),
            sync_addr: "127.0.0.1:9501".into(),
        })
        .await
        .unwrap();
    assert_eq!(resp.into_inner().peer_count, 2);

    let list = client
        .list_peers(ListPeersRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.peers.len(), 2);
}

#[tokio::test]
async fn test_registrar_lookup() {
    let (addr, _svc) = start_registrar_server().await;
    let mut client = RegistrarClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    client
        .register(RegisterRequest {
            node_id: "node-x".into(),
            sync_addr: "127.0.0.1:9509".into(),
        })
        .await
        .unwrap();

    let resp = client
        .lookup(LookupRequest {
            node_id: "node-x".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(resp.found);
    let peer = resp.peer.unwrap();
    assert_eq!(peer.node_id, "node-x");
    assert_eq!(peer.sync_addr, "127.0.0.1:9509");

    let resp = client
        .lookup(LookupRequest {
            node_id: "nonexistent".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!resp.found);
}

#[tokio::test]
async fn test_registrar_heartbeat() {
    let (addr, _svc) = start_registrar_server().await;
    let mut client = RegistrarClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    client
        .register(RegisterRequest {
            node_id: "node-hb".into(),
            sync_addr: "127.0.0.1:9510".into(),
        })
        .await
        .unwrap();

    let resp = client
        .heartbeat(HeartbeatRequest {
            node_id: "node-hb".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.peer_count, 1);
}

#[tokio::test]
async fn test_registrar_deregister() {
    let (addr, _svc) = start_registrar_server().await;
    let mut client = RegistrarClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    client
        .register(RegisterRequest {
            node_id: "node-c".into(),
            sync_addr: "127.0.0.1:9511".into(),
        })
        .await
        .unwrap();

    let count_before = client
        .list_peers(ListPeersRequest {})
        .await
        .unwrap()
        .into_inner()
        .peers
        .len();
    assert_eq!(count_before, 1);

    client
        .deregister(DeregisterRequest {
            node_id: "node-c".into(),
        })
        .await
        .unwrap();

    let count_after = client
        .list_peers(ListPeersRequest {})
        .await
        .unwrap()
        .into_inner()
        .peers
        .len();
    assert_eq!(count_after, 0);
}
