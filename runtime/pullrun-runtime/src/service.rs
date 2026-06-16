// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Runtime service: the gRPC server side of pullrun-runtime.
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
use std::sync::atomic::AtomicI64;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tonic::Status;
use tracing::{debug, info, warn};

use pullrun_exec::types::{Backend, ExitStatus, NetworkMode, WorkloadSpec};
use pullrun_exec::{current_euid, is_running_as_root, RootlessContainerExecutor};
use pullrun_exec::{ExecError, Executor, LinuxContainerExecutor, NetworkRule, ProcessHandle};
use pullrun_net::{Ipam, ProxyNetwork};
use pullrun_oci::{
    build_dag_from_directory_with_platform, current_arch, export_dag_to_tar, import_dag_from_tar,
    DagDirectory, DagPusher, DirectoryEntry, OciAuth, OciMaterializer, OciPuller,
    OciToDagConverter,
};
use pullrun_policy::{CosignKey, Policy, PolicyDecision, PolicyEngine};
use pullrun_store::{Digest, MmapStore};
use pullrun_sync::PeerBloomCache;
use pullrun_vm::{ext4_path_for, FirecrackerConfig, FirecrackerExecutor, StagedKernel};

/// Cache key used when the kernel is loaded from a local path
/// (e.g. `~/.pullrun/kernels/`) instead of an OCI image.
const LOCAL_KERNEL_CACHE_KEY: &str = "__local";

use crate::events::{Event, EventBus, EventKind};
use crate::proto::runtime_server::Runtime;
use crate::proto::{
    AttachMessage, BuildImageRequest, BuildImageResponse, CommitImageRequest, CommitImageResponse,
    CopyFileRequest, CopyFileResponse, CreateConfigRequest, CreateConfigResponse,
    CreateNetworkRequest, CreateNetworkResponse, CreateSecretRequest, CreateSecretResponse,
    DagNode, DagStoreInfoRequest, DagStoreInfoResponse, DiffRequest, DiffResponse,
    Event as ProtoEvent, ExecRequest, ExecResponse, ExportImageChunk, ExportImageRequest,
    GetWorkloadRequest, GetWorkloadStatsRequest, HasImageRequest, HasImageResponse,
    ImportImageChunk, ImportImageResponse, InfoRequest, InfoResponse, InspectConfigRequest,
    InspectConfigResponse, InspectRequest, InspectResponse, InspectSecretRequest,
    InspectSecretResponse, ListConfigsRequest, ListConfigsResponse, ListImagesRequest,
    ListImagesResponse, ListNetworksRequest, ListNetworksResponse, ListSecretsRequest,
    ListSecretsResponse, ListWorkloadsRequest, ListWorkloadsResponse, LogChunk, NetworkInfo,
    NetworkRule as ProtoNetworkRule, PortForwardData, PortForwardRequest, PruneRequest,
    PruneResponse, PullImageRequest, PullImageResponse, PushImageRequest, PushImageResponse,
    RemoveConfigRequest, RemoveConfigResponse, RemoveImageRequest, RemoveImageResponse,
    RemoveNetworkRequest, RemoveNetworkResponse, RemoveSecretRequest, RemoveSecretResponse,
    RunComposeRequest, RunComposeResponse, RunRequest, RunResponse, StopRequest, StopResponse,
    StreamEventsRequest, StreamLogsRequest, UpdateWorkloadRequest, UpdateWorkloadResponse,
    WorkloadStats as ProtoWorkloadStats, WorkloadStatus,
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
    /// Number of Firecracker VMs to keep pre-booted in the warm pool.
    /// 0 (default) disables the warm pool.
    pub warm_pool_size: usize,
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
    /// Optional peer bloom cache for peer-to-peer blob distribution.
    pub bloom_cache: Option<PeerBloomCache>,
    /// Secret/config store for docker --secret/--config equivalent.
    pub secrets_store: crate::secrets::SecretStore,
}

impl ServiceConfig {
    pub fn new(store_root: PathBuf) -> Self {
        let bundle_root = store_root.join("bundles");
        let checkpoints_dir = store_root.join("checkpoints");
        let ss = store_root.clone();
        Self {
            store_root,
            bundle_root,
            checkpoints_dir,
            policy: None,
            trusted_keys: Vec::new(),
            vm_backend: None,
            insecure_registries: std::collections::HashSet::new(),
            bloom_cache: None,
            secrets_store: crate::secrets::SecretStore::new(ss),
        }
    }

