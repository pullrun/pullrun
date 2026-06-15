use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::{info, warn};

use pullrun_exec::ExecError;
use pullrun_net::{Ipam, NetworkEndpoint, ProxyNetwork};
use pullrun_store::MmapStore;

use crate::network::{create_tap, VmNetwork};
use crate::{FirecrackerConfig, VmSidecar};

const DEFAULT_POOL_SIZE: usize = 4;
const POOL_ROOTFS_SIZE_MB: u64 = 32;
const POOL_ROOTFS_FILENAME: &str = "pool-rootfs.ext4";
const REFILL_INTERVAL_SECS: u64 = 5;
const HEALTH_INTERVAL_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct VmPoolConfig {
    pub pool_size: usize,
    pub vm_root: PathBuf,
}

impl Default for VmPoolConfig {
    fn default() -> Self {
        Self {
            pool_size: DEFAULT_POOL_SIZE,
            vm_root: PathBuf::from("/var/lib/pullrun/vm-pool"),
        }
    }
}

struct PoolEntry {
    id: String,
    api_socket: PathBuf,
    vm_dir: PathBuf,
    guest_ip: Ipv4Addr,
    tap_name: String,
    vm_net: VmNetwork,
    endpoint: NetworkEndpoint,
    pid: u32,
    #[allow(dead_code)]
    tap_fd: Option<std::fs::File>,
    _born_at: Instant,
}

pub struct VmPool {
    config: VmPoolConfig,
    fc_config: FirecrackerConfig,
    ipam: Arc<Ipam>,
    proxy: Arc<ProxyNetwork>,
    pool: Mutex<VecDeque<PoolEntry>>,
    #[allow(dead_code)]
    pool_rootfs_path: PathBuf,
}

impl VmPool {
    pub fn new(
        config: VmPoolConfig,
        fc_config: FirecrackerConfig,
        _store: Arc<MmapStore>,
        ipam: Arc<Ipam>,
        proxy: Arc<ProxyNetwork>,
    ) -> Arc<Self> {
        let pool_rootfs_path = config.vm_root.join(POOL_ROOTFS_FILENAME);
        let _ = std::fs::create_dir_all(&config.vm_root);

        let pool = Self {
            config: config.clone(),
            fc_config,
            ipam,
            proxy,
            pool: Mutex::new(VecDeque::new()),
            pool_rootfs_path,
        };

        // Trigger initial fill eagerly
        let this = Arc::new(pool);
        let refill_this = this.clone();
        tokio::spawn(async move {
            refill_this.refill_loop().await;
        });
        let health_this = this.clone();
        tokio::spawn(async move {
            health_this.health_loop().await;
        });
        let init_this = this.clone();
        tokio::spawn(async move {
            init_this.refill_pool().await;
        });

        // Unwrap to return inner value (Arc unwrap only works when Arc has
        // strong_count 1, but we just created it so we're fine).
        // Actually, we can't unwrap Arc since it's now shared. Return the Arc.
        // The caller gets Arc<VmPool>.
        this
    }

    /// Acquire a pre-booted VM from the pool. Returns `None` if the pool
    /// is empty (caller should fall back to cold boot).
    pub fn acquire(&self) -> Option<PooledVm> {
        let entry = {
            let mut pool = self.pool.lock().expect("pool lock poisoned");
            pool.pop_front()
        };
        match entry {
            Some(entry) => {
                info!(
                    pool_id = %entry.id,
                    ip = %entry.guest_ip,
                    "acquired warm VM from pool"
                );
                Some(PooledVm { entry: Some(entry) })
            }
            None => None,
        }
    }

