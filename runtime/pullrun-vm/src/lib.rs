// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_lazy_continuation)]

pub mod attach;
pub mod ext4;
pub mod network;
pub mod oci_kernel;
pub mod pool;

#[cfg(target_os = "macos")]
pub mod apple;

#[cfg(target_os = "macos")]
pub use apple::{
    AcquiredVm, AppleVirtError, AppleVirtPool, AppleVirtPoolConfig, DEFAULT_POOL_SIZE,
    DEFAULT_VM_CPUS, DEFAULT_VM_MEM_MIB,
};

pub use attach::VmPersistentHandle;
pub use attach::{
    attach_to_vm, run_session_blocking, spawn_apple_virt_vm, spawn_vm, AppleVirtAttachConfig,
    AppleVirtAttachHandle, AttachError, FrameSink, FrameSource, VmCommand, DEFAULT_VSOCK_PORT,
};
pub use oci_kernel::{OciKernelError, StagedKernel, KERNEL_INITRAMFS_PATH, KERNEL_VMLINUX_PATH};

pub use network::{
    create_slirp_tap, create_tap, create_tap_on_bridge, derive_cidr, ensure_bridge,
    ensure_bridge_named, mac_from_ip, teardown_tap, VmNetError, VmNetwork, BRIDGE_NAME, GATEWAY_IP,
    NETMASK,
};
pub use pool::{PooledVm, VmPool, VmPoolConfig};

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pullrun_exec::types::{Backend, ExecOutput, ExitStatus, WorkloadSpec, WorkloadStats};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{info, warn};

use pullrun_exec::types::NetworkMode;
use pullrun_exec::{ExecError, Executor, ProcessHandle};
use pullrun_net::{Ipam, NetworkEndpoint, NetworkManager, ProxyNetwork};
use pullrun_store::MmapStore;

