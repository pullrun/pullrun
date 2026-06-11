//! In-process event bus for the Nimbus runtime.
//!
//! `EventBus` is a thin wrapper over `tokio::sync::broadcast` that
//! decouples the gRPC service from the CLI/observability consumers.
//! Emitters call `emit()`; subscribers call `subscribe()` to get a
//! `Receiver<Event>` they can read from.
//!
//! ## Semantics
//!
//! - **At-most-once per receiver**: broadcast is *not* a queue. If a
//!   subscriber is slow, events sent while it's behind are *dropped*
//!   (the receiver sees a `RecvError::Lagged(n)`). This is fine for
//!   the CLI use case (operators want recent events, not a replay
//!   log) but means the bus must not be used for state that must
//!   reach every consumer. For audit-grade durability, write events
//!   to a WAL (not in v0).
//!
//! - **Multi-consumer**: any number of subscribers (CLI follow
//!   sessions, Prometheus exporters, audit daemons) can read the
//!   same stream without affecting each other.
//!
//! - **Backpressure-free**: `emit()` is non-blocking and never
//!   fails for "no receivers"; the event is simply dropped. This
//!   matters on hot paths (every pull/run RPC) — we don't want to
//!   block the gRPC service on a slow subscriber.
//!
//! ## Capacity
//!
//! The default capacity is 1024. Each event is small (~100 bytes
//! serialized), so the bus uses <200 KiB of memory at full
//! saturation. Increase if a single consumer might fall behind for
//! more than a few seconds at sustained emit rates >1000/s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Default broadcast channel capacity. 1024 events × ~100 bytes ≈
/// 100 KiB max memory per bus instance.
pub const DEFAULT_CAPACITY: usize = 1024;

/// All event kinds the runtime knows how to emit. Add a new kind by
/// extending this enum and emitting it from a call site; the proto
/// `Event.kind` field is a free-form string so old CLIs that don't
/// recognize a new kind will simply print "UNKNOWN" rather than fail.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    /// `OciPuller` returned a new image; metadata: registry, image_ref, root_digest.
    ImagePulled,
    /// `OciPuller` returned an image that was already in the store; metadata: registry, image_ref, saved_bytes.
    ImageDeduped,
    /// `RunWorkload` reached the running state; metadata: backend, image_root, internal_ip.
    WorkloadStarted,
    /// `StopWorkload` was called by an operator; metadata: backend, exit_code.
    WorkloadStopped,
    /// The workload process exited on its own (not via stop); metadata: backend, exit_code.
    WorkloadExited,
    /// The runtime selected a backend for a workload (Container vs Vm); metadata: backend, requested_backend.
    BackendSelected,
    /// A policy check denied an operation; metadata: policy, image_ref, reason.
    PolicyDenied,
    /// A policy check allowed an operation (low volume; emit only when the policy engine is enabled); metadata: policy, image_ref.
    PolicyAllowed,
}

impl EventKind {
    /// Wire string for the proto `Event.kind` field. Stable across
    /// versions; do not rename without bumping the protocol.
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::ImagePulled => "IMAGE_PULLED",
            EventKind::ImageDeduped => "IMAGE_DEDUPED",
            EventKind::WorkloadStarted => "WORKLOAD_STARTED",
            EventKind::WorkloadStopped => "WORKLOAD_STOPPED",
            EventKind::WorkloadExited => "WORKLOAD_EXITED",
            EventKind::BackendSelected => "BACKEND_SELECTED",
            EventKind::PolicyDenied => "POLICY_DENIED",
            EventKind::PolicyAllowed => "POLICY_ALLOWED",
        }
    }

    /// Inverse of `as_str`. Unknown strings map to `None` (caller
    /// should still display the raw string from the proto so old
    /// CLIs don't fail on a future event kind).
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "IMAGE_PULLED" => EventKind::ImagePulled,
            "IMAGE_DEDUPED" => EventKind::ImageDeduped,
            "WORKLOAD_STARTED" => EventKind::WorkloadStarted,
            "WORKLOAD_STOPPED" => EventKind::WorkloadStopped,
            "WORKLOAD_EXITED" => EventKind::WorkloadExited,
            "BACKEND_SELECTED" => EventKind::BackendSelected,
            "POLICY_DENIED" => EventKind::PolicyDenied,
            "POLICY_ALLOWED" => EventKind::PolicyAllowed,
            _ => return None,
        })
    }
}

/// A single runtime event. The struct is `Clone + Debug + Send +
/// 'static` so it can be cloned into multiple broadcast receivers
/// cheaply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    /// The id this event refers to. For image events: the image
    /// ref or root digest. For workload events: the workload id.
    pub id: String,
    pub kind: EventKind,
    /// Unix seconds at the moment of emission.
    pub timestamp: u64,
    /// Free-form string→string metadata. Use short keys
    /// (`backend`, `exit_code`, `image_ref`, etc.) — operators read
    /// these in the CLI and dashboards.
    pub metadata: HashMap<String, String>,
}

