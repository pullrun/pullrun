//! Runtime service: the gRPC server side of nimbus-runtime.
//!
//! This module wires the policy engine into the pull and run paths. The
//! security contract is:
//!
//! - `PullImage` will not return success for an image that the configured
//!   policy rejects (signature missing/invalid, vulnerable SBOM, banned
//!   license, etc.).
//! - `RunWorkload` re-evaluates the policy as defense in depth. This catches
//!   the case where the policy was tightened after the image was pulled.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tonic::Status;
use tracing::{debug, info, warn};

use nimbus_exec::types::{Backend, ExitStatus, NetworkMode, WorkloadSpec};
use nimbus_exec::{ExecError, Executor, LinuxContainerExecutor, NetworkRule, ProcessHandle};
use nimbus_exec::{current_euid, is_running_as_root, RootlessContainerExecutor};
use nimbus_net::{Ipam, ProxyNetwork};
use nimbus_oci::{OciMaterializer, OciPuller, OciToDagConverter, DagPusher, OciAuth, export_dag_to_tar, import_dag_from_tar};
use nimbus_policy::{CosignKey, Policy, PolicyDecision, PolicyEngine};
use nimbus_store::MmapStore;
use nimbus_vm::{FirecrackerConfig, FirecrackerExecutor, StagedKernel};

use crate::events::{Event, EventBus, EventKind};
use crate::proto::runtime_server::Runtime;
use crate::proto::{
    AttachMessage, CopyFileRequest, CopyFileResponse, DagNode, Event as ProtoEvent, ExecRequest,
    ExecResponse, GetWorkloadRequest, HasImageRequest, HasImageResponse, InspectRequest,
    InspectResponse, ListImagesRequest, ListImagesResponse, ListWorkloadsRequest,
    ListWorkloadsResponse, LogChunk, Mount as ProtoMount, NetworkRule as ProtoNetworkRule,
    PullImageRequest, PullImageResponse, RemoveImageRequest, RemoveImageResponse,
    RunComposeRequest, RunComposeResponse, DagStoreInfoRequest, DagStoreInfoResponse,
    RunRequest, RunResponse, StopRequest, StopResponse, StreamEventsRequest, StreamLogsRequest,
    UpdateWorkloadRequest, UpdateWorkloadResponse, WorkloadStatus, PortForwardRequest,
    PortForwardData, GetWorkloadStatsRequest, WorkloadStats as ProtoWorkloadStats,
    BuildImageRequest, BuildImageResponse,
    PushImageRequest, PushImageResponse,
    ExportImageRequest, ExportImageChunk,
    ImportImageChunk, ImportImageResponse,
};

use crate::metrics::{
    record_pull, record_workload_exit, record_workload_started, register_pull_timer,
    register_start_timer,
};

/// Optional Firecracker VM backend configuration. When set, the runtime
/// will route `Backend::Vm` workloads to a `FirecrackerExecutor` that
/// shares the IPAM and proxy with the container backend.
#[derive(Clone, Debug)]
pub struct VmBackendConfig {
    pub firecracker_path: PathBuf,
    pub kernel_path: PathBuf,
    pub vm_root: PathBuf,
    pub vcpus: u8,
    pub mem_mib: u32,
    pub size_mb: u64,
}

/// All knobs needed to build a `RuntimeService`.
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub store_root: PathBuf,
    pub bundle_root: PathBuf,
    pub checkpoints_dir: PathBuf,
    pub policy: Option<Policy>,
    pub trusted_keys: Vec<CosignKey>,
    pub vm_backend: Option<VmBackendConfig>,
    /// Registries to reach over plain HTTP. Useful for
    /// local dev with `registry:2` containers or
    /// self-hosted registries without TLS.
    pub insecure_registries: std::collections::HashSet<String>,
}

impl ServiceConfig {
    pub fn new(store_root: PathBuf) -> Self {
        let bundle_root = store_root.join("bundles");
        let checkpoints_dir = store_root.join("checkpoints");
        Self {
            store_root,
            bundle_root,
            checkpoints_dir,
            policy: None,
            trusted_keys: Vec::new(),
            vm_backend: None,
            insecure_registries: std::collections::HashSet::new(),
        }
    }

    pub fn with_policy(mut self, p: Policy) -> Self {
        self.policy = Some(p);
        self
    }

    pub fn add_trusted_key(mut self, key: CosignKey) -> Self {
        self.trusted_keys.push(key);
        self
    }

    pub fn trusted_keys(mut self, keys: Vec<CosignKey>) -> Self {
        self.trusted_keys = keys;
        self
    }

    pub fn with_vm_backend(mut self, cfg: VmBackendConfig) -> Self {
        self.vm_backend = Some(cfg);
        self
    }

    pub fn with_insecure_registries(
        mut self,
        registries: std::collections::HashSet<String>,
    ) -> Self {
        self.insecure_registries = registries;
        self
    }
}

/// Executor dispatcher: routes to container or VM backend based on
/// `Backend`. Both backends share the same IPAM and proxy so workloads
/// are on the same L2 segment.
pub struct ExecutorRouter {
    container: Arc<LinuxContainerExecutor>,
    rootless: Option<Arc<RootlessContainerExecutor>>,
    vm: Option<Arc<FirecrackerExecutor>>,
    proxy: Arc<ProxyNetwork>,
}

impl ExecutorRouter {
    pub fn container(&self) -> &Arc<LinuxContainerExecutor> {
        &self.container
    }

    pub fn proxy(&self) -> &Arc<ProxyNetwork> {
        &self.proxy
    }

    pub fn ipam(&self) -> Arc<Ipam> {
        self.proxy.ipam_handle()
    }
}

#[async_trait]
impl Executor for ExecutorRouter {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        match spec.backend {
            Backend::Container => {
                // Auto-detect: if a rootless executor is configured and
                // we are not running as root, use the rootless path so
                // pasta/slirp4netns handles networking without iptables.
                if let Some(ref rootless) = self.rootless {
                    if !is_running_as_root() {
                        return rootless.create(spec).await;
                    }
                }
                self.container.create(spec).await
            }
            Backend::ContainerRootless => match &self.rootless {
                Some(exec) => exec.create(spec).await,
                None => Err(ExecError::BackendNotAvailable(
                    "Rootless container backend not configured (run as non-root or configure --rootless)".into(),
                )),
            },
            Backend::Vm => match &self.vm {
                Some(vm) => vm.create(spec).await,
                None => Err(ExecError::BackendNotAvailable(
                    "VM backend not configured (use --vm-firecracker etc.)".into(),
                )),
            },
            Backend::Sandbox => Err(ExecError::BackendNotAvailable(
                "Sandbox backend is a Phase 5 stub".into(),
            )),
        }
    }

    async fn start(&self, handle: &ProcessHandle) -> Result<(), ExecError> {
        match handle.backend.as_str() {
            "container" => self.container.start(handle).await,
            "container-rootless" => match &self.rootless {
                Some(exec) => exec.start(handle).await,
                None => Err(ExecError::BackendNotAvailable(
                    "Rootless container backend not configured".into(),
                )),
            },
            "vm" => match &self.vm {
                Some(vm) => vm.start(handle).await,
                None => Err(ExecError::BackendNotAvailable("VM backend not configured".into())),
            },
            other => Err(ExecError::BackendNotAvailable(format!(
                "unknown backend in handle: {other}"
            ))),
        }
    }

    async fn stop(&self, id: &str) -> Result<(), ExecError> {
        // Try rootless first (cheap — checks bundle dir, won't touch
        // non-rootless workloads).
        if let Some(ref rootless) = self.rootless {
            if rootless.bundle_dir_for(id).exists() {
                return rootless.stop(id).await;
            }
        }
        // Try VM next (sidecar file check).
        if let Some(vm) = &self.vm {
            let sidecar = vm.sidecar_path_for(id);
            if sidecar.exists() {
                return vm.stop(id).await;
            }
        }
        // Fall back to regular container.
        self.container.stop(id).await
    }

    async fn wait(&self, id: &str) -> Result<ExitStatus, ExecError> {
        // Try rootless first.
        if let Some(ref rootless) = self.rootless {
            if rootless.bundle_dir_for(id).exists() {
                return rootless.wait(id).await;
            }
        }
        // Try VM next.
        if let Some(vm) = &self.vm {
            let sidecar = vm.sidecar_path_for(id);
            if sidecar.exists() {
                return vm.wait(id).await;
            }
        }
        // Fall back to regular container.
        self.container.wait(id).await
    }

    async fn status(&self, id: &str) -> Result<String, ExecError> {
        // Try rootless first.
        if let Some(ref rootless) = self.rootless {
            if rootless.bundle_dir_for(id).exists() {
                return rootless.status(id).await;
            }
        }
        // Try VM next.
        if let Some(vm) = &self.vm {
            let sidecar = vm.sidecar_path_for(id);
            if sidecar.exists() {
                return vm.status(id).await;
            }
        }
        // Fall back to regular container.
        self.container.status(id).await
    }

    async fn update(
        &self,
        id: &str,
        cpu_millicores: Option<u64>,
        memory_bytes: Option<u64>,
    ) -> Result<(), ExecError> {
        // Try rootless first.
        if let Some(ref rootless) = self.rootless {
            if rootless.bundle_dir_for(id).exists() {
                return rootless.update(id, cpu_millicores, memory_bytes).await;
            }
        }
        // Try VM next.
        if let Some(vm) = &self.vm {
            let sidecar = vm.sidecar_path_for(id);
            if sidecar.exists() {
                return vm.update(id, cpu_millicores, memory_bytes).await;
            }
        }
        // Fall back to regular container.
        self.container.update(id, cpu_millicores, memory_bytes).await
    }

    async fn stats(&self, id: &str) -> Result<nimbus_exec::WorkloadStats, ExecError> {
        // Try rootless first.
        if let Some(ref rootless) = self.rootless {
            if rootless.bundle_dir_for(id).exists() {
                return rootless.stats(id).await;
            }
        }
        // Try VM next.
        if let Some(vm) = &self.vm {
            let sidecar = vm.sidecar_path_for(id);
            if sidecar.exists() {
                return vm.stats(id).await;
            }
        }
        // Fall back to regular container.
        self.container.stats(id).await
    }

    async fn exec(&self, id: &str, command: &[String], timeout_secs: u64) -> Result<i32, ExecError> {
        if let Some(ref rootless) = self.rootless {
            if rootless.bundle_dir_for(id).exists() {
                return rootless.exec(id, command, timeout_secs).await;
            }
        }
        if let Some(vm) = &self.vm {
            let sidecar = vm.sidecar_path_for(id);
            if sidecar.exists() {
                return vm.exec(id, command, timeout_secs).await;
            }
        }
        self.container.exec(id, command, timeout_secs).await
    }
}

/// Builder: call `.service()` to get the actual gRPC service.
pub struct RuntimeCommand {
    config: ServiceConfig,
}

impl RuntimeCommand {
    pub fn new(config: ServiceConfig) -> Self {
        Self { config }
    }