pub use ext4::{
    ext4_path_for, firecracker_config, materialize_ext4_rootfs, Ext4Error, Ext4Options,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSidecar {
    pub vm_net: VmNetwork,
    pub endpoint: NetworkEndpoint,
    pub tap_name: String,
}

#[derive(Debug, Clone)]
pub struct FirecrackerConfig {
    pub firecracker_path: PathBuf,
    pub kernel_path: PathBuf,
    pub rootfs_dir: PathBuf,
    pub jailer_path: Option<PathBuf>,
    pub vcpus: u8,
    pub mem_mib: u32,
    pub size_mb: u64,
}

impl FirecrackerConfig {
    pub fn new(firecracker_path: PathBuf, kernel_path: PathBuf, rootfs_dir: PathBuf) -> Self {
        Self {
            firecracker_path,
            kernel_path,
            rootfs_dir,
            jailer_path: None,
            vcpus: 2,
            mem_mib: 512,
            size_mb: 256,
        }
    }
}

pub struct FirecrackerExecutor {
    config: FirecrackerConfig,
    store: Arc<MmapStore>,
    /// Shared IPAM with the container backend. Firecracker VMs and
    /// runc containers get IPs from the same `10.42.0.0/16` pool, so
    /// they can talk to each other over the bridge.
    ipam: Arc<Ipam>,
    /// Used to start inbound TCP proxy listeners (`0.0.0.0:port` →
    /// `vm_ip:port`) for declared inbound rules.
    proxy: Arc<ProxyNetwork>,
    /// Open fds on `/dev/net/tun`, keyed by workload id. Kept alive to
    /// prevent the kernel from destroying the TAP devices. Held briefly
    /// (insert/remove of a `File` handle + optional `teardown_tap()`), so
    /// the `Mutex` contention is negligible. The inner `File` is `Send` but
    /// not `Sync`, hence `Mutex` rather than `RwLock`.
    tap_fds: Mutex<HashMap<String, File>>,
    /// Long-running slirp4netns child processes, keyed by workload id.
    /// Each child provides userspace NAT for one VM's TAP device. Killed
    /// in `stop()`.
    slirp_children: Mutex<HashMap<String, std::process::Child>>,
    /// Warm VM pool: pre-booted Firecracker VMs for fast allocation.
    /// When set, `create()` attempts to acquire from the pool before
    /// falling back to cold boot.
    pool: Option<Arc<VmPool>>,
    /// Track workload ids that were allocated from the warm pool.
    /// These skip certain cleanup in `stop()` because the pool entry
    /// has already been consumed.
    pool_allocations: Mutex<HashSet<String>>,
}

impl FirecrackerExecutor {
    pub fn new(
        config: FirecrackerConfig,
        store: Arc<MmapStore>,
        ipam: Arc<Ipam>,
        proxy: Arc<ProxyNetwork>,
    ) -> Self {
        Self {
            config,
            store,
            ipam,
            proxy,
            tap_fds: Mutex::new(HashMap::new()),
            slirp_children: Mutex::new(HashMap::new()),
            pool: None,
            pool_allocations: Mutex::new(HashSet::new()),
        }
    }

    pub fn with_pool(mut self, pool: Arc<VmPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    fn vm_dir(&self, id: &str) -> PathBuf {
        self.config.rootfs_dir.join(id)
    }

    /// Public accessor: where the sidecar file for a given VM id lives.
    /// `RuntimeService` uses this to detect whether a given workload id
    /// belongs to the VM backend (and route `stop()` / `status()` to it).
    pub fn sidecar_path_for(&self, id: &str) -> PathBuf {
        self.vm_dir(id).join("network.json")
    }

    async fn check_firecracker(&self) -> Result<(), ExecError> {
        let output = Command::new(&self.config.firecracker_path)
            .arg("--version")
            .output()
            .await
            .map_err(|e| {
                ExecError::BackendNotAvailable(format!(
                    "firecracker not found at {}: {e}",
                    self.config.firecracker_path.display()
                ))
            })?;

        if !output.status.success() {
            return Err(ExecError::BackendNotAvailable(
                "firecracker returned non-zero".into(),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        info!(version = %stdout.trim(), "firecracker found");
        Ok(())
    }

    fn tap_name_for(&self, id: &str) -> String {
        // Linux tap names max out at 15 chars. Use a stable prefix and a
        // hash of the id so we don't collide.
        use sha2::{Digest, Sha256};
        let h = Sha256::digest(id.as_bytes());
        let suffix = format!("{:02x}{:02x}{:02x}{:02x}", h[0], h[1], h[2], h[3]);
        format!("tap-{suffix}")
    }

    /// Fast path: create a workload from a pre-booted pool VM.
    /// Materializes the workload's ext4 rootfs, hot-swaps it into the
    /// running pool VM via the Firecracker API, and reboots the VM so
    /// it boots with the new rootfs. Returns a `ProcessHandle` with the
    /// pool VM's pre-allocated IP and network info.
    async fn create_from_pool(
        &self,
        spec: WorkloadSpec,
        pooled: crate::pool::PooledVm,
    ) -> Result<ProcessHandle, ExecError> {
        let pool_id = pooled.pool_id().to_string();
        let guest_ip = pooled.guest_ip();
        let vm_dir = self.vm_dir(&spec.id);
        std::fs::create_dir_all(&vm_dir)?;

        // Materialize the workload's ext4 rootfs.
        let ext4_path = ext4_path_for(&self.config.rootfs_dir, &spec.id);
        let options = Ext4Options {
            size_mb: self.config.size_mb,
            label: Some(format!("pullrun-{}", spec.id)),
        };
        let store = self.store.clone();
        let root_digest = spec.image_root;
        let ext4_path_owned = ext4_path.clone();

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                materialize_ext4_rootfs(&store, &root_digest, &ext4_path_owned, options)
                    .await
                    .map_err(|e| ExecError::ExecutionFailed(format!("ext4 materialization: {e}")))
            })
        })
        .await
        .map_err(|e| ExecError::ExecutionFailed(format!("spawn_blocking: {e}")))??;

        // Hot-swap the rootfs on the running pool VM.
        pooled
            .swap_rootfs(&ext4_path)
            .map_err(|e| ExecError::ExecutionFailed(format!("rootfs hot-swap: {e}")))?;

        // Persist the network sidecar (pool VM's pre-configured network).
        let sidecar = VmSidecar {
            vm_net: pooled.vm_net().clone(),
            endpoint: pooled.endpoint().clone(),
            tap_name: pooled.tap_name().to_string(),
        };
        let sidecar_path = vm_dir.join("network.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar)
            .map_err(|e| ExecError::ExecutionFailed(format!("serialize sidecar: {e}")))?;
        std::fs::write(&sidecar_path, sidecar_json)?;

        // Persist the kernel path sidecar if specified.
        if let Some(kp) = &spec.kernel_path {
            let kernel_path_sidecar = vm_dir.join("kernel_path");
            std::fs::write(&kernel_path_sidecar, kp.to_string_lossy().as_bytes())?;
        }

        // Copy the pool VM's console.log for attach streaming.
        let pool_console = pooled.vm_dir().join("console.log");
        let wl_console = vm_dir.join("console.log");
        if pool_console.exists() {
            let _ = std::fs::copy(&pool_console, &wl_console);
        }

        // Reboot the VM so it boots with the new rootfs.
        pooled
            .reboot()
            .map_err(|e| ExecError::ExecutionFailed(format!("reboot: {e}")))?;

        info!(
            id = %spec.id,
            pool_id = %pool_id,
            ip = %guest_ip,
            "workload created from warm pool VM"
        );

        // Track this id as a pool allocation so start() becomes a no-op
        // and stop() skips Firecracker-specific cleanup.
        self.pool_allocations
            .lock()
            .expect("pool_allocations lock poisoned")
            .insert(spec.id.clone());

        // Register proxy listeners for declared inbound rules.
        let _ = self
            .proxy
            .register_endpoint(&spec.id, guest_ip.to_string(), &spec.network_rules)
            .await;

        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: Some(guest_ip.to_string()),
            host_ports: vec![],
            backend: "vm".to_string(),
            bridge_name: None,
            network_join: None,
        };

        Ok(handle)
    }

    fn is_pool_allocated(&self, id: &str) -> bool {
        self.pool_allocations
            .lock()
            .expect("pool_allocations lock poisoned")
            .contains(id)
    }
}

#[async_trait]
impl Executor for FirecrackerExecutor {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        // VMs cannot share a network namespace with another workload
        // (no netns inside the guest); reject the pod model outright.
        if matches!(
            spec.network_mode,
            pullrun_exec::types::NetworkMode::Container(_)
        ) {
            return Err(ExecError::ExecutionFailed(
                "network mode 'container:<id>' is only valid for container backends, not VMs"
                    .to_string(),
            ));
        }
        self.check_firecracker().await?;

        info!(id = %spec.id, "creating Firecracker VM");

        // Try warm pool first (fast path: pre-booted VM, hot-swap rootfs).
        if let Some(pool) = &self.pool {
            if let Some(pooled) = pool.acquire() {
                return self.create_from_pool(spec, pooled).await;
            }
            info!("warm pool exhausted, falling back to cold boot");
        }

        // 1. Allocate an IP. Use a per-project IPAM if bridge_name
        //    is set (per-project isolation), otherwise the global pool.
        let (_bridge_name, _netmask, _gateway, guest_ip, _ipam_owned) =
            if let Some(ref bn) = spec.bridge_name {
                let (gateway, netmask, project_ipam) = crate::network::derive_cidr(bn);
                let ip_u32 = project_ipam
                    .allocate()
                    .ok_or_else(|| ExecError::ExecutionFailed("no IPs in project subnet".into()))?;
                let guest_ip = Ipv4Addr::from(ip_u32);
                (bn.clone(), netmask, gateway, guest_ip, Some(project_ipam))
            } else {
                let ip_u32 = self.ipam.allocate().ok_or_else(|| {
                    ExecError::ExecutionFailed("no IPs available in shared IPAM".into())
                })?;
                let guest_ip = Ipv4Addr::from(ip_u32);
                (
                    crate::network::BRIDGE_NAME.to_string(),
                    crate::network::NETMASK,
                    crate::network::GATEWAY_IP,
                    guest_ip,
                    None,
                )
            };

        // 2. Plumb the host-side network: bridge + tap, or slirp4netns.
        let tap_name = self.tap_name_for(&spec.id);
        let use_slirp = matches!(spec.network_mode, NetworkMode::Slirp);
        let (vm_net, tap_fd, slirp_child) = if use_slirp {
            let (vm_net, child) =
                crate::network::create_slirp_tap(&tap_name, guest_ip, _netmask, _gateway)
                    .map_err(|e| ExecError::ExecutionFailed(format!("slirp network setup: {e}")))?;
            (vm_net, None, Some(child))
        } else if let Some(ref bn) = spec.bridge_name {
            let (gw, nm, _) = crate::network::derive_cidr(bn);
            let cidr = format!("10.{}.{}.0/24", gw.octets()[1], gw.octets()[2]);
            crate::network::ensure_bridge_named(bn, &cidr, gw)
                .map_err(|e| ExecError::ExecutionFailed(format!("bridge setup: {e}")))?;
            let (vm_net, fd) = create_tap_on_bridge(&tap_name, guest_ip, bn, nm, gw)
                .map_err(|e| ExecError::ExecutionFailed(format!("vm network setup: {e}")))?;
            (vm_net, Some(fd), None)
        } else {
            let (vm_net, fd) = create_tap(&tap_name, guest_ip)
                .map_err(|e| ExecError::ExecutionFailed(format!("vm network setup: {e}")))?;
            (vm_net, Some(fd), None)
        };
        if let Some(fd) = tap_fd {
            self.tap_fds
                .lock()
                .expect("tap_fds lock poisoned")
                .insert(spec.id.clone(), fd);
        }
        if let Some(child) = slirp_child {
            self.slirp_children
                .lock()
                .expect("slirp_children lock poisoned")
                .insert(spec.id.clone(), child);
        }

        // 3. Start the inbound proxy listeners (for any declared rules).
        //    The listeners bind on 0.0.0.0:host_port and forward into
        //    the VM over the bridge. The VM does not need any extra
        //    configuration: the kernel `ip=` boot arg already set the
        //    guest's IP and gateway.
        let endpoint = self
            .proxy
            .register_endpoint(&spec.id, guest_ip.to_string(), &spec.network_rules)
            .await
            .map_err(|e| ExecError::ExecutionFailed(format!("proxy register: {e}")))?;

        let vm_dir = self.vm_dir(&spec.id);
        std::fs::create_dir_all(&vm_dir)?;

        // 4. Materialize the DAG root as an ext4 rootfs.
        let ext4_path = ext4_path_for(&self.config.rootfs_dir, &spec.id);
        let options = Ext4Options {
            size_mb: self.config.size_mb,
            label: Some(format!("pullrun-{}", spec.id)),
        };

        let store = self.store.clone();
        let root_digest = spec.image_root;
        let ext4_path_owned = ext4_path.clone();
        let ext4_id = spec.id.clone();

        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                materialize_ext4_rootfs(&store, &root_digest, &ext4_path_owned, options)
                    .await
                    .map_err(|e| ExecError::ExecutionFailed(format!("ext4 materialization: {e}")))
            })
        })
        .await
        .map_err(|e| ExecError::ExecutionFailed(format!("spawn_blocking: {e}")))??;

        info!(id = %ext4_id, ip = %guest_ip, "ext4 rootfs ready, VM wired to bridge");

        // 5. Persist the kernel path sidecar if the workload
        //    specified a kernel_image (OCI-staged kernel).
        if let Some(kp) = &spec.kernel_path {
            let kernel_path_sidecar = vm_dir.join("kernel_path");
            std::fs::write(&kernel_path_sidecar, kp.to_string_lossy().as_bytes())?;
            info!(id = %ext4_id, kernel = %kp.display(), "per-workload kernel path saved");
        }

        // 6. Persist the network sidecar (so stop() can teardown).
        let sidecar = VmSidecar {
            vm_net: vm_net.clone(),
            endpoint: endpoint.clone(),
            tap_name: tap_name.clone(),
        };
        let sidecar_path = vm_dir.join("network.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar)
            .map_err(|e| ExecError::ExecutionFailed(format!("serialize sidecar: {e}")))?;
        std::fs::write(&sidecar_path, sidecar_json)?;

        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: Some(guest_ip.to_string()),
            host_ports: endpoint.host_port_mappings.to_vec(),
            backend: "vm".to_string(),
            bridge_name: None,
            network_join: None,
        };

        Ok(handle)
    }

    async fn start(&self, handle: &mut ProcessHandle) -> Result<(), ExecError> {
        // Pool-allocated VMs are already booted — no-op.
        {
            let pa = self
                .pool_allocations
                .lock()
                .expect("pool_allocations lock poisoned");
            if pa.contains(&handle.id) {
                info!(id = %handle.id, "pool VM already running; skipping start");
                return Ok(());
            }
        }

        info!(id = %handle.id, "starting Firecracker VM");

        let vm_dir = self.vm_dir(&handle.id);
        let api_socket = vm_dir.join("firecracker.sock");

        if api_socket.exists() {
            std::fs::remove_file(&api_socket).ok();
        }

        // Read the sidecar so we know which tap device and IP to use.
        let sidecar_path = vm_dir.join("network.json");
        let sidecar: VmSidecar = serde_json::from_str(
            &std::fs::read_to_string(&sidecar_path)
                .map_err(|e| ExecError::NotFound(format!("sidecar: {e}")))?,
        )
        .map_err(|e| ExecError::ExecutionFailed(format!("parse sidecar: {e}")))?;

        let ext4_path = ext4_path_for(&self.config.rootfs_dir, &handle.id);

        // Use per-workload kernel path if available (OCI-staged kernel),
        // otherwise fall back to the default kernel_path from config.
        let kernel_path_sidecar = vm_dir.join("kernel_path");
        let kernel_path = if let Ok(kp) = std::fs::read_to_string(&kernel_path_sidecar) {
            let trimmed = kp.trim().to_string();
            if !trimmed.is_empty() {
                info!(path = %trimmed, "using per-workload kernel path");
                PathBuf::from(trimmed)
            } else {
                self.config.kernel_path.clone()
            }
        } else {
            self.config.kernel_path.clone()
        };

        let config = firecracker_config(
            &handle.id,
            &kernel_path,
            &ext4_path,
            Some(&sidecar.vm_net),
            self.config.vcpus,
            self.config.mem_mib,
        );

        let config_path = vm_dir.join("vm-config.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serialization never fails"),
        )?;

        // Firecracker's --log-path target must exist before the process starts.
        let log_path = vm_dir.join("firecracker.log");
        if !log_path.exists() {
            std::fs::write(&log_path, b"")?;
        }

        // Redirect serial console output (ttyS0) to console.log so
        // the AttachWorkload handler can stream it to the client.
        let console_path = vm_dir.join("console.log");
        let console_file = std::fs::File::create(&console_path)
            .map_err(|e| ExecError::ExecutionFailed(format!("create console.log: {e}")))?;

        let child = Command::new(&self.config.firecracker_path)
            .arg("--api-sock")
            .arg(&api_socket)
            .arg("--config-path")
            .arg(&config_path)
            .arg("--log-path")
            .arg(&log_path)
            .arg("--metrics-path")
            .arg(vm_dir.join("firecracker.metrics"))
            .arg("--level")
            .arg("Info")
            .stdout(
                console_file
                    .try_clone()
                    .map_err(|e| ExecError::ExecutionFailed(format!("clone console fd: {e}")))?,
            )
            .stderr(console_file)
            .spawn()?;

        let pid_path = vm_dir.join("firecracker.pid");
        std::fs::write(&pid_path, child.id().unwrap_or(0).to_string())?;

        info!(
            id = %handle.id,
            ip = %sidecar.vm_net.guest_ip,
            tap = %sidecar.vm_net.tap_name,
            pid = ?child.id(),
            socket = %api_socket.display(),
            "Firecracker VM started"
        );

        std::mem::forget(child);
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), ExecError> {
        // Pool-allocated VMs: the Firecracker process and TAP device
        // belong to the pool, not the workload. We only clean up the
        // workload's ext4 rootfs and vm_dir.
        if self.is_pool_allocated(id) {
            self.pool_allocations
                .lock()
                .expect("pool_allocations lock poisoned")
                .remove(id);
            info!(%id, "stopping pool-allocated VM workload (keeping FC + TAP)");
            let ext4_path = ext4_path_for(&self.config.rootfs_dir, id);
            if ext4_path.exists() {
                let _ = std::fs::remove_file(&ext4_path);
            }
            let vm_dir = self.vm_dir(id);
            if vm_dir.exists() {
                tokio::fs::remove_dir_all(&vm_dir).await.ok();
            }
            let proxy = self.proxy.clone();
            let id_owned = id.to_string();
            let dummy_endpoint = pullrun_net::NetworkEndpoint {
                internal_ip: String::new(),
                host_port_mappings: vec![],
                namespace_path: None,
            };
            tokio::spawn(async move {
                let _ = proxy.teardown(&id_owned, &dummy_endpoint).await;
            });
            return Ok(());
        }

        info!(%id, "stopping Firecracker VM");

        let vm_dir = self.vm_dir(id);

        let pid_file = vm_dir.join("firecracker.pid");
        if pid_file.exists() {
            if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid_str.trim().parse::<u32>() {
                    let _ = Command::new("kill")
                        .arg("-9")
                        .arg(pid.to_string())
                        .output()
                        .await;
                }
            }
        }

        // Tear down the host-side tap device (and proxy listeners).
        let sidecar_path = vm_dir.join("network.json");
        if let Ok(json) = std::fs::read_to_string(&sidecar_path) {
            if let Ok(sidecar) = serde_json::from_str::<VmSidecar>(&json) {
                // Extract the TAP fd for this workload and drop it
                // (which destroys the device), also run ip link del.
                let tap_fd = self
                    .tap_fds
                    .lock()
                    .expect("tap_fds lock poisoned")
                    .remove(id);
                if let Err(e) = teardown_tap(&sidecar.tap_name, tap_fd) {
                    warn!(tap = %sidecar.tap_name, "tap teardown warning: {e}");
                }
                // Inbound listeners for this workload id are owned by
                // the proxy; ask it to release them. The endpoint
                // argument is mostly informational for the proxy.
                let endpoint_for_proxy = sidecar.endpoint.clone();
                let proxy = self.proxy.clone();
                let id_owned = id.to_string();
                tokio::spawn(async move {
                    let _ = proxy.teardown(&id_owned, &endpoint_for_proxy).await;
                });
            }
        }

        // Kill slirp4netns if it was used (closes the TAP fd, destroying
        // the device). This runs unconditionally — the map entry is
        // absent for bridge-mode VMs.
        if let Some(mut child) = self
            .slirp_children
            .lock()
            .expect("slirp_children lock poisoned")
            .remove(id)
        {
            let _ = child.kill();
            let _ = child.wait();
            info!(%id, "slirp4netns child killed");
        }

        let ext4_path = ext4_path_for(&self.config.rootfs_dir, id);
        if ext4_path.exists() {
            let _ = std::fs::remove_file(&ext4_path);
        }

        if vm_dir.exists() {
            tokio::fs::remove_dir_all(&vm_dir).await.ok();
        }

        Ok(())
    }

    async fn wait(&self, id: &str) -> Result<ExitStatus, ExecError> {
        warn!(%id, "Firecracker wait not fully implemented in Phase 2 scaffold");
        Ok(ExitStatus {
            exit_code: 0,
            signal: None,
        })
    }

    async fn status(&self, id: &str) -> Result<String, ExecError> {
        let pid_file = self.vm_dir(id).join("firecracker.pid");
        if pid_file.exists() {
            if let Ok(pid_str) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = pid_str.trim().parse::<i32>() {
                    // Check if the process is still running.
                    // `kill(pid, 0)` returns 0 if the process exists
                    // (including zombies).  We follow up with
                    // `waitpid(WNOHANG)` to reap a zombie and return
                    // "stopped" for a process that has exited.
                    let exists = unsafe { libc::kill(pid, 0) == 0 };
                    if exists {
                        let mut status: i32 = 0;
                        let reaped =
                            unsafe { libc::waitpid(pid, &mut status as *mut _, libc::WNOHANG) };
                        if reaped > 0 {
                            // Child was a zombie and has been reaped
                            // (or just exited). Report as stopped.
                            return Ok("stopped".to_string());
                        }
                        // Child is still running (reaped == 0, or
                        // reaped == -1 with ECHILD if another task
                        // already reaped it while we were in the
                        // syscall — in that case also treat as
                        // stopped).
                        if reaped == 0 {
                            return Ok("running".to_string());
                        }
                    }
                }
            }
            // Process is gone but PID file still exists (firecracker
            // exited, or the PID was recycled). Report as stopped.
            Ok("stopped".to_string())
        } else {
            Ok("stopped".to_string())
        }
    }

    async fn update(
        &self,
        _id: &str,
        _cpu_millicores: Option<u64>,
        _memory_bytes: Option<u64>,
    ) -> Result<(), ExecError> {
        Err(ExecError::BackendNotAvailable(
            "FirecrackerExecutor does not support live resource updates yet".into(),
        ))
    }

    async fn stats(&self, _id: &str) -> Result<WorkloadStats, ExecError> {
        Err(ExecError::BackendNotAvailable(
            "FirecrackerExecutor does not support live stats yet".into(),
        ))
    }

    async fn exec(
        &self,
        _id: &str,
        _command: &[String],
        _timeout_secs: u64,
    ) -> Result<ExecOutput, ExecError> {
        Err(ExecError::BackendNotAvailable(
            "Firecracker VM exec: use 'pullrun run --backend vm --cmd <cmd>' to run a one-shot command, \
             or 'pullrun exec --tty <id> -- <cmd>' for interactive container exec".into(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct AppleVirtConfig {
    pub linux_runtime_path: PathBuf,
    pub warm_pool_size: usize,
}

pub struct AppleVirtExecutor {
    pub config: AppleVirtConfig,
}

impl AppleVirtExecutor {
    pub fn new(config: AppleVirtConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Executor for AppleVirtExecutor {
    async fn create(&self, _spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        Err(ExecError::BackendNotAvailable(
            "AppleVirtExecutor is a Phase 5 stub - macOS VM support requires Virtualization framework bindings".into(),
        ))
    }

    async fn start(&self, _handle: &mut ProcessHandle) -> Result<(), ExecError> {
        Err(ExecError::BackendNotAvailable(
            "AppleVirtExecutor stub".into(),
        ))
    }

    async fn stop(&self, _id: &str) -> Result<(), ExecError> {
        Err(ExecError::BackendNotAvailable(
            "AppleVirtExecutor stub".into(),
        ))
    }

    async fn wait(&self, _id: &str) -> Result<ExitStatus, ExecError> {
        Err(ExecError::BackendNotAvailable(
            "AppleVirtExecutor stub".into(),
        ))
    }

    async fn status(&self, _id: &str) -> Result<String, ExecError> {
        Ok("unavailable".to_string())
    }

    async fn update(
        &self,
        _id: &str,
        _cpu_millicores: Option<u64>,
        _memory_bytes: Option<u64>,
    ) -> Result<(), ExecError> {
        Err(ExecError::BackendNotAvailable(
            "AppleVirtExecutor stub".into(),
        ))
    }

    async fn stats(&self, _id: &str) -> Result<WorkloadStats, ExecError> {
        Err(ExecError::BackendNotAvailable(
            "AppleVirtExecutor stub".into(),
        ))
    }

    async fn exec(
        &self,
        _id: &str,
        _command: &[String],
        _timeout_secs: u64,
    ) -> Result<ExecOutput, ExecError> {
        Err(ExecError::BackendNotAvailable(
            "Apple Virt VM exec: use 'pullrun workload run <id> --tty' for interactive shell, \
             or 'pullrun exec --tty <id> -- <cmd>' for interactive container exec"
                .into(),
        ))
    }
}

pub fn select_executor_for(spec: &WorkloadSpec) -> &'static str {
    match spec.backend {
        Backend::Container => "LinuxContainerExecutor",
        Backend::ContainerRootless => "RootlessContainerExecutor",
        Backend::Vm => "FirecrackerExecutor / AppleVirtExecutor",
        Backend::Sandbox => "SandboxExecutor (Phase 5)",
    }
}