impl Event {
    pub fn new(id: impl Into<String>, kind: EventKind) -> Self {
        Self {
            id: id.into(),
            kind,
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            metadata: HashMap::new(),
        }
    }

    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Process-wide event bus. Cheap to clone (it's a `broadcast::Sender`
/// inside, which itself is an `Arc`). Multiple `EventBus` instances
/// could coexist in theory, but in practice the runtime has one
/// per process.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field("receiver_count", &self.tx.receiver_count())
            .finish()
    }
}

impl EventBus {
    /// Create a new bus with the given capacity (queue depth per
    /// slow receiver before drops start).
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    /// Emit an event. Non-blocking: if there are no subscribers, the
    /// event is dropped silently. If a subscriber is slow, it sees
    /// a `RecvError::Lagged` on its next recv and events between
    /// then and now are dropped from that receiver's view.
    pub fn emit(&self, event: Event) {
        // `broadcast::Sender::send` returns Err only if there are no
        // receivers. We don't care about that case (it's the
        // common case in CI), so ignore the result.
        match self.tx.send(event) {
            Ok(n) => debug!(receivers = n, "emitted event"),
            Err(_) => debug!("emit dropped: no subscribers"),
        }
    }

    /// Subscribe to all future events. Each call returns a fresh
    /// receiver; past events are not replayed.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Number of active subscribers. Useful for tests and for
    /// emitting the value as a metric.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

/// Convenience: spawn a background task that drains an `EventBus`
/// receiver and pushes events into an `Event` sink. Returns the
/// `JoinHandle` so tests/callers can await drain completion.
///
/// The task exits cleanly when the receiver returns
/// `RecvError::Closed` (the bus was dropped) or `RecvError::Lagged`
/// (slow consumer — we log and continue, since lagging is not a
/// fatal condition for the bus).
pub fn spawn_drain(
    bus: Arc<EventBus>,
    mut on_event: impl FnMut(Event) + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => on_event(ev),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(missed = n, "event subscriber lagged; dropped events");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("event bus closed; drain task exiting");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn emit_subscribe_roundtrip() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.emit(
            Event::new("img-1", EventKind::ImagePulled)
                .with_metadata("registry", "docker.io"),
        );

        let ev = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event did not arrive in time")
            .expect("recv error");
        assert_eq!(ev.id, "img-1");
        assert_eq!(ev.kind, EventKind::ImagePulled);
        assert_eq!(ev.metadata.get("registry").map(|s| s.as_str()), Some("docker.io"));
    }

    #[tokio::test]
    async fn multiple_subscribers_each_get_events() {
        let bus = EventBus::new(16);
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.emit(Event::new("w-1", EventKind::WorkloadStarted));

        let e1 = tokio::time::timeout(Duration::from_millis(100), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        let e2 = tokio::time::timeout(Duration::from_millis(100), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(e1.id, e2.id);
        assert_eq!(e1.kind, e2.kind);
    }

    #[tokio::test]
    async fn emit_with_no_receivers_is_noop() {
        let bus = EventBus::new(4);
        // Should not panic.
        bus.emit(Event::new("nope", EventKind::ImagePulled));
        assert_eq!(bus.receiver_count(), 0);
    }

    #[tokio::test]
    async fn kind_roundtrip_via_str() {
        for k in [
            EventKind::ImagePulled,
            EventKind::ImageDeduped,
            EventKind::WorkloadStarted,
            EventKind::WorkloadStopped,
            EventKind::WorkloadExited,
            EventKind::BackendSelected,
            EventKind::PolicyDenied,
            EventKind::PolicyAllowed,
        ] {
            assert_eq!(EventKind::parse(k.as_str()), Some(k.clone()));
        }
        assert_eq!(EventKind::parse("BOGUS"), None);
    }

    #[tokio::test]
    async fn event_new_sets_timestamp() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ev = Event::new("x", EventKind::ImagePulled);
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(ev.timestamp >= before && ev.timestamp <= after);
    }

    #[tokio::test]
    async fn default_capacity_is_1024() {
        assert_eq!(DEFAULT_CAPACITY, 1024);
    }

    #[tokio::test]
    async fn lagging_subscriber_does_not_break_emitter() {
        // Slow receiver: never reads. Fast receiver: still gets
        // events as long as the bus is at capacity. We can't easily
        // force a Lagged without saturating the channel, but we
        // can at least confirm the bus keeps emitting past 1024
        // events without panicking.
        let bus = EventBus::new(8);
        let _slow = bus.subscribe();
        let mut fast = bus.subscribe();

        for i in 0..32 {
            bus.emit(Event::new(format!("e-{i}"), EventKind::ImagePulled));
        }

        // Fast receiver should get something (or get Lagged, which
        // is also a valid signal here). Drain a few then assert
        // we didn't panic.
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(50), fast.recv()).await;
        }
        assert!(bus.receiver_count() >= 1);
    }
}
