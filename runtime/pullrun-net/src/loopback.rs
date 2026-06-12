// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use async_trait::async_trait;
use tracing::{debug, info};

use crate::{NetError, NetworkEndpoint, NetworkManager, NetworkRule};

pub struct LoopbackNetwork;

impl LoopbackNetwork {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LoopbackNetwork {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NetworkManager for LoopbackNetwork {
    async fn setup(
        &self,
        workload_id: &str,
        _rules: &[NetworkRule],
    ) -> Result<NetworkEndpoint, NetError> {
        info!(%workload_id, "setting up loopback network (isolated)");

        let endpoint = NetworkEndpoint {
            internal_ip: "127.0.0.1".to_string(),
            host_port_mappings: vec![],
            namespace_path: None,
        };

        debug!(%workload_id, ip = %endpoint.internal_ip, "loopback network configured");
        Ok(endpoint)
    }

    async fn teardown(
        &self,
        workload_id: &str,
        _endpoint: &NetworkEndpoint,
    ) -> Result<(), NetError> {
        info!(%workload_id, "tearing down loopback network");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_loopback_setup_teardown() {
        let net = LoopbackNetwork::new();
        let endpoint = net.setup("test-wl", &[]).await.unwrap();

        assert_eq!(endpoint.internal_ip, "127.0.0.1");
        assert!(endpoint.host_port_mappings.is_empty());

        net.teardown("test-wl", &endpoint).await.unwrap();
    }
}