    pub fn service(&self) -> RuntimeService {
        let store = Arc::new(MmapStore::new(self.config.store_root.clone()));
        if let Some(parent) = self.config.bundle_root.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::create_dir_all(&self.config.bundle_root);
        let _ = std::fs::create_dir_all(&self.config.checkpoints_dir);

        let policy_engine = self.config.policy.as_ref().map(|p| {
            Arc::new(
                PolicyEngine::new(p.clone())
                    .with_trusted_keys(self.config.trusted_keys.clone()),
            )
        });

        if let Some(engine) = &policy_engine {
            info!(
                "policy engine enabled: required_signature={}, require_sbom={}, max_cvss={:?}, deny_licenses={:?}, trusted_keys={}",
                engine.default_policy().required_signature,
                engine.default_policy().require_sbom,
                engine.default_policy().max_cvss_score,
                engine.default_policy().deny_licenses,
                engine.trusted_keys().len(),
            );
        } else {
            info!("policy engine disabled");
        }

        // Shared IPAM + proxy. The container backend uses
        // ProxyNetwork::setup() (allocates IP + starts listeners); the
        // VM backend uses ProxyNetwork::register_endpoint() (starts
        // listeners only — VM has already allocated from the same IPAM
        // and attached a tap device to the shared bridge).
        let proxy = Arc::new(
            ProxyNetwork::new().expect("ProxyNetwork::new requires valid CIDR"),
        );
        let ipam = proxy.ipam_handle();
        info!("shared workload network: 10.42.0.0/16 (bridge {})", nimbus_vm::BRIDGE_NAME);

        let container = Arc::new(
            LinuxContainerExecutor::new(
                MmapStore::new(self.config.store_root.clone()),
                None,
                self.config.bundle_root.clone(),
            )
            .with_network(ipam.clone(), proxy.clone()),
        );

        let vm = self.config.vm_backend.as_ref().map(|cfg| {
            let fc_cfg = FirecrackerConfig {
                firecracker_path: cfg.firecracker_path.clone(),
                kernel_path: cfg.kernel_path.clone(),
                rootfs_dir: cfg.vm_root.clone(),
                jailer_path: None,
                vcpus: cfg.vcpus,
                mem_mib: cfg.mem_mib,
                size_mb: cfg.size_mb,
            };
            let _ = std::fs::create_dir_all(&cfg.vm_root);
            Arc::new(FirecrackerExecutor::new(
                fc_cfg,
                Arc::new(MmapStore::new(self.config.store_root.clone())),
                ipam.clone(),
                proxy.clone(),
            ))
        });

        if vm.is_some() {
            info!("VM backend enabled (firecracker)");
        } else {
            info!("VM backend disabled");
        }

        // Rootless container executor: auto-detected when running as non-root.
        let rootless = if !is_running_as_root() {
            let uid = current_euid();
            info!("enabling rootless container backend (euid={})", uid);
            Some(Arc::new(RootlessContainerExecutor::new(
                MmapStore::new(self.config.store_root.clone()),
                self.config.bundle_root.clone(),
                uid,
            )))
        } else {
            info!("rootless container backend disabled (running as root)");
            None
        };

        let executor = Arc::new(ExecutorRouter {
            container,
            rootless,
            vm,
            proxy: proxy.clone(),
        });

        let event_bus = Arc::new(EventBus::default());
        // Load persisted workload checkpoints so the runtime survives
        // restarts without losing tracked state. Any workload that was
        // "running" at crash time is conservatively marked "exited" —
        // the executor can't be relied on to still own the process.
        let mut recovered = load_workload_checkpoints(&self.config.checkpoints_dir);
        for state in recovered.values_mut() {
            if state.status == "running" {
                info!(
                    "workload running at last checkpoint; marking as exited (post-crash recovery)"
                );
                state.status = "exited".to_string();
                state.exit_code = state.exit_code.or(Some(137));
                if state.exit_time == 0 {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    state.exit_time = now;
                }
            }
            info!(
                status = %state.status,
                "recovered workload state"
            );
        }
        let workloads: Arc<RwLock<HashMap<String, WorkloadState>>> =
            Arc::new(RwLock::new(recovered));
        let image_tags: Arc<RwLock<HashMap<String, String>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn the workload-exit watcher. Every 5s it walks the
        // `workloads` map and asks the executor for the live status
        // of each id. If the executor reports anything other than
        // "running" and the map still says "running", we mark the
        // workload as exited and emit a `WorkloadExited` event.
        //
        // The watcher keeps a small set of "already announced"
        // workload ids so it doesn't double-emit on subsequent ticks.
        // This works because `WorkloadExited` is a terminal state
        // for a workload (no further exit events are possible).
        let watcher_bus = event_bus.clone();
        let watcher_executor = executor.clone();
        let watcher_workloads = workloads.clone();
        let watcher_store = store.clone();
        let watcher_checkpoints_dir = self.config.checkpoints_dir.clone();
        tokio::spawn(async move {
            use std::collections::HashSet;
            use std::time::Duration;
            let mut announced: HashSet<String> = HashSet::new();
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            // Skip the first immediate tick; nothing has had time to
            // exit yet. `interval::tick` fires immediately on the
            // first call by default; we drain that here.
            interval.tick().await;
            loop {
                interval.tick().await;

                // Snapshot the ids + status without holding the lock
                // across the .await.
                let snapshot: Vec<(String, String, String)> = {
                    let map = watcher_workloads.read().await;
                    map.iter()
                        .filter(|(_, s)| s.status == "running")
                        .map(|(id, s)| (id.clone(), s.backend.clone(), s.status.clone()))
                        .collect()
                };

                for (id, backend, _status) in snapshot {
                    if announced.contains(&id) {
                        continue;
                    }
                    // Ask the executor for the live status. The
                    // contract of Executor::status is to return a
                    // short string like "running" or "exited" with an
                    // optional exit code embedded. For runc, this
                    // inspects the container's state.json.
                    match watcher_executor.status(&id).await {
                        Ok(s) if s != "running" && !s.is_empty() => {
                            // Parse an exit code out of the status
                            // string if present. Format is executor-
                            // specific; v0 only does the simplest
                            // "exit_code=NN" suffix and the runc
                            // "stopped" / "exited" strings.
                            let exit_code = parse_exit_code(&s);
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            let mut exit_code_for_restart = None;
                            let mut should_restart = false;
                            {
                                let mut map = watcher_workloads.write().await;
                                if let Some(state) = map.get_mut(&id) {
                                    // Only mark as exited if still running
                                    // (prevents race with operator stop).
                                    if state.status != "running" {
                                        continue;
                                    }
                                    state.status = "exited".to_string();
                                    state.exit_time = now;
                                    state.exit_code = exit_code;
                                    exit_code_for_restart = exit_code;
                                    // Check restart policy before dropping lock.
                                    should_restart = matches!(
                                        state.restart_policy,
                                        nimbus_exec::types::RestartPolicy::Always
                                            | nimbus_exec::types::RestartPolicy::UnlessStopped
                                    ) || (matches!(
                                        state.restart_policy,
                                        nimbus_exec::types::RestartPolicy::OnFailure
                                    ) && exit_code != Some(0));
                                    let checkpoint = state.clone();
                                    // Drop the lock before writing to disk.
                                    drop(map);
                                    write_workload_checkpoint(
                                        &watcher_checkpoints_dir,
                                        &id,
                                        &checkpoint,
                                    );
                                } else {
                                    // No state to checkpoint if the workload
                                    // was already removed from the map.
                                }
                            }
                            record_workload_exit(
                                &backend,
                                exit_code_for_restart.map(|c| c as i32),
                            );
                            watcher_bus.emit(
                                Event::new(&id, EventKind::WorkloadExited)
                                    .with_metadata("backend", &backend)
                                    .with_metadata(
                                        "exit_code",
                                        exit_code_for_restart
                                            .map(|c| c.to_string())
                                            .unwrap_or_else(|| "unknown".into()),
                                    )
                                    .with_metadata("source", "watcher"),
                            );
                            // Attempt automatic restart if policy allows.
                            if should_restart {
                                attempt_restart(
                                    &watcher_executor,
                                    &watcher_workloads,
                                    &watcher_store,
                                    &watcher_checkpoints_dir,
                                    &watcher_bus,
                                    &id,
                                    &backend,
                                )
                                .await;
                            } else {
                                announced.insert(id);
                            }
                        }
                        Ok(_) => {
                            // Still running; do nothing.
                        }
                        Err(e) => {
                            // Executor doesn't know about this id
                            // (it was probably never properly
                            // registered, or it was a fire-and-forget
                            // workload that already completed). Emit a
                            // best-effort exit event so consumers
                            // don't see a phantom "running" forever.
                            debug!(id = %id, error = %e, "watcher: executor.status failed; emitting best-effort exit");
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            let mut should_restart = false;
                            {
                                let mut map = watcher_workloads.write().await;
                                if let Some(state) = map.get_mut(&id) {
                                    if state.status != "running" {
                                        continue;
                                    }
                                    state.status = "exited".to_string();
                                    state.exit_time = now;
                                    state.exit_code = Some(137); // assume killed
                                    // Restart unless policy is No.
                                    should_restart = !matches!(
                                        state.restart_policy,
                                        nimbus_exec::types::RestartPolicy::No
                                    );
                                    let checkpoint = state.clone();
                                    drop(map);
                                    write_workload_checkpoint(
                                        &watcher_checkpoints_dir,
                                        &id,
                                        &checkpoint,
                                    );
                                }
                            }
                            record_workload_exit(&backend, Some(137));
                            watcher_bus.emit(
                                Event::new(&id, EventKind::WorkloadExited)
                                    .with_metadata("backend", &backend)
                                    .with_metadata("exit_code", "137")
                                    .with_metadata("source", "watcher_best_effort"),
                            );
                            if should_restart {
                                attempt_restart(
                                    &watcher_executor,
                                    &watcher_workloads,
                                    &watcher_store,
                                    &watcher_checkpoints_dir,
                                    &watcher_bus,
                                    &id,
                                    &backend,
                                )
                                .await;
                            } else {
                                announced.insert(id);
                            }
                        }
                    }
                }

                // Prune announced entries for ids that have been
                // removed from the workloads map (operator did a
                // manual cleanup). Prevents the HashSet from
                // growing without bound.
                let live: std::collections::HashSet<String> = {
                    let map = watcher_workloads.read().await;
                    map.keys().cloned().collect()
                };
                announced.retain(|id| live.contains(id));
            }
        });

        // Health check watcher: runs every 10 seconds, probes workloads
        // that have health_check configured, updates health status.
        let hc_workloads = workloads.clone();
        let hc_executor = executor.clone();
        tokio::spawn(async move {
            use std::time::Duration;
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.tick().await;
            loop {
                interval.tick().await;
                let probes: Vec<(String, Vec<String>, u32, u32, u32, i64)> = {
                    let map = hc_workloads.read().await;
                    map.iter()
                        .filter(|(_, s)| s.status == "running")
                        .filter_map(|(id, s)| {
                            s.health_check.as_ref().map(|hc| {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs() as i64)
                                    .unwrap_or(0);
                                (id.clone(), hc.test.clone(), hc.interval_seconds.max(1),
                                 hc.timeout_seconds.max(1), hc.retries.max(1),
                                 s.start_time + hc.start_period_seconds as i64)
                            })
                        })
                        .filter(|(_, _, interval, _, _, grace_end)| {
                            let interval = *interval as u64;
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as u64)
                                .unwrap_or(0);
                            now % interval == 0 || now >= *grace_end as u64
                        })
                        .collect()
                };
                for (id, test, _, timeout, retries, grace_end) in probes {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    if now < grace_end {
                        continue;
                    }
                    // Run the probe command via runc exec
                    let healthy = hc_executor.exec(&id, &test, timeout as u64).await.map(|r| r == 0).unwrap_or(false);
                    let mut map = hc_workloads.write().await;
                    if let Some(state) = map.get_mut(&id) {
                        if healthy {
                            state.health = "healthy".to_string();
                            state.health_failures = 0;
                            state.health_last_success = now;
                        } else {
                            state.health_failures += 1;
                            if state.health_failures >= retries {
                                state.health = "unhealthy".to_string();
                            } else if state.health != "unhealthy" {
                                state.health = "starting".to_string();
                            }
                        }
                    }
                }
            }
        });

        RuntimeService {
            store,
            policy_engine,
            workloads,
            executor,
            image_tags,
            event_bus: event_bus.clone(),
            kernel_cache: Arc::new(RwLock::new(HashMap::new())),
            rootfs_cache: Arc::new(RwLock::new(HashMap::new())),
            config: Arc::new(self.config.clone()),
        }
    }
}

/// BFS-walk the OCI DAG starting at `image_root`, returning a flat
/// list of `DagNode` proto messages ordered root-first. Cycles are
/// broken by a `visited` set; missing digests in the store are
/// silently skipped (the OCI converter never produces dangling edges
/// in v0, but the helper is defensive about that).
///
/// `size_bytes` is the on-disk file size for non-blob nodes (we
/// have the mmap length) and 0 for the manifest (the manifest is
/// inlined in the root `DagNode` and not stored as a separate
/// blob, in our converter). For blobs, we report 0 to keep this
/// helper's contract simple; a future version can resolve
/// blob sizes via `MmapStore::blob_path`.
fn walk_dag(store: &MmapStore, image_root: &str) -> Vec<DagNode> {
    use std::collections::{HashSet, VecDeque};
    let mut out: Vec<DagNode> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    if !image_root.is_empty() {
        queue.push_back(image_root.to_string());
    }

    while let Some(digest) = queue.pop_front() {
        if !visited.insert(digest.clone()) {
            continue;
        }

        // Try to read the node from the store. If it's not there
        // (e.g. the converter hasn't finished, or the digest is
        // bogus), silently skip it. The OCI converter never
        // produces dangling edges in v0, so this branch is
        // defensive against future bugs and against bad inputs
        // from operators who craft arbitrary inspect requests.
        let archived = match store.get_archived(&digest) {
            Ok(a) => a,
            Err(_) => continue,
        };

        let kind = if archived.is_manifest() {
            "manifest"
        } else if archived.is_tree() {
            "tree"
        } else if archived.is_layer() {
            "layer"
        } else if archived.is_blob() {
            "blob"
        } else {
            "unknown"
        };

        // Best-effort size: the mmap length if we have a backing
        // file. For inline data the store has already mmap'd the
        // backing file (even for inline-only nodes, the converter
        // writes a file with the inline bytes). For a true
        // manifest in the v0 layout, we don't have a separate
        // file. Use 0 to mean "unknown".
        let size_bytes = store.exists(&digest) as i64; // cheap proxy: 1 if present, 0 if not
        let _ = size_bytes; // keep the symbol; the real size resolution
                            // is left to a future patch that adds a
                            // `MmapStore::node_size` API.

        out.push(DagNode {
            digest: digest.clone(),
            kind: kind.to_string(),
            size_bytes: 0, // see comment above
        });

        // Enqueue edges (children). For Blob nodes there are no
        // edges (the OCI converter stores them as leafs); for
        // Tree/Manifest nodes the edges point at sub-trees or
        // layers. We deliberately do not distinguish "this edge
        // goes to a layer" from "this edge goes to a tree" here
        // because that's recorded on the child node via its own
        // `kind`.
        for edge in archived.edges.iter() {
            let edge_str: String = edge.as_str().to_string();
            if !edge_str.is_empty() {
                queue.push_back(edge_str);
            }
        }
    }

    out
}

#[cfg(test)]
mod walk_dag_tests {
    use super::walk_dag;
    use nimbus_store::{DagNode as StoreDagNode, MmapStore, NodeKind};

