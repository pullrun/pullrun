// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end inspect and events tests.
//!
//! These tests build a `RuntimeService` directly (no gRPC socket),
//! insert a workload state by hand, and verify that `inspect_workload`
//! returns the expected snapshot. We do not test `run_workload` here
//! (it needs an OCI runtime); we test the inspect path and the
//! `WorkloadState` → `InspectResponse` translation.

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use pullrun_runtime::proto::runtime_server::Runtime;
    use pullrun_runtime::service::{RuntimeCommand, ServiceConfig};

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pullrun-runtime-inspect-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn inspect_unknown_workload_returns_found_false() {
        // Build a service with no policy. The service constructs the
        // workloads map, the executor, the event bus, and the
        // background watcher. The watcher spawns a tokio task; for
        // this test we just need the service to come up and respond
        // to inspect calls.
        let dir = fresh_dir("unknown");
        let cfg = ServiceConfig::new(dir);
        let svc = RuntimeCommand::new(cfg).service().await;

        // Drive the trait method directly. We don't have a tonic
        // Request wrapper in scope; build one on the fly. The
        // InspectRequest is just { id: String }, so this is cheap.
        let req = tonic::Request::new(pullrun_runtime::proto::InspectRequest {
            id: "wl-nonexistent".to_string(),
        });
        let resp = svc.inspect_workload(req).await.expect("inspect rpc");
        let inner = resp.into_inner();
        assert!(!inner.found, "unknown workload should return found=false");
        assert_eq!(inner.id, "wl-nonexistent");
        assert_eq!(inner.state, "unknown");
    }

    #[tokio::test]
    async fn inspect_known_workload_returns_full_snapshot() {
        // Build the service.
        let dir = fresh_dir("known");
        let cfg = ServiceConfig::new(dir);
        let svc = RuntimeCommand::new(cfg).service().await;

        // Insert a synthetic workload. We bypass `run_workload` and
        // write the WorkloadState directly into the shared map. This
        // is what the run path does in v0 anyway; the inspect path
        // only reads the map.
        let id = "wl-test-1".to_string();
        let image_root = "sha256:deadbeef".to_string();
        let command = vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()];
        let network_rule = pullrun_net::NetworkRule {
            direction: pullrun_net::Direction::Inbound,
            protocol: pullrun_net::Protocol::Tcp,
            port: 8080,
            host_port: 0,
            to_host: None,
            from_cidrs: Some(vec!["10.0.0.0/8".to_string()]),
        };
        let mut policy_decisions = HashMap::new();
        policy_decisions.insert("default".to_string(), "allow".to_string());

        {
            let mut map = svc.workloads.write().await;
            map.insert(
                id.clone(),
                pullrun_runtime::service::WorkloadState {
                    status: "running".to_string(),
                    start_time: 1_700_000_000,
                    exit_time: 0,
                    exit_code: None,
                    backend: "container".to_string(),
                    internal_ip: Some("10.42.0.5".to_string()),
                    pid: 1234,
                    image_root: image_root.clone(),
                    command: command.clone(),
                    network_rules: vec![network_rule.clone()],
                    policy_decisions: policy_decisions.clone(),
                    kernel_image_ref: String::new(),
                    kernel_image_digest: String::new(),
                    working_dir: String::new(),
                    rootfs_dir: None,
                    health_check: None,
                    health: String::new(),
                    health_failures: 0,
                    health_last_success: 0,
                    restart_policy: Default::default(),
                    restart_count: 0,
                    env: Default::default(),
                    cpu_millicores: None,
                    memory_bytes: None,
                    bridge_name: None,
                    network_mode: String::new(),
                    mounts: vec![],
                    privileged: false,
                    readonly_rootfs: false,
                    no_new_privileges: false,
                    seccomp_profile: None,
                    allowed_syscalls: vec![],
                    console_log_path: None,
                },
            );
        }

        // Inspect it.
        let req = tonic::Request::new(pullrun_runtime::proto::InspectRequest { id: id.clone() });
        let resp = svc.inspect_workload(req).await.expect("inspect rpc");
        let inner = resp.into_inner();

        assert!(inner.found);
        assert_eq!(inner.id, id);
        assert_eq!(inner.state, "running");
        assert_eq!(inner.backend, "container");
        assert_eq!(inner.image_root, image_root);
        assert_eq!(inner.internal_ip, "10.42.0.5");
        assert_eq!(inner.pid, 1234);
        assert_eq!(inner.start_time, 1_700_000_000);
        assert_eq!(inner.exit_time, 0);
        assert_eq!(inner.exit_code, 0);
        assert_eq!(inner.command, command);
        assert_eq!(inner.network_rules.len(), 1);
        assert_eq!(inner.network_rules[0].direction, "inbound");
        assert_eq!(inner.network_rules[0].protocol, "tcp");
        assert_eq!(inner.network_rules[0].port, 8080);
        assert_eq!(
            inner.network_rules[0].from_cidrs,
            vec!["10.0.0.0/8".to_string()]
        );
        assert_eq!(
            inner.policy_decisions.get("default"),
            Some(&"allow".to_string())
        );
    }

    #[tokio::test]
    async fn event_to_proto_conversion_preserves_fields() {
        // The `From<Event> for proto::Event` impl in lib.rs is
        // used by the `stream_events` RPC to forward events to
        // gRPC clients. Verify the conversion is lossless.
        let ev = pullrun_runtime::Event::new("wl-1", pullrun_runtime::EventKind::WorkloadStarted)
            .with_metadata("backend", "container")
            .with_metadata("pid", "1234");
        let proto: pullrun_runtime::proto::Event = ev.clone().into();
        assert_eq!(proto.id, ev.id);
        assert_eq!(proto.kind, "WORKLOAD_STARTED");
        assert_eq!(proto.timestamp, ev.timestamp as i64);
        assert_eq!(
            proto.metadata.get("backend").map(|s| s.as_str()),
            Some("container")
        );
        assert_eq!(proto.metadata.get("pid").map(|s| s.as_str()), Some("1234"));
    }
}
