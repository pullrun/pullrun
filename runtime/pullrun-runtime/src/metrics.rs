//! Prometheus metrics for the Pullrun runtime.
//!
//! Uses the `metrics` facade with a `metrics-exporter-prometheus` backend.
//! All call sites go through the facade (`counter!`, `gauge!`, `histogram!`),
//! so the recorder can be swapped (e.g. for OTLP in v1) without touching
//! the instrumentation in `service.rs`.
//!
//! ## Exposed metrics
//!
//! | Name | Type | Labels | Meaning |
//! |---|---|---|---|
//! | `pullrun_pulls_total` | counter | `registry`, `status` | Image pulls, by outcome |
//! | `pullrun_pull_duration_seconds` | histogram | — | Wall time of `PullImage` |
//! | `pullrun_workloads_started_total` | counter | `backend` | Workloads that successfully started |
//! | `pullrun_workloads_exited_total` | counter | `backend`, `exit_code` | Workloads that exited (via stop) |
//! | `pullrun_workloads_running` | gauge | `backend` | Current count of running workloads |
//! | `pullrun_workload_start_duration_seconds` | histogram | — | Wall time of `RunWorkload` (request → started) |
//! | `pullrun_store_nodes` | gauge | — | DAG nodes currently in the in-process cache |
//! | `pullrun_store_bytes` | gauge | — | Total mmap'd bytes across cached nodes |
//! | `pullrun_build_info` | gauge | `version` | Build metadata, always `1` |
//!
//! ## HTTP endpoint
//!
//! `install_recorder()` sets the global recorder and returns a handle that
//! can render the current snapshot. `service::run_daemon` exposes that
//! on `/metrics` over HTTP. `/healthz` is exposed alongside for K8s probes.
//!
//! ## Calling convention
//!
//! - Use `histogram!(NAME).start_timer()` for latency around an await.
//! - Use `counter!(NAME, "label" => value).increment(1)` for event counts.
//! - Use `gauge!(NAME, "label" => value).set(f64)` for instantaneous values.
//!
//! Label values are interpolated into the Prometheus text format, so
//! keep them bounded (enums / known strings). Free-form input must be
//! hashed or bucketed to avoid label cardinality explosion.

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Instant;

use axum::{routing::get, Router};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tracing::{info, warn};

// Reentrancy: `OnceLock::get_or_init` runs the init closure exactly
// once even under concurrent callers, and any later caller receives
// a clone of the value the first call stashed. This is the right
// primitive for "install a process-wide global recorder, return its
// handle"; `OnceLock::set` would race and only one of N callers
// would succeed.
static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// All metric names. Keeping them as constants avoids typos and makes
/// it easy to grep for usages.
pub mod names {
    pub const PULLS_TOTAL: &str = "pullrun_pulls_total";
    pub const PULL_DURATION_SECONDS: &str = "pullrun_pull_duration_seconds";
    pub const WORKLOADS_STARTED_TOTAL: &str = "pullrun_workloads_started_total";
    pub const WORKLOADS_EXITED_TOTAL: &str = "pullrun_workloads_exited_total";
    pub const WORKLOADS_RUNNING: &str = "pullrun_workloads_running";
    pub const WORKLOAD_START_DURATION_SECONDS: &str = "pullrun_workload_start_duration_seconds";
    pub const STORE_NODES: &str = "pullrun_store_nodes";
    pub const STORE_BYTES: &str = "pullrun_store_bytes";
    pub const BUILD_INFO: &str = "pullrun_build_info";
    pub const SYNC_PEER_COUNT: &str = "pullrun_sync_peer_count";
}

const CARGO_PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Install the Prometheus recorder as the global metrics recorder.
///
/// Idempotent under concurrent callers: `get_or_init` runs the init
/// closure exactly once, and any racing caller receives a clone of
/// the value the first caller stashed. This is the only safe shape
/// for installing a process-wide global; manual `set` would race
/// and panic in the loser.
///
/// Returns the handle that can be used to render the current snapshot
/// for an HTTP `/metrics` response.
pub fn install_recorder() -> PrometheusHandle {
    HANDLE
        .get_or_init(|| {
            let builder_res = PrometheusBuilder::new()
                // Pick explicit histogram buckets so the quantile
                // summaries line up with operator expectations:
                // sub-second for workload startup, multi-second
                // for image pulls (network bound).
                .set_buckets_for_metric(
                    Matcher::Full(names::PULL_DURATION_SECONDS.to_string()),
                    &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
                )
                .and_then(|b| {
                    b.set_buckets_for_metric(
                        Matcher::Full(names::WORKLOAD_START_DURATION_SECONDS.to_string()),
                        &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0],
                    )
                });

            let builder = match builder_res {
                Ok(b) => b,
                Err(e) => {
                    warn!(error = %e, "could not set histogram buckets; using default");
                    PrometheusBuilder::new()
                }
            };

            let handle = builder
                .install_recorder()
                .expect("first install of metrics recorder (subsequent installs reuse the cached handle)");
            describe_metrics();
            handle
        })
        .clone()
}

