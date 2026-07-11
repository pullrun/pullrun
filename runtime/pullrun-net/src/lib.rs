// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

pub mod dns;
pub mod firewall;
pub mod ipam;
pub mod loopback;
pub mod proxy;

pub use firewall::{
    detect_backend, FirewallBackend, FirewallError, IptablesBackend, NftablesBackend,
};
pub use ipam::{IpRange, Ipam};
pub use loopback::LoopbackNetwork;
pub use proxy::ProxyNetwork;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Direction {
    Inbound,
    Outbound,
}

impl Direction {
    pub fn as_str(&self) -> &str {
        match self {
            Direction::Inbound => "inbound",
            Direction::Outbound => "outbound",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    pub fn as_str(&self) -> &str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkRule {
    pub direction: Direction,
    pub protocol: Protocol,
    pub port: u16,
    pub host_port: u16,
    pub to_host: Option<String>,
    pub from_cidrs: Option<Vec<String>>,
}

impl NetworkRule {
    pub fn outbound_tcp(to_host: impl Into<String>, port: u16) -> Self {
        Self {
            direction: Direction::Outbound,
            protocol: Protocol::Tcp,
            port,
            host_port: 0,
            to_host: Some(to_host.into()),
            from_cidrs: None,
        }
    }

    pub fn inbound(port: u16) -> Self {
        Self {
            direction: Direction::Inbound,
            protocol: Protocol::Tcp,
            port,
            host_port: 0,
            to_host: None,
            from_cidrs: None,
        }
    }

    pub fn inbound_mapped(host_port: u16, container_port: u16) -> Self {
        Self {
            direction: Direction::Inbound,
            protocol: Protocol::Tcp,
            port: container_port,
            host_port,
            to_host: None,
            from_cidrs: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEndpoint {
    pub internal_ip: String,
    pub host_port_mappings: Vec<(u16, u16)>,
    pub namespace_path: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Network setup failed: {0}")]
    SetupFailed(String),
    #[error("No available IPs")]
    NoAvailableIps,
    #[error("Port already in use: {0}")]
    PortInUse(u16),
}

#[async_trait]
pub trait NetworkManager: Send + Sync {
    async fn setup(
        &self,
        workload_id: &str,
        rules: &[NetworkRule],
    ) -> Result<NetworkEndpoint, NetError>;

    async fn teardown(&self, workload_id: &str, endpoint: &NetworkEndpoint)
        -> Result<(), NetError>;
}
