use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::ipam::Ipam;
use crate::{NetError, NetworkEndpoint, NetworkManager, NetworkRule, Protocol};

const BRIDGE_NAME: &str = "nimbus-br0";
const CIDR: &str = "10.42.0.0/16";
const GATEWAY_IP: &str = "10.42.0.1";

pub struct ProxyNetwork {
    ipam: Arc<Ipam>,
    active_sessions: Arc<Mutex<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>>,
}

impl ProxyNetwork {
    pub fn new() -> Result<Self, String> {
        let ipam = Ipam::from_cidr(CIDR)?;
        Ok(Self {
            ipam: Arc::new(ipam),
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Build a ProxyNetwork that shares its IPAM with an existing one.
    /// This is what the Firecracker VM backend uses: it allocates IPs from
    /// the same 10.42.0.0/16 pool as the container backend, so containers
    /// and VMs can talk to each other over the same bridge.
    pub fn with_shared_ipam(ipam: Arc<Ipam>) -> Self {
        Self {
            ipam,
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Direct access to the shared IPAM. Use this when you need to allocate
    /// an IP yourself (e.g. a VM backend that will then attach the IP to
    /// a tap device).
    pub fn ipam_handle(&self) -> Arc<Ipam> {
        self.ipam.clone()
    }

    pub fn ipam(&self) -> &Ipam {
        &self.ipam
    }

    /// Register an already-allocated IP as a workload endpoint. Starts
    /// the inbound proxy listeners (for any inbound rules) and stores the
    /// outbound session guard. Does NOT allocate an IP and does NOT create
    /// any host-side plumbing — that is the caller's responsibility
    /// (FirecrackerExecutor does the tap device; LinuxContainerExecutor
    /// does veth + netns).
    pub async fn register_endpoint(
        &self,
        workload_id: &str,
        internal_ip: String,
        rules: &[NetworkRule],
    ) -> Result<NetworkEndpoint, NetError> {
        info!(%workload_id, %internal_ip, rules = rules.len(), "registering endpoint with proxy network");

        let mut proxy_handles = Vec::new();
        let mut host_port_mappings = Vec::new();

        for rule in rules.iter().filter(|r| matches!(r.direction, crate::Direction::Inbound)) {
            if !matches!(rule.protocol, Protocol::Tcp) {
                warn!(?rule, "non-TCP protocol not yet supported, skipping");
                continue;
            }

            let target_port = rule.port;
            let target_ip = internal_ip.clone();
            let port = if rule.host_port != 0 { rule.host_port } else { rule.port };

            match self
                .start_inbound_proxy(workload_id, port, target_ip, target_port)
                .await
            {
                Ok(handle) => {
                    proxy_handles.push(handle);
                    host_port_mappings.push((port, target_port));
                }
                Err(e) => {
                    warn!(%workload_id, "failed to start inbound proxy: {e}");
                }
            }
        }

        if let Ok(handle) = self
            .start_outbound_proxy(workload_id, rules.to_vec())
            .await
        {
            proxy_handles.push(handle);
        }

        let mut sessions = self.active_sessions.lock().await;
        sessions.insert(workload_id.to_string(), proxy_handles);

        Ok(NetworkEndpoint {
            internal_ip,
            host_port_mappings,
            namespace_path: None,
        })
    }

    async fn start_inbound_proxy(
        &self,
        workload_id: &str,
        host_port: u16,
        target_ip: String,
        target_port: u16,
    ) -> Result<tokio::task::JoinHandle<()>, NetError> {
        let bind_addr: SocketAddr = format!("0.0.0.0:{host_port}").parse().unwrap();
        let listener = TcpListener::bind(bind_addr).await.map_err(|e| {
            NetError::SetupFailed(format!("bind inbound proxy on {host_port}: {e}"))
        })?;

        info!(%workload_id, %host_port, %target_ip, %target_port, "inbound proxy started");

        let workload_id_owned = workload_id.to_string();
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((client, _)) => {
                        let target = format!("{target_ip}:{target_port}");
                        let id = workload_id_owned.clone();
                        tokio::spawn(async move {
                            if let Err(e) = forward_connection(client, &target, "inbound").await {
                                warn!(workload = %id, "inbound forward error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        error!(workload = %workload_id_owned, "inbound accept error: {e}");
                        break;
                    }
                }
            }
        });

        Ok(handle)
    }

    async fn start_outbound_proxy(
        &self,
        workload_id: &str,
        rules: Vec<NetworkRule>,
    ) -> Result<tokio::task::JoinHandle<()>, NetError> {
        let workload_id_owned = workload_id.to_string();
        let handle = tokio::spawn(async move {
            for rule in rules {
                if !matches!(rule.direction, crate::Direction::Outbound) {
                    continue;
                }
                info!(workload = %workload_id_owned, ?rule, "outbound rule active");
            }
        });
        Ok(handle)
    }

    pub async fn start_outbound_session(
        &self,
        workload_id: &str,
        target: &str,
        rules: &[NetworkRule],
    ) -> Result<(), NetError> {
        let allowed = rules.iter().any(|r| {
            matches!(r.direction, crate::Direction::Outbound)
                && r.to_host.as_deref() == Some(target.split(':').next().unwrap_or(""))
        });

        if !allowed {
            return Err(NetError::SetupFailed(format!(
                "outbound to {target} denied for workload {workload_id}"
            )));
        }

        Ok(())
    }
}

impl Default for ProxyNetwork {
    fn default() -> Self {
        Self::new().expect("ProxyNetwork::new")
    }
}

async fn forward_connection(
    client: TcpStream,
    target: &str,
    direction: &str,
) -> Result<(), String> {
    let upstream = TcpStream::connect(target)
        .await
        .map_err(|e| format!("connect {target}: {e}"))?;

    let _ = client.set_nodelay(true);
    let _ = upstream.set_nodelay(true);

    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();

    let client_to_upstream = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match cr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if uw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let upstream_to_client = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match ur.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if cw.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    debug!(%direction, "forwarding connection");
    let _ = tokio::join!(client_to_upstream, upstream_to_client);
    Ok(())
}

#[async_trait]
impl NetworkManager for ProxyNetwork {
    async fn setup(
        &self,
        workload_id: &str,
        rules: &[NetworkRule],
    ) -> Result<NetworkEndpoint, NetError> {
        info!(%workload_id, rules = rules.len(), "setting up proxy network");

        let internal_ip_int = self
            .ipam
            .allocate()
            .ok_or(NetError::NoAvailableIps)?;
        let internal_ip = std::net::Ipv4Addr::from(internal_ip_int).to_string();

        let endpoint = self
            .register_endpoint(workload_id, internal_ip.clone(), rules)
            .await?;

        debug!(%workload_id, %internal_ip, "proxy network configured");
        Ok(endpoint)
    }

    async fn teardown(
        &self,
        workload_id: &str,
        _endpoint: &NetworkEndpoint,
    ) -> Result<(), NetError> {
        info!(%workload_id, "tearing down proxy network");

        let mut sessions = self.active_sessions.lock().await;
        if let Some(handles) = sessions.remove(workload_id) {
            for handle in handles {
                handle.abort();
            }
        }

        Ok(())
    }
}

pub const fn bridge_name() -> &'static str {
    BRIDGE_NAME
}

pub fn gateway_ip() -> &'static str {
    GATEWAY_IP
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_proxy_network_setup() {
        let net = ProxyNetwork::new().unwrap();
        let endpoint = net.setup("test-wl", &[]).await.unwrap();

        assert!(endpoint.internal_ip.starts_with("10.42."));
        assert!(endpoint.host_port_mappings.is_empty());

        net.teardown("test-wl", &endpoint).await.unwrap();
    }

    #[tokio::test]
    async fn test_proxy_network_with_inbound_rule() {
        let net = ProxyNetwork::new().unwrap();
        let rules = vec![NetworkRule::inbound(8080)];
        let endpoint = net.setup("test-wl", &rules).await.unwrap();

        assert_eq!(endpoint.host_port_mappings.len(), 1);
        assert_eq!(endpoint.host_port_mappings[0], (8080, 8080));

        net.teardown("test-wl", &endpoint).await.unwrap();
    }

    /// Two ProxyNetworks that share an IPAM must allocate different IPs
    /// (the IPAM is the source of truth, not each instance).
    #[tokio::test]
    async fn test_shared_ipam_does_not_double_allocate() {
        let net_a = ProxyNetwork::new().unwrap();
        let ipam = net_a.ipam_handle();
        let net_b = ProxyNetwork::with_shared_ipam(ipam);

        let endpoint_a = net_a.setup("wl-a", &[]).await.unwrap();
        let endpoint_b = net_b.setup("wl-b", &[]).await.unwrap();

        let ip_a: std::net::Ipv4Addr = endpoint_a.internal_ip.parse().unwrap();
        let ip_b: std::net::Ipv4Addr = endpoint_b.internal_ip.parse().unwrap();
        let a_int = u32::from(ip_a);
        let b_int = u32::from(ip_b);

        assert_ne!(a_int, b_int, "shared IPAM must give different IPs");
        assert_eq!(b_int, a_int + 1, "shared IPAM should hand out sequential IPs");

        net_a.teardown("wl-a", &endpoint_a).await.unwrap();
        net_b.teardown("wl-b", &endpoint_b).await.unwrap();
    }

    /// register_endpoint is what the VM backend calls: it has already
    /// allocated an IP, it just needs the proxy listeners started.
    #[tokio::test]
    async fn test_register_endpoint_with_known_ip() {
        let net = ProxyNetwork::new().unwrap();
        let endpoint = net
            .register_endpoint("vm-1", "10.42.0.42".to_string(), &[NetworkRule::inbound(9090)])
            .await
            .unwrap();

        assert_eq!(endpoint.internal_ip, "10.42.0.42");
        assert_eq!(endpoint.host_port_mappings, vec![(9090, 9090)]);

        net.teardown("vm-1", &endpoint).await.unwrap();
    }
}