/// Register metric descriptions (HELP/TYPE) in the Prometheus output.
/// Called once at startup. Safe to call multiple times.
pub fn describe_metrics() {
    describe_counter!(
        names::PULLS_TOTAL,
        "Total image pulls handled by the runtime, labeled by registry and final status"
    );
    describe_counter!(
        names::WORKLOADS_STARTED_TOTAL,
        "Total workloads that reached the running state, labeled by backend (container/vm)"
    );
    describe_counter!(
        names::WORKLOADS_EXITED_TOTAL,
        "Total workload exits, labeled by backend and exit_code (string \"0\"..\"255\" or \"unknown\")"
    );
    describe_gauge!(
        names::WORKLOADS_RUNNING,
        "Workloads currently in the running state, labeled by backend"
    );
    describe_histogram!(
        names::PULL_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Wall-clock duration of a successful or failed image pull"
    );
    describe_histogram!(
        names::WORKLOAD_START_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Wall-clock duration of a RunWorkload request (create + start)"
    );
    describe_gauge!(
        names::STORE_NODES,
        metrics::Unit::Count,
        "DAG nodes currently held in the in-process mmap cache"
    );
    describe_gauge!(
        names::STORE_BYTES,
        metrics::Unit::Bytes,
        "Total bytes mmap'd across cached DAG nodes"
    );
    describe_gauge!(
        names::BUILD_INFO,
        "Build metadata, always 1; the version label is the only useful field"
    );
    describe_gauge!(
        names::SYNC_PEER_COUNT,
        "Current number of block sync peers known via mDNS / bloom gossip"
    );

    // Set the static build-info gauge once. Operators use this for
    // version skew detection (e.g. a fleet where some nodes are still
    // on v0.1.0 and others are on v0.2.0).
    gauge!(names::BUILD_INFO, "version" => CARGO_PKG_VERSION.to_string()).set(1.0);
}

/// Build the axum router that serves `/metrics` and `/healthz`.
pub fn router(handle: PrometheusHandle) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(move || {
                let handle = handle.clone();
                async move { handle.render() }
            }),
        )
        .route(
            "/healthz",
            get(|| async { (axum::http::StatusCode::OK, "ok\n") }),
        )
}

/// Bind the metrics HTTP server to `addr` and serve until the process
/// exits. Intended to be `tokio::spawn`ed from `run_daemon`.
///
/// Returns immediately if `addr` cannot be parsed; the gRPC server keeps
/// running and operators get a clear log line instead of a panic.
pub async fn serve(addr: SocketAddr, handle: PrometheusHandle) {
    let app = router(handle);
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            info!(%addr, "metrics endpoint listening on http://{addr}/metrics");
            if let Err(e) = axum::serve(listener, app).await {
                warn!(error = %e, "metrics HTTP server exited with error");
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                %addr,
                "could not bind metrics HTTP server (gRPC daemon continues; metrics unavailable)"
            );
        }
    }
}

/// Convenience: increment the `pulls_total` counter with the canonical
/// labels. Use this instead of hand-rolling the macro call to keep the
/// label set consistent across call sites.
pub fn record_pull(registry: &str, status: &'static str) {
    counter!(
        names::PULLS_TOTAL,
        "registry" => registry.to_string(),
        "status" => status,
    )
    .increment(1);
}

/// Convenience: increment the workloads-started counter and bump the
/// running gauge. The exit path (`record_exit`) decrements the same
/// gauge, so the two must agree on the `backend` label value.
pub fn record_workload_started(backend: &str) {
    counter!(
        names::WORKLOADS_STARTED_TOTAL,
        "backend" => backend.to_string(),
    )
    .increment(1);
    gauge!(names::WORKLOADS_RUNNING, "backend" => backend.to_string()).increment(1.0);
}

