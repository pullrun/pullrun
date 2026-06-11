use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const MULTICAST_ADDR: &str = "239.255.0.100:54321";
const BROADCAST_INTERVAL: Duration = Duration::from_secs(30);
const PEER_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeAnnouncement {
    pub node_id: String,
    pub sync_addr: String,
    pub version: u32,
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub node_id: String,
    pub sync_addr: SocketAddr,
    pub last_seen: Instant,
}

#[derive(Clone)]
pub struct Discovery {
    node_id: String,
    sync_addr: SocketAddr,
    peers: Arc<RwLock<HashMap<String, PeerInfo>>>,
    /// Per-source rate limiting: tracks last announcement time per source IP.
    rate_limiter: Arc<RwLock<HashMap<String, Instant>>>,
}

impl Discovery {
    pub fn new(node_id: String, sync_addr: SocketAddr) -> Self {
        Self {
            node_id,
            sync_addr,
            peers: Arc::new(RwLock::new(HashMap::new())),
            rate_limiter: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn run(&self) {
        let bind_addr = if self.sync_addr.ip().is_unspecified() {
            "0.0.0.0:0"
        } else {
            &format!("{}:0", self.sync_addr.ip())
        };

        let sock = match UdpSocket::bind(bind_addr).await {
            Ok(s) => {
                info!(local = %s.local_addr().expect("bound socket").port(), "discovery listening");
                s
            }
            Err(e) => {
                warn!(error = %e, "failed to bind discovery socket");
                return;
            }
        };

        // Join multicast group (best-effort on macOS/Linux).
        let mc_addr: SocketAddr = MULTICAST_ADDR.parse().unwrap();
        let addr_ip = mc_addr.ip();
        let sync_ip = self.sync_addr.ip();
        if let std::net::IpAddr::V4(mc_ipv4) = addr_ip {
            let iface = if let std::net::IpAddr::V4(ip) = sync_ip {
                ip
            } else {
                std::net::Ipv4Addr::new(0, 0, 0, 0)
            };
            let _ = sock.join_multicast_v4(mc_ipv4, iface);
        }

        let broadcast_sock = match UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to bind broadcast socket");
                return;
            }
        };

        let announcement = NodeAnnouncement {
            node_id: self.node_id.clone(),
            sync_addr: self.sync_addr.to_string(),
            version: 1,
        };
        let announcement_bytes = serde_json::to_vec(&announcement).unwrap_or_default();

        let peers = self.peers.clone();
        let node_id = self.node_id.clone();
        let rate_limiter = self.rate_limiter.clone();
        let peers_for_evict = self.peers.clone();

        // Broadcast task
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(BROADCAST_INTERVAL);
            loop {
                interval.tick().await;
                if let Err(e) = broadcast_sock.send_to(&announcement_bytes, mc_addr).await {
                    debug!(error = %e, "discovery broadcast failed");
                }
            }
        });

        // Periodic stale peer eviction (runs every 30s, independent of recv).
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let mut peers = peers_for_evict.write().await;
                let threshold = Instant::now() - PEER_TIMEOUT;
                peers.retain(|id, info| {
                    let keep = info.last_seen >= threshold;
                    if !keep {
                        info!(peer = %id, addr = %info.sync_addr, "evicting stale peer");
                    }
                    keep
                });
            }
        });

        // Listen for peer announcements (rate-limited per source).
        let mut buf = [0u8; 4096];
        loop {
            match sock.recv_from(&mut buf).await {
                Ok((len, src)) => {
                    let data = &buf[..len];
                    if let Ok(ann) = serde_json::from_slice::<NodeAnnouncement>(data) {
                        if ann.node_id == node_id {
                            continue;
                        }
                        // Rate limit: at most 1 announcement per source per 10s.
                        {
                            let mut rl = rate_limiter.write().await;
                            let last = rl.get(&src.ip().to_string()).copied();
                            if let Some(last) = last {
                                if Instant::now().duration_since(last) < Duration::from_secs(10) {
                                    continue;
                                }
                            }
                            rl.insert(src.ip().to_string(), Instant::now());
                        }
                        if let Ok(addr) = ann.sync_addr.parse::<SocketAddr>() {
                            let mut peers = peers.write().await;
                            let is_new = !peers.contains_key(&ann.node_id);
                            let peer_id = ann.node_id.clone();
                            peers.insert(
                                peer_id.clone(),
                                PeerInfo {
                                    node_id: peer_id.clone(),
                                    sync_addr: addr,
                                    last_seen: Instant::now(),
                                },
                            );
                            if is_new {
                                info!(peer = %peer_id, addr = %addr, "discovered peer");
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!(error = %e, "discovery recv error");
                }
            }
        }
    }

    pub async fn get_peers(&self) -> Vec<PeerInfo> {
        let peers = self.peers.read().await;
        peers.values().cloned().collect()
    }

    pub async fn get_peer_count(&self) -> usize {
        self.peers.read().await.len()
    }
}

pub fn generate_node_id() -> String {
    let mut rng = rand::thread_rng();
    let bytes: [u8; 8] = rng.gen();
    format!("pullrun-{}", hex::encode(bytes))
}