    #[test]
    fn walks_manifest_tree_layer() {
        // Build a tiny DAG: manifest -> tree -> layer.
        let tmp = tempdir();
        let store = MmapStore::new(tmp);

        // Insert children first so the edges resolve.
        let layer = StoreDagNode::new(NodeKind::Layer, vec![], b"layer-bytes".to_vec());
        let tree = StoreDagNode::new(
            NodeKind::Tree,
            vec!["layer-digest".to_string()],
            b"tree-bytes".to_vec(),
        );
        let manifest = StoreDagNode::new(
            NodeKind::Manifest,
            vec!["tree-digest".to_string()],
            b"manifest-bytes".to_vec(),
        );

        // Put each; the store hashes the inline_data and stores it.
        let _layer_digest = store.put_blocking(&layer).unwrap();
        let tree_digest = store.put_blocking(&tree).unwrap();
        let manifest_digest = store.put_blocking(&manifest).unwrap();

        // The manifest we constructed has edges pointing at the
        // literal strings "tree-digest" and "layer-digest" which we
        // never inserted under those names (the store returns
        // content-hashed digests). So the first walk produces just
        // the manifest — the BFS visits the manifest, sees the
        // dangling edges, and the visited set stops the descent.
        // This is *expected* behaviour; the walk is robust to
        // dangling edges (real OCI images are well-formed, but the
        // helper is defensive).
        let path = walk_dag(&store, &manifest_digest);
        assert_eq!(path.len(), 1, "expected just the manifest, got {:?}", path);
        assert_eq!(path[0].kind, "manifest");
        assert_eq!(path[0].digest, manifest_digest);

        // Now build a manifest whose edges reference the *real*
        // digests returned by put_blocking, and walk that. We
        // should get manifest → tree. (The tree's edge to
        // "layer-digest" is also dangling, by design — same
        // reason.)
        let real_manifest = StoreDagNode::new(
            NodeKind::Manifest,
            vec![tree_digest.clone()],
            b"real-manifest-bytes".to_vec(),
        );
        let real_manifest_digest = store.put_blocking(&real_manifest).unwrap();

        let path = walk_dag(&store, &real_manifest_digest);
        assert_eq!(path.len(), 2, "expected manifest+tree, got {:?}", path);
        assert_eq!(path[0].kind, "manifest");
        assert_eq!(path[0].digest, real_manifest_digest);
        assert_eq!(path[1].kind, "tree");
        assert_eq!(path[1].digest, tree_digest);
    }

    /// Minimal tempdir shim so we don't pull in a `tempfile` crate
    /// just for one test. Uses `std::env::temp_dir()` with a random
    /// suffix; the OS reaps stale dirs eventually. We don't
    /// `remove_dir_all` to keep the test dependency-free.
    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("nimbus-runtime-walkdag-{pid}-{n}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}

/// The contract of `Executor::status` is executor-specific, so this
/// helper tries a few common encodings in order:
///   1. Explicit `exit_code=NN` (used by our VM executor)
///   2. `exited(NN)` (runc and cri-api)
///   3. `stopped (signal NN)` (runc on SIGTERM etc.)
///
/// Anything we don't recognise yields `None` and we treat the exit as
/// "unknown" in the event metadata.
fn parse_exit_code(s: &str) -> Option<u32> {
    if let Some(rest) = s.strip_prefix("exit_code=") {
        return rest.split_whitespace().next()?.parse().ok();
    }
    if let Some(start) = s.find("exited(") {
        let rest = &s[start + 7..];
        if let Some(end) = rest.find(')') {
            return rest[..end].parse().ok();
        }
    }
    if let Some(start) = s.find("signal ") {
        let rest = &s[start + 7..];
        if let Some(end) = rest.find(|c: char| !c.is_ascii_digit()) {
            // Treat signals as exit_code = 128 + signal.
            if let Ok(sig) = rest[..end].parse::<u32>() {
                return Some(128 + sig);
            }
        }
    }
    None
}

#[cfg(test)]
mod parse_exit_code_tests {
    use super::parse_exit_code;

    #[test]
    fn parses_explicit_kv() {
        assert_eq!(parse_exit_code("exit_code=42 done"), Some(42));
    }

    #[test]
    fn parses_runc_exited() {
        assert_eq!(parse_exit_code("exited(7)"), Some(7));
    }

    #[test]
    fn parses_signal_as_128_plus() {
        assert_eq!(parse_exit_code("stopped (signal 15)"), Some(143));
    }

    #[test]
    fn returns_none_for_garbage() {
        assert_eq!(parse_exit_code("running"), None);
        assert_eq!(parse_exit_code("exited()"), None);
        assert_eq!(parse_exit_code(""), None);
    }
}

/// Write a single workload's state to the checkpoints directory as a
/// JSON file. Called on every state transition (run, stop, exit).
/// Idempotent: subsequent writes replace the previous checkpoint.
fn write_workload_checkpoint(
    dir: &std::path::Path,
    id: &str,
    state: &WorkloadState,
) {
    let path = dir.join(format!("{id}.json"));
    match serde_json::to_string_pretty(state) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, &json) {
                warn!(%id, error = %e, "failed to write workload checkpoint");
            }
        }
        Err(e) => {
            warn!(%id, error = %e, "failed to serialize workload checkpoint");
        }
    }
}

/// Load all workload checkpoints from the checkpoints directory.
/// Returns a map of workload id → WorkloadState. Skips files that
/// cannot be parsed.
fn load_workload_checkpoints(dir: &std::path::Path) -> HashMap<String, WorkloadState> {
    let mut workloads = HashMap::new();
    if !dir.exists() {
        return workloads;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "failed to read checkpoints directory");
            return workloads;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "json").unwrap_or(false) {
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            match serde_json::from_str::<WorkloadState>(&content) {
                Ok(state) => {
                    if let Some(ref id_str) = id {
                        info!(%id_str, status = %state.status, "recovered workload checkpoint");
                        workloads.insert(id_str.clone(), state);
                    }
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "ignoring invalid checkpoint");
                }
            }
        }
    }
    workloads
}

/// Parse a proto RestartPolicy enum value into the runtime type.
/// Proto RESTART_UNSPECIFIED (0) and RESTART_NO (1) both map to No.
fn parse_restart_policy(p: i32) -> nimbus_exec::types::RestartPolicy {
    use nimbus_exec::types::RestartPolicy;
    match p {
        2 => RestartPolicy::OnFailure,
        3 => RestartPolicy::Always,
        4 => RestartPolicy::UnlessStopped,
        _ => RestartPolicy::No,
    }
}