    async fn refill_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(REFILL_INTERVAL_SECS)).await;
            self.refill_pool().await;
        }
    }

    async fn refill_pool(&self) {
        let current_size = {
            let pool = self.pool.lock().expect("pool lock poisoned");
            pool.len()
        };
        let needed = self.config.pool_size.saturating_sub(current_size);
        if needed == 0 {
            return;
        }
        info!(
            current = current_size,
            target = self.config.pool_size,
            fill = needed,
            "refilling warm VM pool"
        );
        for _ in 0..needed {
            match self.boot_pool_vm().await {
                Ok(entry) => {
                    let mut pool = self.pool.lock().expect("pool lock poisoned");
                    pool.push_back(entry);
                }
                Err(e) => {
                    warn!(error = %e, "failed to boot pool VM");
                    break;
                }
            }
        }
    }

    async fn boot_pool_vm(&self) -> Result<PoolEntry, ExecError> {
        let id = format!("pool-{}", uuid_v4());

        let vm_dir = self.config.vm_root.join(&id);
        std::fs::create_dir_all(&vm_dir)?;

        let api_socket = vm_dir.join("firecracker.sock");
        let ext4_path = vm_dir.join(POOL_ROOTFS_FILENAME);

        create_pool_rootfs(&ext4_path, POOL_ROOTFS_SIZE_MB)
            .map_err(|e| ExecError::ExecutionFailed(format!("create pool rootfs: {e}")))?;

        let tap_name = format!("tap-{}", &id[..8]);
        let ip_u32 = self
            .ipam
            .allocate()
            .ok_or_else(|| ExecError::ExecutionFailed("no IPs available in shared IPAM".into()))?;
        let guest_ip = Ipv4Addr::from(ip_u32);

        let (vm_net, tap_fd) = create_tap(&tap_name, guest_ip)
            .map_err(|e| ExecError::ExecutionFailed(format!("tap setup: {e}")))?;

        // Hold the TAP fd in the PoolEntry so the device stays alive.

        let endpoint = self
            .proxy
            .register_endpoint(&id, guest_ip.to_string(), &[])
            .await
            .map_err(|e| ExecError::ExecutionFailed(format!("proxy register: {e}")))?;

        let sidecar = VmSidecar {
            vm_net: vm_net.clone(),
            endpoint: endpoint.clone(),
            tap_name: tap_name.clone(),
        };
        let sidecar_path = vm_dir.join("network.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar)
            .map_err(|e| ExecError::ExecutionFailed(format!("serialize sidecar: {e}")))?;
        std::fs::write(&sidecar_path, sidecar_json)?;

        let config = crate::ext4::firecracker_config(
            &id,
            &self.fc_config.kernel_path,
            &ext4_path,
            Some(&vm_net),
            self.fc_config.vcpus,
            self.fc_config.mem_mib,
        );
        let config_path = vm_dir.join("vm-config.json");
        std::fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config serialization never fails"),
        )?;

        let log_path = vm_dir.join("firecracker.log");
        if !log_path.exists() {
            std::fs::write(&log_path, b"")?;
        }
        let console_path = vm_dir.join("console.log");
        let console_file = std::fs::File::create(&console_path)
            .map_err(|e| ExecError::ExecutionFailed(format!("create console.log: {e}")))?;

        let child = tokio::process::Command::new(&self.fc_config.firecracker_path)
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
            .spawn()
            .map_err(|e| ExecError::ExecutionFailed(format!("spawn firecracker: {e}")))?;

        let pid = child.id().unwrap_or(0);
        let pid_path = vm_dir.join("firecracker.pid");
        std::fs::write(&pid_path, pid.to_string())?;
        std::mem::forget(child);

        info!(pool_id = %id, ip = %guest_ip, tap = %tap_name, pid = pid, "pool VM booted");

        Ok(PoolEntry {
            id,
            api_socket,
            vm_dir,
            guest_ip,
            tap_name,
            vm_net,
            endpoint,
            pid,
            tap_fd: Some(tap_fd),
            _born_at: Instant::now(),
        })
    }

    async fn health_loop(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(HEALTH_INTERVAL_SECS)).await;
            self.check_health();
        }
    }

    fn check_health(&self) {
        let mut pool = self.pool.lock().expect("pool lock poisoned");
        let mut healthy = VecDeque::new();
        while let Some(entry) = pool.pop_front() {
            if is_firecracker_alive(entry.pid) {
                healthy.push_back(entry);
            } else {
                warn!(pool_id = %entry.id, pid = entry.pid, "pool VM dead, removing");
                let _ = std::fs::remove_dir_all(&entry.vm_dir);
            }
        }
        *pool = healthy;
    }
}

/// A VM acquired from the warm pool. The caller swaps the rootfs via
/// the Firecracker API, then reboots.
pub struct PooledVm {
    entry: Option<PoolEntry>,
}

impl PooledVm {
    pub fn swap_rootfs(&self, new_ext4_path: &Path) -> Result<(), ExecError> {
        let entry = self
            .entry
            .as_ref()
            .ok_or_else(|| ExecError::ExecutionFailed("pool entry already consumed".into()))?;

        let body = serde_json::json!({
            "drive_id": "rootfs",
            "path_on_host": new_ext4_path.to_string_lossy(),
            "is_root_device": true,
            "is_read_only": false,
        });

        firecracker_api_put(
            &entry.api_socket,
            "/drives/rootfs",
            &serde_json::to_string(&body).unwrap(),
        )
        .map_err(|e| ExecError::ExecutionFailed(format!("rootfs hot-swap: {e}")))?;

        info!(
            pool_id = %entry.id,
            new_rootfs = %new_ext4_path.display(),
            "rootfs hot-swapped"
        );
        Ok(())
    }

