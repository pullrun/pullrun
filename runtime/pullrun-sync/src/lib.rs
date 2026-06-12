// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

pub mod block_sync;
pub mod bloom;
pub mod delta;
pub mod discovery;
pub mod gossip;
pub mod registrar;
pub mod sync_puller;

pub mod proto {
    tonic::include_proto!("pullrun.sync");
}

pub use block_sync::{BlockSyncClient, BlockSyncServer, BlockSyncService};
pub use bloom::BloomFilter;
pub use delta::compute_delta;
pub use discovery::{generate_node_id, Discovery, PeerInfo};
pub use gossip::{BloomGossip, PeerBloomCache, PeerBloomInfo};
pub use registrar::{run_registrar_client, RegistrarClient, RegistrarServer, RegistrarService};
pub use sync_puller::SyncPuller;
