// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_lazy_continuation)]

pub mod attach;
pub mod ext4;
pub mod network;
pub mod oci_kernel;
pub mod pool;
pub mod snapshot;

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
use pullrun_exec::types::{
    Backend, ExecOutput, ExitStatus, SnapshotFidelity, SnapshotInfo, WorkloadSpec, WorkloadStats,
};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{info, warn};

use pullrun_exec::types::NetworkMode;
use pullrun_exec::{ExecError, Executor, ProcessHandle};
use pullrun_net::{Ipam, NetworkEndpoint, NetworkManager, ProxyNetwork};
use pullrun_oci::cow;
use pullrun_store::{Digest, MmapStore};

use crate::snapshot::FirecrackerApi;

pub use ext4::{
    ext4_path_for, firecracker_config, materialize_ext4_rootfs, Ext4Error, Ext4Options,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSidecar {
    pub vm_net: VmNetwork,
    pub endpoint: NetworkEndpoint,
    pub tap_name: String,
    /// True for `--net none`: no IP, tap, bridge, or network device.
    #[serde(default)]
    pub network_disabled: bool,
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
    /// Single-flight locks for per-digest ext4 base images.
    base_locks: Mutex<HashMap<Digest, Arc<tokio::sync::Mutex<()>>>>,
    /// Reflink capability of the rootfs filesystem. Without reflink a
    /// shared base would only add one more full copy, so ext4 hosts
    /// keep building each VM's image directly.
    cow_mode: cow::CowMode,
}

impl FirecrackerExecutor {
    pub fn new(
        config: FirecrackerConfig,
        store: Arc<MmapStore>,
        ipam: Arc<Ipam>,
        proxy: Arc<ProxyNetwork>,
    ) -> Self {
        let cow_mode = cow::probe(&config.rootfs_dir);
        Self {
            config,
            store,
            ipam,
            proxy,
            tap_fds: Mutex::new(HashMap::new()),
            slirp_children: Mutex::new(HashMap::new()),
            pool: None,
            pool_allocations: Mutex::new(HashSet::new()),
            base_locks: Mutex::new(HashMap::new()),
            cow_mode,
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

    fn ext4_base_path(&self, digest: &Digest) -> PathBuf {
        self.config
            .rootfs_dir
            .join("bases")
            .join(format!("{}.ext4", digest.as_hex()))
    }

    /// Build (once) the flattened ext4 image for `digest`.
    async fn ensure_ext4_base(&self, digest: &Digest) -> Result<PathBuf, ExecError> {
        let base = self.ext4_base_path(digest);
        let ready = PathBuf::from(format!("{}.ready", base.display()));
        if base.is_file() && ready.is_file() {
            return Ok(base);
        }

        let lock = {
            let mut locks = self.base_locks.lock().expect("base_locks poisoned");
            locks
                .entry(*digest)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;

        if base.is_file() && ready.is_file() {
            return Ok(base);
        }

        let bases_dir = self.config.rootfs_dir.join("bases");
        std::fs::create_dir_all(&bases_dir)
            .map_err(|e| ExecError::ExecutionFailed(format!("create bases dir: {e}")))?;
        let tmp = bases_dir.join(format!(
            ".tmp-{}-{}.ext4",
            digest.as_hex(),
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        let options = Ext4Options {
            size_mb: self.config.size_mb,
            label: Some("pullrun-base".to_string()),
        };
        info!(%digest, path = %base.display(), "building ext4 image base");
        materialize_ext4_rootfs(&self.store, digest, &tmp, options, &[])
            .await
            .map_err(|e| ExecError::ExecutionFailed(format!("ext4 base build: {e}")))?;
        match std::fs::rename(&tmp, &base) {
            Ok(()) => {}
            Err(_) if base.is_file() => {
                let _ = std::fs::remove_file(&tmp);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(ExecError::ExecutionFailed(format!(
                    "install ext4 base: {e}"
                )));
            }
        }
        std::fs::write(&ready, b"v1")
            .map_err(|e| ExecError::ExecutionFailed(format!("write base marker: {e}")))?;
        Ok(base)
    }

    /// Provision this VM's ext4 image as a CoW clone of the base.
    async fn provision_vm_rootfs(
        &self,
        digest: &Digest,
        id: &str,
        init_command: &[String],
    ) -> Result<PathBuf, ExecError> {
        let dest = ext4_path_for(&self.config.rootfs_dir, id);
        // A workload command needs its own /init, so it cannot reuse the
        // per-digest base image (which carries the OCI default command).
        if self.cow_mode == cow::CowMode::Copy || !init_command.is_empty() {
            // No reflink: a shared base cannot save bytes, so build the
            // VM image directly (same as the previous behaviour).
            let options = Ext4Options {
                size_mb: self.config.size_mb,
                label: Some(format!("pullrun-{id}")),
            };
            materialize_ext4_rootfs(&self.store, digest, &dest, options, init_command)
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("ext4 materialization: {e}")))?;
            return Ok(dest);
        }
        let base = self.ensure_ext4_base(digest).await?;
        let dest_for_clone = dest.clone();
        let cloned =
            tokio::task::spawn_blocking(move || cow::try_clone_file(&base, &dest_for_clone))
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("clone join: {e}")))?
                .map_err(|e| ExecError::ExecutionFailed(format!("clone ext4 base: {e}")))?;
        debug_assert!(dest.is_file());
        let _ = cloned;
        Ok(dest)
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

        // Provision the workload's ext4 rootfs from the image base.
        let ext4_path = self
            .provision_vm_rootfs(&spec.image_root, &spec.id, &spec.command)
            .await?;

        // Hot-swap the rootfs on the running pool VM.
        pooled
            .swap_rootfs(&ext4_path)
            .map_err(|e| ExecError::ExecutionFailed(format!("rootfs hot-swap: {e}")))?;

        // Persist the network sidecar (pool VM's pre-configured network).
        let sidecar = VmSidecar {
            vm_net: pooled.vm_net().clone(),
            endpoint: pooled.endpoint().clone(),
            tap_name: pooled.tap_name().to_string(),
            network_disabled: false,
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
            gateway: None,
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

impl FirecrackerExecutor {
    /// Actual rootfs path for a VM. Restored snapshots keep the drive
    /// path recorded in the snapshot, so a sidecar overrides the
    /// default `<rootfs_dir>/<id>.ext4`.
    fn drive_path_for(&self, id: &str) -> std::path::PathBuf {
        let sidecar = self.vm_dir(id).join("drive_path");
        if let Ok(p) = std::fs::read_to_string(&sidecar) {
            let p = p.trim();
            if !p.is_empty() {
                return std::path::PathBuf::from(p);
            }
        }
        ext4_path_for(&self.config.rootfs_dir, id)
    }
}

/// Copy a VM disk image, preserving holes where the platform supports it.
fn copy_image_file(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let ok = std::process::Command::new("cp")
            .arg("--sparse=always")
            .arg(src)
            .arg(dst)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(());
        }
    }
    std::fs::copy(src, dst).map(|_| ())
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

        // `Loopback` is Pullrun's isolated/none mode for VMs: no external
        // network device, no IP, no tap/bridge.
        if spec.egress_deny {
            return Err(ExecError::BackendNotAvailable(
                "fail-closed egress is not implemented for Firecracker VMs yet".into(),
            ));
        }
        let no_network = matches!(spec.network_mode, NetworkMode::Loopback);
        info!(id = %spec.id, no_network, "creating Firecracker VM");

        // Try warm pool first (fast path: pre-booted VM, hot-swap rootfs).
        // Pool VMs have networking pre-configured, so `--net none` must
        // take the cold path.
        if !no_network {
            if let Some(pool) = &self.pool {
                if let Some(pooled) = pool.acquire() {
                    return self.create_from_pool(spec, pooled).await;
                }
                info!("warm pool exhausted, falling back to cold boot");
            }
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
        let (vm_net, tap_fd, slirp_child) = if no_network {
            (
                VmNetwork {
                    tap_name: String::new(),
                    bridge_name: String::new(),
                    guest_ip,
                    guest_mac: String::new(),
                    netmask: Ipv4Addr::UNSPECIFIED,
                    gateway: Ipv4Addr::UNSPECIFIED,
                },
                None,
                None,
            )
        } else if use_slirp {
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
        let endpoint = if no_network {
            NetworkEndpoint {
                internal_ip: String::new(),
                host_port_mappings: vec![],
                namespace_path: None,
            }
        } else {
            self.proxy
                .register_endpoint(&spec.id, guest_ip.to_string(), &spec.network_rules)
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("proxy register: {e}")))?
        };

        let vm_dir = self.vm_dir(&spec.id);
        std::fs::create_dir_all(&vm_dir)?;

        // 4. Provision the ext4 rootfs as a CoW clone of the image base.
        self.provision_vm_rootfs(&spec.image_root, &spec.id, &spec.command)
            .await?;
        let ext4_id = spec.id.clone();

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
            network_disabled: no_network,
        };
        let sidecar_path = vm_dir.join("network.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar)
            .map_err(|e| ExecError::ExecutionFailed(format!("serialize sidecar: {e}")))?;
        std::fs::write(&sidecar_path, sidecar_json)?;

        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: if no_network {
                None
            } else {
                Some(guest_ip.to_string())
            },
            host_ports: endpoint.host_port_mappings.to_vec(),
            gateway: None,
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

        let vm_net = if sidecar.network_disabled {
            None
        } else {
            Some(&sidecar.vm_net)
        };
        let config = firecracker_config(
            &handle.id,
            &kernel_path,
            &ext4_path,
            vm_net,
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
            .arg("--config-file")
            .arg(&config_path)
            .arg("--log-path")
            .arg(&log_path)
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
            let ext4_path = self.drive_path_for(id);
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
                if !sidecar.network_disabled {
                    if let Err(e) = teardown_tap(&sidecar.tap_name, tap_fd) {
                        warn!(tap = %sidecar.tap_name, "tap teardown warning: {e}");
                    }
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

        let ext4_path = self.drive_path_for(id);
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
        let vm_dir = self.vm_dir(id);
        // The guest writes PULLRUN_EXIT=<code> to ttyS0 and powers off.
        // Firecracker itself stays alive after the guest halts, so use
        // the marker as the authoritative exit and reap the VMM.
        if let Ok(console) = std::fs::read_to_string(vm_dir.join("console.log")) {
            if let Some(pos) = console.rfind("PULLRUN_EXIT=") {
                let digits: String = console[pos + "PULLRUN_EXIT=".len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                if let Ok(code) = digits.parse::<i32>() {
                    if let Ok(pid_str) = std::fs::read_to_string(vm_dir.join("firecracker.pid")) {
                        if let Ok(pid) = pid_str.trim().parse::<i32>() {
                            unsafe {
                                libc::kill(pid, libc::SIGKILL);
                            }
                        }
                    }
                    return Ok(format!("exited({code})"));
                }
            }
        }
        let pid_file = vm_dir.join("firecracker.pid");
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

    async fn snapshot(
        &self,
        id: &str,
        out_dir: &std::path::Path,
        fidelity: SnapshotFidelity,
    ) -> Result<SnapshotInfo, ExecError> {
        let vm_dir = self.vm_dir(id);
        let api = FirecrackerApi::new(vm_dir.join("firecracker.sock"));
        tokio::fs::create_dir_all(out_dir).await?;
        let ext4_path = self.drive_path_for(id);
        let rootfs_path = out_dir.join("rootfs.ext4");

        if matches!(fidelity, SnapshotFidelity::Volumes) {
            return Err(ExecError::BackendNotAvailable(
                "VOLUMES fidelity is not implemented for Firecracker snapshots yet".into(),
            ));
        }

        api.pause()
            .await
            .map_err(|e| ExecError::ExecutionFailed(format!("pause VM: {e}")))?;

        if matches!(fidelity, SnapshotFidelity::Memory) {
            let state_path = out_dir.join("state");
            let mem_path = out_dir.join("mem");
            if let Err(e) = api.create_full_snapshot(&state_path, &mem_path).await {
                let _ = api.resume().await;
                return Err(ExecError::ExecutionFailed(format!("create snapshot: {e}")));
            }
            let copy_src = ext4_path.clone();
            let copy_dst = rootfs_path.clone();
            let copied = tokio::task::spawn_blocking(move || copy_image_file(&copy_src, &copy_dst))
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("copy join: {e}")))?;
            if let Err(e) = copied {
                let _ = api.resume().await;
                return Err(ExecError::ExecutionFailed(format!(
                    "copy snapshot rootfs {}: {e}",
                    ext4_path.display()
                )));
            }
            let mem_bytes = std::fs::metadata(&mem_path).map(|m| m.len()).unwrap_or(0);
            return Ok(SnapshotInfo {
                state_path,
                mem_path,
                rootfs_path,
                drive_path: ext4_path,
                mem_bytes,
            });
        }

        // ROOTFS fidelity: copy the disk while paused, then let the
        // source VM keep running.
        let copy_src = ext4_path.clone();
        let copy_dst = rootfs_path.clone();
        let copied = tokio::task::spawn_blocking(move || copy_image_file(&copy_src, &copy_dst))
            .await
            .map_err(|e| ExecError::ExecutionFailed(format!("copy join: {e}")))?;
        let _ = api.resume().await;
        if let Err(e) = copied {
            return Err(ExecError::ExecutionFailed(format!(
                "copy rootfs snapshot {}: {e}",
                ext4_path.display()
            )));
        }
        let mem_bytes = 0;
        Ok(SnapshotInfo {
            state_path: std::path::PathBuf::new(),
            mem_path: std::path::PathBuf::new(),
            rootfs_path,
            drive_path: ext4_path,
            mem_bytes,
        })
    }

    async fn restore_snapshot(
        &self,
        id: &str,
        state_path: &std::path::Path,
        mem_path: &std::path::Path,
        rootfs_path: &std::path::Path,
        drive_path: &std::path::Path,
    ) -> Result<ProcessHandle, ExecError> {
        let vm_dir = self.vm_dir(id);
        std::fs::create_dir_all(&vm_dir)?;
        if let Some(parent) = drive_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        copy_image_file(rootfs_path, drive_path).map_err(|e| {
            ExecError::ExecutionFailed(format!(
                "restore rootfs {} -> {}: {e}",
                rootfs_path.display(),
                drive_path.display()
            ))
        })?;

        let api_socket = vm_dir.join("firecracker.sock");
        if api_socket.exists() {
            std::fs::remove_file(&api_socket).ok();
        }
        let console_path = vm_dir.join("console.log");
        let console_file = std::fs::File::create(&console_path)?;
        let child = Command::new(&self.config.firecracker_path)
            .arg("--api-sock")
            .arg(&api_socket)
            .stdout(console_file.try_clone()?)
            .stderr(console_file)
            .spawn()?;
        let pid = child.id().unwrap_or(0);
        std::fs::write(vm_dir.join("firecracker.pid"), pid.to_string())?;

        crate::snapshot::wait_for_api(&api_socket, std::time::Duration::from_secs(10))
            .await
            .map_err(|e| ExecError::ExecutionFailed(format!("restore API: {e}")))?;
        let api = FirecrackerApi::new(&api_socket);
        if let Err(e) = api.load_snapshot(state_path, mem_path, true).await {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            return Err(ExecError::ExecutionFailed(format!("load snapshot: {e}")));
        }
        std::mem::forget(child);

        // Restored snapshots are only supported for the net-less mode;
        // record that so stop() does not try to tear down a tap.
        let sidecar = VmSidecar {
            vm_net: VmNetwork {
                tap_name: String::new(),
                bridge_name: String::new(),
                guest_ip: Ipv4Addr::LOCALHOST,
                guest_mac: String::new(),
                netmask: Ipv4Addr::UNSPECIFIED,
                gateway: Ipv4Addr::UNSPECIFIED,
            },
            endpoint: NetworkEndpoint {
                internal_ip: String::new(),
                host_port_mappings: vec![],
                namespace_path: None,
            },
            tap_name: String::new(),
            network_disabled: true,
        };
        std::fs::write(
            vm_dir.join("network.json"),
            serde_json::to_vec_pretty(&sidecar)
                .map_err(|e| ExecError::ExecutionFailed(format!("serialize sidecar: {e}")))?,
        )?;
        std::fs::write(
            vm_dir.join("drive_path"),
            drive_path.to_string_lossy().as_bytes(),
        )?;

        info!(id = %id, pid, "restored Firecracker VM from snapshot");
        Ok(ProcessHandle {
            id: id.to_string(),
            pid: Some(pid),
            internal_ip: None,
            host_ports: vec![],
            gateway: None,
            backend: "vm".to_string(),
            bridge_name: None,
            network_join: None,
        })
    }

    async fn restore_rootfs(
        &self,
        id: &str,
        rootfs_path: &std::path::Path,
        drive_path: &std::path::Path,
        kernel_path: &std::path::Path,
        vcpus: u8,
        mem_mib: u32,
    ) -> Result<ProcessHandle, ExecError> {
        let vm_dir = self.vm_dir(id);
        std::fs::create_dir_all(&vm_dir)?;
        if let Some(parent) = drive_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        copy_image_file(rootfs_path, drive_path).map_err(|e| {
            ExecError::ExecutionFailed(format!(
                "restore rootfs {} -> {}: {e}",
                rootfs_path.display(),
                drive_path.display()
            ))
        })?;
        let config = firecracker_config(id, kernel_path, drive_path, None, vcpus, mem_mib);
        let config_path = vm_dir.join("vm-config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec_pretty(&config)
                .map_err(|e| ExecError::ExecutionFailed(format!("serialize config: {e}")))?,
        )?;
        let console_path = vm_dir.join("console.log");
        let console_file = std::fs::File::create(&console_path)?;
        let child = Command::new(&self.config.firecracker_path)
            .arg("--api-sock")
            .arg(vm_dir.join("firecracker.sock"))
            .arg("--config-file")
            .arg(&config_path)
            .stdout(console_file.try_clone()?)
            .stderr(console_file)
            .spawn()?;
        let pid = child.id().unwrap_or(0);
        std::fs::write(vm_dir.join("firecracker.pid"), pid.to_string())?;
        let sidecar = VmSidecar {
            vm_net: VmNetwork {
                tap_name: String::new(),
                bridge_name: String::new(),
                guest_ip: Ipv4Addr::LOCALHOST,
                guest_mac: String::new(),
                netmask: Ipv4Addr::UNSPECIFIED,
                gateway: Ipv4Addr::UNSPECIFIED,
            },
            endpoint: NetworkEndpoint {
                internal_ip: String::new(),
                host_port_mappings: vec![],
                namespace_path: None,
            },
            tap_name: String::new(),
            network_disabled: true,
        };
        std::fs::write(
            vm_dir.join("network.json"),
            serde_json::to_vec_pretty(&sidecar)
                .map_err(|e| ExecError::ExecutionFailed(format!("serialize sidecar: {e}")))?,
        )?;
        std::fs::write(
            vm_dir.join("drive_path"),
            drive_path.to_string_lossy().as_bytes(),
        )?;
        std::mem::forget(child);
        info!(id = %id, pid, "cold-booted VM from ROOTFS snapshot");
        Ok(ProcessHandle {
            id: id.to_string(),
            pid: Some(pid),
            internal_ip: None,
            host_ports: vec![],
            gateway: None,
            backend: "vm".to_string(),
            bridge_name: None,
            network_join: None,
        })
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

#[cfg(all(test, target_os = "linux"))]
mod ext4_base_cache_tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    fn have_mkfs() -> bool {
        std::process::Command::new("mkfs.ext4")
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn ext4_base_is_built_once_and_cloned() {
        if !have_mkfs() {
            eprintln!("skipping: mkfs.ext4 not available");
            return;
        }
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("etc")).unwrap();
        std::fs::write(src.path().join("etc/passwd"), b"root:x:0:0\n").unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MmapStore::new(store_dir.path().to_path_buf()));
        let built = pullrun_oci::build_dag_from_directory(&store, src.path())
            .await
            .expect("build DAG from dir");

        let vm_root = tempfile::tempdir().unwrap();
        let proxy = Arc::new(ProxyNetwork::new().unwrap());
        let mut cfg = FirecrackerConfig::new(
            PathBuf::from("/nonexistent/firecracker"),
            PathBuf::from("/nonexistent/vmlinux"),
            vm_root.path().to_path_buf(),
        );
        cfg.size_mb = 32;
        let exec = FirecrackerExecutor::new(cfg, store, proxy.ipam_handle(), proxy);

        let base_a = exec.ensure_ext4_base(&built.manifest_digest).await.unwrap();
        let base_b = exec.ensure_ext4_base(&built.manifest_digest).await.unwrap();
        assert_eq!(base_a, base_b, "base must be built once per digest");
        assert!(base_a.is_file());

        let vm_a = exec
            .provision_vm_rootfs(&built.manifest_digest, "vm-a", &[])
            .await
            .unwrap();
        let vm_b = exec
            .provision_vm_rootfs(&built.manifest_digest, "vm-b", &[])
            .await
            .unwrap();
        assert_ne!(vm_a, vm_b);

        // Bytes at offset 0 (the ext4 superblock area) must be
        // independent: writing to one clone cannot change the base or
        // the other clone.
        let read4 = |p: &std::path::Path| -> Vec<u8> {
            let mut f = std::fs::File::open(p).unwrap();
            let mut buf = [0u8; 8];
            f.seek(SeekFrom::Start(0)).unwrap();
            let _ = f.read(&mut buf);
            buf.to_vec()
        };
        let base_before = read4(&base_a);
        let b_before = read4(&vm_b);
        {
            let mut f = std::fs::OpenOptions::new().write(true).open(&vm_a).unwrap();
            f.seek(SeekFrom::Start(0)).unwrap();
            f.write_all(b"COWPROBE").unwrap();
        }
        assert_eq!(read4(&base_a), base_before, "base image was modified");
        assert_eq!(read4(&vm_b), b_before, "sibling clone was modified");
    }
}