/// Convenience: increment the workloads-exited counter and decrement
/// the running gauge. `exit_code` is `Some(0)` for clean stops,
/// `Some(n)` for crashes, or `None` if the executor didn't report one.
pub fn record_workload_exit(backend: &str, exit_code: Option<i32>) {
    let code_label = match exit_code {
        Some(c) => c.to_string(),
        None => "unknown".to_string(),
    };
    counter!(
        names::WORKLOADS_EXITED_TOTAL,
        "backend" => backend.to_string(),
        "exit_code" => code_label,
    )
    .increment(1);
    gauge!(names::WORKLOADS_RUNNING, "backend" => backend.to_string()).decrement(1.0);
}

/// Convenience: set the store-size gauges. Called by the periodic
/// updater in `run_daemon` (every 60s by default).
pub fn record_store_stats(nodes: usize, bytes: u64) {
    gauge!(names::STORE_NODES).set(nodes as f64);
    gauge!(names::STORE_BYTES).set(bytes as f64);
}

/// Convenience: set the block sync peer count gauge. Called by the
/// periodic updater in `run_daemon` (every 30s).
pub fn record_sync_peer_count(count: usize) {
    gauge!(names::SYNC_PEER_COUNT).set(count as f64);
}

/// RAII timer for `pullrun_pull_duration_seconds`. Construct at the
/// top of `pull_image`, drop at the end. Records elapsed seconds to
/// the histogram on drop.
pub struct PullTimer {
    start: Instant,
}

impl PullTimer {
    pub fn start() -> Self {
        Self { start: Instant::now() }
    }
}

impl Drop for PullTimer {
    fn drop(&mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        histogram!(names::PULL_DURATION_SECONDS).record(secs);
    }
}

/// RAII timer for `pullrun_workload_start_duration_seconds`. Same shape
/// as `PullTimer`, kept as a distinct type so call sites can't mix
/// the two up.
pub struct StartTimer {
    start: Instant,
}

impl StartTimer {
    pub fn start() -> Self {
        Self { start: Instant::now() }
    }
}

impl Drop for StartTimer {
    fn drop(&mut self) {
        let secs = self.start.elapsed().as_secs_f64();
        histogram!(names::WORKLOAD_START_DURATION_SECONDS).record(secs);
    }
}

/// Backward-compat aliases for the original `register_*_timer` naming.
/// The `_` suffix version is the old name; new call sites should use
/// `PullTimer::start()` / `StartTimer::start()` directly.
pub fn register_pull_timer() -> PullTimer {
    PullTimer::start()
}
pub fn register_start_timer() -> StartTimer {
    StartTimer::start()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_recorder_succeeds_twice() {
        // The second install must not panic, even though the global
        // recorder is already set. Useful in tests that spin up
        // multiple `RuntimeService` instances in the same process.
        let _h1 = install_recorder();
        let _h2 = install_recorder();
    }

    #[test]
    fn describe_metrics_is_idempotent() {
        install_recorder();
        describe_metrics();
        describe_metrics();
    }

    #[test]
    fn record_helpers_dont_panic() {
        install_recorder();
        record_pull("docker.io", "success");
        record_pull("ghcr.io", "failed");
        record_workload_started("container");
        record_workload_started("vm");
        record_workload_exit("container", Some(0));
        record_workload_exit("vm", Some(137));
        record_workload_exit("container", None);
        record_store_stats(42, 1_234_567);
    }

    #[tokio::test]
    async fn render_after_increment_contains_pullrun_prefix() {
        // Render the global recorder; ensure all our names appear
        // (even if zero) so the Prometheus text format is valid.
        let h = install_recorder();
        record_pull("docker.io", "success");
        record_workload_started("container");
        record_workload_exit("container", Some(0));
        record_store_stats(7, 4096);
        let body = h.render();
        assert!(body.contains("pullrun_pulls_total"), "missing pulls_total in render");
        assert!(
            body.contains("pullrun_workloads_started_total"),
            "missing workloads_started_total in render"
        );
        assert!(
            body.contains("pullrun_workloads_exited_total"),
            "missing workloads_exited_total in render"
        );
        assert!(body.contains("pullrun_workloads_running"));
        assert!(body.contains("pullrun_store_nodes"));
        assert!(body.contains("pullrun_store_bytes"));
        assert!(body.contains("pullrun_build_info"));
    }

    #[test]
    fn router_includes_metrics_and_healthz() {
        // Smoke-test the router shape: must construct without
        // panicking. The HTTP-level assertion that both routes
        // actually respond lives in the runtime integration test
        // (an axum `Router` doesn't expose a route list without
        // typed-routing, and the routes are dynamic anyway).
        let h = install_recorder();
        let _r = router(h);
    }
}
