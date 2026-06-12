// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

pub mod proto {
    tonic::include_proto!("pullrun.runtime");
}

pub mod secrets;
pub use secrets::SecretStore;

pub mod binfmt;
pub mod builder;
pub mod events;
pub mod metrics;
pub mod service;

pub use events::{Event, EventBus, EventKind};
pub use service::{RuntimeCommand, RuntimeService, ServiceConfig};

/// Convert an in-process `Event` into its proto representation. The
/// metadata map is moved over wholesale — proto `Event.metadata` is a
/// `map<string, string>`, so the conversion is lossless.
impl From<Event> for proto::Event {
    fn from(ev: Event) -> Self {
        proto::Event {
            id: ev.id,
            kind: ev.kind.as_str().to_string(),
            timestamp: ev.timestamp as i64,
            metadata: ev.metadata,
        }
    }
}