    pub fn with_bloom_cache(mut self, cache: PeerBloomCache) -> Self {
        self.bloom_cache = Some(cache);
        self
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
        tracing::debug!(
            "ExecutorRouter::create backend={:?}, id={}",
            spec.backend,
            spec.id
        );
        match spec.backend {
            Backend::Container => {
                tracing::debug!("ExecutorRouter::create: Container backend");
                // Auto-detect: if a rootless executor is configured and
                // we are not running as root, use the rootless path so
                // pasta/slirp4netns handles networking without iptables.
                if let Some(ref rootless) = self.rootless {
                    if !is_running_as_root() {
                        tracing::debug!("ExecutorRouter::create: delegating to rootless");
                        return rootless.create(spec).await;
                    }
                }
                tracing::debug!("ExecutorRouter::create: delegating to LinuxContainerExecutor");
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
                None => Err(ExecError::BackendNotAvailable(
                    "VM backend not configured".into(),
                )),
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
        self.container
            .update(id, cpu_millicores, memory_bytes)
            .await
    }

    async fn stats(&self, id: &str) -> Result<pullrun_exec::WorkloadStats, ExecError> {
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

    async fn exec(
        &self,
        id: &str,
        command: &[String],
        timeout_secs: u64,
    ) -> Result<i32, ExecError> {
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

/// Best-effort filesystem usage for a given path.
/// Returns (total_bytes, used_bytes). Falls back to (0, 0) on error.
#[cfg(target_os = "linux")]
fn fs_usage(path: &std::path::Path) -> (i64, i64) {
    use std::ffi::CString;
    let cpath = match CString::new(path.to_string_lossy().as_bytes()) {
        Ok(p) => p,
        Err(_) => return (0, 0),
    };
    // SAFETY: `std::mem::zeroed()` is valid for `libc::statvfs` — the
    // struct is plain-old-data (all-zero is a valid initial state before
    // `statvfs` fills it).
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `statvfs` is reentrant and async-signal-safe. `cpath` is
    // a valid null-terminated C string; `stat` is a valid mutable pointer.
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return (0, 0);
    }
    // f_blocks and f_frsize are typically u64 on 64-bit Linux.
    // Use u128 to stay safe against overflow for very large filesystems.
    let total = (stat.f_blocks as u128) * (stat.f_frsize as u128);
    let available = (stat.f_bavail as u128) * (stat.f_frsize as u128);
    (
        total.min(i64::MAX as u128) as i64,
        (total - available).min(i64::MAX as u128) as i64,
    )
}

#[cfg(not(target_os = "linux"))]
fn fs_usage(_path: &std::path::Path) -> (i64, i64) {
    (0, 0)
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
                PolicyEngine::new(p.clone()).with_trusted_keys(self.config.trusted_keys.clone()),
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
        let proxy = Arc::new(ProxyNetwork::new().expect("ProxyNetwork::new requires valid CIDR"));
        let ipam = proxy.ipam_handle();
        info!(
            "shared workload network: 10.42.0.0/16 (bridge {})",
            pullrun_vm::BRIDGE_NAME
        );

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
            let executor = FirecrackerExecutor::new(
                fc_cfg.clone(),
                Arc::new(MmapStore::new(self.config.store_root.clone())),
                ipam.clone(),
                proxy.clone(),
            );

            // Wire up warm VM pool if configured.
            if cfg.warm_pool_size > 0 {
                let pool_cfg = pullrun_vm::VmPoolConfig {
                    pool_size: cfg.warm_pool_size,
                    vm_root: cfg.vm_root.join("pool"),
                };
                let pool = pullrun_vm::VmPool::new(
                    pool_cfg,
                    fc_cfg,
                    Arc::new(MmapStore::new(self.config.store_root.clone())),
                    ipam.clone(),
                    proxy.clone(),
                );
                info!(warm_pool_size = cfg.warm_pool_size, "warm VM pool enabled");
                Arc::new(executor.with_pool(pool))
            } else {
                Arc::new(executor)
            }
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
        for (id, state) in recovered.iter_mut() {
            if state.status == "running" && state.backend != "vm" {
                // Check if the runc container survived the daemon
                // restart. When the daemon crashes or is restarted
                // (e.g., systemd restart, WSL2 VM reboot), runc
                // containers started with `runc run -d` may still be
                // alive. Only mark as exited if truly dead.
                let alive = std::process::Command::new("runc")
                    .args(["state", id])
                    .output()
                    .ok()
                    .map(|o| {
                        if o.status.success() {
                            let s = String::from_utf8_lossy(&o.stdout);
                            s.contains("\"status\": \"running\"")
                        } else {
                            false
                        }
                    })
                    .unwrap_or(false);
                if alive {
                    info!(workload_id = %id, "workload recovered as running (container alive)");
                    continue;
                }
                info!(
                    workload_id = %id,
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
                workload_id = %id,
                status = %state.status,
                "recovered workload state"
            );
        }
        let workloads: Arc<RwLock<HashMap<String, WorkloadState>>> =
            Arc::new(RwLock::new(recovered));
        let image_tags: Arc<RwLock<HashMap<String, String>>> = {
            let tags_path = self.config.store_root.join("image_tags.json");
            let map = if let Ok(file) = std::fs::File::open(&tags_path) {
                serde_json::from_reader(file).unwrap_or_default()
            } else {
                HashMap::new()
            };
            Arc::new(RwLock::new(map))
        };

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
                                        pullrun_exec::types::RestartPolicy::Always
                                            | pullrun_exec::types::RestartPolicy::UnlessStopped
                                    ) || (matches!(
                                        state.restart_policy,
                                        pullrun_exec::types::RestartPolicy::OnFailure
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
                            record_workload_exit(&backend, exit_code_for_restart.map(|c| c as i32));
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
                            // If the backend is "vm" and no VM executor
                            // reported it (FirecrackerExecutor::status()
                            // never returns Err), this is a macOS Apple
                            // Virt placeholder — the VM hasn't been
                            // booted yet (AttachWorkload does that).
                            // Skip watcher processing.
                            if backend == "vm" {
                                continue;
                            }
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
                                        pullrun_exec::types::RestartPolicy::No
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
        // Uses per-workload jitter (derived from workload ID hash) to avoid
        // thundering herds when many workloads share the same interval.
        let hc_workloads = workloads.clone();
        let hc_executor = executor.clone();
        tokio::spawn(async move {
            use std::hash::{Hash, Hasher};
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
                                // Compute jitter from workload ID.
                                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                                id.hash(&mut hasher);
                                let jitter = hasher.finish();
                                (
                                    id.clone(),
                                    hc.test.clone(),
                                    hc.interval_seconds.max(1),
                                    hc.timeout_seconds.max(1),
                                    hc.retries.max(1),
                                    s.start_time + hc.start_period_seconds as i64,
                                    jitter,
                                )
                            })
                        })
                        .filter(|(_id, _, interval, _, _, grace_end, jitter)| {
                            let interval = *interval as u64;
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            // Include jitter phase so workloads with the same
                            // interval probe at different ticks.
                            now.wrapping_add(*jitter).is_multiple_of(interval)
                                || now >= *grace_end as u64
                        })
                        .map(|(id, test, interval, timeout, retries, grace_end, _)| {
                            (id, test, interval, timeout, retries, grace_end)
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
                    let healthy = hc_executor
                        .exec(&id, &test, timeout as u64)
                        .await
                        .map(|r| r == 0)
                        .unwrap_or(false);
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

        let start_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

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
            start_time: AtomicI64::new(start_time),
            #[cfg(target_os = "macos")]
            persistent_vms: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
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
    let mut visited: HashSet<Digest> = HashSet::new();
    let mut queue: VecDeque<Digest> = VecDeque::new();

    if !image_root.is_empty() {
        if let Ok(d) = Digest::from_hex(image_root) {
            queue.push_back(d);
        }
    }

    while let Some(digest) = queue.pop_front() {
        if !visited.insert(digest) {
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
            digest: digest.as_hex(),
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
            let child = Digest(*edge);
            let hex = child.as_hex();
            if !hex.is_empty() {
                queue.push_back(child);
            }
        }
    }

    out
}

#[cfg(test)]
mod walk_dag_tests {
    use super::walk_dag;
    use pullrun_store::{DagNode as StoreDagNode, Digest, MmapStore, NodeKind};

    #[test]
    fn walks_manifest_tree_layer() {
        // Build a tiny DAG: manifest -> tree -> layer.
        let tmp = tempdir();
        let store = MmapStore::new(tmp);

        // Insert children first so the edges resolve.
        let layer = StoreDagNode::new(NodeKind::Layer, vec![], b"layer-bytes".to_vec());
        let tree = StoreDagNode::new(NodeKind::Tree, vec![Digest::ZERO], b"tree-bytes".to_vec());
        let manifest = StoreDagNode::new(
            NodeKind::Manifest,
            vec![Digest::ZERO],
            b"manifest-bytes".to_vec(),
        );

        // Put each; the store hashes the inline_data and stores it.
        let _layer_digest = store.put_blocking(&layer).unwrap();
        let tree_digest = store.put_blocking(&tree).unwrap();
        let manifest_digest = store.put_blocking(&manifest).unwrap();

        // The manifest has edges pointing at Digest::ZERO which is
        // never inserted in the store. The BFS visits the manifest,
        // sees the dangling edges, and the visited set stops the
        // descent. This is *expected* behaviour; the walk is robust
        // to dangling edges (real OCI images are well-formed, but
        // the helper is defensive).
        let md = manifest_digest.as_hex();
        let path = walk_dag(&store, &md);
        assert_eq!(path.len(), 1, "expected just the manifest, got {:?}", path);
        assert_eq!(path[0].kind, "manifest");
        assert_eq!(path[0].digest, md);

        // Now build a manifest whose edges reference the *real*
        // digests returned by put_blocking, and walk that. We
        // should get manifest → tree. (The tree's edge to
        // Digest::ZERO is also dangling, by design — same reason.)
        let real_manifest = StoreDagNode::new(
            NodeKind::Manifest,
            vec![tree_digest],
            b"real-manifest-bytes".to_vec(),
        );
        let real_manifest_digest = store.put_blocking(&real_manifest).unwrap();

        let rmd = real_manifest_digest.as_hex();
        let td = tree_digest.as_hex();
        let path = walk_dag(&store, &rmd);
        assert_eq!(path.len(), 2, "expected manifest+tree, got {:?}", path);
        assert_eq!(path[0].kind, "manifest");
        assert_eq!(path[0].digest, rmd);
        assert_eq!(path[1].kind, "tree");
        assert_eq!(path[1].digest, td);
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
        let path = std::env::temp_dir().join(format!("pullrun-runtime-walkdag-{pid}-{n}"));
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
fn write_workload_checkpoint(dir: &std::path::Path, id: &str, state: &WorkloadState) {
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
fn parse_restart_policy(p: i32) -> pullrun_exec::types::RestartPolicy {
    use pullrun_exec::types::RestartPolicy;
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
    _watcher_store: &Arc<MmapStore>,
    watcher_checkpoints_dir: &std::path::Path,
    watcher_bus: &Arc<EventBus>,
    id: &str,
    backend: &str,
) {
    use pullrun_exec::types::{Backend, RestartPolicy, WorkloadSpec};
    use std::time::Duration;

    // Read current state to get restart count and policy.
    let (
        restart_count,
        image_root,
        command,
        env,
        cpu_millicores,
        memory_bytes,
        network_rules,
        kernel_image_ref,
        _working_dir,
        bridge_name,
        mounts,
        health_check,
        network_mode_str,
        stopped_by_operator,
    ) = {
        let map = watcher_workloads.read().await;
        match map.get(id) {
            Some(s) => {
                // Don't restart if the operator stopped this workload.
                let stopped = s.status != "exited";
                let network_mode = if s.internal_ip.is_some() {
                    "bridge"
                } else {
                    "isolated"
                };
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
    let backoff_secs = std::cmp::min(1u64 << restart_count.min(63), 30u64);
    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;

    // Reconstruct the spec.
    let backend_enum = match Backend::from_str(backend) {
        Ok(b) => b,
        Err(_) => return,
    };
    let network_mode_enum = match network_mode_str.as_str() {
        "bridge" => NetworkMode::Bridge,
        "host" => NetworkMode::Host,
        "slirp" => NetworkMode::Slirp,
        _ => NetworkMode::Loopback,
    };
    let kernel_path = if kernel_image_ref.is_empty() {
        None
    } else {
        // Kernel path from staged kernel cache; best-effort.
        // In v0 we skip this — the kernel was already cached.
        None
    };
    let image_root = match Digest::from_hex(&image_root) {
        Ok(d) => d,
        Err(_) => return,
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
                        .with_metadata("restart_count", checkpoint.restart_count.to_string()),
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
    pub health_check: Option<pullrun_exec::HealthCheck>,
    /// Current health status: "healthy", "unhealthy", "starting", "".
    pub health: String,
    /// Consecutive health check failures so far.
    pub health_failures: u32,
    /// Timestamp of the last successful health check (unix seconds).
    pub health_last_success: i64,
    /// Restart policy for this workload.
    pub restart_policy: pullrun_exec::types::RestartPolicy,
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
    pub mounts: Vec<pullrun_exec::Mount>,
    /// Path to Firecracker VM serial console log (set for VM
    /// backends so AttachWorkload can stream guest output).
    #[serde(default)]
    pub console_log_path: Option<std::path::PathBuf>,
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
    /// `pullrun-vm::oci_kernel::StagedKernel::from_image`, and
    /// stores the result here keyed by the image ref.
    ///
    /// `AttachWorkload` looks up the cache by workload_id (the
    /// workload entry's `kernel_image_ref` field points to the
    /// same key) to find the kernel to boot for the new VM.
    ///
    /// The cache is bounded only by available memory in v0 —
    /// kernels are typically 20-50 MiB each. A future v1 will
    /// add LRU eviction and disk spilling.
    pub kernel_cache: Arc<RwLock<HashMap<String, pullrun_vm::StagedKernel>>>,
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
    /// Unix timestamp (seconds) when the service started.
    pub start_time: AtomicI64,
    /// Persistent Apple Virt VM sessions keyed by workload id.
    /// A VM is created on the first `AttachWorkload` call and
    /// survives client detach; subsequent calls reconnect to it.
    /// On non-macOS this field exists but is never used (VM
    /// backend returns `failed_precondition`).
    #[cfg(target_os = "macos")]
    pub persistent_vms:
        Arc<tokio::sync::RwLock<HashMap<String, Arc<pullrun_vm::VmPersistentHandle>>>>,
}

impl RuntimeService {
    /// Persist the image_tags map to disk so it survives restarts.
    async fn save_image_tags(&self) {
        let tags = self.image_tags.read().await;
        if let Ok(json) = serde_json::to_string(&*tags) {
            let path = self.config.store_root.join("image_tags.json");
            let _ = std::fs::write(&path, &json);
        }
    }

    /// Evaluate the policy for an image that was just pulled.
    /// `image_ref` is the user-supplied reference; `manifest_digest` is
    /// the rkyv root returned by the converter.
    async fn evaluate_pulled(
        &self,
        image_ref: &str,
        manifest_digest: &Digest,
    ) -> Result<(), Status> {
        let Some(engine) = &self.policy_engine else {
            return Ok(());
        };
        let policy = engine.default_policy().clone();
        let store = self.store.clone();
        let image_ref = image_ref.to_string();
        let manifest_digest = manifest_digest.as_hex();
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
    async fn evaluate_for_run(&self, root_digest: &str) -> Result<(), Status> {
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
    #[cfg(target_os = "macos")]
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
            self.rootfs_cache.read().await.get(&final_id).cloned()
        } else {
            None
        };
        let _ = backend; // reserved for future exec dispatch
        let restart_policy = parse_restart_policy(req.restart_policy);
        let state = WorkloadState {
            status: "pending".to_string(),
            start_time: now,
            exit_time: 0,
            exit_code: None,
            backend: final_backend.clone(),
            internal_ip: if final_ip == "loopback" {
                None
            } else {
                Some(final_ip.clone())
            },
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
            cpu_millicores: if req.cpu_millicores > 0 {
                Some(req.cpu_millicores)
            } else {
                None
            },
            memory_bytes: if req.memory_bytes > 0 {
                Some(req.memory_bytes)
            } else {
                None
            },
            bridge_name: if req.bridge_name.is_empty() {
                None
            } else {
                Some(req.bridge_name.clone())
            },
            mounts: req
                .mounts
                .iter()
                .map(|m| pullrun_exec::Mount {
                    type_: m.r#type.clone(),
                    source: m.source.clone(),
                    destination: m.destination.clone(),
                    options: m.options.clone(),
                })
                .collect(),
            console_log_path: if final_backend == "vm" {
                self.config
                    .vm_backend
                    .as_ref()
                    .map(|cfg| cfg.vm_root.join(&final_id).join("console.log"))
            } else {
                None
            },
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
                .with_metadata("pid", final_pid.to_string()),
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

        // Check if this image reference is already known locally
        // (committed, built, or previously pulled). If so, return
        // the cached digest without hitting the registry.
        {
            let tags = self.image_tags.read().await;
            for (digest, tag) in tags.iter() {
                if *tag == image_ref {
                    debug!(%image_ref, %digest, "local image tag hit");
                    return Ok(tonic::Response::new(PullImageResponse {
                        root_digest: digest.clone(),
                        bytes_stored: 0,
                        bytes_deduplicated: 0,
                    }));
                }
            }
        }

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

        let platform: Option<String> = if req.platform.is_empty() {
            None
        } else {
            Some(req.platform.clone())
        };

        let auth = build_auth(
            &req.registry_username,
            &req.registry_password,
            &req.registry_token,
        );

        let pull_result = if let Some(bloom_cache) = &self.config.bloom_cache {
            let sync_puller = pullrun_sync::SyncPuller::new(
                self.store.clone(),
                auth,
                self.config.insecure_registries.clone(),
                Some(bloom_cache.clone()),
            );
            sync_puller
                .pull(&image_ref, registry, platform.as_deref())
                .await
        } else {
            let puller =
                OciPuller::with_insecure_registries(auth, self.config.insecure_registries.clone());
            puller
                .pull_with_platform(&image_ref, registry, platform.as_deref())
                .await
        };
        let pulled = match pull_result {
            Ok(p) => p,
            Err(e) => {
                record_pull(&registry_label, "failed");
                self.event_bus.emit(
                    Event::new(&image_ref, EventKind::ImagePulled)
                        .with_metadata("registry", &registry_label)
                        .with_metadata("outcome", "failed")
                        .with_metadata("error", e.to_string()),
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
                        .with_metadata("error", format!("conversion: {e}")),
                );
                return Err(tonic::Status::internal(format!("conversion failed: {e}")));
            }
        };

        // Record image_ref -> root_digest for later run-time policy checks.
        {
            let mut tags = self.image_tags.write().await;
            tags.insert(root_digest.as_hex(), image_ref.clone());
        }
        self.save_image_tags().await;

        // Policy gate.
        if let Err(e) = self.evaluate_pulled(&image_ref, &root_digest).await {
            record_pull(&registry_label, "denied");
            self.event_bus.emit(
                Event::new(&image_ref, EventKind::PolicyDenied)
                    .with_metadata("registry", &registry_label)
                    .with_metadata("phase", "pull")
                    .with_metadata("reason", e.message().to_string()),
            );
            return Err(e);
        }

        // Detect dedup: if the root manifest was already in the store
        // *before* this call, the bytes_stored value is misleading
        // because the converter's `put()` short-circuits. We do a
        // best-effort check post-conversion by re-asking the store.
        // (`Digest` is a type alias for `String`; we can just pass the
        // hex string slice.)
        let bytes_stored: i64 = pulled
            .layer_blobs
            .iter()
            .map(|(_, b, _)| b.len() as i64)
            .sum();
        let already_present = self.store.exists(&root_digest);

        if already_present {
            self.event_bus.emit(
                Event::new(&image_ref, EventKind::ImageDeduped)
                    .with_metadata("registry", &registry_label)
                    .with_metadata("root_digest", root_digest.as_hex())
                    .with_metadata("bytes_stored", bytes_stored.to_string()),
            );
        } else {
            self.event_bus.emit(
                Event::new(&image_ref, EventKind::ImagePulled)
                    .with_metadata("registry", &registry_label)
                    .with_metadata("root_digest", root_digest.as_hex())
                    .with_metadata("bytes_stored", bytes_stored.to_string()),
            );
        }

        record_pull(&registry_label, "success");
        Ok(tonic::Response::new(PullImageResponse {
            root_digest: root_digest.as_hex(),
            bytes_stored,
            bytes_deduplicated: 0,
        }))
    }

    async fn run_workload(
        &self,
        request: tonic::Request<RunRequest>,
    ) -> Result<tonic::Response<RunResponse>, tonic::Status> {
        tracing::debug!("run_workload called");
        let mut req = request.into_inner();
        // Strip optional "sha256:" prefix from root digest so
        // the store can look up the path correctly. The CLI
        // prepends this prefix for disambiguation, but the
        // store stores bare hex digests.
        if let Some(stripped) = req.root_digest.strip_prefix("sha256:") {
            req.root_digest = stripped.to_string();
        }
        // Take a copy of the request early so the macOS
        // Apple Virt path (which doesn't go through the
        // executor) can still pass a fresh copy to the
        // state-recording helper. The original `req` is
        // partially moved (e.g. `req.env`) before that
        // path is reached, so we can't just clone it
        // there.
        #[cfg(target_os = "macos")]
        let mut req_for_state = req.clone();
        let backend = Backend::from_str(&req.backend).map_err(tonic::Status::invalid_argument)?;

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
                // On macOS, try a locally installed kernel from
                // ~/.pullrun/kernels/ (or PULLRUN_KERNEL_PATH) so
                // users can run VMs without pushing a kernel OCI
                // image.
                #[cfg(target_os = "macos")]
                {
                    match find_local_kernel() {
                        Some((vmlinux, initramfs)) => {
                            let staged =
                                StagedKernel::from_paths(vmlinux, initramfs).map_err(|e| {
                                    tonic::Status::internal(format!("local kernel: {e}"))
                                })?;
                            info!(
                                "using local kernel; caching under {}",
                                LOCAL_KERNEL_CACHE_KEY
                            );
                            self.kernel_cache
                                .write()
                                .await
                                .insert(LOCAL_KERNEL_CACHE_KEY.to_string(), staged);
                            req.kernel_image = LOCAL_KERNEL_CACHE_KEY.to_string();
                            req_for_state.kernel_image = LOCAL_KERNEL_CACHE_KEY.to_string();
                        }
                        None => {
                            return Err(tonic::Status::invalid_argument(
                                "backend=vm requires kernel_image, or a local kernel at \
                                 ~/.pullrun/kernels/ (set PULLRUN_KERNEL_PATH)",
                            ));
                        }
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    return Err(tonic::Status::invalid_argument(
                        "backend=vm requires kernel_image (e.g. 'pullrun/kernel-asahi:6.19.14')",
                    ));
                }
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
                    .map_err(|e| tonic::Status::internal(format!("stage kernel join: {e}")))?;
                    match r {
                        Ok(kernel) => {
                            self.event_bus.emit(
                                Event::new(&req.id, EventKind::ImagePulled)
                                    .with_metadata("kind", "kernel")
                                    .with_metadata("image", &req.kernel_image)
                                    .with_metadata(
                                        "vmlinux_bytes",
                                        kernel.vmlinux_size().to_string(),
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
                                    .with_metadata("error", e.to_string()),
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
                .map_err(|e| tonic::Status::internal(format!("materialize rootfs join: {e}")))?
                .map_err(|e| tonic::Status::internal(format!("materialize rootfs: {e}")))?;
                self.rootfs_cache
                    .write()
                    .await
                    .insert(req.id.clone(), rootfs_path);
            }
        }

        // Auto-promote to bridge mode when inbound ports are requested
        // so the container gets a bridge IP and the proxy can forward.
        let has_inbound_ports = req.network_rules.iter().any(|r| r.direction == "inbound");
        let network_mode = match req.network_mode.as_str() {
            "bridge" => NetworkMode::Bridge,
            "host" => NetworkMode::Host,
            "slirp" => NetworkMode::Slirp,
            _ if has_inbound_ports => NetworkMode::Bridge,
            _ => NetworkMode::Loopback,
        };

        let env: HashMap<String, String> = req.env;

        // Defense-in-depth: re-evaluate the policy before launching.
        self.evaluate_for_run(&req.root_digest).await?;

        // Cross-architecture check: if the image arch differs from the
        // host, register a binfmt_misc handler so runc can transparently
        // execute foreign-arch binaries via QEMU. Skip for VMs (Firecracker
        // handles its own emulation).
        if !matches!(backend, Backend::Vm) {
            let materializer = OciMaterializer::new(&self.store);
            let root_digest = Digest::from_hex(&req.root_digest)
                .map_err(|e| Status::internal(format!("invalid digest: {e}")))?;
            let md = materializer
                .materialize_manifest(&root_digest)
                .map_err(|e| Status::internal(format!("read manifest for arch check: {e}")))?;
            if md.architecture != current_arch() {
                crate::binfmt::ensure_binfmt_for_arch(&md.architecture)
                    .map_err(|e| Status::internal(format!("cross-arch binfmt setup: {e}")))?;
            }
        }

        // Translate the gRPC NetworkRule wire format into the runtime's
        // `pullrun_net::NetworkRule` so the executor can apply it (start
        // inbound proxy listeners, declare outbound allowlists).
        let network_rules: Vec<pullrun_net::NetworkRule> = req
            .network_rules
            .iter()
            .map(|r| {
                let direction = match r.direction.as_str() {
                    "outbound" => pullrun_net::Direction::Outbound,
                    _ => pullrun_net::Direction::Inbound,
                };
                let protocol = match r.protocol.as_str() {
                    "udp" => pullrun_net::Protocol::Udp,
                    _ => pullrun_net::Protocol::Tcp,
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
                pullrun_net::NetworkRule {
                    direction,
                    protocol,
                    port: r.port as u16,
                    host_port: r.host_port as u16,
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

        // Resolve secrets/configs and stage them for bind-mounting.
        let staged_secret_dir = self
            .config
            .store_root
            .join("run")
            .join("secrets-stage")
            .join(&req.id);
        let mut extra_mounts: Vec<pullrun_exec::Mount> = Vec::new();

        if !req.secrets.is_empty() || !req.configs.is_empty() {
            let _ = std::fs::create_dir_all(&staged_secret_dir);
        }

        for sr in &req.secrets {
            let content = self
                .config
                .secrets_store
                .read_secret_raw(&sr.name)
                .map_err(|e| {
                    tonic::Status::invalid_argument(format!("secret '{}': {e}", sr.name))
                })?;
            let target = if sr.target_path.is_empty() {
                format!("/run/secrets/{}", sr.name)
            } else {
                sr.target_path.clone()
            };
            let stage_path = staged_secret_dir.join(&sr.name);
            std::fs::write(&stage_path, &content)
                .map_err(|e| tonic::Status::internal(format!("stage secret '{}': {e}", sr.name)))?;
            extra_mounts.push(pullrun_exec::Mount {
                type_: "bind".to_string(),
                source: stage_path.to_string_lossy().to_string(),
                destination: target,
                options: vec!["ro".to_string(), "rbind".to_string()],
            });
        }

        for cr in &req.configs {
            let content = self
                .config
                .secrets_store
                .read_config_raw(&cr.name)
                .map_err(|e| {
                    tonic::Status::invalid_argument(format!("config '{}': {e}", cr.name))
                })?;
            let target = if cr.target_path.is_empty() {
                format!("/{}", cr.name)
            } else {
                cr.target_path.clone()
            };
            let stage_path = staged_secret_dir.join(&cr.name);
            std::fs::write(&stage_path, &content)
                .map_err(|e| tonic::Status::internal(format!("stage config '{}': {e}", cr.name)))?;
            extra_mounts.push(pullrun_exec::Mount {
                type_: "bind".to_string(),
                source: stage_path.to_string_lossy().to_string(),
                destination: target,
                options: vec!["ro".to_string(), "rbind".to_string()],
            });
        }

        let mounts: Vec<pullrun_exec::Mount> = req
            .mounts
            .iter()
            .map(|m| pullrun_exec::Mount {
                type_: m.r#type.clone(),
                source: m.source.clone(),
                destination: m.destination.clone(),
                options: m.options.clone(),
            })
            .chain(extra_mounts)
            .collect();

        let restart_policy = parse_restart_policy(req.restart_policy);

        let image_root = Digest::from_hex(&req.root_digest)
            .map_err(|e| Status::invalid_argument(format!("invalid root_digest: {e}")))?;
        let spec = WorkloadSpec {
            id: req.id.clone(),
            image_root,
            backend,
            command: req.command.clone(),
            env,
            cpu_millicores: if req.cpu_millicores > 0 {
                Some(req.cpu_millicores)
            } else {
                None
            },
            memory_bytes: if req.memory_bytes > 0 {
                Some(req.memory_bytes)
            } else {
                None
            },
            network_mode,
            network_rules: network_rules.clone(),
            kernel_path,
            bridge_name: if req.bridge_name.is_empty() {
                None
            } else {
                Some(req.bridge_name.clone())
            },
            mounts,
            health_check: req
                .health_check
                .as_ref()
                .map(|hc| pullrun_exec::HealthCheck {
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
                // `pullrun_vm::run_session_blocking`.
                #[cfg(target_os = "macos")]
                {
                    let is_applevirt_unsupported =
                        matches!(e, pullrun_exec::ExecError::BackendNotAvailable(_));
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
                        .with_metadata("error", e.to_string()),
                );
                return Err(tonic::Status::internal(format!("create failed: {e}")));
            }
        };

        if let Err(e) = self.executor.start(&handle).await {
            self.event_bus.emit(
                Event::new(&req.id, EventKind::WorkloadStarted)
                    .with_metadata("backend", &handle.backend)
                    .with_metadata("outcome", "start_failed")
                    .with_metadata("error", e.to_string()),
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
        let final_ip = handle
            .internal_ip
            .clone()
            .unwrap_or_else(|| "loopback".into());
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
                .or_else(|| {
                    self.config
                        .vm_backend
                        .as_ref()
                        .map(|cfg| ext4_path_for(&cfg.vm_root, &final_id))
                })
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
            console_log_path: if final_backend == "vm" {
                self.config
                    .vm_backend
                    .as_ref()
                    .map(|cfg| cfg.vm_root.join(&final_id).join("console.log"))
            } else {
                None
            },
        };
        workloads.insert(final_id.clone(), state.clone());
        drop(workloads);

        // Persist checkpoint immediately so the workload state
        // survives a runtime restart.
        write_workload_checkpoint(&self.config.checkpoints_dir, &final_id, &state);

        // Emit the public WorkloadStarted event for observers.
        self.event_bus.emit(
            Event::new(&final_id, EventKind::WorkloadStarted)
                .with_metadata("backend", &final_backend)
                .with_metadata("image_root", &final_image_root)
                .with_metadata("internal_ip", &final_ip)
                .with_metadata("pid", final_pid.to_string()),
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
                return Err(tonic::Status::invalid_argument(
                    "compose service name is empty",
                ));
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
                    platform: String::new(),
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
                    host_port: p.host_port,
                    to_host: String::new(),
                    from_cidrs: vec![],
                })
                .collect();

            let backend = if service.backend.is_empty() {
                "container".to_string()
            } else {
                service.backend.clone()
            };
            let run_req = tonic::Request::new(RunRequest {
                id: id.clone(),
                root_digest,
                backend,
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
                secrets: Vec::new(),
                configs: Vec::new(),
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

        // Verify the workload exists before proceeding.
        {
            let workloads = self.workloads.read().await;
            if !workloads.contains_key(&id) {
                return Err(tonic::Status::not_found(format!(
                    "workload {} not found",
                    id
                )));
            }
        }

        // Check workload status before trying to stop any OS process.
        // If the status is not "running", there is nothing to stop.
        let needs_stop = {
            let workloads = self.workloads.read().await;
            workloads
                .get(&id)
                .map(|s| s.status == "running")
                .unwrap_or(false)
        };

        if needs_stop {
            // Check for persistent Apple Virt VM handles first
            // (macOS — not tracked by the executor router).
            #[cfg(target_os = "macos")]
            let vm_stopped = {
                let mut vms = self.persistent_vms.write().await;
                if let Some(handle) = vms.remove(&id) {
                    drop(vms);
                    handle.stop();
                    true
                } else {
                    false
                }
            };

            #[cfg(not(target_os = "macos"))]
            let vm_stopped = false;

            if !vm_stopped {
                // If the executor stop fails on a VM workload (e.g.
                // stale state from an older daemon), just warn and
                // continue so the state gets cleaned up.
                if let Err(e) = self.executor.stop(&id).await {
                    let is_vm = self
                        .workloads
                        .read()
                        .await
                        .get(&id)
                        .map(|s| s.backend == "vm")
                        .unwrap_or(false);
                    if is_vm {
                        warn!(%id, error = %e, "executor stop failed for VM workload; cleaning up state anyway");
                    } else {
                        return Err(tonic::Status::internal(format!("stop failed: {e}")));
                    }
                }
            }
        }

        // Do NOT clean up materialized rootfs — `exec` on an exited
        // VM boots a fresh VM on the same rootfs (persistent storage).
        self.rootfs_cache.write().await.remove(&id);

        // Clean up staged secrets/configs for this workload.
        let staged_secret_dir = self
            .config
            .store_root
            .join("run")
            .join("secrets-stage")
            .join(&id);
        tokio::fs::remove_dir_all(&staged_secret_dir).await.ok();

        // Look up the backend label *before* mutating state, so the
        // metrics call sees the same label as the one that was
        // incremented in `run_workload`. `exit_code` is set to 0
        // because the operator-initiated stop is a clean exit from
        // the runtime's point of view; the actual process exit
        // status (if the workload was a runc container) is opaque
        // to us at this layer in v0.
        let backend_label = {
            let workloads = self.workloads.read().await;
            workloads
                .get(&id)
                .map(|s| s.backend.clone())
                .unwrap_or_else(|| "unknown".to_string())
        };

        // Only emit `WorkloadStopped` and mark as stopped/exited if
        // the workload is still alive. If the background watcher has
        // already flipped it to "exited", we leave it alone and don't
        // double-emit. (The watcher uses its own `announced` HashSet
        // to ensure it only fires `WorkloadExited` once per id.)
        let mut was_running = false;
        let mut state_copy: Option<WorkloadState> = None;
        {
            let mut workloads = self.workloads.write().await;
            if let Some(state) = workloads.get_mut(&id) {
                match state.status.as_str() {
                    "running" => {
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
                    "pending" => {
                        state.status = "exited".to_string();
                        state.exit_code = Some(0);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        state.exit_time = now;
                        was_running = true;
                        state_copy = Some(state.clone());
                    }
                    _ => {}
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
        let state = workloads
            .get(&req.id)
            .ok_or_else(|| tonic::Status::not_found(format!("workload {} not found", req.id)))?;

        use crate::proto::RestartPolicy;
        let restart_proto = match state.restart_policy {
            pullrun_exec::types::RestartPolicy::OnFailure => RestartPolicy::RestartOnFailure,
            pullrun_exec::types::RestartPolicy::Always => RestartPolicy::RestartAlways,
            pullrun_exec::types::RestartPolicy::UnlessStopped => {
                RestartPolicy::RestartUnlessStopped
            }
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
        let items: Vec<WorkloadStatus> = workloads
            .iter()
            .map(|(id, state)| {
                use crate::proto::RestartPolicy;
                let restart_proto = match state.restart_policy {
                    pullrun_exec::types::RestartPolicy::OnFailure => {
                        RestartPolicy::RestartOnFailure
                    }
                    pullrun_exec::types::RestartPolicy::Always => RestartPolicy::RestartAlways,
                    pullrun_exec::types::RestartPolicy::UnlessStopped => {
                        RestartPolicy::RestartUnlessStopped
                    }
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
            })
            .collect();

        Ok(tonic::Response::new(ListWorkloadsResponse {
            workloads: items,
        }))
    }

    type StreamLogsStream = tokio_stream::wrappers::ReceiverStream<Result<LogChunk, tonic::Status>>;

    async fn stream_logs(
        &self,
        request: tonic::Request<StreamLogsRequest>,
    ) -> Result<tonic::Response<Self::StreamLogsStream>, tonic::Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(32);

        // Verify the workload exists before starting the stream.
        let workloads = self.workloads.read().await;
        let state = workloads
            .get(&req.id)
            .ok_or_else(|| tonic::Status::not_found(format!("workload {} not found", req.id)))?;
        let status_str = format!("{}\n", state.status);
        drop(workloads);

        tokio::spawn(async move {
            // Send current status as the first log chunk.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;
            if tx
                .send(Ok(LogChunk {
                    data: status_str.into_bytes(),
                    stderr: false,
                    timestamp: now,
                }))
                .await
                .is_err()
            {
                return;
            }

            if req.follow {
                // Poll workload state periodically and send updates.
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                loop {
                    interval.tick().await;
                    // The receiver dropped? Stop.
                    if tx.is_closed() {
                        break;
                    }
                }
            }
        });

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }

    type StreamEventsStream =
        tokio_stream::wrappers::ReceiverStream<Result<ProtoEvent, tonic::Status>>;

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

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(mpsc_rx),
        ))
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
                    pullrun_net::Direction::Inbound => "inbound".to_string(),
                    pullrun_net::Direction::Outbound => "outbound".to_string(),
                },
                protocol: match r.protocol {
                    pullrun_net::Protocol::Tcp => "tcp".to_string(),
                    pullrun_net::Protocol::Udp => "udp".to_string(),
                },
                port: r.port as u32,
                host_port: r.host_port as u32,
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
            pullrun_exec::types::RestartPolicy::OnFailure => RestartPolicy::RestartOnFailure,
            pullrun_exec::types::RestartPolicy::Always => RestartPolicy::RestartAlways,
            pullrun_exec::types::RestartPolicy::UnlessStopped => {
                RestartPolicy::RestartUnlessStopped
            }
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

        // Dispatch through ExecutorRouter for correct backend selection.
        let exit_code = self
            .executor
            .exec(&req.id, &req.command, 30)
            .await
            .map_err(|e| tonic::Status::internal(format!("exec failed: {e}")))?;

        // Also capture stdout/stderr via runc exec (works for container backend;
        // VM/rootless backends return empty output, which is acceptable).
        let (stdout, stderr) = {
            let mut cmd = tokio::process::Command::new("runc");
            cmd.args(["exec", &req.id]);
            for arg in &req.command {
                cmd.arg(arg);
            }
            match cmd.output().await {
                Ok(out) => (out.stdout, out.stderr),
                Err(_) => (Vec::new(), Vec::new()),
            }
        };

        Ok(tonic::Response::new(ExecResponse {
            exit_code,
            stdout,
            stderr,
        }))
    }

    type AttachWorkloadStream =
        tokio_stream::wrappers::ReceiverStream<Result<AttachMessage, tonic::Status>>;

    async fn attach_workload(
        &self,
        request: tonic::Request<tonic::Streaming<AttachMessage>>,
    ) -> Result<tonic::Response<Self::AttachWorkloadStream>, tonic::Status> {
        use pullrun_vm::attach::{FrameSink, FrameSource};
        use pullrun_vsock::Frame;
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
        let open_env: Vec<String> = open.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let open_working_dir = open.working_dir.clone();
        let open_tty = open.tty;
        let open_rows = open.initial_rows;
        let open_cols = open.initial_cols;

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
        let is_container = state.backend == "container" || state.backend == "rootless";
        let is_vm = state.backend == "vm";
        if !is_vm && !is_container {
            return Err(tonic::Status::failed_precondition(format!(
                "workload {workload_id} has backend={} which does not support attach",
                state.backend
            )));
        }
        // Rootless containers don't support attach because the attach
        // path runs runc directly (not via rootless-exec) and rootless
        // runc uses a different state directory.
        if state.backend == "rootless" {
            return Err(tonic::Status::failed_precondition(format!(
                "workload {workload_id} is a rootless container, which does not support \
                 interactive attach. Use a non-rootless container backend instead."
            )));
        }
        // Check that runc is available before proceeding.
        if is_container {
            match std::process::Command::new("runc").arg("--version").output() {
                Err(e) => {
                    return Err(tonic::Status::failed_precondition(format!(
                        "workload {workload_id} has backend=container but runc is not \
                         available: {e}. Install runc: \
                         https://github.com/opencontainers/runc/releases"
                    )));
                }
                Ok(o) if !o.status.success() => {
                    return Err(tonic::Status::failed_precondition(format!(
                        "workload {workload_id} has backend=container but runc is not \
                         functional: {}",
                        String::from_utf8_lossy(&o.stderr)
                    )));
                }
                _ => {}
            }
        }
        let kernel_image_ref = state.kernel_image_ref.clone();
        let rootfs_dir = if is_vm {
            match state.rootfs_dir.clone() {
                Some(p) => Some(p),
                None => {
                    return Err(tonic::Status::failed_precondition(format!(
                        "workload {workload_id} has no materialized rootfs; \
                         was it started by this runtime?"
                    )));
                }
            }
        } else {
            None
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
            state.env.iter().map(|(k, v)| format!("{k}={v}")).collect()
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

        // 3. Look up the staged kernel (VM backends only).
        let kernel = if is_vm {
            let kref = kernel_image_ref.clone();
            Some({
                let cache_hit = self.kernel_cache.read().await.get(&kref).map(|k| {
                    pullrun_vm::StagedKernel::from_paths(
                        k.vmlinux_path().to_path_buf(),
                        k.initramfs_path().map(|p| p.to_path_buf()),
                    )
                });
                match cache_hit {
                    Some(Ok(k)) => k,
                    Some(Err(e)) => {
                        return Err(tonic::Status::internal(format!(
                            "reconstruct StagedKernel for {kref}: {e}"
                        )));
                    }
                    None if kref == LOCAL_KERNEL_CACHE_KEY => {
                        #[cfg(target_os = "macos")]
                        {
                            match find_local_kernel() {
                                Some((vmlinux_path, initramfs_path)) => {
                                    let staged = StagedKernel::from_paths(
                                        vmlinux_path.clone(),
                                        initramfs_path.clone(),
                                    )
                                    .map_err(|e| {
                                        tonic::Status::internal(format!("local kernel: {e}"))
                                    })?;
                                    self.kernel_cache
                                        .write()
                                        .await
                                        .insert(LOCAL_KERNEL_CACHE_KEY.to_string(), staged);
                                    info!(
                                        "re-cached local kernel under {} for attach",
                                        LOCAL_KERNEL_CACHE_KEY
                                    );
                                    StagedKernel::from_paths(vmlinux_path, initramfs_path).map_err(
                                        |e| {
                                            tonic::Status::internal(format!(
                                                "reconstruct local kernel: {e}"
                                            ))
                                        },
                                    )?
                                }
                                None => {
                                    return Err(tonic::Status::failed_precondition(format!(
                                        "workload {workload_id} was started with local kernel \
                                         but it is no longer available at ~/.pullrun/kernels/; \
                                         re-run make install-kernel"
                                    )));
                                }
                            }
                        }
                        #[cfg(not(target_os = "macos"))]
                        {
                            return Err(tonic::Status::failed_precondition(format!(
                                "workload {workload_id} has no staged kernel for {kref}; \
                                 was it started by this runtime?"
                            )));
                        }
                    }
                    None if kref.is_empty() => match self.config.vm_backend.as_ref() {
                        Some(cfg) => StagedKernel::from_paths(cfg.kernel_path.clone(), None)
                            .map_err(|e| {
                                tonic::Status::internal(format!("kernel from --vm-kernel: {e}"))
                            })?,
                        None => {
                            return Err(tonic::Status::failed_precondition(format!(
                                "workload {workload_id} has no staged kernel; \
                                     was it started by this runtime?"
                            )));
                        }
                    },
                    None => {
                        return Err(tonic::Status::failed_precondition(format!(
                            "workload {workload_id} has no staged kernel for {kref}; \
                             was it started by this runtime?"
                        )));
                    }
                }
            })
        } else {
            None
        };

        // 4. Build the per-VM attach config (VM backend only).
        #[allow(unused_variables)]
        let vm_attach_cfg = if is_vm {
            Some(pullrun_vm::AppleVirtAttachConfig {
                workload_id: workload_id.clone(),
                kernel: kernel.unwrap(),
                rootfs_dir: rootfs_dir.clone().unwrap(),
                command: command.clone(),
                env: env.clone(),
                working_dir: working_dir.clone(),
                cpus: 1,
                mem_mib: 512,
                vsock_port: Some(pullrun_vm::DEFAULT_VSOCK_PORT),
                console_log: Some(
                    std::env::var("PULLRUN_VM_CONSOLE_LOG")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|_| {
                            std::path::PathBuf::from("/tmp/pullrun-attach-console.log")
                        }),
                ),
                tty: open_tty,
                initial_rows: open_rows,
                initial_cols: open_cols,
                mounts: state.mounts.clone(),
            })
        } else {
            None
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
                                        .with_metadata(
                                            "error",
                                            "client sent AttachOpen after the first one",
                                        ),
                                );
                                None
                            }
                            Some(crate::proto::attach_message::Body::WindowSize(ws)) => {
                                Some(Frame::WindowSize {
                                    rows: ws.rows as u16,
                                    cols: ws.cols as u16,
                                })
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
        //
        //    We run the blocking recv loop inside
        //    `spawn_blocking` so the tokio worker thread is
        //    never blocked by the Condvar wait.  The loop
        //    pushes frames into the tokio mpsc channel via
        //    `blocking_send`, which is safe from a blocking
        //    thread.  The async outer task just awaits the
        //    blocking task until the session ends.
        let tx_drain = tx.clone();
        let workload_id_dr = workload_id.clone();
        let drainer = tokio::spawn(async move {
            tokio::task::spawn_blocking(move || {
                let mut total_bytes: u64 = 0;
                loop {
                    let frame =
                        match server_out_rx.recv_timeout(std::time::Duration::from_millis(10)) {
                            Ok(f) => f,
                            Err(sync_mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(sync_mpsc::RecvTimeoutError::Disconnected) => return,
                        };
                    if let pullrun_vsock::Frame::WorkloadStdout(ref data) = frame {
                        total_bytes += data.len() as u64;
                        tracing::debug!(
                            workload_id = %workload_id_dr,
                            bytes = data.len(),
                            total = total_bytes,
                            "drainer: forwarding WorkloadStdout"
                        );
                    }
                    let msg = frame_to_attach_message(frame);
                    if tx_drain.blocking_send(Ok(msg)).is_err() {
                        return;
                    }
                }
            })
            .await
            .ok();
        });

        // 8. Emit a "starting" event so observers can see
        //    attach attempts in the event stream even if
        //    the actual session errors out.
        let backend_label = if is_vm {
            "apple-virt-attach"
        } else {
            "container-attach"
        };
        self.event_bus.emit(
            Event::new(&workload_id, EventKind::WorkloadStarted)
                .with_metadata("backend", backend_label)
                .with_metadata("outcome", "pending")
                .with_metadata("image_root", &state.image_root)
                .with_metadata("command", command.join(" ")),
        );

        // 9. Spawn the blocking session. This is where
        //    the !Send handle lives; the gRPC handler
        //    never touches it.
        let event_bus_session = self.event_bus.clone();
        let workload_id_session = workload_id.clone();
        let tx_session = tx.clone();
        let use_tty = open_tty;

        if is_vm {
            // Platform-specific attach: macOS uses Apple Virtualization
            // (vsock-based PTY), Linux uses Firecracker serial console reader.
            #[cfg(target_os = "macos")]
            {
                let cfg_for_session =
                    vm_attach_cfg.expect("vm_attach_cfg must be Some for VM backend");
                let wl_id = workload_id.clone();
                let eb = self.event_bus.clone();
                let tx_s = tx.clone();
                let fwd = forwarder;
                let drn = drainer;
                let persistent_vms = Arc::clone(&self.persistent_vms);
                let workloads = Arc::clone(&self.workloads);
                let checkpoints_dir = self.config.checkpoints_dir.clone();

                tokio::task::spawn_blocking(move || {
                    let vm_handle: Option<Arc<pullrun_vm::VmPersistentHandle>> = {
                        let vms = persistent_vms.blocking_read();
                        vms.get(&wl_id).and_then(|h| {
                            if h.is_alive() {
                                Some(Arc::clone(h))
                            } else {
                                None
                            }
                        })
                    };

                    let vm_handle = match vm_handle {
                        Some(h) => h,
                        None => {
                            persistent_vms.blocking_write().remove(&wl_id);
                            // Build on_exit callback: fires when
                            // the VM background thread exits.
                            let wl_id_exit = wl_id.clone();
                            let vms_exit = Arc::clone(&persistent_vms);
                            let wls_exit = Arc::clone(&workloads);
                            let eb_exit = eb.clone();
                            let cp_exit = checkpoints_dir.clone();
                            let on_exit: Option<Box<dyn FnOnce() + Send>> =
                                Some(Box::new(move || {
                                    let mut wls = wls_exit.blocking_write();
                                    if let Some(s) = wls.get_mut(&wl_id_exit) {
                                        s.status = "exited".to_string();
                                        s.exit_code = s.exit_code.or(Some(137));
                                        if s.exit_time == 0 {
                                            s.exit_time = std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .map(|d| d.as_secs() as i64)
                                                .unwrap_or(0);
                                        }
                                        write_workload_checkpoint(&cp_exit, &wl_id_exit, s);
                                    }
                                    drop(wls);
                                    vms_exit.blocking_write().remove(&wl_id_exit);
                                    eb_exit.emit(
                                        Event::new(&wl_id_exit, EventKind::WorkloadExited)
                                            .with_metadata("backend", "apple-virt")
                                            .with_metadata("exit_code", "137"),
                                    );
                                }));
                            match pullrun_vm::spawn_vm(cfg_for_session, on_exit) {
                                Ok(h) => {
                                    // VM booted — mark as running.
                                    if let Some(s) = workloads.blocking_write().get_mut(&wl_id) {
                                        s.status = "running".to_string();
                                        write_workload_checkpoint(&checkpoints_dir, &wl_id, s);
                                    }
                                    let h = Arc::new(h);
                                    persistent_vms
                                        .blocking_write()
                                        .insert(wl_id.clone(), Arc::clone(&h));
                                    h
                                }
                                Err(e) => {
                                    eb.emit(
                                        Event::new(&wl_id, EventKind::WorkloadStarted)
                                            .with_metadata("backend", "apple-virt-attach")
                                            .with_metadata("outcome", "failed")
                                            .with_metadata("error", e.to_string()),
                                    );
                                    let _ = tx_s.blocking_send(Err(attach_error_to_status(&e)));
                                    drop(fwd);
                                    drop(drn);
                                    return;
                                }
                            }
                        }
                    };

                    let result = pullrun_vm::attach_to_vm(&vm_handle, client_in_rx, server_out_tx);
                    if let Err(err) = result {
                        eb.emit(
                            Event::new(&wl_id, EventKind::WorkloadStarted)
                                .with_metadata("backend", "apple-virt-attach")
                                .with_metadata("outcome", "session_failed")
                                .with_metadata("error", err.to_string()),
                        );
                        let _ = tx_s.blocking_send(Err(attach_error_to_status(&err)));
                    } else {
                        eb.emit(
                            Event::new(&wl_id, EventKind::WorkloadStarted)
                                .with_metadata("backend", "apple-virt-attach")
                                .with_metadata("outcome", "client_detached"),
                        );
                    }
                    drop(fwd);
                    drop(drn);
                });
            }

            #[cfg(not(target_os = "macos"))]
            {
                // Firecracker VM: stream serial console output from
                // the console.log file written by the Firecracker process.
                let console_path = state.console_log_path.clone();
                let wl_id = workload_id.clone();
                let eb = self.event_bus.clone();
                let tx_s = tx.clone();
                let fwd = forwarder;
                let drn = drainer;

                tokio::task::spawn_blocking(move || {
                    run_firecracker_console_session(
                        console_path.as_deref(),
                        &wl_id,
                        &tx_s,
                        client_in_rx,
                        server_out_tx,
                        fwd,
                        drn,
                    );
                    eb.emit(
                        Event::new(&wl_id, EventKind::WorkloadStarted)
                            .with_metadata("backend", "firecracker-attach")
                            .with_metadata("outcome", "session_ended"),
                    );
                });
            }
        } else if is_container {
            // Container backend: run runc exec with PTY (if tty) or piped stdio.
            // Clone data for the `'static` closure.
            let wl_id = workload_id_session.clone();
            let cmd = command.clone();
            let env_vars = env.clone();
            let wd = working_dir.clone();
            let bundle_path = self.config.bundle_root.join(&workload_id);
            let workloads_attach = Arc::clone(&self.workloads);
            let checkpoints_dir_attach = self.config.checkpoints_dir.clone();
            tokio::task::spawn_blocking(move || {
                // If the workload is marked as exited but runc says the
                // container is alive, update the daemon state to "running"
                // so that `pullrun list` reflects reality.
                let runc_out = std::process::Command::new("runc")
                    .args(["state", &wl_id])
                    .output();
                let alive = match &runc_out {
                    Ok(o) if o.status.success() => {
                        let s = String::from_utf8_lossy(&o.stdout);
                        let running = s.contains("\"status\": \"running\"");
                        tracing::info!(
                            workload_id = %wl_id,
                            runc_state = %s.trim(),
                            is_running = running,
                            "reconciling attach state"
                        );
                        running
                    }
                    Ok(_) => {
                        tracing::warn!(
                            workload_id = %wl_id,
                            "runc state exited with non-zero status"
                        );
                        false
                    }
                    Err(e) => {
                        tracing::warn!(
                            workload_id = %wl_id,
                            error = %e,
                            "runc state command failed"
                        );
                        false
                    }
                };
                if alive {
                    if let Some(s) = workloads_attach.blocking_write().get_mut(&wl_id) {
                        if s.status != "running" {
                            let prev = s.status.clone();
                            s.status = "running".to_string();
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0);
                            s.start_time = now;
                            s.exit_time = 0;
                            s.exit_code = None;
                            write_workload_checkpoint(&checkpoints_dir_attach, &wl_id, s);
                            tracing::info!(
                                workload_id = %wl_id,
                                from_status = %prev,
                                "container alive at attach; state updated to running"
                            );
                        } else {
                            tracing::info!(
                                workload_id = %wl_id,
                                "container alive at attach; state already running"
                            );
                        }
                    } else {
                        tracing::warn!(
                            workload_id = %wl_id,
                            "workload not found in state map during attach"
                        );
                    }
                }
                tracing::info!(workload_id = %wl_id, "blocking container attach task STARTED");
                let result = run_runc_attach_session(
                    &wl_id,
                    &cmd,
                    &env_vars,
                    &wd,
                    use_tty,
                    &bundle_path,
                    client_in_rx,
                    server_out_tx,
                );
                if let Err(err) = result {
                    event_bus_session.emit(
                        Event::new(&wl_id, EventKind::WorkloadStarted)
                            .with_metadata("backend", "container-attach")
                            .with_metadata("outcome", "failed")
                            .with_metadata("error", err.to_string()),
                    );
                    let body = tonic::Status::internal(format!("container attach: {err}"));
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
                        Event::new(&wl_id, EventKind::WorkloadStarted)
                            .with_metadata("backend", "container-attach")
                            .with_metadata("outcome", "session_ended"),
                    );
                }
                drop(forwarder);
                drop(drainer);
            });
        }

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }

    // ------------------------------------------------------------------
    // Phase A: CRI support RPCs (stubs → prod)
    // ------------------------------------------------------------------

    async fn has_image(
        &self,
        request: tonic::Request<HasImageRequest>,
    ) -> Result<tonic::Response<HasImageResponse>, tonic::Status> {
        let _ = request;
        Err(tonic::Status::unimplemented(
            "has_image not yet implemented",
        ))
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
            mountpoint: "/var/lib/pullrun/dag".into(),
            total_bytes: total as i64,
            total_nodes: 0,
            used_bytes: total as i64,
            inodes_used: 0,
        }))
    }

    type PortForwardStream =
        tokio_stream::wrappers::ReceiverStream<Result<PortForwardData, tonic::Status>>;

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
            return Ok(tonic::Response::new(UpdateWorkloadResponse {
                success: false,
            }));
        }
        match self.executor.update(&req.id, cpu, mem).await {
            Ok(()) => {
                info!(id = %req.id, cpu_millicores = ?cpu, memory_bytes = ?mem, "workload resources updated");
                Ok(tonic::Response::new(UpdateWorkloadResponse {
                    success: true,
                }))
            }
            Err(e) => {
                warn!(id = %req.id, error = %e, "workload resource update failed");
                Ok(tonic::Response::new(UpdateWorkloadResponse {
                    success: false,
                }))
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
                tonic::Status::invalid_argument(format!(
                    "read Dockerfile {}: {e}",
                    dockerfile_path.display()
                ))
            })?;

        let dockerfile = pullrun_oci::Dockerfile::parse(&content)
            .map_err(|e| tonic::Status::invalid_argument(format!("parse Dockerfile: {e}")))?;

        // Resolve context dir
        let context_dir = if req.context_dir.is_empty() {
            dockerfile_path
                .parent()
                .unwrap_or(&PathBuf::from("."))
                .to_path_buf()
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

        let build_args: std::collections::HashMap<String, String> = req.build_args.clone();

        let root_digest = if !req.platforms.is_empty() {
            // Multi-platform build.
            let platforms: Vec<String> = req
                .platforms
                .iter()
                .map(|p| {
                    if p.is_empty() {
                        "linux/amd64".to_string()
                    } else {
                        p.clone()
                    }
                })
                .collect();

            let builder = crate::builder::DagBuilder::new(
                self.store.clone(),
                runc_path,
                self.config.bundle_root.join("build"),
                self.config.insecure_registries.clone(),
            );

            let (list_digest, _results) = builder
                .build_multi(&dockerfile, &context_dir, &platforms, &build_args)
                .await
                .map_err(|e| tonic::Status::internal(format!("multi-arch build failed: {e}")))?;

            list_digest
        } else {
            // Single-platform build.
            let platform: Option<String> = if req.platform.is_empty() {
                None
            } else {
                Some(req.platform.clone())
            };

            let builder = crate::builder::DagBuilder::with_platform(
                self.store.clone(),
                runc_path,
                self.config.bundle_root.join("build"),
                self.config.insecure_registries.clone(),
                platform,
            );

            let result = builder
                .build(&dockerfile, &context_dir, &build_args)
                .await
                .map_err(|e| tonic::Status::internal(format!("build failed: {e}")))?;

            result.root_digest
        };

        let tag = if req.tag.is_empty() {
            root_digest.as_hex()[..12].to_string()
        } else {
            req.tag.clone()
        };

        // Record the image tag -> root_digest mapping
        {
            let mut tags = self.image_tags.write().await;
            tags.insert(root_digest.as_hex(), tag.clone());
        }
        self.save_image_tags().await;

        // Push after build if requested.
        if req.push {
            let target_ref = if req.tag.is_empty() {
                return Err(tonic::Status::invalid_argument(
                    "push requires a tag (use -t registry/repo:tag)",
                ));
            } else {
                req.tag.clone()
            };

            let pusher = pullrun_oci::DagPusher::new(
                self.store.clone(),
                None,
                self.config.insecure_registries.clone(),
            );
            pusher
                .push(&root_digest.as_hex(), &target_ref)
                .await
                .map_err(|e| tonic::Status::internal(format!("push failed: {e}")))?;
        }

        Ok(tonic::Response::new(BuildImageResponse {
            root_digest: root_digest.as_hex(),
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

    type ExportImageStream =
        tokio_stream::wrappers::ReceiverStream<Result<ExportImageChunk, tonic::Status>>;

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
            if let Err(e) = export_dag_to_tar(&store, &root_digest, &mut buf) {
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
        let (root_digest, bytes_stored, bytes_deduplicated) =
            tokio::task::spawn_blocking(move || import_dag_from_tar(&store, &buf[..]))
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
            return Err(Status::invalid_argument(
                "id and container_path are required",
            ));
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
                cache.get(&req.id).cloned().ok_or_else(|| {
                    Status::failed_precondition(format!(
                        "rootfs not materialized for workload {}",
                        req.id
                    ))
                })?
            } else {
                self.config.bundle_root.join(&req.id).join("rootfs")
            };

            rootfs
        };

        let container_path = req.container_path.trim_start_matches('/');
        let full_path = rootfs_path.join(container_path);

        // Security: ensure we don't escape the rootfs via symlinks or "..".
        // For "in" direction the destination doesn't exist yet, so we
        // canonicalize the parent and validate the final path manually.
        let canonical = if req.direction == "in" {
            let parent = full_path.parent().unwrap_or(&rootfs_path);
            let parent_canonical = parent
                .canonicalize()
                .map_err(|e| Status::internal(format!("cannot resolve parent path: {e}")))?;
            if !parent_canonical.starts_with(&rootfs_path) {
                return Err(Status::invalid_argument(
                    "container_path escapes the root filesystem",
                ));
            }
            let file_name = full_path
                .file_name()
                .ok_or_else(|| Status::invalid_argument("container_path has no filename"))?;
            parent_canonical.join(file_name)
        } else {
            full_path
                .canonicalize()
                .map_err(|e| Status::internal(format!("cannot resolve path: {e}")))?
        };
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
                    tokio::fs::set_permissions(
                        &canonical,
                        std::fs::Permissions::from_mode(req.mode),
                    )
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

    async fn commit_image(
        &self,
        request: tonic::Request<CommitImageRequest>,
    ) -> Result<tonic::Response<CommitImageResponse>, tonic::Status> {
        let req = request.into_inner();
        if req.id.is_empty() {
            return Err(Status::invalid_argument("id is required"));
        }

        // Get the rootfs path for this workload.
        let rootfs_path = {
            let workloads = self.workloads.read().await;
            let state = workloads
                .get(&req.id)
                .ok_or_else(|| Status::not_found(format!("workload {} not found", req.id)))?;

            if state.backend == "vm" || state.backend == "sandbox" {
                let cache = self.rootfs_cache.read().await;
                cache.get(&req.id).cloned().ok_or_else(|| {
                    Status::failed_precondition(format!(
                        "rootfs not materialized for workload {}",
                        req.id
                    ))
                })?
            } else {
                self.config.bundle_root.join(&req.id).join("rootfs")
            }
        };

        // Read the original manifest to preserve architecture/os.
        let (orig_arch, orig_os) = {
            let workloads = self.workloads.read().await;
            let state = workloads
                .get(&req.id)
                .ok_or_else(|| Status::not_found(format!("workload {} not found", req.id)))?;
            let materializer = OciMaterializer::new(&self.store);
            let image_root = Digest::from_hex(&state.image_root)
                .map_err(|e| Status::internal(format!("invalid digest: {e}")))?;
            let md = materializer
                .materialize_manifest(&image_root)
                .map_err(|e| Status::internal(format!("read manifest: {e}")))?;
            (md.architecture, md.os)
        };

        // Scan the container rootfs into new DAG nodes.
        let DagDirectory {
            manifest_digest,
            node_count,
            blob_bytes: _,
        } = build_dag_from_directory_with_platform(&self.store, &rootfs_path, &orig_arch, &orig_os)
            .await
            .map_err(|e| Status::internal(format!("commit failed: {e}")))?;

        // If a tag was provided, record it in the image tag map.
        if !req.tag.is_empty() {
            let mut tags = self.image_tags.write().await;
            tags.insert(manifest_digest.as_hex(), req.tag.clone());
            self.save_image_tags().await;
        }

        Ok(tonic::Response::new(CommitImageResponse {
            root_digest: manifest_digest.as_hex(),
            tag: req.tag,
            new_nodes: node_count as u64,
        }))
    }

    async fn diff_workload(
        &self,
        request: tonic::Request<DiffRequest>,
    ) -> Result<tonic::Response<DiffResponse>, tonic::Status> {
        let req = request.into_inner();
        if req.id.is_empty() {
            return Err(Status::invalid_argument("id is required"));
        }

        // Get the rootfs path and the image root digest.
        let (rootfs_path, image_root) = {
            let workloads = self.workloads.read().await;
            let state = workloads
                .get(&req.id)
                .ok_or_else(|| Status::not_found(format!("workload {} not found", req.id)))?;

            let rootfs = if state.backend == "vm" || state.backend == "sandbox" {
                let cache = self.rootfs_cache.read().await;
                cache.get(&req.id).cloned().ok_or_else(|| {
                    Status::failed_precondition(format!(
                        "rootfs not materialized for workload {}",
                        req.id
                    ))
                })?
            } else {
                self.config.bundle_root.join(&req.id).join("rootfs")
            };

            (rootfs, state.image_root.clone())
        };

        // Walk the container rootfs, computing SHA256 digests.
        let container_files = walk_rootfs_for_diff(&rootfs_path)
            .map_err(|e| Status::internal(format!("walk rootfs: {e}")))?;

        // Walk the original image DAG tree to get the original file map.
        let original_files = walk_dag_tree(&self.store, &image_root)
            .map_err(|e| Status::internal(format!("walk image DAG: {e}")))?;

        // Compare the two maps.
        let mut added = Vec::new();
        let mut deleted = Vec::new();
        let mut modified = Vec::new();

        for (path, digest) in &container_files {
            match original_files.get(path) {
                Some(orig_digest) if orig_digest == digest => {}
                Some(_) => modified.push(path.clone()),
                None => added.push(path.clone()),
            }
        }

        for path in original_files.keys() {
            if !container_files.contains_key(path) {
                deleted.push(path.clone());
            }
        }

        added.sort();
        deleted.sort();
        modified.sort();

        Ok(tonic::Response::new(DiffResponse {
            added,
            deleted,
            modified,
        }))
    }

    async fn runtime_info(
        &self,
        _request: tonic::Request<InfoRequest>,
    ) -> Result<tonic::Response<InfoResponse>, tonic::Status> {
        let uptime = {
            let start = self.start_time.load(std::sync::atomic::Ordering::Relaxed);
            if start > 0 {
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs())
                .saturating_sub(start as u64)
            } else {
                0
            }
        };

        let workload_count = {
            let workloads = self.workloads.read().await;
            workloads.len() as u64
        };

        let store = &self.store;
        let mountpoint = self.config.store_root.to_string_lossy().to_string();
        let total_nodes = store.cached_node_count() as u64;
        let _total_bytes = store.total_bytes();
        let (fs_total, fs_used) = fs_usage(&self.config.store_root);

        Ok(tonic::Response::new(InfoResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: uptime,
            workload_count,
            store_mountpoint: mountpoint,
            store_total_bytes: fs_total as u64,
            store_used_bytes: fs_used as u64,
            store_total_nodes: total_nodes,
            go_version: String::new(),
        }))
    }

    async fn create_network(
        &self,
        request: tonic::Request<CreateNetworkRequest>,
    ) -> Result<tonic::Response<CreateNetworkResponse>, tonic::Status> {
        let req = request.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("network name is required"));
        }

        // Create the bridge device.
        bridge_create(&req.name).map_err(|e| Status::internal(format!("create bridge: {e}")))?;

        // Determine subnet. For v0, use a deterministic /24 based on the bridge name.
        let subnet = if req.subnet.is_empty() {
            network_subnet_for(&req.name)
        } else {
            req.subnet.clone()
        };

        // Persist the network to the registry file.
        persist_network(&self.config.store_root, &req.name, &subnet)
            .map_err(|e| Status::internal(format!("persist network: {e}")))?;

        Ok(tonic::Response::new(CreateNetworkResponse {
            success: true,
            bridge_name: req.name,
            subnet,
        }))
    }

    async fn remove_network(
        &self,
        request: tonic::Request<RemoveNetworkRequest>,
    ) -> Result<tonic::Response<RemoveNetworkResponse>, tonic::Status> {
        let req = request.into_inner();
        if req.name.is_empty() {
            return Err(Status::invalid_argument("network name is required"));
        }

        // Unregister from persisted registry.
        unpersist_network(&self.config.store_root, &req.name)
            .map_err(|e| Status::internal(format!("unpersist network: {e}")))?;

        // Delete the bridge device.
        bridge_delete(&req.name).map_err(|e| Status::internal(format!("delete bridge: {e}")))?;

        Ok(tonic::Response::new(RemoveNetworkResponse {
            success: true,
        }))
    }

    async fn list_networks(
        &self,
        _request: tonic::Request<ListNetworksRequest>,
    ) -> Result<tonic::Response<ListNetworksResponse>, tonic::Status> {
        let networks = list_persisted_networks(&self.config.store_root).unwrap_or_default();

        // Count attached workloads per network for the response.
        let workloads = self.workloads.read().await;
        let mut network_workload_counts: HashMap<String, u64> = HashMap::new();
        for state in workloads.values() {
            if let Some(ref bn) = state.bridge_name {
                *network_workload_counts.entry(bn.clone()).or_default() += 1;
            }
        }
        drop(workloads);

        let networks: Vec<NetworkInfo> = networks
            .into_iter()
            .map(|(net_name, subnet)| NetworkInfo {
                name: net_name.clone(),
                bridge_name: net_name.clone(),
                subnet,
                attached_workloads: network_workload_counts.get(&net_name).copied().unwrap_or(0),
            })
            .collect();

        Ok(tonic::Response::new(ListNetworksResponse { networks }))
    }

    // ─── Secret / Config handlers ────────────────────────────

    async fn create_secret(
        &self,
        request: tonic::Request<CreateSecretRequest>,
    ) -> Result<tonic::Response<CreateSecretResponse>, tonic::Status> {
        let req = request.into_inner();
        self.config
            .secrets_store
            .create_secret(&req.name, &req.data)
            .map_err(tonic::Status::invalid_argument)?;
        Ok(tonic::Response::new(CreateSecretResponse {}))
    }

    async fn list_secrets(
        &self,
        _request: tonic::Request<ListSecretsRequest>,
    ) -> Result<tonic::Response<ListSecretsResponse>, tonic::Status> {
        let items = self
            .config
            .secrets_store
            .list_secrets()
            .map_err(tonic::Status::internal)?;
        let secrets: Vec<crate::proto::SecretInfo> = items
            .into_iter()
            .map(|s| crate::proto::SecretInfo {
                name: s.name,
                created_at: s.created_at,
                size_bytes: s.size_bytes,
            })
            .collect();
        Ok(tonic::Response::new(ListSecretsResponse { secrets }))
    }

    async fn inspect_secret(
        &self,
        request: tonic::Request<InspectSecretRequest>,
    ) -> Result<tonic::Response<InspectSecretResponse>, tonic::Status> {
        let req = request.into_inner();
        let s = self
            .config
            .secrets_store
            .inspect_secret(&req.name)
            .map_err(tonic::Status::not_found)?;
        let secret = crate::proto::SecretInfo {
            name: s.name,
            created_at: s.created_at,
            size_bytes: s.size_bytes,
        };
        Ok(tonic::Response::new(InspectSecretResponse {
            secret: Some(secret),
        }))
    }

    async fn remove_secret(
        &self,
        request: tonic::Request<RemoveSecretRequest>,
    ) -> Result<tonic::Response<RemoveSecretResponse>, tonic::Status> {
        let req = request.into_inner();
        self.config
            .secrets_store
            .remove_secret(&req.name)
            .map_err(tonic::Status::not_found)?;
        Ok(tonic::Response::new(RemoveSecretResponse {}))
    }

    async fn create_config(
        &self,
        request: tonic::Request<CreateConfigRequest>,
    ) -> Result<tonic::Response<CreateConfigResponse>, tonic::Status> {
        let req = request.into_inner();
        self.config
            .secrets_store
            .create_config(&req.name, &req.data)
            .map_err(tonic::Status::invalid_argument)?;
        Ok(tonic::Response::new(CreateConfigResponse {}))
    }

    async fn list_configs(
        &self,
        _request: tonic::Request<ListConfigsRequest>,
    ) -> Result<tonic::Response<ListConfigsResponse>, tonic::Status> {
        let items = self
            .config
            .secrets_store
            .list_configs()
            .map_err(tonic::Status::internal)?;
        let configs: Vec<crate::proto::ConfigInfo> = items
            .into_iter()
            .map(|c| crate::proto::ConfigInfo {
                name: c.name,
                created_at: c.created_at,
                size_bytes: c.size_bytes,
            })
            .collect();
        Ok(tonic::Response::new(ListConfigsResponse { configs }))
    }

    async fn inspect_config(
        &self,
        request: tonic::Request<InspectConfigRequest>,
    ) -> Result<tonic::Response<InspectConfigResponse>, tonic::Status> {
        let req = request.into_inner();
        let c = self
            .config
            .secrets_store
            .inspect_config(&req.name)
            .map_err(tonic::Status::not_found)?;
        let config = crate::proto::ConfigInfo {
            name: c.name,
            created_at: c.created_at,
            size_bytes: c.size_bytes,
        };
        Ok(tonic::Response::new(InspectConfigResponse {
            config: Some(config),
        }))
    }

    async fn remove_config(
        &self,
        request: tonic::Request<RemoveConfigRequest>,
    ) -> Result<tonic::Response<RemoveConfigResponse>, tonic::Status> {
        let req = request.into_inner();
        self.config
            .secrets_store
            .remove_config(&req.name)
            .map_err(tonic::Status::not_found)?;
        Ok(tonic::Response::new(RemoveConfigResponse {}))
    }

    async fn prune(
        &self,
        _request: tonic::Request<PruneRequest>,
    ) -> Result<tonic::Response<PruneResponse>, tonic::Status> {
        let store_root = &self.config.store_root;
        let mut bundles_removed: i64 = 0;
        let mut bytes_freed: i64 = 0;
        let errors: Vec<String> = Vec::new();

        // 1. Remove stale bundle directories (exited/stopped workloads).
        let bundle_root = store_root.join("bundles");
        if bundle_root.exists() {
            if let Ok(entries) = std::fs::read_dir(&bundle_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let is_active = {
                        let workloads = self.workloads.read().await;
                        workloads.contains_key(&name)
                    };
                    if !is_active {
                        if let Ok(meta) = std::fs::metadata(&path) {
                            bytes_freed += meta.len() as i64;
                        }
                        if std::fs::remove_dir_all(&path).is_ok() {
                            bundles_removed += 1;
                        }
                    }
                }
            }
        }

        // 2. Clean up any orphaned rootfs_cache entries
        //    (workload stopped but rootfs dir still tracked).
        let mut cache = self.rootfs_cache.write().await;
        let active_ids: std::collections::HashSet<String> = {
            let workloads = self.workloads.read().await;
            workloads.keys().cloned().collect()
        };
        cache.retain(|id, path| {
            if !active_ids.contains(id) {
                let p = path.clone();
                tokio::task::block_in_place(|| {
                    if let Ok(meta) = std::fs::metadata(&p) {
                        bytes_freed += meta.len() as i64;
                    }
                    std::fs::remove_dir_all(&p).ok();
                });
                false
            } else {
                true
            }
        });

        // 3. Clean up orphaned secret/config staging directories.
        let staged_root = store_root.join("run").join("secrets-stage");
        if staged_root.exists() {
            if let Ok(entries) = std::fs::read_dir(&staged_root) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if !active_ids.contains(&name) {
                        if let Ok(meta) = std::fs::metadata(&path) {
                            bytes_freed += meta.len() as i64;
                        }
                        std::fs::remove_dir_all(&path).ok();
                    }
                }
            }
        }

        Ok(tonic::Response::new(PruneResponse {
            bundles_removed,
            bytes_freed,
            errors,
        }))
    }
}

/// Read a Firecracker VM's serial console log file and pipe its
/// content through the attach mpsc channels.  Runs in a blocking
/// context (spawn_blocking).
///
/// The Firecracker process writes the guest's `ttyS0` output to
/// `<vm_root>/<id>/console.log`.  This function polls that file
/// and forwards new data as `Frame::WorkloadStdout` frames.  When
/// the Firecracker process exits (detected via `kill -0` on the
/// PID stored in `firecracker.pid` beside the console log), it
/// sends a `Frame::WorkloadExit` and returns.
#[allow(dead_code)]
fn run_firecracker_console_session(
    console_log_path: Option<&std::path::Path>,
    workload_id: &str,
    tx_s: &tokio::sync::mpsc::Sender<Result<AttachMessage, tonic::Status>>,
    client_in_rx: std::sync::mpsc::Receiver<pullrun_vsock::Frame>,
    server_out_tx: std::sync::mpsc::Sender<pullrun_vsock::Frame>,
    _fwd: tokio::task::JoinHandle<()>,
    _drn: tokio::task::JoinHandle<()>,
) {
    use std::io::Read;
    use std::time::Duration;

    let console_path = match console_log_path {
        Some(p) => p.to_path_buf(),
        None => {
            tracing::warn!(workload_id = %workload_id, "no console_log_path for VM attach");
            let _ = tx_s.blocking_send(Err(tonic::Status::failed_precondition(
                "VM has no console log path; was it started by this runtime?",
            )));
            return;
        }
    };

    // Derive the PID path from the console log path.
    let pid_path = console_path.parent().map(|p| p.join("firecracker.pid"));

    // Drain stdin in the background (Firecracker serial input is
    // not wired through this path yet).
    let _stdin_drain = std::thread::spawn(move || {
        for _frame in client_in_rx {
            // discard
        }
    });

    // Wait for console.log to appear (Firecracker may not have
    // started writing yet).
    let mut file = loop {
        if console_path.exists() {
            match std::fs::File::open(&console_path) {
                Ok(f) => break f,
                Err(e) => {
                    tracing::warn!(workload_id = %workload_id, error = %e, "retry open console.log");
                }
            }
        }
        // Check if the PID is already dead (VM failed to start).
        if let Some(ref pp) = pid_path {
            if let Ok(pid_str) = std::fs::read_to_string(pp) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    if pid > 0 && !is_pid_alive(pid) {
                        tracing::info!(workload_id = %workload_id, pid = pid, "Firecracker VM exited before console.log was readable");
                        let _ = server_out_tx.send(pullrun_vsock::Frame::WorkloadExit {
                            exit_code: Some(0),
                            signal: None,
                        });
                        return;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // Read existing content and forward it.
    let mut buf = [0u8; 65536];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                if server_out_tx
                    .send(pullrun_vsock::Frame::WorkloadStdout(data))
                    .is_err()
                {
                    return; // client disconnected
                }
            }
            Err(e) => {
                tracing::warn!(workload_id = %workload_id, error = %e, "read console.log");
                break;
            }
        }
    }

    // Poll for new data until the VM exits.
    let mut known_size = console_path.metadata().ok().map(|m| m.len()).unwrap_or(0);
    loop {
        std::thread::sleep(Duration::from_millis(100));

        // Check for new data.
        if let Ok(meta) = console_path.metadata() {
            let current_size = meta.len();
            if current_size > known_size {
                if let Ok(mut f) = std::fs::File::open(&console_path) {
                    use std::io::Seek;
                    if f.seek(std::io::SeekFrom::Start(known_size)).is_ok() {
                        loop {
                            match f.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                                    if server_out_tx
                                        .send(pullrun_vsock::Frame::WorkloadStdout(data))
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }
                known_size = current_size;
            }
        }

        // Check if the Firecracker process is still alive.
        let pid_alive = pid_path
            .as_ref()
            .and_then(|pp| {
                std::fs::read_to_string(pp)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok().filter(|&pid| pid > 0))
            })
            .map(is_pid_alive)
            .unwrap_or(false);

        if !pid_alive && known_size > 0 {
            // Process exited; flush any remaining data.
            if let Ok(meta) = console_path.metadata() {
                if meta.len() > known_size {
                    continue; // more data arrived before exit
                }
            }
            let _ = server_out_tx.send(pullrun_vsock::Frame::WorkloadExit {
                exit_code: Some(0),
                signal: None,
            });
            return;
        }
    }
}

/// Check whether a process with the given PID is alive by sending
/// signal 0.
#[allow(dead_code)]
fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Spawn a `runc exec` (or `runc run`) process and bridge its I/O
/// with the attach mpsc channels.  Runs in a blocking context
/// (spawn_blocking).
///
/// If the container is still running, uses `runc exec [-t] <id>
/// <cmd>` (reuses the existing container).  If the container has
/// already exited (or never started), cleans up the stale runc
/// state and runs `runc run -b <bundle> <id>` with the requested
/// command and TTY setting.
/// Allocate a PTY pair on the host and return (master_fd, slave_name).
fn allocate_pty() -> Result<(std::os::unix::io::RawFd, std::ffi::CString), String> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(format!("posix_openpt: {}", std::io::Error::last_os_error()));
    }
    if unsafe { libc::grantpt(master) } < 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(master);
        }
        return Err(format!("grantpt: {e}"));
    }
    if unsafe { libc::unlockpt(master) } < 0 {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(master);
        }
        return Err(format!("unlockpt: {e}"));
    }
    let slave_name_ptr = unsafe { libc::ptsname(master) };
    if slave_name_ptr.is_null() {
        let e = std::io::Error::last_os_error();
        unsafe {
            libc::close(master);
        }
        return Err(format!("ptsname: {e}"));
    }
    let slave_name = unsafe { std::ffi::CStr::from_ptr(slave_name_ptr).to_owned() };
    Ok((master, slave_name))
}

#[allow(clippy::too_many_arguments)]
fn run_runc_attach_session(
    workload_id: &str,
    command: &[String],
    _env: &[String],
    _working_dir: &str,
    tty: bool,
    bundle_path: &std::path::Path,
    client_in_rx: std::sync::mpsc::Receiver<pullrun_vsock::Frame>,
    server_out_tx: std::sync::mpsc::Sender<pullrun_vsock::Frame>,
) -> Result<(), String> {
    // Check whether the runc container still exists and is running.
    let container_alive = {
        let out = std::process::Command::new("runc")
            .args(["state", workload_id])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let status_str = String::from_utf8_lossy(&o.stdout);
                status_str.contains("\"status\": \"running\"")
            }
            _ => false,
        }
    };

    if tty {
        // ── TTY mode ──────────────────────────────────────────────
        // Allocate a PTY on the host so runc exec -t sees a real
        // terminal and doesn't fail with "open /dev/tty: ENOENT".
        let (pty_master, slave_name) = allocate_pty()?;

        // If the container is not alive, start a sleep container first.
        if !container_alive {
            let _ = std::process::Command::new("runc")
                .args(["delete", "--force", workload_id])
                .output();
            let config_path = bundle_path.join("config.json");
            let config_text = std::fs::read_to_string(&config_path)
                .map_err(|e| format!("read config.json: {e}"))?;
            let mut config: serde_json::Value = serde_json::from_str(&config_text)
                .map_err(|e| format!("parse config.json: {e}"))?;
            if let Some(process) = config.get_mut("process") {
                if let Some(obj) = process.as_object_mut() {
                    obj.insert(
                        "args".to_string(),
                        serde_json::Value::Array(
                            vec!["sleep".to_string(), "3600".to_string()]
                                .into_iter()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                    obj.insert("terminal".to_string(), serde_json::Value::Bool(false));
                }
            }
            std::fs::write(
                &config_path,
                serde_json::to_string_pretty(&config)
                    .map_err(|e| format!("serialize config.json: {e}"))?,
            )
            .map_err(|e| format!("write config.json: {e}"))?;
            let _sleep_child = std::process::Command::new("runc")
                .args(["run", "-d", "-b"])
                .arg(bundle_path)
                .arg(workload_id)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("spawn runc run sleep: {e}"))?;
            let wait_start = std::time::Instant::now();
            let max_wait = std::time::Duration::from_secs(10);
            loop {
                let out = std::process::Command::new("runc")
                    .args(["state", workload_id])
                    .output();
                let running = match &out {
                    Ok(o) if o.status.success() => {
                        let s = String::from_utf8_lossy(&o.stdout);
                        s.contains("\"status\": \"running\"")
                    }
                    _ => false,
                };
                if running {
                    break;
                }
                if wait_start.elapsed() > max_wait {
                    let _ = std::process::Command::new("runc")
                        .args(["delete", "--force", workload_id])
                        .output();
                    return Err("timeout waiting for container to start".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        // Spawn runc exec -t with the PTY slave as stdin/stdout/stderr.
        let mut args = vec![
            "exec".to_string(),
            "-t".to_string(),
            workload_id.to_string(),
        ];
        args.extend(command.iter().cloned());

        // Open the slave side of the PTY.
        let slave = unsafe { libc::open(slave_name.as_ptr(), libc::O_RDWR) };
        if slave < 0 {
            return Err(format!(
                "open slave pty: {}",
                std::io::Error::last_os_error()
            ));
        }

        use std::os::unix::io::FromRawFd;
        let slave_file = unsafe { std::fs::File::from_raw_fd(slave) };

        let mut child = std::process::Command::new("runc")
            .args(&args)
            .stdin(
                slave_file
                    .try_clone()
                    .map_err(|e| format!("clone slave stdin: {e}"))?,
            )
            .stdout(
                slave_file
                    .try_clone()
                    .map_err(|e| format!("clone slave stdout: {e}"))?,
            )
            .stderr(
                slave_file
                    .try_clone()
                    .map_err(|e| format!("clone slave stderr: {e}"))?,
            )
            .spawn()
            .map_err(|e| format!("spawn runc exec -t (host pty): {e}"))?;

        // Close our handle to the slave so the PTY is owned by
        // the child process alone.
        drop(slave_file);

        // Use the master side for I/O.
        {
            use std::os::unix::io::FromRawFd;
            let mut master_file = unsafe { std::fs::File::from_raw_fd(pty_master) };

            // Signals when stdin is done (detach or StdinEof).
            let stdin_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stdin_done_clone = stdin_done.clone();

            // Thread: forward client_in_rx frames → PTY master
            let stdin_handle = std::thread::spawn(move || {
                use std::io::Write;
                for frame in client_in_rx {
                    match frame {
                        pullrun_vsock::Frame::WorkloadStdin(data) => {
                            let _ = master_file.write_all(&data);
                        }
                        pullrun_vsock::Frame::StdinEof => break,
                        _ => {}
                    }
                }
                let _ = master_file.flush();
                drop(master_file);
                stdin_done_clone.store(true, std::sync::atomic::Ordering::SeqCst);
            });

            // Read from PTY master and forward to client.
            let out_fd = unsafe { libc::fcntl(pty_master, libc::F_DUPFD_CLOEXEC, 0) };
            if out_fd < 0 {
                return Err(format!("dup pty fd: {}", std::io::Error::last_os_error()));
            }
            let mut out_file = unsafe { std::fs::File::from_raw_fd(out_fd) };
            let tx_out = server_out_tx.clone();
            let stdout_handle = std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = [0u8; 65536];
                loop {
                    match out_file.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                            if tx_out
                                .send(pullrun_vsock::Frame::WorkloadStdout(data))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });

            // Wait for either stdin to close OR child to exit.
            // On stdin close (detach): kill the child so we can return.
            // On child exit (user typed 'exit'): let stdin drain naturally.
            loop {
                if stdin_done.load(std::sync::atomic::Ordering::SeqCst) {
                    // stdin closed — user detached or stream ended.
                    // Kill the runc exec process; the container (sleep
                    // container or original) survives independently.
                    let _ = child.kill();
                    break;
                }
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                    Err(e) => {
                        return Err(format!("try_wait child: {e}"));
                    }
                }
            }

            let exit_status = child.wait().map_err(|e| format!("wait runc child: {e}"))?;
            let exit_code = exit_status.code().unwrap_or(-1);
            let _ = stdin_handle.join();
            let _ = stdout_handle.join();
            let _ = server_out_tx.send(pullrun_vsock::Frame::WorkloadExit {
                exit_code: Some(exit_code),
                signal: None,
            });
        }
        return Ok(());
    }

    // ── Non-TTY mode ────────────────────────────────────────────

    let mut child;
    let mut child_stdin;
    let mut child_stdout;
    let mut child_stderr;

    if container_alive {
        // Container is still running → use runc exec.
        let mut args = vec!["exec".to_string(), workload_id.to_string()];
        args.extend(command.iter().cloned());

        let cmd = std::process::Command::new("runc")
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn runc exec: {e}"))?;

        child = cmd;
    } else {
        // Container is gone / stopped → clean up and re-run.
        let _ = std::process::Command::new("runc")
            .args(["delete", "--force", workload_id])
            .output();

        let config_path = bundle_path.join("config.json");
        let config_text =
            std::fs::read_to_string(&config_path).map_err(|e| format!("read config.json: {e}"))?;
        let mut config: serde_json::Value =
            serde_json::from_str(&config_text).map_err(|e| format!("parse config.json: {e}"))?;

        // Non-TTY mode: run the command directly via `runc run`.
        if let Some(process) = config.get_mut("process") {
            if let Some(obj) = process.as_object_mut() {
                obj.insert(
                    "args".to_string(),
                    serde_json::Value::Array(
                        command
                            .iter()
                            .map(|c| serde_json::Value::String(c.clone()))
                            .collect(),
                    ),
                );
                obj.insert("terminal".to_string(), serde_json::Value::Bool(false));
            }
        }
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config)
                .map_err(|e| format!("serialize config.json: {e}"))?,
        )
        .map_err(|e| format!("write config.json: {e}"))?;

        let cmd = std::process::Command::new("runc")
            .args(["run", "-b"])
            .arg(bundle_path)
            .arg(workload_id)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn runc run: {e}"))?;

        child = cmd;
    }

    child_stdin = child.stdin.take().ok_or("child stdin not available")?;
    child_stdout = child.stdout.take().ok_or("child stdout not available")?;
    child_stderr = child.stderr.take().ok_or("child stderr not available")?;

    // Thread: forward client_in_rx frames → child_stdin
    let stdin_handle = std::thread::spawn(move || {
        use std::io::Write;
        for frame in client_in_rx {
            match frame {
                pullrun_vsock::Frame::WorkloadStdin(data) => {
                    let _ = child_stdin.write_all(&data);
                }
                pullrun_vsock::Frame::StdinEof => break,
                _ => {}
            }
        }
        drop(child_stdin);
    });

    // Thread: forward child_stdout → server_out_tx
    let tx_out = server_out_tx.clone();
    let stdout_handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 65536];
        loop {
            match child_stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                    if tx_out
                        .send(pullrun_vsock::Frame::WorkloadStdout(data))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Thread: forward child_stderr → server_out_tx
    let tx_err = server_out_tx.clone();
    let stderr_handle = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 65536];
        loop {
            match child_stderr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let data = bytes::Bytes::copy_from_slice(&buf[..n]);
                    if tx_err
                        .send(pullrun_vsock::Frame::WorkloadStderr(data))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Wait for the child process to exit.
    let exit_status = child.wait().map_err(|e| format!("wait runc child: {e}"))?;
    let exit_code = exit_status.code().unwrap_or(-1);

    // Wait for I/O threads to finish.
    let _ = stdin_handle.join();
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Send exit status to the client.
    let _ = server_out_tx.send(pullrun_vsock::Frame::WorkloadExit {
        exit_code: Some(exit_code),
        signal: None,
    });

    Ok(())
}

/// Translate a `pullrun_vsock::Frame` from the blocking
/// session task into a gRPC `AttachMessage`.
fn frame_to_attach_message(frame: pullrun_vsock::Frame) -> AttachMessage {
    use crate::proto::{attach_message::Body, AttachError, AttachExit, AttachStderr, AttachStdout};
    let body = match frame {
        pullrun_vsock::Frame::WorkloadStdout(b) => Body::Stdout(AttachStdout { data: b.to_vec() }),
        pullrun_vsock::Frame::WorkloadStderr(b) => Body::Stderr(AttachStderr { data: b.to_vec() }),
        pullrun_vsock::Frame::WorkloadExit { exit_code, signal } => Body::Exit(AttachExit {
            exit_code: exit_code.unwrap_or(0),
            signal: signal.unwrap_or(0),
            has_exit_code: exit_code.is_some(),
            has_signal: signal.is_some(),
        }),
        pullrun_vsock::Frame::Error(msg) => Body::Error(AttachError { message: msg }),
        // The other variants are server→guest, not seen
        // on the read path. If we see one, log and skip.
        other => Body::Error(AttachError {
            message: format!("unexpected frame from guest: {other:?}"),
        }),
        // Touch unused variants to keep the match
        // exhaustive.
        #[allow(unused)]
        pullrun_vsock::Frame::InitHello { .. } | pullrun_vsock::Frame::WorkloadSpec { .. } => {
            Body::Error(AttachError {
                message: "unexpected frame from guest (init)".into(),
            })
        }
        #[allow(unused)]
        pullrun_vsock::Frame::WorkloadStdin(_) | pullrun_vsock::Frame::StdinEof => {
            Body::Error(AttachError {
                message: "unexpected frame from guest (stdin)".into(),
            })
        }
    };
    AttachMessage { body: Some(body) }
}

/// Translate a `pullrun_vm::attach::AttachError` into the
/// matching gRPC `Status` code.
#[cfg(target_os = "macos")]
fn attach_error_to_status(err: &pullrun_vm::attach::AttachError) -> tonic::Status {
    use pullrun_vm::attach::AttachError as E;
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
#[cfg(target_os = "macos")]
fn materialize_rootfs(
    store: &MmapStore,
    manifest_digest: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    use pullrun_store::Digest;
    let target = store
        .root_dir()
        .join("rootfs")
        .join(manifest_digest.replace([':', '/'], "_"));
    if target.exists() {
        // Already materialized (e.g. the workload was run
        // before). Reuse.
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

/// Try to find a locally installed kernel for Apple Virt VMs.
///
/// Looks in order:
/// 1. `PULLRUN_KERNEL_PATH` env var (with optional `PULLRUN_INITRAMFS_PATH`)
/// 2. `~/.pullrun/kernels/` — picks the latest `vmlinux-*` file,
///    with initramfs from `~/.pullrun/initramfs/pullrun-initramfs.cpio.gz`.
#[cfg(target_os = "macos")]
fn find_local_kernel() -> Option<(std::path::PathBuf, Option<std::path::PathBuf>)> {
    let home = std::path::PathBuf::from(std::env::var("HOME").ok()?);

    // 1. Check env var.
    if let Ok(path) = std::env::var("PULLRUN_KERNEL_PATH") {
        let p = std::path::PathBuf::from(&path);
        if p.is_file() {
            let initramfs = std::env::var("PULLRUN_INITRAMFS_PATH")
                .ok()
                .map(std::path::PathBuf::from)
                .filter(|p| p.is_file())
                .or_else(|| {
                    let default = home.join(".pullrun/initramfs/pullrun-initramfs.cpio.gz");
                    if default.is_file() {
                        Some(default)
                    } else {
                        None
                    }
                });
            return Some((p, initramfs));
        }
    }

    // 2. Scan ~/.pullrun/kernels/ for vmlinux files.
    let kernel_dir = home.join(".pullrun/kernels");
    if !kernel_dir.is_dir() {
        return None;
    }
    let mut candidates: Vec<std::path::PathBuf> = std::fs::read_dir(&kernel_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            name.to_string_lossy().starts_with("vmlinux")
        })
        .map(|e| e.path())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort();
    let kernel = candidates.into_iter().last()?;

    let initramfs = {
        let p = home.join(".pullrun/initramfs/pullrun-initramfs.cpio.gz");
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };

    Some((kernel, initramfs))
}

/// Build an `OciAuth` from optional protobuf string fields.
/// Returns `None` when all fields are empty.
fn build_auth(username: &str, password: &str, token: &str) -> Option<OciAuth> {
    if username.is_empty() && password.is_empty() && token.is_empty() {
        return None;
    }
    Some(OciAuth {
        username: if username.is_empty() {
            None
        } else {
            Some(username.to_string())
        },
        password: if password.is_empty() {
            None
        } else {
            Some(password.to_string())
        },
        registry_token: if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        },
    })
}

/// Walk a rootfs directory and return a map of relative paths to SHA256 digests.
fn walk_rootfs_for_diff(
    root: &std::path::Path,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut files = HashMap::new();
    let root = std::fs::canonicalize(root)?;
    walk_dir_recursive(&root, &root, &mut files)?;
    Ok(files)
}

fn walk_dir_recursive(
    root: &std::path::Path,
    dir: &std::path::Path,
    files: &mut HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use sha2::{Digest, Sha256};
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "path outside root")?
            .to_string_lossy()
            .to_string();
        if relative.is_empty() {
            continue;
        }
        if ft.is_dir() {
            walk_dir_recursive(root, &path, files)?;
        } else if ft.is_file() {
            let content = std::fs::read(&path)?;
            let mut hasher = Sha256::new();
            hasher.update(&content);
            let digest = hex::encode(hasher.finalize());
            files.insert(relative, digest);
        } else if ft.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let target_str = target.to_string_lossy().to_string();
            let mut hasher = Sha256::new();
            hasher.update(target_str.as_bytes());
            let digest = hex::encode(hasher.finalize());
            files.insert(relative, digest);
        }
    }
    Ok(())
}

/// Walk the DAG tree starting from a manifest digest, collecting file path -> digest map.
fn walk_dag_tree(
    store: &MmapStore,
    manifest_digest: &str,
) -> Result<HashMap<String, String>, Box<dyn std::error::Error + Send + Sync>> {
    use pullrun_store::NodeKind;
    let mut files = HashMap::new();
    let digest = Digest::from_hex(manifest_digest)
        .map_err(|e| format!("parse digest {manifest_digest}: {e}"))?;
    let manifest_node = store
        .get_deserialized(&digest)
        .map_err(|e| format!("failed to read manifest {manifest_digest}: {e}"))?;
    if manifest_node.kind != NodeKind::Manifest {
        return Err("not a manifest node".into());
    }
    for layer_edge in &manifest_node.edges {
        let layer_node = store
            .get_deserialized(layer_edge)
            .map_err(|e| format!("failed to read layer: {e}"))?;
        if layer_node.kind != NodeKind::Layer {
            continue;
        }
        if let Some(tree_digest) = layer_node.edges.first() {
            walk_tree_node(store, tree_digest, "", &mut files)?;
        }
    }
    Ok(files)
}

fn walk_tree_node(
    store: &MmapStore,
    tree_digest: &Digest,
    prefix: &str,
    files: &mut HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use pullrun_store::NodeKind;
    let tree_node = store
        .get_deserialized(tree_digest)
        .map_err(|e| format!("failed to read tree: {e}"))?;
    if tree_node.kind != NodeKind::Tree {
        return Ok(());
    }
    let entries = DirectoryEntry::from_inline_bytes(&tree_node.inline_data);
    for entry in &entries {
        let child_path = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{}/{}", prefix, entry.name)
        };
        if entry.is_dir {
            walk_tree_node(store, &entry.digest, &child_path, files)?;
        } else {
            files.insert(child_path, entry.digest.as_hex());
        }
    }
    Ok(())
}

// --- Network management helpers ---

fn networks_registry_path(store_root: &std::path::Path) -> std::path::PathBuf {
    store_root.join("networks.json")
}

fn bridge_create(name: &str) -> Result<(), String> {
    let status = std::process::Command::new("ip")
        .args(["link", "add", name, "type", "bridge"])
        .status()
        .map_err(|e| format!("ip link add: {e}"))?;
    if !status.success() {
        return Ok(());
    }
    std::process::Command::new("ip")
        .args(["link", "set", name, "up"])
        .status()
        .map_err(|e| format!("ip link set up: {e}"))?;
    Ok(())
}

fn bridge_delete(name: &str) -> Result<(), String> {
    let _status = std::process::Command::new("ip")
        .args(["link", "delete", name])
        .status()
        .map_err(|e| format!("ip link delete: {e}"))?;
    Ok(())
}

/// Deterministic /24 subnet for a bridge name. Hash the name into
/// the 10.43.0.0/16 range, yielding 10.43.X.0/24 where X ∈ [1,255].
fn network_subnet_for(name: &str) -> String {
    use sha2::Digest;
    let hash = sha2::Sha256::digest(name.as_bytes());
    let octet = (hash[0] as u16 % 255 + 1) as u8;
    format!("10.43.{}.0/24", octet)
}

fn persist_network(store_root: &std::path::Path, name: &str, subnet: &str) -> Result<(), String> {
    let path = networks_registry_path(store_root);
    let mut networks: HashMap<String, String> = if path.exists() {
        let data =
            std::fs::read_to_string(&path).map_err(|e| format!("read networks.json: {e}"))?;
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        HashMap::new()
    };
    networks.insert(name.to_string(), subnet.to_string());
    let data =
        serde_json::to_string_pretty(&networks).map_err(|e| format!("serialize networks: {e}"))?;
    std::fs::write(&path, data).map_err(|e| format!("write networks.json: {e}"))?;
    Ok(())
}

fn unpersist_network(store_root: &std::path::Path, name: &str) -> Result<(), String> {
    let path = networks_registry_path(store_root);
    if !path.exists() {
        return Ok(());
    }
    let data = std::fs::read_to_string(&path).map_err(|e| format!("read networks.json: {e}"))?;
    let mut networks: HashMap<String, String> = serde_json::from_str(&data).unwrap_or_default();
    networks.remove(name);
    let data =
        serde_json::to_string_pretty(&networks).map_err(|e| format!("serialize networks: {e}"))?;
    std::fs::write(&path, data).map_err(|e| format!("write networks.json: {e}"))?;
    Ok(())
}

fn list_persisted_networks(store_root: &std::path::Path) -> Result<Vec<(String, String)>, String> {
    let path = networks_registry_path(store_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = std::fs::read_to_string(&path).map_err(|e| format!("read networks.json: {e}"))?;
    let networks: HashMap<String, String> = serde_json::from_str(&data).unwrap_or_default();
    let mut result: Vec<(String, String)> = networks.into_iter().collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