/// Attempt to restart an exited workload according to its restart policy.
/// Re-creates the workload from the persisted spec fields, applies
/// exponential backoff, and emits a WorkloadStarted event on success.
async fn attempt_restart(
    watcher_executor: &Arc<ExecutorRouter>,
    watcher_workloads: &Arc<RwLock<HashMap<String, WorkloadState>>>,
    watcher_store: &Arc<MmapStore>,
    watcher_checkpoints_dir: &std::path::Path,
    watcher_bus: &Arc<EventBus>,
    id: &str,
    backend: &str,
) {
    use std::time::Duration;
    use nimbus_exec::types::{Backend, RestartPolicy, WorkloadSpec};

    // Read current state to get restart count and policy.
    let (restart_count, image_root, command, env, cpu_millicores, memory_bytes,
          network_rules, kernel_image_ref, working_dir, bridge_name, mounts,
          health_check, network_mode_str, stopped_by_operator) = {
        let map = watcher_workloads.read().await;
        match map.get(id) {
            Some(s) => {
                // Don't restart if the operator stopped this workload.
                let stopped = s.status != "exited";
                let network_mode = if s.internal_ip.is_some() { "bridge" } else { "isolated" };
                (
                    s.restart_count,
                    s.image_root.clone(),
                    s.command.clone(),
                    s.env.clone(),
                    s.cpu_millicores,
                    s.memory_bytes,
                    s.network_rules.clone(),
                    s.kernel_image_ref.clone(),
                    s.working_dir.clone(),
                    s.bridge_name.clone(),
                    s.mounts.clone(),
                    s.health_check.clone(),
                    network_mode.to_string(),
                    stopped,
                )
            }
            None => return,
        }
    };

    if stopped_by_operator {
        return;
    }

    // Exponential backoff: 1s, 2s, 4s, 8s, 16s, max 30s.
    let backoff_secs = std::cmp::min(1u64 << restart_count, 30u64);
    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;

    // Reconstruct the spec.
    let backend_enum = match Backend::from_str(backend) {
        Ok(b) => b,
        Err(_) => return,
    };
    let network_mode_enum = match network_mode_str.as_str() {
        "bridge" => NetworkMode::Bridge,
        "host" => NetworkMode::Host,
        _ => NetworkMode::Loopback,
    };
    let kernel_path = if kernel_image_ref.is_empty() {
        None
    } else {
        // Kernel path from staged kernel cache; best-effort.
        // In v0 we skip this — the kernel was already cached.
        None
    };
    let spec = WorkloadSpec {
        id: id.to_string(),
        image_root,
        backend: backend_enum,
        command,
        env,
        cpu_millicores,
        memory_bytes,
        network_mode: network_mode_enum,
        network_rules,
        kernel_path,
        bridge_name,
        mounts,
        health_check,
        restart_policy: RestartPolicy::Always, // Already decided to restart.
    };

    match watcher_executor.create(spec).await {
        Ok(handle) => {
            if let Err(e) = watcher_executor.start(&handle).await {
                warn!(%id, error = %e, "restart: start failed");
                return;
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let mut map = watcher_workloads.write().await;
            if let Some(state) = map.get_mut(id) {
                state.status = "running".to_string();
                state.start_time = now;
                state.exit_time = 0;
                state.exit_code = None;
                state.pid = handle.pid.unwrap_or(0);
                state.internal_ip = handle.internal_ip.clone();
                state.restart_count += 1;
                let checkpoint = state.clone();
                drop(map);
                write_workload_checkpoint(watcher_checkpoints_dir, id, &checkpoint);
                watcher_bus.emit(
                    Event::new(id, crate::events::EventKind::WorkloadStarted)
                        .with_metadata("backend", &handle.backend)
                        .with_metadata("source", "restart_watcher")
                        .with_metadata("restart_count", &checkpoint.restart_count.to_string()),
                );
                info!(%id, restart_count = checkpoint.restart_count, "workload restarted");
            }
        }
        Err(e) => {
            warn!(%id, error = %e, "restart: create failed");
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadState {
    pub status: String,
    pub start_time: i64,
    /// Set when the workload exits (via stop or natural exit). 0 means
    /// "still running or unknown".
    pub exit_time: i64,
    pub exit_code: Option<u32>,
    pub backend: String,
    pub internal_ip: Option<String>,
    /// Process id reported by the executor (0 if not a process).
    pub pid: u32,
    /// Captured at run time so `inspect` can show the full picture
    /// without re-reading the request.
    pub image_root: String,
    pub command: Vec<String>,
    pub network_rules: Vec<NetworkRule>,
    /// Policy decision log for this workload. Populated by `run_workload`
    /// with the most recent decision per policy. Operators read this
    /// via `inspect` to see *why* a workload was allowed.
    pub policy_decisions: HashMap<String, String>,
    /// OCI image ref for the kernel used by this workload's VM
    /// (only set when `backend == "vm"`). The actual `StagedKernel`
    /// lives in `RuntimeService::kernel_cache` keyed by this ref;
    /// `attach_workload` looks it up by this string to boot the
    /// same VM again. Empty for container workloads.
    pub kernel_image_ref: String,
    /// Working directory inside the workload. Empty means
    /// "/" (the default).
    pub working_dir: String,
    /// Path to the materialized rootfs directory (only set
    /// for `backend == "vm"` workloads). The runtime creates
    /// this on `RunWorkload` and the VM executor mounts
    /// 9p/VirtioFS from it. The path is owned by
    /// `RuntimeService::rootfs_cache`; we hold a copy of
    /// the `PathBuf` here for fast lookup.
    pub rootfs_dir: Option<std::path::PathBuf>,
    /// Health check configuration (if any).
    pub health_check: Option<nimbus_exec::HealthCheck>,
    /// Current health status: "healthy", "unhealthy", "starting", "".
    pub health: String,
    /// Consecutive health check failures so far.
    pub health_failures: u32,
    /// Timestamp of the last successful health check (unix seconds).
    pub health_last_success: i64,
    /// Restart policy for this workload.
    pub restart_policy: nimbus_exec::types::RestartPolicy,
    /// Number of times this workload has been automatically restarted.
    pub restart_count: u32,
    /// Environment variables (stored for restart reconstruction).
    pub env: HashMap<String, String>,
    /// CPU millicores limit (stored for restart reconstruction).
    pub cpu_millicores: Option<u64>,
    /// Memory bytes limit (stored for restart reconstruction).
    pub memory_bytes: Option<u64>,
    /// Bridge name for network isolation (stored for restart reconstruction).
    pub bridge_name: Option<String>,
    /// Volume/bind mount specs (stored for restart reconstruction).
    pub mounts: Vec<nimbus_exec::Mount>,
}

pub struct RuntimeService {
    pub store: Arc<MmapStore>,
    pub policy_engine: Option<Arc<PolicyEngine>>,
    /// Workload state map. Shared via Arc so the background exit
    /// watcher can poll for natural exits while the gRPC handlers
    /// insert/update entries. `RwLock::write()` is callable on
    /// `Arc<RwLock<...>>` via deref coercion, so the existing
    /// `self.workloads.write().await` call sites keep working.
    pub workloads: Arc<RwLock<HashMap<String, WorkloadState>>>,
    pub executor: Arc<ExecutorRouter>,
    /// root_digest -> image_ref, populated by PullImage so RunWorkload
    /// can re-check the policy with the right image_ref (signature is
    /// keyed on image_ref, SBOM on manifest_digest). Arc so the
    /// watcher can read it for `inspect` if needed.
    pub image_tags: Arc<RwLock<HashMap<String, String>>>,
    /// Process-wide event bus. Service emitters call `.emit()`;
    /// subscribers (CLI follow sessions, future audit daemons) call
    /// `.subscribe()`. The watcher task also subscribes to its own
    /// bus; in practice the watcher is the emitter, not a consumer.
    pub event_bus: Arc<EventBus>,
    /// In-memory kernel cache: `kernel_image_ref -> StagedKernel`.
    ///
    /// Populated by `RunWorkload` when `backend == "vm"` and
    /// `kernel_image` is set: the runtime pulls the OCI kernel
    /// image, materializes `/boot/vmlinux` (+ optional
    /// `/boot/initramfs.cpio.gz`) into a temp dir via
    /// `nimbus-vm::oci_kernel::StagedKernel::from_image`, and
    /// stores the result here keyed by the image ref.
    ///
    /// `AttachWorkload` looks up the cache by workload_id (the
    /// workload entry's `kernel_image_ref` field points to the
    /// same key) to find the kernel to boot for the new VM.
    ///
    /// The cache is bounded only by available memory in v0 —
    /// kernels are typically 20-50 MiB each. A future v1 will
    /// add LRU eviction and disk spilling.
    pub kernel_cache: Arc<RwLock<HashMap<String, nimbus_vm::StagedKernel>>>,
    /// In-memory rootfs cache: `workload_id -> materialized
    /// rootfs dir PathBuf`.
    ///
    /// Populated by `RunWorkload` when `backend == "vm"`:
    /// the runtime materializes the OCI image's DAG root
    /// into a temp dir and stores the path here. The VM
    /// executor mounts 9p/VirtioFS from this path.
    ///
    /// `AttachWorkload` looks up the cache by workload_id
    /// to find the rootfs to mount for the new VM.
    ///
    /// The temp dir is removed when the workload is stopped
    /// or the service exits. A future v1 will add disk
    /// spilling for large images.
    pub rootfs_cache: Arc<RwLock<HashMap<String, std::path::PathBuf>>>,
    /// Runtime config (e.g. insecure-registry list). Shared
    /// via `Arc` so background tasks and gRPC handlers can
    /// read it without taking a service-level lock.
    pub config: Arc<ServiceConfig>,
}

impl RuntimeService {
    /// Evaluate the policy for an image that was just pulled.
    /// `image_ref` is the user-supplied reference; `manifest_digest` is
    /// the rkyv root returned by the converter.
    async fn evaluate_pulled(
        &self,
        image_ref: &str,
        manifest_digest: &str,
    ) -> Result<(), Status> {
        let Some(engine) = &self.policy_engine else {
            return Ok(());
        };
        let policy = engine.default_policy().clone();
        let store = self.store.clone();
        let image_ref = image_ref.to_string();
        let manifest_digest = manifest_digest.to_string();
        let engine = engine.clone();
        let decision_image_ref = image_ref.clone();
        let decision_manifest = manifest_digest.clone();
        let decision = tokio::task::spawn_blocking(move || {
            engine.evaluate_for_image(&policy, &store, &image_ref, &manifest_digest)
        })
        .await
        .map_err(|e| Status::internal(format!("policy task join failed: {e}")))?
        .map_err(|e| Status::internal(format!("policy evaluation failed: {e}")))?;

        match decision {
            PolicyDecision::Allow => Ok(()),
            PolicyDecision::Deny(reason) => {
                warn!(image_ref = %decision_image_ref, manifest = %decision_manifest, %reason, "policy denied pulled image");
                Err(Status::permission_denied(format!(
                    "Policy denied image {decision_image_ref}: {reason}"
                )))
            }
        }
    }

    /// Defense-in-depth check at run time. Uses the recorded image_ref
    /// (from PullImage) when available, falls back to a no-signature
    /// SBOM-only check otherwise.
    async fn evaluate_for_run(
        &self,
        root_digest: &str,
    ) -> Result<(), Status> {
        let Some(engine) = &self.policy_engine else {
            return Ok(());
        };
        let image_ref = {
            let tags = self.image_tags.read().await;
            tags.get(root_digest).cloned()
        };
        let image_ref = image_ref.unwrap_or_else(|| root_digest.to_string());
        if image_ref == root_digest {
            debug!(
                %root_digest,
                "no image_ref recorded for this root_digest; signature check will be skipped"
            );
        }
        let policy = engine.default_policy().clone();
        let store = self.store.clone();
        let engine = engine.clone();
        let image_ref = image_ref;
        let root_digest = root_digest.to_string();
        let decision_root = root_digest.clone();
        let decision = tokio::task::spawn_blocking(move || {
            engine.evaluate_for_image(&policy, &store, &image_ref, &root_digest)
        })
        .await
        .map_err(|e| Status::internal(format!("policy task join failed: {e}")))?
        .map_err(|e| Status::internal(format!("policy evaluation failed: {e}")))?;

        match decision {
            PolicyDecision::Allow => Ok(()),
            PolicyDecision::Deny(reason) => {
                warn!(%decision_root, %reason, "policy denied run");
                Err(Status::permission_denied(reason))
            }
        }
    }

    /// Record the workload state in the workloads map and
    /// emit the public WorkloadStarted event. Returns the
    /// tonic `RunResponse` for the gRPC call.
    ///
    /// Extracted so the macOS Apple Virt path (which doesn't
    /// have a real executor) can record state without
    /// duplicating the bookkeeping.
    #[allow(clippy::too_many_arguments)]
    async fn record_workload_state(
        &self,
        req: crate::proto::RunRequest,
        backend_label: String,
        backend: Backend,
        final_backend: String,
        final_ip: String,
        final_pid: u32,
        now: i64,
        final_image_root: String,
        final_command: Vec<String>,
        final_kernel_image_ref: String,
        final_working_dir: String,
        network_rules: Vec<NetworkRule>,
        policy_decisions: HashMap<String, String>,
    ) -> Result<tonic::Response<RunResponse>, tonic::Status> {
        let final_id = req.id.clone();
        let mut workloads = self.workloads.write().await;
        // Look up the materialized rootfs path (if any) so
        // `attach_workload` can mount it on the new VM.
        let rootfs_dir = if final_backend == "vm" {
            self.rootfs_cache
                .read()
                .await
                .get(&final_id)
                .cloned()
        } else {
            None
        };
        let _ = backend; // reserved for future exec dispatch
        let restart_policy = parse_restart_policy(req.restart_policy);
        let state = WorkloadState {
            status: "running".to_string(),
            start_time: now,
            exit_time: 0,
            exit_code: None,
            backend: final_backend.clone(),
            internal_ip: if final_ip == "loopback" { None } else { Some(final_ip.clone()) },
            pid: final_pid,
            image_root: final_image_root.clone(),
            command: final_command.clone(),
            network_rules: network_rules.clone(),
            policy_decisions,
            kernel_image_ref: final_kernel_image_ref.clone(),
            working_dir: final_working_dir.clone(),
            rootfs_dir,
            health_check: None,
            health: String::new(),
            health_failures: 0,
            health_last_success: 0,
            restart_policy,
            restart_count: 0,
            env: req.env.clone(),
            cpu_millicores: if req.cpu_millicores > 0 { Some(req.cpu_millicores) } else { None },
            memory_bytes: if req.memory_bytes > 0 { Some(req.memory_bytes) } else { None },
            bridge_name: if req.bridge_name.is_empty() { None } else { Some(req.bridge_name.clone()) },
            mounts: req.mounts.iter().map(|m| nimbus_exec::Mount {
                type_: m.r#type.clone(),
                source: m.source.clone(),
                destination: m.destination.clone(),
                options: m.options.clone(),
            }).collect(),
        };
        workloads.insert(final_id.clone(), state.clone());
        drop(workloads);

        // Persist checkpoint immediately so the workload state
        // survives a runtime restart.
        write_workload_checkpoint(&self.config.checkpoints_dir, &final_id, &state);

        record_workload_started(&backend_label);

        // Emit the public WorkloadStarted event for observers.
        self.event_bus.emit(
            Event::new(&final_id, EventKind::WorkloadStarted)
                .with_metadata("backend", &final_backend)
                .with_metadata("image_root", &final_image_root)
                .with_metadata("internal_ip", &final_ip)
                .with_metadata("pid", &final_pid.to_string()),
        );

        Ok(tonic::Response::new(RunResponse {
            id: final_id,
            pid: final_pid,
            backend_used: final_backend,
            internal_ip: final_ip,
        }))
    }
}

#[tonic::async_trait]
impl Runtime for RuntimeService {
    async fn pull_image(
        &self,
        request: tonic::Request<PullImageRequest>,
    ) -> Result<tonic::Response<PullImageResponse>, tonic::Status> {
        let req = request.into_inner();
        let registry = if req.registry.is_empty() {
            None
        } else {
            Some(req.registry.as_str())
        };
        let image_ref = req.image_ref.clone();

        // Metrics: record the registry label even on the failure
        // path. The `registry` label is the user-supplied string
        // (or "default" if empty). We do *not* put the full image
        // ref there — that would explode label cardinality.
        let registry_label: String = match registry {
            Some(r) if !r.is_empty() => r.to_string(),
            _ => "default".to_string(),
        };
        // RAII timer: records on drop, regardless of success/failure.
        let _timer = register_pull_timer();
        record_pull(&registry_label, "started");

        let auth = build_auth(
            &req.registry_username,
            &req.registry_password,
            &req.registry_token,
        );
        let puller =
            OciPuller::with_insecure_registries(auth, self.config.insecure_registries.clone());
        let pull_result = puller.pull(&image_ref, registry).await;
        let pulled = match pull_result {
            Ok(p) => p,
            Err(e) => {
                record_pull(&registry_label, "failed");
                self.event_bus.emit(
                    Event::new(&image_ref, EventKind::ImagePulled)
                        .with_metadata("registry", &registry_label)
                        .with_metadata("outcome", "failed")
                        .with_metadata("error", &e.to_string()),
                );
                return Err(tonic::Status::internal(format!("pull failed: {e}")));
            }
        };

        let converter = OciToDagConverter::new(self.store.clone());
        let convert_result = converter.convert(&pulled).await;
        let root_digest = match convert_result {
            Ok(d) => d,
            Err(e) => {
                record_pull(&registry_label, "failed");
                self.event_bus.emit(
                    Event::new(&image_ref, EventKind::ImagePulled)
                        .with_metadata("registry", &registry_label)
                        .with_metadata("outcome", "failed")
                        .with_metadata("error", &format!("conversion: {e}")),
                );
                return Err(tonic::Status::internal(format!("conversion failed: {e}")));
            }
        };

        // Record image_ref -> root_digest for later run-time policy checks.
        {
            let mut tags = self.image_tags.write().await;
            tags.insert(root_digest.clone(), image_ref.clone());
        }

        // Policy gate.
        if let Err(e) = self.evaluate_pulled(&image_ref, &root_digest).await {
            record_pull(&registry_label, "denied");
            self.event_bus.emit(
                Event::new(&image_ref, EventKind::PolicyDenied)
                    .with_metadata("registry", &registry_label)
                    .with_metadata("phase", "pull")
                    .with_metadata("reason", &e.message().to_string()),
            );
            return Err(e);
        }

        // Detect dedup: if the root manifest was already in the store
        // *before* this call, the bytes_stored value is misleading
        // because the converter's `put()` short-circuits. We do a
        // best-effort check post-conversion by re-asking the store.
        // (`Digest` is a type alias for `String`; we can just pass the
        // hex string slice.)
        let bytes_stored: i64 = pulled.layer_blobs.iter().map(|(_, b)| b.len() as i64).sum();
        let already_present = self.store.exists(&root_digest);

        if already_present {
            self.event_bus.emit(
                Event::new(&image_ref, EventKind::ImageDeduped)
                    .with_metadata("registry", &registry_label)
                    .with_metadata("root_digest", &root_digest)
                    .with_metadata("bytes_stored", &bytes_stored.to_string()),
            );
        } else {
            self.event_bus.emit(
                Event::new(&image_ref, EventKind::ImagePulled)
                    .with_metadata("registry", &registry_label)
                    .with_metadata("root_digest", &root_digest)
                    .with_metadata("bytes_stored", &bytes_stored.to_string()),
            );
        }

        record_pull(&registry_label, "success");
        Ok(tonic::Response::new(PullImageResponse {
            root_digest,
            bytes_stored,
            bytes_deduplicated: 0,
        }))
    }

    async fn run_workload(
        &self,
        request: tonic::Request<RunRequest>,
    ) -> Result<tonic::Response<RunResponse>, tonic::Status> {
        let req = request.into_inner();
        // Take a copy of the request early so the macOS
        // Apple Virt path (which doesn't go through the
        // executor) can still pass a fresh copy to the
        // state-recording helper. The original `req` is
        // partially moved (e.g. `req.env`) before that
        // path is reached, so we can't just clone it
        // there.
        let req_for_state = req.clone();
        let backend = Backend::from_str(&req.backend)
            .map_err(|e| tonic::Status::invalid_argument(e))?;

        // RAII timer: records wall-clock duration of the whole RPC
        // (parse, policy, create, start) on drop, regardless of
        // success or failure.
        let _timer = register_start_timer();
        let backend_label = backend.as_str().to_string();

        // If the backend is "vm", stage the kernel OCI image and
        // stash it in the kernel cache so a later AttachWorkload
        // can boot the same VM.
        // On Linux/Firecracker, the daemon has a pre-configured
        // kernel path (--vm-kernel), so kernel_image is optional.
        if backend_label == "vm" {
            let has_local_kernel = self.config.vm_backend.is_some();
            if req.kernel_image.is_empty() && !has_local_kernel {
                return Err(tonic::Status::invalid_argument(
                    "backend=vm requires kernel_image (e.g. 'nimbus/kernel-asahi:6.19.14')",
                ));
            }
            // When kernel_image is non-empty, stage the kernel from
            // OCI for Apple Virt (macOS). For Firecracker (Linux),
            // the daemon's VmBackendConfig.kernel_path is used
            // directly, so skip OCI staging.
            if !req.kernel_image.is_empty() {
            let already_staged = self
                .kernel_cache
                .read()
                .await
                .contains_key(&req.kernel_image);
            if !already_staged {
                let store = self.store.clone();
                let kernel_image = req.kernel_image.clone();
                let insecure_registries = self.config.insecure_registries.clone();
                let r = tokio::task::spawn_blocking(move || {
                    stage_kernel_image(&store, &kernel_image, &insecure_registries)
                })
                .await
                .map_err(|e| {
                    tonic::Status::internal(format!("stage kernel join: {e}"))
                })?;
                match r {
                    Ok(kernel) => {
                        self.event_bus.emit(
                            Event::new(&req.id, EventKind::ImagePulled)
                                .with_metadata("kind", "kernel")
                                .with_metadata("image", &req.kernel_image)
                                .with_metadata(
                                    "vmlinux_bytes",
                                    &kernel.vmlinux_size().to_string(),
                                ),
                        );
                        self.kernel_cache
                            .write()
                            .await
                            .insert(req.kernel_image.clone(), kernel);
                    }
                    Err(e) => {
                        self.event_bus.emit(
                            Event::new(&req.id, EventKind::ImagePulled)
                                .with_metadata("kind", "kernel")
                                .with_metadata("image", &req.kernel_image)
                                .with_metadata("outcome", "failed")
                                .with_metadata("error", &e.to_string()),
                        );
                        return Err(tonic::Status::internal(format!(
                            "stage kernel {}: {e}",
                            req.kernel_image
                        )));
                    }
                }
            }
            }

            // Materialize the workload's OCI image to a temp
            // dir so the VM can mount it as 9p/VirtioFS
            // (macOS Apple Virt path). On Linux, the
            // FirecrackerExecutor handles its own ext4 rootfs
            // materialization in create(), so skip this to
            // avoid 2x DAG walk + 2x disk space.
            #[cfg(target_os = "macos")]
            {
                let store_for_rootfs = self.store.clone();
                let root_digest_for_rootfs = req.root_digest.clone();
                let rootfs_path = tokio::task::spawn_blocking(move || {
                    materialize_rootfs(&store_for_rootfs, &root_digest_for_rootfs)
                })
                .await
                .map_err(|e| {
                    tonic::Status::internal(format!("materialize rootfs join: {e}"))
                })?
                .map_err(|e| {
                    tonic::Status::internal(format!("materialize rootfs: {e}"))
                })?;
                self.rootfs_cache
                    .write()
                    .await
                    .insert(req.id.clone(), rootfs_path);
            }
        }

        let network_mode = match req.network_mode.as_str() {
            "bridge" => NetworkMode::Bridge,
            "host" => NetworkMode::Host,
            _ => NetworkMode::Loopback,
        };

        let env: HashMap<String, String> = req.env;

        // Defense-in-depth: re-evaluate the policy before launching.
        self.evaluate_for_run(&req.root_digest).await?;

        // Translate the gRPC NetworkRule wire format into the runtime's
        // `nimbus_net::NetworkRule` so the executor can apply it (start
        // inbound proxy listeners, declare outbound allowlists).
        let network_rules: Vec<nimbus_net::NetworkRule> = req
            .network_rules
            .iter()
            .map(|r| {
                let direction = match r.direction.as_str() {
                    "outbound" => nimbus_net::Direction::Outbound,
                    _ => nimbus_net::Direction::Inbound,
                };
                let protocol = match r.protocol.as_str() {
                    "udp" => nimbus_net::Protocol::Udp,
                    _ => nimbus_net::Protocol::Tcp,
                };
                let to_host = if r.to_host.is_empty() {
                    None
                } else {
                    Some(r.to_host.clone())
                };
                let from_cidrs = if r.from_cidrs.is_empty() {
                    None
                } else {
                    Some(r.from_cidrs.clone())
                };
                nimbus_net::NetworkRule {
                    direction,
                    protocol,
                    port: r.port as u16,
                    to_host,
                    from_cidrs,
                }
            })
            .collect();

        // If a kernel_image was specified, look up the staged kernel's
        // vmlinux path so the Firecracker executor can use it instead
        // of the default kernel_path from its config.
        let kernel_path = if req.kernel_image.is_empty() {
            None
        } else {
            self.kernel_cache
                .read()
                .await
                .get(&req.kernel_image)
                .map(|k| k.vmlinux_path().to_path_buf())
        };

        let mounts: Vec<nimbus_exec::Mount> = req.mounts.iter().map(|m| nimbus_exec::Mount {
            type_: m.r#type.clone(),
            source: m.source.clone(),
            destination: m.destination.clone(),
            options: m.options.clone(),
        }).collect();

        let restart_policy = parse_restart_policy(req.restart_policy);

        let spec = WorkloadSpec {
            id: req.id.clone(),
            image_root: req.root_digest.clone(),
            backend,
            command: req.command.clone(),
            env,
            cpu_millicores: if req.cpu_millicores > 0 { Some(req.cpu_millicores) } else { None },
            memory_bytes: if req.memory_bytes > 0 { Some(req.memory_bytes) } else { None },
            network_mode,
            network_rules: network_rules.clone(),
            kernel_path,
            bridge_name: if req.bridge_name.is_empty() {
                None
            } else {
                Some(req.bridge_name.clone())
            },
            mounts,
            health_check: req.health_check.as_ref().map(|hc| nimbus_exec::HealthCheck {
                test: hc.test.clone(),
                interval_seconds: hc.interval_seconds,
                timeout_seconds: hc.timeout_seconds,
                retries: hc.retries,
                start_period_seconds: hc.start_period_seconds,
            }),
            restart_policy: restart_policy.clone(),
        };

        // Emit BackendSelected *before* we touch the executor. This
        // gives observers a clear "operator requested X, runtime
        // selected Y" record even if the actual create/start fails
        // later. We always pick the requested backend; the executor
        // router has no auto-fallback in v0.
        self.event_bus.emit(
            Event::new(&req.id, EventKind::BackendSelected)
                .with_metadata("requested_backend", &backend_label)
                .with_metadata("selected_backend", &backend_label)
                .with_metadata("image_root", &req.root_digest),
        );

        let handle = match self.executor.create(spec.clone()).await {
            Ok(h) => h,
            Err(e) => {
                // Special case: on macOS, the VM backend
                // (Apple Virt) doesn't have a real executor —
                // the VM is per-attach and only boots when
                // the client calls `AttachWorkload`. If the
                // configured VM backend isn't available
                // (no `--vm-firecracker` on a Linux host, or
                // any VM backend on a non-Linux host) AND
                // we're targeting `vm`, fall through and
                // record the workload state. The subsequent
                // `AttachWorkload` call will perform the
                // actual VM boot via
                // `nimbus_vm::run_session_blocking`.
                #[cfg(target_os = "macos")]
                {
                    let is_applevirt_unsupported = matches!(
                        e,
                        nimbus_exec::ExecError::BackendNotAvailable(_)
                    );
                    if backend_label == "vm" && is_applevirt_unsupported {
                        warn!(
                            workload_id = %req.id,
                            "vm backend executor not configured; recording state for AttachWorkload to boot the Apple Virt VM"
                        );
                        // Build the per-policy decision log
                        // inline (same logic as the success
                        // path below).
                        let mut policy_decisions = HashMap::new();
                        if self.policy_engine.is_some() {
                            policy_decisions.insert("default".to_string(), "allow".to_string());
                        }
                        // Synthesize a placeholder handle. The
                        // real pid/IP come from the VM at
                        // attach time.
                        let final_backend = "vm".to_string();
                        let final_ip = "loopback".to_string();
                        let final_pid: u32 = 0;
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        // Use the `req_for_state` clone taken
                        // at function entry (the original
                        // `req` has been partially moved by
                        // the env extraction above).
                        let image_root = req_for_state.root_digest.clone();
                        let command = req_for_state.command.clone();
                        let kernel_image = req_for_state.kernel_image.clone();
                        let working_dir = if req_for_state.working_dir.is_empty() {
                            "/".to_string()
                        } else {
                            req_for_state.working_dir.clone()
                        };
                        // `backend` was consumed by
                        // `backend_label = backend.as_str()`;
                        // we don't need a copy here, the
                        // helper takes a `Backend` only for
                        // the placeholder `let _ = backend`
                        // that signals future exec dispatch.
                        let backend_for_state = Backend::Vm;
                        return self
                            .record_workload_state(
                                req_for_state,
                                backend_label,
                                backend_for_state,
                                final_backend,
                                final_ip,
                                final_pid,
                                now,
                                image_root,
                                command,
                                kernel_image,
                                working_dir,
                                network_rules,
                                policy_decisions,
                            )
                            .await;
                    }
                }
                self.event_bus.emit(
                    Event::new(&req.id, EventKind::WorkloadStarted)
                        .with_metadata("backend", &backend_label)
                        .with_metadata("outcome", "create_failed")
                        .with_metadata("error", &e.to_string()),
                );
                return Err(tonic::Status::internal(format!("create failed: {e}")));
            }
        };

        if let Err(e) = self.executor.start(&handle).await {
            self.event_bus.emit(
                Event::new(&req.id, EventKind::WorkloadStarted)
                    .with_metadata("backend", &handle.backend)
                    .with_metadata("outcome", "start_failed")
                    .with_metadata("error", &e.to_string()),
            );
            return Err(tonic::Status::internal(format!("start failed: {e}")));
        }

        // Workload reached the running state. Bump the started
        // counter and the running gauge. The exit path (in
        // `stop_workload` and the background watcher) is the only
        // place that decrements the gauge.
        record_workload_started(&backend_label);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Build the per-policy decision log. v0 only writes a single
        // entry per decision; the map key is the policy name (e.g.
        // "default") and the value is "allow" or "deny: <reason>".
        // We hardcode "allow" here because the policy was already
        // enforced above (evaluate_for_run returned Ok); if it had
        // denied, we'd have errored out before reaching this point.
        let mut policy_decisions = HashMap::new();
        if self.policy_engine.is_some() {
            policy_decisions.insert("default".to_string(), "allow".to_string());
        }

        let final_backend = handle.backend.clone();
        let final_pid = handle.pid.unwrap_or(0);
        let final_ip = handle.internal_ip.clone().unwrap_or_else(|| "loopback".into());
        let final_id = req.id.clone();
        let final_image_root = req.root_digest.clone();
        let final_command = req.command.clone();
        let final_kernel_image_ref = req.kernel_image.clone();
        let final_working_dir = if req.working_dir.is_empty() {
            "/".to_string()
        } else {
            req.working_dir.clone()
        };

        let mut workloads = self.workloads.write().await;
        // Look up the materialized rootfs path (if any) so
        // `attach_workload` can mount it on the new VM.
        let rootfs_dir = if final_backend == "vm" {
            self.rootfs_cache
                .read()
                .await
                .get(&final_id)
                .cloned()
        } else {
            None
        };
        let hc = spec.health_check.clone();
        let state = WorkloadState {
            status: "running".to_string(),
            start_time: now,
            exit_time: 0,
            exit_code: None,
            backend: final_backend.clone(),
            internal_ip: handle.internal_ip.clone(),
            pid: final_pid,
            image_root: final_image_root.clone(),
            command: final_command.clone(),
            network_rules: network_rules.clone(),
            policy_decisions,
            kernel_image_ref: final_kernel_image_ref.clone(),
            working_dir: final_working_dir.clone(),
            rootfs_dir,
            health_check: hc,
            health: "starting".to_string(),
            health_failures: 0,
            health_last_success: 0,
            restart_policy: restart_policy.clone(),
            restart_count: 0,
            env: spec.env.clone(),
            cpu_millicores: spec.cpu_millicores,
            memory_bytes: spec.memory_bytes,
            bridge_name: spec.bridge_name.clone(),
            mounts: spec.mounts.clone(),
        };
        workloads.insert(final_id.clone(), state.clone());
        drop(workloads);

        // Persist checkpoint immediately so the workload state
        // survives a runtime restart.
        write_workload_checkpoint(&self.config.checkpoints_dir, &final_id, &state);

        record_workload_started(&backend_label);

        // Emit the public WorkloadStarted event for observers.
        self.event_bus.emit(
            Event::new(&final_id, EventKind::WorkloadStarted)
                .with_metadata("backend", &final_backend)
                .with_metadata("image_root", &final_image_root)
                .with_metadata("internal_ip", &final_ip)
                .with_metadata("pid", &final_pid.to_string()),
        );

        Ok(tonic::Response::new(RunResponse {
            id: final_id,
            pid: final_pid,
            backend_used: final_backend,
            internal_ip: final_ip,
        }))
    }

    async fn run_compose(
        &self,
        request: tonic::Request<RunComposeRequest>,
    ) -> Result<tonic::Response<RunComposeResponse>, tonic::Status> {
        let req = request.into_inner();
        let mut workload_ids = Vec::new();
        let mut service_to_id = HashMap::new();

        for service in req.services {
            let id = if service.name.is_empty() {
                return Err(tonic::Status::invalid_argument("compose service name is empty"));
            } else {
                format!("{}-{}", req.project_name, service.name)
            };

            let root_digest = if service.root_digest.is_empty() {
                // Pull the image first.
                let pull_req = tonic::Request::new(PullImageRequest {
                    image_ref: service.image.clone(),
                    registry: String::new(),
                    registry_username: String::new(),
                    registry_password: String::new(),
                    registry_token: String::new(),
                });
                let pull_resp = self.pull_image(pull_req).await?;
                pull_resp.into_inner().root_digest
            } else {
                service.root_digest.clone()
            };

            // Translate ComposePort to NetworkRule (inbound only).
            let network_rules: Vec<ProtoNetworkRule> = service
                .ports
                .iter()
                .map(|p| ProtoNetworkRule {
                    direction: "inbound".to_string(),
                    protocol: if p.protocol.is_empty() {
                        "tcp".to_string()
                    } else {
                        p.protocol.clone()
                    },
                    port: p.container_port,
                    to_host: String::new(),
                    from_cidrs: vec![],
                })
                .collect();

            let run_req = tonic::Request::new(RunRequest {
                id: id.clone(),
                root_digest,
                backend: "container".to_string(),
                command: service.command.clone(),
                env: service.environment.clone(),
                cpu_millicores: service.cpu_millicores,
                memory_bytes: service.memory_bytes,
                network_mode: if service.network_mode.is_empty() {
                    "bridge".to_string()
                } else {
                    service.network_mode.clone()
                },
                network_rules,
                kernel_image: String::new(),
                working_dir: service.working_dir.clone(),
                bridge_name: service.bridge_name.clone(),
                mounts: service.mounts.clone(),
                health_check: service.health_check.clone(),
                restart_policy: 0, // default: no restart for compose
            });

            let run_resp = self.run_workload(run_req).await?;
            let run_id = run_resp.into_inner().id;

            workload_ids.push(run_id.clone());
            service_to_id.insert(service.name.clone(), run_id);
        }

        Ok(tonic::Response::new(RunComposeResponse {
            workload_ids,
            service_to_id,
        }))
    }

    async fn stop_workload(
        &self,
        request: tonic::Request<StopRequest>,
    ) -> Result<tonic::Response<StopResponse>, tonic::Status> {
        let req = request.into_inner();
        let id = req.id.clone();
        self.executor.stop(&id).await
            .map_err(|e| tonic::Status::internal(format!("stop failed: {e}")))?;

        // Look up the backend label *before* mutating state, so the
        // metrics call sees the same label as the one that was
        // incremented in `run_workload`. `exit_code` is set to 0
        // because the operator-initiated stop is a clean exit from
        // the runtime's point of view; the actual process exit
        // status (if the workload was a runc container) is opaque
        // to us at this layer in v0.
        let backend_label = {
            let workloads = self.workloads.read().await;
            workloads.get(&id).map(|s| s.backend.clone()).unwrap_or_else(|| "unknown".to_string())
        };

        // Only emit `WorkloadStopped` and mark "stopped" if the
        // workload is still in the running state. If the background
        // watcher has already flipped it to "exited", we leave it
        // alone and don't double-emit. (The watcher uses its own
        // `announced` HashSet to ensure it only fires
        // `WorkloadExited` once per id.)
        let mut was_running = false;
        let mut state_copy: Option<WorkloadState> = None;
        {
            let mut workloads = self.workloads.write().await;
            if let Some(state) = workloads.get_mut(&id) {
                if state.status == "running" {
                    state.status = "stopped".to_string();
                    state.exit_code = Some(0);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    state.exit_time = now;
                    was_running = true;
                    state_copy = Some(state.clone());
                }
            }
        }
        // Persist checkpoint after stopping.
        if let Some(s) = &state_copy {
            write_workload_checkpoint(&self.config.checkpoints_dir, &id, s);
        }

        if was_running {
            record_workload_exit(&backend_label, Some(0));
            self.event_bus.emit(
                Event::new(&id, EventKind::WorkloadStopped)
                    .with_metadata("backend", &backend_label)
                    .with_metadata("exit_code", "0")
                    .with_metadata("source", "operator"),
            );
        } else {
            // The watcher may not have ticked yet; record the exit
            // anyway so the gauge stays accurate even on the
            // operator-stop-after-natural-exit race.
            record_workload_exit(&backend_label, Some(0));
        }

        Ok(tonic::Response::new(StopResponse { success: true }))
    }

    async fn get_workload(
        &self,
        request: tonic::Request<GetWorkloadRequest>,
    ) -> Result<tonic::Response<WorkloadStatus>, tonic::Status> {
        let req = request.into_inner();
        let workloads = self.workloads.read().await;
        let state = workloads.get(&req.id)
            .ok_or_else(|| tonic::Status::not_found(format!("workload {} not found", req.id)))?;

        use crate::proto::RestartPolicy;
        let restart_proto = match state.restart_policy {
            nimbus_exec::types::RestartPolicy::OnFailure => RestartPolicy::RestartOnFailure,
            nimbus_exec::types::RestartPolicy::Always => RestartPolicy::RestartAlways,
            nimbus_exec::types::RestartPolicy::UnlessStopped => RestartPolicy::RestartUnlessStopped,
            _ => RestartPolicy::RestartNo,
        };
        Ok(tonic::Response::new(WorkloadStatus {
            id: req.id,
            state: state.status.clone(),
            backend: state.backend.clone(),
            exit_code: state.exit_code.unwrap_or(0),
            start_time: state.start_time,
            internal_ip: state.internal_ip.clone().unwrap_or_default(),
            network_isolated: true,
            health: state.health.clone(),
            restart_policy: restart_proto.into(),
            restart_count: state.restart_count,
        }))
    }

    async fn list_workloads(
        &self,
        _request: tonic::Request<ListWorkloadsRequest>,
    ) -> Result<tonic::Response<ListWorkloadsResponse>, tonic::Status> {
        let workloads = self.workloads.read().await;
        let items: Vec<WorkloadStatus> = workloads.iter().map(|(id, state)| {
            use crate::proto::RestartPolicy;
            let restart_proto = match state.restart_policy {
                nimbus_exec::types::RestartPolicy::OnFailure => RestartPolicy::RestartOnFailure,
                nimbus_exec::types::RestartPolicy::Always => RestartPolicy::RestartAlways,
                nimbus_exec::types::RestartPolicy::UnlessStopped => RestartPolicy::RestartUnlessStopped,
                _ => RestartPolicy::RestartNo,
            };
            WorkloadStatus {
                id: id.clone(),
                state: state.status.clone(),
                backend: state.backend.clone(),
                exit_code: state.exit_code.unwrap_or(0),
                start_time: state.start_time,
                internal_ip: state.internal_ip.clone().unwrap_or_default(),
                network_isolated: true,
                health: state.health.clone(),
                restart_policy: restart_proto.into(),
                restart_count: state.restart_count,
            }
        }).collect();

        Ok(tonic::Response::new(ListWorkloadsResponse { workloads: items }))
    }

    type StreamLogsStream = tokio_stream::wrappers::ReceiverStream<Result<LogChunk, tonic::Status>>;

    async fn stream_logs(
        &self,
        request: tonic::Request<StreamLogsRequest>,
    ) -> Result<tonic::Response<Self::StreamLogsStream>, tonic::Status> {
        let _req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            tx.send(Ok(LogChunk {
                data: "logs streaming...\n".into(),
                stderr: false,
                timestamp: 0,
            })).await.ok();
        });
        Ok(tonic::Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    type StreamEventsStream = tokio_stream::wrappers::ReceiverStream<Result<ProtoEvent, tonic::Status>>;

    async fn stream_events(
        &self,
        request: tonic::Request<StreamEventsRequest>,
    ) -> Result<tonic::Response<Self::StreamEventsStream>, tonic::Status> {
        let req = request.into_inner();
        // The set of kinds to keep. An empty filter means "all kinds".
        // We use `EventKind::parse` so that a CLI / forward-compat
        // client passing an unknown kind just gets the no-op list
        // (i.e. we never silently drop events due to filter
        // construction errors).
        let allow: std::collections::HashSet<String> = req
            .event_types
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect();

        // Subscribe BEFORE returning the stream so we don't miss the
        // events emitted during the rest of this RPC.
        let mut rx = self.event_bus.subscribe();

        let (tx, mpsc_rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if !allow.is_empty() && !allow.contains(event.kind.as_str()) {
                            continue;
                        }
                        let proto: ProtoEvent = event.into();
                        if tx.send(Ok(proto)).await.is_err() {
                            // The client disconnected; stop the task.
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        // We dropped `n` events because the consumer
                        // is too slow. We don't surface this to the
                        // gRPC client (would require an extension);
                        // v0 just logs and continues.
                        warn!(dropped = n, "stream_events: lagged; events dropped");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // The bus was closed (runtime shutdown).
                        return;
                    }
                }
            }
        });

        Ok(tonic::Response::new(tokio_stream::wrappers::ReceiverStream::new(mpsc_rx)))
    }

    async fn inspect_workload(
        &self,
        request: tonic::Request<InspectRequest>,
    ) -> Result<tonic::Response<InspectResponse>, tonic::Status> {
        let req = request.into_inner();
        let id = req.id.clone();

        // Snapshot the workload state without holding the read lock
        // across any subsequent work.
        let (state_opt, internal_ip) = {
            let map = self.workloads.read().await;
            map.get(&id)
                .map(|s| (Some(s.clone()), s.internal_ip.clone()))
                .unwrap_or((None, None))
        };

        let Some(state) = state_opt else {
            // Not found: return a response with `found=false` rather
            // than a gRPC error, so the CLI can distinguish "GC'd"
            // from "RPC failed".
            return Ok(tonic::Response::new(InspectResponse {
                id,
                state: "unknown".to_string(),
                backend: String::new(),
                image_root: String::new(),
                internal_ip: String::new(),
                pid: 0,
                start_time: 0,
                exit_time: 0,
                exit_code: 0,
                command: Vec::new(),
                network_rules: Vec::new(),
                dag_path: Vec::new(),
                policy_decisions: std::collections::HashMap::new(),
                found: false,
                restart_policy: 0,
                restart_count: 0,
            }));
        };

        // Build the proto `NetworkRule` list from the runtime form
        // we captured at run time. (The runtime form is the source
        // of truth — round-tripping back from the proto form would
        // be lossy if we'd added fields.)
        let proto_network_rules: Vec<ProtoNetworkRule> = state
            .network_rules
            .iter()
            .map(|r| ProtoNetworkRule {
                direction: match r.direction {
                    nimbus_net::Direction::Inbound => "inbound".to_string(),
                    nimbus_net::Direction::Outbound => "outbound".to_string(),
                },
                protocol: match r.protocol {
                    nimbus_net::Protocol::Tcp => "tcp".to_string(),
                    nimbus_net::Protocol::Udp => "udp".to_string(),
                },
                port: r.port as u32,
                to_host: r.to_host.clone().unwrap_or_default(),
                from_cidrs: r.from_cidrs.clone().unwrap_or_default(),
            })
            .collect();

        // Walk the DAG from the workload's image_root. BFS, manifest
        // first. We use a VecDeque and a HashSet to guard against
        // accidental cycles (the OCI DAG is acyclic, but defensive
        // coding is cheap and the store itself can have weird shapes
        // if a converter ever introduces one).
        let dag_path = walk_dag(&self.store, &state.image_root);

        use crate::proto::RestartPolicy;
        let restart_proto = match state.restart_policy {
            nimbus_exec::types::RestartPolicy::OnFailure => RestartPolicy::RestartOnFailure,
            nimbus_exec::types::RestartPolicy::Always => RestartPolicy::RestartAlways,
            nimbus_exec::types::RestartPolicy::UnlessStopped => RestartPolicy::RestartUnlessStopped,
            _ => RestartPolicy::RestartNo,
        };
        Ok(tonic::Response::new(InspectResponse {
            id: id.clone(),
            state: state.status.clone(),
            backend: state.backend.clone(),
            image_root: state.image_root.clone(),
            internal_ip: internal_ip.unwrap_or_default(),
            pid: state.pid,
            start_time: state.start_time,
            exit_time: state.exit_time,
            exit_code: state.exit_code.unwrap_or(0),
            command: state.command.clone(),
            network_rules: proto_network_rules,
            dag_path,
            policy_decisions: state.policy_decisions.clone(),
            found: true,
            restart_policy: restart_proto.into(),
            restart_count: state.restart_count,
        }))
    }

    async fn exec_in_workload(
        &self,
        request: tonic::Request<ExecRequest>,
    ) -> Result<tonic::Response<ExecResponse>, tonic::Status> {
        let req = request.into_inner();
        let mut cmd = tokio::process::Command::new("runc");
        cmd.args(["exec", &req.id]);
        for arg in &req.command { cmd.arg(arg); }

        let output = cmd.output().await
            .map_err(|e| tonic::Status::internal(format!("exec failed: {e}")))?;

        Ok(tonic::Response::new(ExecResponse {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        }))
    }

    type AttachWorkloadStream = tokio_stream::wrappers::ReceiverStream<
        Result<AttachMessage, tonic::Status>,
    >;

    async fn attach_workload(
        &self,
        request: tonic::Request<tonic::Streaming<AttachMessage>>,
    ) -> Result<tonic::Response<Self::AttachWorkloadStream>, tonic::Status> {
        use nimbus_vsock::Frame;
        use nimbus_vm::attach::{FrameSink, FrameSource};
        use std::sync::mpsc as sync_mpsc;

        tracing::info!("AttachWorkload RPC opened");

        let mut in_stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AttachMessage, tonic::Status>>(64);

        // 1. Wait for the AttachOpen that starts the session.
        let open = match in_stream.message().await {
            Ok(Some(msg)) => match msg.body {
                Some(crate::proto::attach_message::Body::Open(o)) => o,
                _ => {
                    return Err(tonic::Status::invalid_argument(
                        "first message must be AttachOpen",
                    ));
                }
            },
            Ok(None) => {
                return Err(tonic::Status::invalid_argument(
                    "client closed stream before AttachOpen",
                ));
            }
            Err(e) => {
                return Err(tonic::Status::internal(format!("stream error: {e}")));
            }
        };

        // 2. Snapshot the workload state. We can't hold
        //    the lock across the .await points, so we
        //    clone what we need.
        let workload_id = open.workload_id.clone();
        let open_command = open.command.clone();
        let open_env: Vec<String> = open
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let open_working_dir = open.working_dir.clone();

        let state = {
            let workloads = self.workloads.read().await;
            workloads.get(&workload_id).cloned()
        };
        let state = match state {
            Some(s) => s,
            None => {
                return Err(tonic::Status::not_found(format!(
                    "workload not found: {workload_id}"
                )));
            }
        };
        if state.backend != "vm" {
            return Err(tonic::Status::failed_precondition(format!(
                "workload {workload_id} was run with backend={}, not vm; \
                 attach is only supported for vm-backend workloads in v0",
                state.backend
            )));
        }
        let kernel_image_ref = state.kernel_image_ref.clone();
        let rootfs_dir = match state.rootfs_dir.clone() {
            Some(p) => p,
            None => {
                return Err(tonic::Status::failed_precondition(format!(
                    "workload {workload_id} has no materialized rootfs; \
                     was it started by this runtime?"
                )));
            }
        };
        // The AttachOpen can override the command/env/working_dir;
        // if the client sent empty, fall back to the workload's
        // original spec.
        let command = if open_command.is_empty() {
            state.command.clone()
        } else {
            open_command
        };
        let env = if open_env.is_empty() {
            state
                .command
                .iter()
                .filter_map(|s| s.split_once('=').map(|(k, v)| format!("{k}={v}")))
                .collect()
        } else {
            open_env
        };
        let working_dir = if open_working_dir.is_empty() {
            if state.working_dir.is_empty() {
                "/".to_string()
            } else {
                state.working_dir.clone()
            }
        } else {
            open_working_dir
        };

        // 3. Look up the staged kernel. `StagedKernel` is
        //    not `Clone` (it owns a `TempDir`), so we
        //    hold a clone of the cache `Arc` and
        //    re-construct the kernel inside the blocking
        //    task via `from_paths` (which doesn't take
        //    ownership of the dir).
        let kernel_paths = {
            let cache = self.kernel_cache.read().await;
            cache.get(&kernel_image_ref).map(|k| {
                (
                    k.vmlinux_path().to_path_buf(),
                    k.initramfs_path().map(|p| p.to_path_buf()),
                )
            })
        };
        let (vmlinux_path, initramfs_path) = match kernel_paths {
            Some((v, i)) => (v, i),
            None => {
                return Err(tonic::Status::failed_precondition(format!(
                    "workload {workload_id} has no staged kernel for {kernel_image_ref}; \
                     was it started by this runtime?"
                )));
            }
        };
        let kernel = match nimbus_vm::StagedKernel::from_paths(vmlinux_path, initramfs_path) {
            Ok(k) => k,
            Err(e) => {
                return Err(tonic::Status::internal(format!(
                    "reconstruct StagedKernel for {kernel_image_ref}: {e}"
                )));
            }
        };

        // 4. Build the per-VM attach config.
        let cfg = nimbus_vm::AppleVirtAttachConfig {
            kernel,
            rootfs_dir: rootfs_dir.clone(),
            command: command.clone(),
            env: env.clone(),
            working_dir: working_dir.clone(),
            cpus: 1,
            mem_mib: 512,
            vsock_port: Some(nimbus_vm::DEFAULT_VSOCK_PORT),
            // Enable console logging so kernel/init
            // messages land in /tmp/nimbus-attach-console.log
            // for post-mortem debugging. The path is
            // overridable via the NIMBUS_VM_CONSOLE_LOG
            // env var (used by `tools/apple-virt-exec`).
            console_log: Some(
                std::env::var("NIMBUS_VM_CONSOLE_LOG")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/nimbus-attach-console.log")),
            ),
        };

        // 5. Build the std::sync::mpsc channels that
        //    bridge the gRPC stream and the blocking
        //    session task.
        let (client_in_tx, client_in_rx): (sync_mpsc::Sender<Frame>, FrameSource) =
            sync_mpsc::channel();
        let (server_out_tx, server_out_rx): (FrameSink, sync_mpsc::Receiver<Frame>) =
            sync_mpsc::channel();

        // 6. Forwarder: gRPC in_stream → client_in_tx
        //    (std mpsc Sender). Sendable because it
        //    never touches the !Send handle.
        let event_bus_fwd = self.event_bus.clone();
        let workload_id_fwd = workload_id.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                match in_stream.message().await {
                    Ok(Some(msg)) => {
                        let frame_opt = match msg.body {
                            Some(crate::proto::attach_message::Body::Stdin(s)) => {
                                Some(Frame::WorkloadStdin(bytes::Bytes::from(s.data)))
                            }
                            Some(crate::proto::attach_message::Body::StdinEof(_)) => {
                                Some(Frame::StdinEof)
                            }
                            Some(crate::proto::attach_message::Body::Open(_)) => {
                                event_bus_fwd.emit(
                                    Event::new(&workload_id_fwd, EventKind::WorkloadStarted)
                                        .with_metadata("backend", "apple-virt-attach")
                                        .with_metadata("outcome", "duplicate_open")
                                        .with_metadata("error", "client sent AttachOpen after the first one"),
                                );
                                None
                            }
                            Some(crate::proto::attach_message::Body::Stdout(_))
                            | Some(crate::proto::attach_message::Body::Stderr(_))
                            | Some(crate::proto::attach_message::Body::Exit(_))
                            | Some(crate::proto::attach_message::Body::Error(_)) => {
                                // Server-to-client only; ignore.
                                None
                            }
                            None => None,
                        };
                        if let Some(frame) = frame_opt {
                            if client_in_tx.send(frame).is_err() {
                                // Blocking session is gone; stop.
                                return;
                            }
                        }
                    }
                    Ok(None) => {
                        // gRPC client closed the stream;
                        // signal the session.
                        drop(client_in_tx);
                        return;
                    }
                    Err(e) => {
                        let _ = e;
                        drop(client_in_tx);
                        return;
                    }
                }
            }
        });

        // 7. Drainer: server_out_rx (std mpsc Receiver) →
        //    tokio mpsc Sender (gRPC response stream).
        let tx_drain = tx.clone();
        let drainer = tokio::spawn(async move {
            loop {
                let frame = match server_out_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(f) => f,
                    Err(sync_mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(sync_mpsc::RecvTimeoutError::Disconnected) => return,
                };
                let msg = frame_to_attach_message(frame);
                if tx_drain.send(Ok(msg)).await.is_err() {
                    return;
                }
            }
        });

        // 8. Emit a "starting" event so observers can see
        //    attach attempts in the event stream even if
        //    the actual session errors out.
        self.event_bus.emit(
            Event::new(&workload_id, EventKind::WorkloadStarted)
                .with_metadata("backend", "apple-virt-attach")
                .with_metadata("outcome", "pending")
                .with_metadata("image_root", &state.image_root)
                .with_metadata("kernel_image", &kernel_image_ref)
                .with_metadata("command", &command.join(" ")),
        );

        // 9. Spawn the blocking session. This is where
        //    the !Send handle lives; the gRPC handler
        //    never touches it.
        let event_bus_session = self.event_bus.clone();
        let workload_id_session = workload_id.clone();
        let tx_session = tx.clone();
        let cfg_for_session = cfg;
        tokio::task::spawn_blocking(move || {
            let result = nimbus_vm::run_session_blocking(cfg_for_session, client_in_rx, server_out_tx);
            // Map the result into either a final AttachError
            // message (on error) or just close the stream
            // (on success — the WorkloadExit was already
            // forwarded by the drainer).
            if let Err(err) = result {
                event_bus_session.emit(
                    Event::new(&workload_id_session, EventKind::WorkloadStarted)
                        .with_metadata("backend", "apple-virt-attach")
                        .with_metadata("outcome", "failed")
                        .with_metadata("error", &err.to_string()),
                );
                let body = attach_error_to_status(&err);
                // Best-effort: try to push the error on the
                // gRPC response stream. The receiver may
                // already be gone (client hung up); that's
                // fine.
                let msg = AttachMessage {
                    body: Some(crate::proto::attach_message::Body::Error(
                        crate::proto::AttachError {
                            message: err.to_string(),
                        },
                    )),
                };
                let _ = tx_session.blocking_send(Err(body));
                let _ = msg;
            } else {
                event_bus_session.emit(
                    Event::new(&workload_id_session, EventKind::WorkloadStarted)
                        .with_metadata("backend", "apple-virt-attach")
                        .with_metadata("outcome", "session_ended"),
                );
            }
            // Drop the forwarder + drainer; their channels
            // are already half-closed (or about to be).
            drop(forwarder);
            drop(drainer);
        });

        Ok(tonic::Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    // ------------------------------------------------------------------
    // Phase A: CRI support RPCs (stubs → prod)
    // ------------------------------------------------------------------

    async fn has_image(
        &self,
        request: tonic::Request<HasImageRequest>,
    ) -> Result<tonic::Response<HasImageResponse>, tonic::Status> {
        let _ = request;
        Err(tonic::Status::unimplemented("has_image not yet implemented"))
    }

    async fn list_images(
        &self,
        _request: tonic::Request<ListImagesRequest>,
    ) -> Result<tonic::Response<ListImagesResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListImagesResponse { images: vec![] }))
    }

    async fn remove_image(
        &self,
        request: tonic::Request<RemoveImageRequest>,
    ) -> Result<tonic::Response<RemoveImageResponse>, tonic::Status> {
        let _ = request;
        Ok(tonic::Response::new(RemoveImageResponse {
            success: false,
            bytes_freed: 0,
        }))
    }

    async fn dag_store_info(
        &self,
        _request: tonic::Request<DagStoreInfoRequest>,
    ) -> Result<tonic::Response<DagStoreInfoResponse>, tonic::Status> {
        let total = self.store.total_bytes();
        Ok(tonic::Response::new(DagStoreInfoResponse {
            mountpoint: "/var/lib/nimbus/dag".into(),
            total_bytes: total as i64,
            total_nodes: 0,
            used_bytes: total as i64,
            inodes_used: 0,
        }))
    }

    type PortForwardStream = tokio_stream::wrappers::ReceiverStream<
        Result<PortForwardData, tonic::Status>,
    >;

    async fn port_forward(
        &self,
        request: tonic::Request<PortForwardRequest>,
    ) -> Result<tonic::Response<Self::PortForwardStream>, tonic::Status> {
        let _ = request;
        Err(tonic::Status::unimplemented(
            "port_forward not yet implemented",
        ))
    }

    async fn update_workload(
        &self,
        request: tonic::Request<UpdateWorkloadRequest>,
    ) -> Result<tonic::Response<UpdateWorkloadResponse>, tonic::Status> {
        let req = request.into_inner();
        let cpu = if req.cpu_millicores > 0 {
            Some(req.cpu_millicores)
        } else {
            None
        };
        let mem = if req.memory_bytes > 0 {
            Some(req.memory_bytes)
        } else {
            None
        };
        if cpu.is_none() && mem.is_none() {
            return Ok(tonic::Response::new(UpdateWorkloadResponse { success: false }));
        }
        match self.executor.update(&req.id, cpu, mem).await {
            Ok(()) => {
                info!(id = %req.id, cpu_millicores = ?cpu, memory_bytes = ?mem, "workload resources updated");
                Ok(tonic::Response::new(UpdateWorkloadResponse { success: true }))
            }
            Err(e) => {
                warn!(id = %req.id, error = %e, "workload resource update failed");
                Ok(tonic::Response::new(UpdateWorkloadResponse { success: false }))
            }
        }
    }

    async fn get_workload_stats(
        &self,
        request: tonic::Request<GetWorkloadStatsRequest>,
    ) -> Result<tonic::Response<ProtoWorkloadStats>, tonic::Status> {
        let req = request.into_inner();
        match self.executor.stats(&req.id).await {
            Ok(s) => Ok(tonic::Response::new(ProtoWorkloadStats {
                id: s.id,
                cpu_usage_percent: s.cpu_usage_percent,
                memory_bytes: s.memory_bytes,
                disk_bytes: s.disk_bytes,
                network_rx_bytes: s.network_rx_bytes,
                network_tx_bytes: s.network_tx_bytes,
            })),
            Err(e) => Err(tonic::Status::not_found(format!("{e}"))),
        }
    }

    // ------------------------------------------------------------------
    // Build/Push/Save/Load RPCs
    // ------------------------------------------------------------------

    /// Native DAG-aware build. Parses a Dockerfile, pulls the base image,
    /// executes RUN instructions via runc, handles COPY/ADD, and snapshots
    /// each layer into the DAG store — all without Docker.
    async fn build_image(
        &self,
        request: tonic::Request<BuildImageRequest>,
    ) -> Result<tonic::Response<BuildImageResponse>, tonic::Status> {
        let req = request.into_inner();

        // Read Dockerfile
        let dockerfile_path = PathBuf::from(&req.dockerfile);
        let content = tokio::fs::read_to_string(&dockerfile_path)
            .await
            .map_err(|e| {
                tonic::Status::invalid_argument(format!("read Dockerfile {}: {e}", dockerfile_path.display()))
            })?;

        let dockerfile = nimbus_oci::Dockerfile::parse(&content).map_err(|e| {
            tonic::Status::invalid_argument(format!("parse Dockerfile: {e}"))
        })?;

        // Resolve context dir
        let context_dir = if req.context_dir.is_empty() {
            dockerfile_path.parent().unwrap_or(&PathBuf::from(".")).to_path_buf()
        } else {
            PathBuf::from(&req.context_dir)
        };

        // Determine runc path
        let runc_path = self.config.bundle_root.join("..").join("runc");
        let runc_path = if runc_path.is_file() {
            runc_path
        } else {
            PathBuf::from("runc")
        };

        let builder = crate::builder::DagBuilder::new(
            self.store.clone(),
            runc_path,
            self.config.bundle_root.join("build"),
            self.config.insecure_registries.clone(),
        );

        let build_args: std::collections::HashMap<String, String> = req.build_args.clone();

        let result = builder
            .build(&dockerfile, &context_dir, &build_args)
            .await
            .map_err(|e| tonic::Status::internal(format!("build failed: {e}")))?;

        let tag = if req.tag.is_empty() {
            format!("{}", &result.root_digest[..12])
        } else {
            req.tag.clone()
        };

        // Record the image tag -> root_digest mapping
        {
            let mut tags = self.image_tags.write().await;
            tags.insert(result.root_digest.clone(), tag.clone());
        }

        Ok(tonic::Response::new(BuildImageResponse {
            root_digest: result.root_digest,
            tag,
        }))
    }

    async fn push_image(
        &self,
        request: tonic::Request<PushImageRequest>,
    ) -> Result<tonic::Response<PushImageResponse>, tonic::Status> {
        let req = request.into_inner();
        let auth = build_auth(
            &req.registry_username,
            &req.registry_password,
            &req.registry_token,
        );
        let pusher = DagPusher::new(
            self.store.clone(),
            auth,
            self.config.insecure_registries.clone(),
        );
        let (manifest_digest, bytes_pushed) = pusher
            .push(&req.root_digest, &req.target_ref)
            .await
            .map_err(|e| tonic::Status::internal(format!("push failed: {e}")))?;

        Ok(tonic::Response::new(PushImageResponse {
            manifest_digest,
            bytes_pushed,
        }))
    }

    type ExportImageStream = tokio_stream::wrappers::ReceiverStream<
        Result<ExportImageChunk, tonic::Status>,
    >;

    async fn export_image(
        &self,
        request: tonic::Request<ExportImageRequest>,
    ) -> Result<tonic::Response<Self::ExportImageStream>, tonic::Status> {
        let req = request.into_inner();

        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ExportImageChunk, tonic::Status>>(8);

        let store = self.store.clone();
        let root_digest = req.root_digest.clone();
        tokio::spawn(async move {
            // Write tar to a Vec, then chunk and send.
            let mut buf = Vec::new();
            if let Err(e) = export_dag_to_tar(&*store, &root_digest, &mut buf) {
                let _ = tx
                    .send(Err(tonic::Status::internal(format!("export failed: {e}"))))
                    .await;
                return;
            }
            // Chunk the buffer into 64KB pieces.
            for chunk in buf.chunks(64 * 1024) {
                if tx
                    .send(Ok(ExportImageChunk {
                        data: chunk.to_vec(),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }

    async fn import_image(
        &self,
        request: tonic::Request<tonic::Streaming<ImportImageChunk>>,
    ) -> Result<tonic::Response<ImportImageResponse>, tonic::Status> {
        let mut in_stream = request.into_inner();
        let store = self.store.clone();

        // Buffer all incoming chunks.
        let mut buf = Vec::new();
        while let Some(chunk) = in_stream
            .message()
            .await
            .map_err(|e| tonic::Status::internal(format!("stream error: {e}")))?
        {
            buf.extend_from_slice(&chunk.data);
        }

        // Import from the buffered data.
        let (root_digest, bytes_stored, bytes_deduplicated) = tokio::task::spawn_blocking(
            move || import_dag_from_tar(&*store, &buf[..]),
        )
        .await
        .map_err(|e| tonic::Status::internal(format!("import task join: {e}")))?
        .map_err(|e| tonic::Status::internal(format!("import failed: {e}")))?;

        Ok(tonic::Response::new(ImportImageResponse {
            root_digest,
            bytes_stored,
            bytes_deduplicated,
        }))
    }

    async fn copy_file(
        &self,
        request: tonic::Request<CopyFileRequest>,
    ) -> Result<tonic::Response<CopyFileResponse>, tonic::Status> {
        let req = request.into_inner();
        if req.id.is_empty() || req.container_path.is_empty() {
            return Err(Status::invalid_argument("id and container_path are required"));
        }

        // Look up the workload to get the rootfs path.
        let rootfs_path = {
            let workloads = self.workloads.read().await;
            let state = workloads
                .get(&req.id)
                .ok_or_else(|| Status::not_found(format!("workload {} not found", req.id)))?;

            // For VM workloads, rootfs is tracked in the rootfs_cache.
            // For container workloads, it's at bundle_root/{id}/rootfs.
            let rootfs = if state.backend == "vm" || state.backend == "sandbox" {
                let cache = self.rootfs_cache.read().await;
                cache
                    .get(&req.id)
                    .cloned()
                    .ok_or_else(|| {
                        Status::failed_precondition(
                            format!("rootfs not materialized for workload {}", req.id),
                        )
                    })?
            } else {
                self.config.bundle_root.join(&req.id).join("rootfs")
            };

            rootfs
        };

        let container_path = req.container_path.trim_start_matches('/');
        let full_path = rootfs_path.join(container_path);

        // Security: ensure we don't escape the rootfs via symlinks or "..".
        let canonical = full_path
            .canonicalize()
            .map_err(|e| Status::internal(format!("cannot resolve path: {e}")))?;
        if !canonical.starts_with(&rootfs_path) {
            return Err(Status::invalid_argument(
                "container_path escapes the root filesystem",
            ));
        }

        match req.direction.as_str() {
            "out" => {
                use std::os::unix::fs::PermissionsExt;
                // Read file from container rootfs.
                let metadata = tokio::fs::metadata(&canonical)
                    .await
                    .map_err(|e| Status::not_found(format!("file not found: {e}")))?;
                if !metadata.is_file() {
                    return Err(Status::invalid_argument("path is not a file"));
                }
                let content = tokio::fs::read(&canonical)
                    .await
                    .map_err(|e| Status::internal(format!("read error: {e}")))?;
                Ok(tonic::Response::new(CopyFileResponse {
                    id: req.id.clone(),
                    container_path: req.container_path,
                    content,
                    mode: metadata.permissions().mode() as u32,
                    size: metadata.len(),
                }))
            }
            "in" => {
                use std::os::unix::fs::PermissionsExt;
                // Write file to container rootfs. Create parent dirs.
                if let Some(parent) = canonical.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| Status::internal(format!("create dirs: {e}")))?;
                }
                tokio::fs::write(&canonical, &req.content)
                    .await
                    .map_err(|e| Status::internal(format!("write error: {e}")))?;
                // Set file mode if specified.
                if req.mode != 0 {
                    tokio::fs::set_permissions(&canonical, std::fs::Permissions::from_mode(req.mode))
                        .await
                        .map_err(|e| Status::internal(format!("chmod error: {e}")))?;
                }
                let metadata = tokio::fs::metadata(&canonical)
                    .await
                    .map_err(|e| Status::internal(format!("stat error: {e}")))?;
                Ok(tonic::Response::new(CopyFileResponse {
                    id: req.id.clone(),
                    container_path: req.container_path,
                    content: Vec::new(),
                    mode: metadata.permissions().mode() as u32,
                    size: metadata.len(),
                }))
            }
            other => Err(Status::invalid_argument(format!(
                "invalid direction: {other:?}; expected 'in' or 'out'"
            ))),
        }
    }
}

/// Translate a `nimbus_vsock::Frame` from the blocking
/// session task into a gRPC `AttachMessage`.
fn frame_to_attach_message(frame: nimbus_vsock::Frame) -> AttachMessage {
    use crate::proto::{
        attach_message::Body as Body, AttachError, AttachExit, AttachStderr,
        AttachStdout,
    };
    let body = match frame {
        nimbus_vsock::Frame::WorkloadStdout(b) => Body::Stdout(AttachStdout { data: b.to_vec() }),
        nimbus_vsock::Frame::WorkloadStderr(b) => Body::Stderr(AttachStderr { data: b.to_vec() }),
        nimbus_vsock::Frame::WorkloadExit { exit_code, signal } => Body::Exit(AttachExit {
            exit_code: exit_code.unwrap_or(0),
            signal: signal.unwrap_or(0),
            has_exit_code: exit_code.is_some(),
            has_signal: signal.is_some(),
        }),
        nimbus_vsock::Frame::Error(msg) => Body::Error(AttachError { message: msg }),
        // The other variants are server→guest, not seen
        // on the read path. If we see one, log and skip.
        other => Body::Error(AttachError {
            message: format!("unexpected frame from guest: {other:?}"),
        }),
        // Touch unused variants to keep the match
        // exhaustive.
        #[allow(unused)]
        nimbus_vsock::Frame::InitHello { .. } | nimbus_vsock::Frame::WorkloadSpec { .. } => {
            Body::Error(AttachError {
                message: "unexpected frame from guest (init)".into(),
            })
        }
        #[allow(unused)]
        nimbus_vsock::Frame::WorkloadStdin(_) | nimbus_vsock::Frame::StdinEof => {
            Body::Error(AttachError {
                message: "unexpected frame from guest (stdin)".into(),
            })
        }
    };
    AttachMessage { body: Some(body) }
}

/// Translate a `nimbus_vm::attach::AttachError` into the
/// matching gRPC `Status` code.
fn attach_error_to_status(err: &nimbus_vm::attach::AttachError) -> tonic::Status {
    use nimbus_vm::attach::AttachError as E;
    match err {
        E::BackendUnavailable(msg) => tonic::Status::failed_precondition(msg),
        E::NotFound(id) => tonic::Status::not_found(format!("workload not found: {id}")),
        E::InvalidConfig(msg) => tonic::Status::invalid_argument(msg),
        E::Vm(msg) => tonic::Status::internal(format!("VM error: {msg}")),
        E::Vsock(msg) => tonic::Status::internal(format!("vsock error: {msg}")),
        E::Workload(msg) => tonic::Status::internal(format!("workload error: {msg}")),
    }
}

/// Stage a kernel OCI image into a temp directory.
///
/// This is a blocking helper (it does file I/O + a network
/// pull via the OCI client). Callers should run it via
/// `tokio::task::spawn_blocking`.
///
/// It uses the standard OCI pipeline:
///   1. `OciPuller::pull` — fetches the manifest + all
///      layer blobs (verifies digests).
///   2. `OciToDagConverter::convert` — builds the rkyv
///      DAG in the runtime's shared `MmapStore`.
///   3. `StagedKernel::from_image` — materializes
///      `/boot/vmlinux` (+ optional `/boot/initramfs.cpio.gz`)
///      from the DAG into a temp dir.
///
/// The returned `StagedKernel` owns the temp dir; dropping
/// it cleans up. The caller is expected to insert it into
/// the `RuntimeService::kernel_cache` (which is `Arc<RwLock>`)
/// so it lives for as long as the workload references it.
fn stage_kernel_image(
    store: &Arc<MmapStore>,
    image_ref: &str,
    insecure_registries: &std::collections::HashSet<String>,
) -> Result<StagedKernel, Box<dyn std::error::Error + Send + Sync>> {
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|e| format!("no tokio runtime in stage_kernel_image: {e}"))?;
    let insecure_registries: Vec<String> = insecure_registries.iter().cloned().collect();
    let staged = rt.block_on(StagedKernel::from_image_with_insecure(
        image_ref,
        store,
        None,
        &insecure_registries,
    ))?;
    Ok(staged)
}

/// Materialize the workload's OCI image to a temp directory.
///
/// `manifest_digest` is the rkyv DAG root digest stored in
/// the runtime's `MmapStore`. The materializer walks the DAG
/// and unpacks it into `target_dir`.
///
/// This is a blocking helper. Callers should run it via
/// `tokio::task::spawn_blocking`. The returned `PathBuf` is
/// owned by the caller; cleaning it up is the caller's
/// responsibility (the runtime inserts the path into
/// `RuntimeService::rootfs_cache` and removes it when the
/// workload is stopped).
fn materialize_rootfs(
    store: &MmapStore,
    manifest_digest: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    use nimbus_store::Digest;
    let target = std::env::temp_dir().join(format!(
        "nimbus-rootfs-{}-{}",
        manifest_digest.replace(':', "_").replace('/', "_"),
        std::process::id(),
    ));
    if target.exists() {
        // Already materialized (e.g. the workload was run
        // before in the same process). Reuse.
        return Ok(target);
    }
    std::fs::create_dir_all(&target)?;
    let digest: Digest = manifest_digest
        .parse()
        .map_err(|e| format!("parse manifest digest {manifest_digest}: {e}"))?;
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|e| format!("no tokio runtime in materialize_rootfs: {e}"))?;
    let materializer = OciMaterializer::new(store);
    rt.block_on(materializer.materialize_into(&digest, &target))?;
    Ok(target)
}

/// Build an `OciAuth` from optional protobuf string fields.
/// Returns `None` when all fields are empty.
fn build_auth(username: &str, password: &str, token: &str) -> Option<OciAuth> {
    if username.is_empty() && password.is_empty() && token.is_empty() {
        return None;
    }
    Some(OciAuth {
        username: if username.is_empty() { None } else { Some(username.to_string()) },
        password: if password.is_empty() { None } else { Some(password.to_string()) },
        registry_token: if token.is_empty() { None } else { Some(token.to_string()) },
    })
}