    pub fn reboot(&self) -> Result<(), ExecError> {
        let entry = self
            .entry
            .as_ref()
            .ok_or_else(|| ExecError::ExecutionFailed("pool entry already consumed".into()))?;

        let body = serde_json::json!({
            "action_type": "SendCtrlAltDel",
        });
        firecracker_api_put(
            &entry.api_socket,
            "/actions",
            &serde_json::to_string(&body).unwrap(),
        )
        .map_err(|e| ExecError::ExecutionFailed(format!("reboot: {e}")))?;

        info!(pool_id = %entry.id, "CtrlAltDel sent, VM rebooting");
        std::thread::sleep(Duration::from_millis(200));
        Ok(())
    }

    pub fn guest_ip(&self) -> Ipv4Addr {
        self.entry
            .as_ref()
            .map(|e| e.guest_ip)
            .unwrap_or(Ipv4Addr::UNSPECIFIED)
    }

    pub fn tap_name(&self) -> &str {
        self.entry
            .as_ref()
            .map(|e| e.tap_name.as_str())
            .unwrap_or("")
    }

    pub fn vm_net(&self) -> &VmNetwork {
        &self.entry.as_ref().expect("pool entry consumed").vm_net
    }

    pub fn endpoint(&self) -> &NetworkEndpoint {
        &self.entry.as_ref().expect("pool entry consumed").endpoint
    }

    pub fn pool_id(&self) -> &str {
        &self.entry.as_ref().expect("pool entry consumed").id
    }

    pub fn api_socket(&self) -> &Path {
        &self.entry.as_ref().expect("pool entry consumed").api_socket
    }

    pub fn vm_dir(&self) -> &Path {
        &self.entry.as_ref().expect("pool entry consumed").vm_dir
    }
}

fn is_firecracker_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn firecracker_api_put(socket_path: &Path, api_path: &str, body: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("connect to {}: {e}", socket_path.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");

    let request = format!(
        "PUT {api_path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         \r\n\
         {body}",
        body.len()
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("write request: {e}"))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("read response: {e}"))?;

    let response_str =
        String::from_utf8(response).map_err(|e| format!("invalid UTF-8 response: {e}"))?;

    if !response_str.starts_with("HTTP/1.1 2") {
        let status_line = response_str.lines().next().unwrap_or("unknown");
        return Err(format!("FC API returned {status_line}"));
    }
    Ok(())
}

fn create_pool_rootfs(path: &Path, size_mb: u64) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
    }

    let dir = tempfile::TempDir::new().map_err(|e| format!("create temp dir: {e}"))?;

    let init_content = "#!/bin/sh\nwhile true; do sleep 3600; done\n";
    let init_path = dir.path().join("init");
    std::fs::write(&init_path, init_content).map_err(|e| format!("write init: {e}"))?;
    std::fs::set_permissions(&init_path, PermissionsExt::from_mode(0o755))
        .map_err(|e| format!("chmod init: {e}"))?;

    let etc_dir = dir.path().join("etc");
    std::fs::create_dir_all(&etc_dir).map_err(|e| format!("create /etc: {e}"))?;
    std::fs::write(
        etc_dir.join("os-release"),
        b"NAME=\"Pullrun Pool\"\nID=pullrun-pool\nVERSION_ID=0\n",
    )
    .map_err(|e| format!("write os-release: {e}"))?;

    let f = std::fs::File::create(path).map_err(|e| format!("create file: {e}"))?;
    f.set_len(size_mb * 1024 * 1024)
        .map_err(|e| format!("truncate: {e}"))?;
    drop(f);

    let output = std::process::Command::new("mkfs.ext4")
        .args(["-F", "-q", "-d"])
        .arg(dir.path())
        .arg(path)
        .output()
        .map_err(|e| format!("mkfs.ext4: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("mkfs.ext4 failed: {stderr}"));
    }

    info!(path = %path.display(), size_mb, "pool rootfs ext4 created");
    Ok(())
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.as_nanos();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (nanos >> 32) as u32,
        (nanos >> 16) as u16,
        (nanos & 0xfff) as u16,
        (nanos >> 48) as u16,
        (nanos >> 64) as u64,
    )
}
