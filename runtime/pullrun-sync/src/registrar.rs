use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use tracing::{debug, info, warn};

use crate::proto::registrar_server::Registrar;
use crate::proto::{
    DeregisterRequest, DeregisterResponse, HeartbeatRequest, HeartbeatResponse,
    ListPeersRequest, ListPeersResponse, LookupRequest, LookupResponse,
    PeerRegistration, RegisterRequest, RegisterResponse,
};

pub use crate::proto::registrar_client::RegistrarClient as RegistrarClientGen;
pub use crate::proto::registrar_server::RegistrarServer;

pub type RegistrarClient = RegistrarClientGen<tonic::transport::Channel>;

const PEER_TTL_SECS: u64 = 120;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone)]
pub struct RegistrarService {
    peers: Arc<RwLock<HashMap<String, PeerRegistration>>>,
}

impl Default for RegistrarService {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistrarService {
    pub fn new() -> Self {
        Self {
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn peer_count(&self) -> usize {
        self.peers.read().await.len()
    }

    /// Run a background eviction loop.  Must be spawned as a task.
    pub async fn run_eviction(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            self.evict_stale().await;
        }
    }

    async fn evict_stale(&self) {
        let cutoff = now_unix().saturating_sub(PEER_TTL_SECS);
        let mut peers = self.peers.write().await;
        let before = peers.len();
        peers.retain(|id, p| {
            let keep = p.last_seen_unix_secs as u64 >= cutoff;
            if !keep {
                info!(peer = %id, "evicted stale peer");
            }
            keep
        });
        let after = peers.len();
        if before != after {
            info!(evicted = before - after, remaining = after, "registrar eviction");
        }
    }
}

#[tonic::async_trait]
impl Registrar for RegistrarService {
    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let req = request.into_inner();
        let reg = PeerRegistration {
            node_id: req.node_id.clone(),
            sync_addr: req.sync_addr.clone(),
            last_seen_unix_secs: now_unix() as i64,
        };
        {
            let mut peers = self.peers.write().await;
            peers.insert(req.node_id.clone(), reg);
        }
        let count = self.peer_count().await;
        info!(peer = %req.node_id, addr = %req.sync_addr, "peer registered");
        Ok(Response::new(RegisterResponse {
            peer_count: count as i32,
        }))
    }

    async fn lookup(
        &self,
        request: Request<LookupRequest>,
    ) -> Result<Response<LookupResponse>, Status> {
        let req = request.into_inner();
        let peers = self.peers.read().await;
        let found = peers.get(&req.node_id).cloned();
        Ok(Response::new(LookupResponse {
            peer: found.clone(),
            found: found.is_some(),
        }))
    }

    async fn list_peers(
        &self,
        _request: Request<ListPeersRequest>,
    ) -> Result<Response<ListPeersResponse>, Status> {
        let peers = self.peers.read().await;
        let list: Vec<PeerRegistration> = peers.values().cloned().collect();
        Ok(Response::new(ListPeersResponse { peers: list }))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        let now = now_unix() as i64;
        {
            let mut peers = self.peers.write().await;
            if let Some(entry) = peers.get_mut(&req.node_id) {
                entry.last_seen_unix_secs = now;
            }
        }
        let count = self.peer_count().await;
        Ok(Response::new(HeartbeatResponse {
            peer_count: count as i32,
        }))
    }

    async fn deregister(
        &self,
        request: Request<DeregisterRequest>,
    ) -> Result<Response<DeregisterResponse>, Status> {
        let req = request.into_inner();
        {
            let mut peers = self.peers.write().await;
            peers.remove(&req.node_id);
        }
        info!(peer = %req.node_id, "peer deregistered");
        Ok(Response::new(DeregisterResponse {}))
    }
}

/// Registrar authentication is planned as a future enhancement.
/// The current implementation accepts all registrations without
/// authentication. A shared-secret HMAC or mTLS handshake should
/// be added before production use.
///
/// Client-side helper: register a node with a remote registrar and
/// run periodic heartbeats.  Spawn the returned future as a task.
pub async fn run_registrar_client(
    client: RegistrarClient,
    node_id: String,
    sync_addr: String,
) {
    let register_req = tonic::Request::new(RegisterRequest {
        node_id: node_id.clone(),
        sync_addr: sync_addr.clone(),
    });
    match client.clone().register(register_req).await {
        Ok(resp) => {
            let count = resp.into_inner().peer_count;
            info!(%node_id, peer_count = count, "registered with registrar");
        }
        Err(e) => {
            warn!(error = %e, "failed to register with registrar");
            return;
        }
    }

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        let hb_req = tonic::Request::new(HeartbeatRequest {
            node_id: node_id.clone(),
        });
        if let Err(e) = client.clone().heartbeat(hb_req).await {
            debug!(error = %e, "registrar heartbeat failed");
        }
    }
}
