// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, info, warn};

use pullrun_net::{Direction, IpRange, Ipam, NetworkManager, NetworkRule, ProxyNetwork};
use pullrun_oci::{bundle_from_manifest, prepare_bundle_from_base, BaseStore, OciMaterializer};
use pullrun_store::MmapStore;

use crate::overlay::{mount_overlay, unmount_overlay};
use crate::types::{
    ExecError, ExecOutput, Executor, ExitStatus, ProcessHandle, RootfsMode, WorkloadSpec,
    WorkloadStats,
};

const DEFAULT_BRIDGE_NAME: &str = "pullrun-br0";
const DEFAULT_GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 1);

pub struct LinuxContainerExecutor {
    store: MmapStore,
    runc_path: PathBuf,
    bundle_root: PathBuf,
    ipam: Option<Arc<Ipam>>,
    proxy: Option<Arc<ProxyNetwork>>,
    /// Immutable per-digest bases; when set, container rootfs comes
    /// from a CoW clone instead of a fresh per-workload extraction.
    base_store: Option<Arc<BaseStore>>,
    /// Foreground `runc run` children, keyed by workload id, so
    /// `status()` can report the workload's real exit code.
    children: Mutex<HashMap<String, std::process::Child>>,
    /// Rootfs preparation strategy for containers.
    rootfs_mode: RootfsMode,
    /// Workloads whose rootfs is an overlay mount (for unmount on stop).
    overlay_mounts: Mutex<HashSet<String>>,
    /// Fail-closed egress configs for workloads that requested it.
    egress: Mutex<HashMap<String, EgressConfig>>,
}

#[derive(Clone, Debug, Default)]
struct EgressConfig {
    /// (host-or-ip, port, protocol)
    allow: Vec<(String, u16, String)>,
    /// DNS servers the workload may reach on port 53.
    nameservers: Vec<std::net::IpAddr>,
}

impl LinuxContainerExecutor {
    pub fn new(store: MmapStore, runc_path: Option<PathBuf>, bundle_root: PathBuf) -> Self {
        let runc_path = runc_path.unwrap_or_else(|| PathBuf::from("runc"));
        std::fs::create_dir_all(&bundle_root).ok();
        Self {
            store,
            runc_path,
            bundle_root,
            ipam: None,
            proxy: None,
            base_store: None,
            children: Mutex::new(HashMap::new()),
            rootfs_mode: RootfsMode::Clone,
            overlay_mounts: Mutex::new(HashSet::new()),
            egress: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_base_store(mut self, base_store: Arc<BaseStore>) -> Self {
        self.base_store = Some(base_store);
        self
    }

    pub fn with_rootfs_mode(mut self, mode: RootfsMode) -> Self {
        self.rootfs_mode = mode;
        self
    }

    pub fn with_network(mut self, ipam: Arc<Ipam>, proxy: Arc<ProxyNetwork>) -> Self {
        self.ipam = Some(ipam);
        self.proxy = Some(proxy);
        self
    }

    fn bundle_dir(&self, id: &str) -> PathBuf {
        self.bundle_root.join(id)
    }

    fn should_setup_bridge(&self, spec: &WorkloadSpec) -> bool {
        // Only the workload that anchors a network namespace (Bridge
        // mode) gets a veth + IP. Joined containers (NetworkMode::Container)
        // share the anchor's netns and must NOT get their own veth pair —
        // the peer name `eth0` would collide with the sandbox's interface.
        matches!(spec.network_mode, crate::types::NetworkMode::Bridge)
    }

    fn ensure_bridge_exists(bridge_name: &str, subnet: Option<&IpRange>) -> Result<(), ExecError> {
        use std::process::Command;
        // Check that `ip` exists before trying to use it (macOS doesn't have it).
        if Command::new("ip").arg("--version").output().is_err() {
            return Err(ExecError::ExecutionFailed(
                "'ip' command not found — bridge networking requires iproute2 (Linux only)".into(),
            ));
        }
        // ip link show exits 0 even when the device doesn't exist (it
        // writes "does not exist" to stderr and returns 0).  So we
        // try to add the bridge and ignore "File exists".
        let status = Command::new("ip")
            .args(["link", "add", bridge_name, "type", "bridge"])
            .status()
            .map_err(|e| {
                ExecError::ExecutionFailed(format!("ip link add bridge {bridge_name}: {e}"))
            })?;
        if status.success() {
            info!(bridge = bridge_name, "created bridge");
        }
        Command::new("ip")
            .args(["link", "set", bridge_name, "up"])
            .status()
            .map_err(|e| {
                ExecError::ExecutionFailed(format!("ip link set {bridge_name} up: {e}"))
            })?;
        // Assign the gateway IP to the bridge so the host kernel has a
        // route to the container subnet. Per-pod bridges get their own
        // /24 with gateway .1; the default bridge keeps the flat /16.
        // Must run regardless of whether the bridge already existed.
        let (gateway_ip, prefix) = match subnet {
            Some(s) => (Ipv4Addr::from(s.base + 1).to_string(), 24),
            None => (DEFAULT_GATEWAY.to_string(), 16),
        };
        // Ignore error — the address may already be assigned.
        let _ = Command::new("ip")
            .args([
                "addr",
                "add",
                &format!("{gateway_ip}/{prefix}"),
                "dev",
                bridge_name,
            ])
            .status();
        // Per-pod bridges need outbound NAT: without it the pod subnet
        // can reach its gateway but nothing beyond the host. Best-effort
        // (iptables may be absent on locked-down hosts; the bridge still
        // works for pod-internal traffic).
        if let Some(s) = subnet {
            Self::ensure_nat_for_subnet(s);
        } else {
            Self::ensure_nat_cidr("10.42.0.0/16");
        }
        Ok(())
    }

    /// Install a MASQUERADE rule for the pod subnet (idempotent) and
    /// enable IPv4 forwarding. Best-effort: failures are logged, not
    /// fatal — pod-internal traffic works without NAT.
    fn ensure_nat_for_subnet(subnet: &IpRange) {
        Self::ensure_nat_cidr(&format!("{}/24", Ipv4Addr::from(subnet.base)));
    }

    fn ensure_nat_cidr(cidr: &str) {
        use std::process::Command;
        if Command::new("iptables").arg("--version").output().is_err() {
            warn!("iptables not found; subnet {cidr} has no outbound NAT");
            return;
        }
        let rule = [
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-s",
            cidr,
            "-j",
            "MASQUERADE",
        ];
        let check = Command::new("iptables").args(rule).status();
        let present = check.map(|s| s.success()).unwrap_or(false);
        if !present {
            let mut add = Vec::with_capacity(rule.len() + 2);
            add.extend_from_slice(&[
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-s",
                cidr,
                "-j",
                "MASQUERADE",
            ]);
            let _ = Command::new("iptables").args(add).status();
        }
        let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1");
    }

    async fn setup_container_network(
        &self,
        id: &str,
        ip: Ipv4Addr,
        host_ports: &[(u16, u16)],
        bridge: &str,
        gateway: Ipv4Addr,
    ) -> Result<(), ExecError> {
        use std::process::Command as SyncCommand;

        // Get container PID from runc state
        let output = Command::new(&self.runc_path)
            .args(["state", id])
            .output()
            .await?;
        if !output.status.success() {
            return Err(ExecError::ExecutionFailed(
                "runc state failed after start".into(),
            ));
        }
        let state: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ExecError::ExecutionFailed(format!("parse runc state: {e}")))?;
        let pid = state["pid"]
            .as_i64()
            .ok_or_else(|| ExecError::ExecutionFailed("no pid in runc state".into()))?;

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        let hash_suffix = format!("{:x}", hasher.finish());
        let veth_host = format!("v{}", &hash_suffix[..8.min(hash_suffix.len())]);

        info!(id = %id, veth = veth_host, bridge = bridge, container_ip = %ip, pid = pid, "setting up container bridge network");

        // Create veth pair with one end in the container's netns. The
        // name is derived from the workload id, so a leftover from a
        // previous incarnation of the same id (e.g. a pod re-created
        // with the same uid) would make this fail with "File exists";
        // delete it first, ignoring errors (nothing to delete).
        let _ = SyncCommand::new("ip")
            .args(["link", "del", &veth_host])
            .status();
        let out = SyncCommand::new("ip")
            .args([
                "link",
                "add",
                &veth_host,
                "type",
                "veth",
                "peer",
                "name",
                "eth0",
                "netns",
                &pid.to_string(),
            ])
            .output()
            .map_err(|e| ExecError::ExecutionFailed(format!("ip link add veth: {e}")))?;
        if !out.status.success() {
            return Err(ExecError::ExecutionFailed(format!(
                "ip link add veth pair failed: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }

        // Attach host end to bridge
        SyncCommand::new("ip")
            .args(["link", "set", &veth_host, "master", bridge])
            .status()?;
        SyncCommand::new("ip")
            .args(["link", "set", &veth_host, "up"])
            .status()?;

        // Configure IP inside the container
        let cidr = format!("{}/16", ip);
        SyncCommand::new("nsenter")
            .args([
                "-t",
                &pid.to_string(),
                "-n",
                "--",
                "ip",
                "addr",
                "add",
                &cidr,
                "dev",
                "eth0",
            ])
            .status()?;
        SyncCommand::new("nsenter")
            .args([
                "-t",
                &pid.to_string(),
                "-n",
                "--",
                "ip",
                "link",
                "set",
                "eth0",
                "up",
            ])
            .status()?;
        SyncCommand::new("nsenter")
            .args([
                "-t",
                &pid.to_string(),
                "-n",
                "--",
                "ip",
                "route",
                "add",
                "default",
                "via",
                &gateway.to_string(),
            ])
            .status()?;

        // Register with proxy for port forwarding
        if let Some(ref proxy) = self.proxy {
            if !host_ports.is_empty() {
                let rules: Vec<NetworkRule> = host_ports
                    .iter()
                    .map(|&(host_port, container_port)| {
                        if host_port != container_port {
                            NetworkRule::inbound_mapped(host_port, container_port)
                        } else {
                            NetworkRule::inbound(host_port)
                        }
                    })
                    .collect();
                proxy
                    .register_endpoint(id, ip.to_string(), &rules)
                    .await
                    .map_err(|e| ExecError::ExecutionFailed(format!("proxy register: {e}")))?;
            }
        }

        Ok(())
    }

    async fn check_runc(&self) -> Result<(), ExecError> {
        let output = Command::new(&self.runc_path)
            .arg("--version")
            .output()
            .await
            .map_err(|e| {
                ExecError::BackendNotAvailable(format!(
                    "runc not found at {} — container backend unavailable. \
                     Install runc: https://github.com/opencontainers/runc/releases ({e})",
                    self.runc_path.display()
                ))
            })?;

        if !output.status.success() {
            return Err(ExecError::BackendNotAvailable(
                "runc returned a non-zero exit code — is it a valid runc binary?".into(),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        debug!(runc_version = %stdout.trim(), "runc found");
        Ok(())
    }
    /// Clone the prebuilt base (or fall back to per-workload extraction).
    async fn clone_or_materialize(
        &self,
        spec: &WorkloadSpec,
        bundle_dir: &std::path::Path,
    ) -> Result<pullrun_oci::MaterializedBundle, ExecError> {
        if let Some(base_store) = &self.base_store {
            let digest = spec.image_root;
            let base = base_store
                .ensure_base(&digest)
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("prepare image base: {e}")))?;
            let (bundle, stats) = prepare_bundle_from_base(&self.store, &base, &digest, bundle_dir)
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("clone image base: {e}")))?;
            debug!(
                id = %spec.id,
                cloned = stats.cloned,
                copied = stats.copied,
                "container rootfs cloned from base"
            );
            Ok(bundle)
        } else {
            let materializer = OciMaterializer::new(&self.store);
            Ok(materializer.materialize_bundle(&spec.image_root, bundle_dir)?)
        }
    }

    /// base (read-only lower) + per-workload upper/work/merged.
    async fn create_overlay_bundle(
        &self,
        spec: &WorkloadSpec,
        bundle_dir: &std::path::Path,
    ) -> Result<pullrun_oci::MaterializedBundle, ExecError> {
        let base_store = self.base_store.as_ref().ok_or_else(|| {
            ExecError::ExecutionFailed("overlay rootfs needs a base store".into())
        })?;
        let digest = spec.image_root;
        let base = base_store
            .ensure_base(&digest)
            .await
            .map_err(|e| ExecError::ExecutionFailed(format!("prepare image base: {e}")))?;

        std::fs::create_dir_all(bundle_dir)?;
        let upper = bundle_dir.join("upper");
        let work = bundle_dir.join("work");
        let merged = bundle_dir.join("merged");
        mount_overlay(&base.path, &upper, &work, &merged).map_err(|e| {
            ExecError::ExecutionFailed(format!("mount overlay ({}): {e}", merged.display()))
        })?;

        match bundle_from_manifest(&self.store, &digest, bundle_dir, &merged) {
            Ok(bundle) => {
                self.overlay_mounts
                    .lock()
                    .expect("overlay_mounts lock poisoned")
                    .insert(spec.id.clone());
                Ok(bundle)
            }
            Err(e) => {
                let _ = unmount_overlay(&merged);
                Err(ExecError::ExecutionFailed(format!("overlay manifest: {e}")))
            }
        }
    }
}

#[async_trait]
impl Executor for LinuxContainerExecutor {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        self.check_runc().await.map_err(|e| {
            tracing::error!("check_runc failed: {e}");
            e
        })?;

        if spec.egress_deny && matches!(spec.network_mode, crate::types::NetworkMode::Host) {
            return Err(ExecError::ExecutionFailed(
                "fail-closed egress is not supported with --net host".into(),
            ));
        }
        {
            let mut egress = self.egress.lock().expect("egress lock poisoned");
            if spec.egress_deny {
                let allow = spec
                    .network_rules
                    .iter()
                    .filter(|r| matches!(r.direction, pullrun_net::Direction::Outbound))
                    .filter_map(|r| {
                        r.to_host.clone().map(|h| {
                            (
                                h,
                                r.port,
                                match r.protocol {
                                    pullrun_net::Protocol::Udp => "udp".to_string(),
                                    _ => "tcp".to_string(),
                                },
                            )
                        })
                    })
                    .collect();
                let nameservers = spec
                    .dns
                    .nameservers
                    .iter()
                    .filter_map(|n| n.parse().ok())
                    .collect();
                egress.insert(spec.id.clone(), EgressConfig { allow, nameservers });
            } else {
                egress.remove(&spec.id);
            }
        }

        info!(id = %spec.id, image_root = %spec.image_root, "creating container");

        let bundle_dir = self.bundle_dir(&spec.id);
        if bundle_dir.exists() {
            std::fs::remove_dir_all(&bundle_dir)?;
        }

        // overlayfs needs CAP_SYS_ADMIN and a writable rootfs.
        let overlay_capable = self.rootfs_mode == RootfsMode::Overlay
            && self.base_store.is_some()
            && !spec.readonly_rootfs
            && unsafe { libc::geteuid() } == 0;

        let bundle = if overlay_capable {
            match self.create_overlay_bundle(&spec, &bundle_dir).await {
                Ok(bundle) => {
                    info!(id = %spec.id, "container rootfs uses overlay lowerdir");
                    bundle
                }
                Err(e) => {
                    warn!(
                        id = %spec.id,
                        error = %e,
                        "overlay rootfs failed; falling back to a clone"
                    );
                    let _ = std::fs::remove_dir_all(&bundle_dir);
                    self.clone_or_materialize(&spec, &bundle_dir).await?
                }
            }
        } else if self.rootfs_mode == RootfsMode::Overlay && unsafe { libc::geteuid() } != 0 {
            warn!(
                id = %spec.id,
                "overlay rootfs requested but the runtime is not root; using a clone"
            );
            self.clone_or_materialize(&spec, &bundle_dir).await?
        } else {
            self.clone_or_materialize(&spec, &bundle_dir).await?
        };

        // Allocate IP for bridge networking before spec.env is consumed
        let mut internal_ip: Option<Ipv4Addr> = None;
        let mut gateway: Option<Ipv4Addr> = None;
        if self.should_setup_bridge(&spec) {
            let bridge_name = spec.bridge_name.as_deref().unwrap_or(DEFAULT_BRIDGE_NAME);
            // Per-pod bridges get their own /24 subnet (gateway .1);
            // the default bridge keeps the flat /16 (gateway 10.42.0.1).
            let subnet = if bridge_name == DEFAULT_BRIDGE_NAME {
                None
            } else {
                self.ipam
                    .as_ref()
                    .and_then(|ipam| ipam.subnet_for(bridge_name))
            };
            Self::ensure_bridge_exists(bridge_name, subnet.as_ref())?;
            if let Some(ref ipam) = self.ipam {
                let ip = match &subnet {
                    Some(s) => ipam.allocate_in(s),
                    None => ipam.allocate(),
                };
                if let Some(ip) = ip {
                    internal_ip = Some(Ipv4Addr::from(ip));
                    gateway = Some(match &subnet {
                        Some(s) => Ipv4Addr::from(s.base + 1),
                        None => DEFAULT_GATEWAY,
                    });
                    let ip = internal_ip.expect("just allocated above");
                    info!(id = %spec.id, bridge = bridge_name, ip = %ip, "allocated bridge IP");
                }
            }
        }

        crate::oci_spec::write_oci_config(&spec, &bundle, false)?;

        // Write /etc/hosts and /etc/resolv.conf into the rootfs so
        // entrypoint scripts that expect these files don't fail.
        let etc_dir = bundle.rootfs_path.join("etc");
        let _ = std::fs::create_dir_all(&etc_dir);
        let hosts_path = etc_dir.join("hosts");
        if !hosts_path.exists() {
            std::fs::write(
                &hosts_path,
                "127.0.0.1 localhost\n::1 localhost ip6-localhost\n",
            )
            .ok();
        }
        let resolv_path = etc_dir.join("resolv.conf");
        if !resolv_path.exists() {
            std::fs::write(&resolv_path, "nameserver 8.8.8.8\nnameserver 1.1.1.1\n").ok();
        }
        if !spec.dns.nameservers.is_empty() || !spec.dns.searches.is_empty() {
            // Explicit DNS config: (re)write resolv.conf with the
            // requested servers/search domains/options. The pod model
            // relies on this so containers and their sandbox resolve
            // the cluster DNS service.
            let mut content = String::new();
            for ns in &spec.dns.nameservers {
                content.push_str(&format!("nameserver {ns}\n"));
            }
            if !spec.dns.searches.is_empty() {
                content.push_str(&format!("search {}\n", spec.dns.searches.join(" ")));
            }
            for o in &spec.dns.options {
                content.push_str(&format!("options {o}\n"));
            }
            std::fs::write(&resolv_path, content).ok();
        }

        let bridge_name = spec.bridge_name.clone();
        let network_join = match &spec.network_mode {
            crate::types::NetworkMode::Container(target) => Some(target.clone()),
            _ => None,
        };
        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: internal_ip.map(|ip| ip.to_string()),
            gateway: gateway.map(|gw| gw.to_string()),
            host_ports: if network_join.is_some() {
                // Port forwarding belongs to the sandbox's network
                // namespace; a joined container shares it and must not
                // register its own listeners.
                Vec::new()
            } else {
                spec.network_rules
                    .iter()
                    .filter(|r| matches!(r.direction, Direction::Inbound))
                    .map(|r| {
                        let host_p = if r.host_port != 0 {
                            r.host_port
                        } else {
                            r.port
                        };
                        (host_p, r.port)
                    })
                    .collect()
            },
            backend: "container".to_string(),
            bridge_name,
            network_join,
        };

        info!(id = %spec.id, "container created");
        Ok(handle)
    }

    async fn start(&self, handle: &mut ProcessHandle) -> Result<(), ExecError> {
        use std::process::Command as SyncCommand;

        info!(id = %handle.id, "starting container");

        let bundle_dir = self.bundle_dir(&handle.id);
        let runc_path = self.runc_path.clone();
        let id = handle.id.clone();
        let bdir = bundle_dir.clone();
        let network_join = handle.network_join.clone();

        // Pod model: resolve the sandbox's netns and patch the OCI spec
        // so runc joins it instead of creating a fresh namespace.
        if let Some(ref target) = network_join {
            let netns = netns_path_of(&runc_path, None, target)?;
            join_netns_in_config(&bdir, &netns)?;
            tracing::debug!(id = %id, target = %target, netns = %netns, "joined network namespace");
        }

        tracing::debug!(
            "start: runc_path={:?}, bundle_dir={:?}, id={}",
            runc_path,
            bdir,
            id
        );

        // Clone for the failure-cleanup path below (the runc spawn
        // closure moves the originals).
        let runc_path_for_cleanup = runc_path.clone();
        let id_for_cleanup = id.clone();

        // Remove stale runc state from a previous workload with the
        // same id (e.g. after a daemon crash).
        let _ = SyncCommand::new(&runc_path_for_cleanup)
            .args(["delete", "--force", &id_for_cleanup])
            .output();

        let stderr_path = bdir.join("runc.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).ok();
        let stderr_for_cmd = stderr_file.as_ref().and_then(|f| f.try_clone().ok());
        let mut child = tokio::task::spawn_blocking(move || {
            tracing::debug!("spawning foreground runc run -b {:?} {}", bdir, id);
            let mut cmd = SyncCommand::new(&runc_path);
            // Foreground run: runc stays alive as the container's
            // parent and exits with the workload status, which is the
            // only reliable way to observe the exit code.
            cmd.arg("run").arg("-b").arg(&bdir).arg(&id);
            cmd.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null());
            match stderr_for_cmd {
                Some(f) => cmd.stderr(std::process::Stdio::from(f)),
                None => cmd.stderr(std::process::Stdio::null()),
            };
            cmd.spawn()
        })
        .await
        .map_err(|e| ExecError::ExecutionFailed(format!("spawn_blocking failed: {e}")))?
        .map_err(|e| ExecError::ExecutionFailed(format!("runc spawn failed: {e}")))?;

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let early = child.try_wait().ok().flatten();
        if let Some(status) = early {
            if !status.success() {
                if let Some(err) = runc_startup_error(&stderr_path) {
                    let cleanup_runc = runc_path_for_cleanup.clone();
                    let cleanup_id = id_for_cleanup.clone();
                    tokio::task::spawn_blocking(move || {
                        let mut del = SyncCommand::new(&cleanup_runc);
                        del.args(["delete", "--force"]).arg(&cleanup_id);
                        let _ = del.output();
                    });
                    return Err(ExecError::ExecutionFailed(format!(
                        "runc run failed for {}: {}",
                        handle.id,
                        err.lines().last().unwrap_or("unknown error")
                    )));
                }
            }
            tracing::info!(
                id = %handle.id,
                exit = ?status.code(),
                "container exited immediately"
            );
        }

        self.children
            .lock()
            .expect("children lock poisoned")
            .insert(handle.id.clone(), child);

        // Set up bridge networking if an IP was allocated in create()
        if let Some(ref ip_str) = handle.internal_ip {
            let ip: Ipv4Addr = ip_str.parse().map_err(|e| {
                ExecError::ExecutionFailed(format!("invalid internal_ip in handle: {e}"))
            })?;
            let bridge = handle.bridge_name.as_deref().unwrap_or(DEFAULT_BRIDGE_NAME);
            let gateway = handle
                .gateway
                .as_deref()
                .and_then(|g| g.parse().ok())
                .unwrap_or(DEFAULT_GATEWAY);
            self.setup_container_network(&handle.id, ip, &handle.host_ports, bridge, gateway)
                .await?;
        }
        let egress_cfg = self
            .egress
            .lock()
            .expect("egress lock poisoned")
            .get(&handle.id)
            .cloned();
        if let Some(cfg) = egress_cfg {
            let gateway = handle
                .gateway
                .as_deref()
                .and_then(|g| g.parse().ok())
                .unwrap_or(DEFAULT_GATEWAY);
            apply_container_egress_policy(&self.runc_path, &handle.id, gateway, &cfg).await?;
        }

        info!(id = %handle.id, "container started");
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), ExecError> {
        info!(%id, "stopping container");

        // Overlay rootfs must be unmounted before the bundle is removed.
        if self
            .overlay_mounts
            .lock()
            .expect("overlay_mounts lock poisoned")
            .remove(id)
        {
            let merged = self.bundle_dir(id).join("merged");
            if let Err(e) = unmount_overlay(&merged) {
                warn!(%id, error = %e, "failed to unmount overlay rootfs");
            }
        }

        // Release proxy listeners (inbound port forwards) for this
        // workload. Container stop is the only place these are torn
        // down; without this, 0.0.0.0:<port> listeners leak and keep
        // forwarding to a dead IP after the workload is gone.
        if let Some(ref proxy) = self.proxy {
            let endpoint = pullrun_net::NetworkEndpoint {
                internal_ip: String::new(),
                host_port_mappings: vec![],
                namespace_path: None,
            };
            if let Err(e) = proxy.teardown(id, &endpoint).await {
                warn!(%id, "proxy teardown warning: {e}");
            }
        }

        // Send SIGTERM first, give the process 10s to shut down gracefully.
        let _ = Command::new(&self.runc_path)
            .args(["kill", id, "SIGTERM"])
            .output()
            .await;

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        // Force kill with SIGKILL
        let _ = Command::new(&self.runc_path)
            .args(["kill", id, "SIGKILL"])
            .output()
            .await;

        let output = Command::new(&self.runc_path)
            .args(["delete", "-f", id])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(%id, "runc delete warning: {stderr}");
        }

        // Reap the foreground runc child now that the container is gone.
        let child = {
            let mut children = self.children.lock().expect("children lock poisoned");
            children.remove(id)
        };
        if let Some(mut child) = child {
            tokio::task::spawn_blocking(move || child.wait()).await.ok();
        }

        let bundle_dir = self.bundle_dir(id);
        if bundle_dir.exists() {
            tokio::fs::remove_dir_all(&bundle_dir).await.ok();
        }

        info!(%id, "container stopped");
        Ok(())
    }

    async fn wait(&self, id: &str) -> Result<ExitStatus, ExecError> {
        let output = Command::new(&self.runc_path)
            .args(["state", id])
            .output()
            .await?;

        if !output.status.success() {
            return Ok(ExitStatus {
                exit_code: 0,
                signal: None,
            });
        }

        #[derive(serde::Deserialize)]
        struct RuncState {
            status: String,
        }

        let state: RuncState = serde_json::from_slice(&output.stdout)
            .map_err(|e| ExecError::ExecutionFailed(format!("parse runc state: {e}")))?;

        match state.status.as_str() {
            "stopped" | "exited" => Ok(ExitStatus {
                exit_code: 0,
                signal: None,
            }),
            _ => Err(ExecError::ExecutionFailed(format!(
                "workload {id} is still {}",
                state.status
            ))),
        }
    }

    async fn status(&self, id: &str) -> Result<String, ExecError> {
        {
            let mut children = self.children.lock().expect("children lock poisoned");
            if let Some(child) = children.get_mut(id) {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().unwrap_or(137);
                        return Ok(format!("exited({code})"));
                    }
                    Ok(None) => return Ok("running".to_string()),
                    Err(_) => {}
                }
            }
        }

        let output = Command::new(&self.runc_path)
            .args(["state", id])
            .output()
            .await?;

        if !output.status.success() {
            return Ok("stopped".to_string());
        }

        #[derive(serde::Deserialize)]
        struct RuncState {
            status: String,
        }

        let state: RuncState =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|_| RuncState {
                status: "unknown".to_string(),
            });

        Ok(state.status)
    }

    async fn update(
        &self,
        id: &str,
        cpu_millicores: Option<u64>,
        memory_bytes: Option<u64>,
    ) -> Result<(), ExecError> {
        let mut args = vec!["update".to_string(), id.to_string()];

        if let Some(cpu) = cpu_millicores {
            args.push("--cpu-quota".to_string());
            args.push((cpu * 100).to_string());
            args.push("--cpu-period".to_string());
            args.push("100000".to_string());
            args.push("--cpu-shares".to_string());
            args.push((cpu * 1024 / 1000).to_string());
        }

        if let Some(mem) = memory_bytes {
            args.push("--memory".to_string());
            args.push(mem.to_string());
        }

        let output = Command::new(&self.runc_path).args(&args).output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ExecError::ExecutionFailed(format!(
                "runc update failed: {stderr}"
            )));
        }

        info!(%id, cpu_millicores = ?cpu_millicores, memory_bytes = ?memory_bytes, "updated container resources");
        Ok(())
    }

    async fn exec(
        &self,
        id: &str,
        command: &[String],
        timeout_secs: u64,
    ) -> Result<ExecOutput, ExecError> {
        let mut args = vec!["exec".to_string(), id.to_string()];
        args.extend(command.iter().cloned());
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            Command::new(&self.runc_path).args(&args).output(),
        )
        .await
        .map_err(|_| {
            ExecError::ExecutionFailed(format!("runc exec timed out after {timeout_secs}s"))
        })?
        .map_err(|e| ExecError::ExecutionFailed(format!("runc exec failed: {e}")))?;
        Ok(ExecOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn stats(&self, id: &str) -> Result<WorkloadStats, ExecError> {
        let output = Command::new(&self.runc_path)
            .args(["state", id])
            .output()
            .await?;
        if !output.status.success() {
            return Err(ExecError::NotFound(id.to_string()));
        }
        let state: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ExecError::ExecutionFailed(format!("parse runc state: {e}")))?;
        let pid = state["pid"]
            .as_i64()
            .ok_or_else(|| ExecError::ExecutionFailed("no pid in runc state".into()))?;

        let mut mem_bytes: u64 = 0;
        let mut cpu_usec: u64 = 0;

        // Read cgroup path from /proc/<pid>/cgroup
        let cgroup_path = std::path::PathBuf::from(format!("/proc/{pid}/cgroup"));
        if let Ok(data) = std::fs::read_to_string(&cgroup_path) {
            for line in data.lines() {
                if line.contains("::") {
                    // cgroups v2: 0::/system.slice/pullrun-...
                    if let Some(path) = line.split("::").nth(1) {
                        let path = path.trim();
                        let mem_file = format!("/sys/fs/cgroup{path}/memory.current");
                        if let Ok(val) = std::fs::read_to_string(&mem_file) {
                            mem_bytes = val.trim().parse().unwrap_or(0);
                        }
                        let cpu_file = format!("/sys/fs/cgroup{path}/cpu.stat");
                        if let Ok(val) = std::fs::read_to_string(&cpu_file) {
                            for line in val.lines() {
                                if let Some(rest) = line.strip_prefix("usage_usec ") {
                                    cpu_usec = rest.trim().parse().unwrap_or(0);
                                }
                            }
                        }
                    }
                } else if !line.starts_with('#') {
                    // cgroups v1: 0::/docker/... (hybrid) or 1:name=systemd:/...
                    // Try v1 paths for memory and cpuacct controllers.
                    if let Some(cg_path) = line.split(':').nth(2) {
                        let cg_path = cg_path.trim();
                        // Try memory cgroup v1
                        let mem_v1 =
                            format!("/sys/fs/cgroup/memory{cg_path}/memory.usage_in_bytes");
                        if mem_bytes == 0 {
                            if let Ok(val) = std::fs::read_to_string(&mem_v1) {
                                mem_bytes = val.trim().parse().unwrap_or(0);
                            }
                        }
                        // Try cpuacct cgroup v1
                        let cpu_v1 = format!("/sys/fs/cgroup/cpuacct{cg_path}/cpuacct.usage");
                        if cpu_usec == 0 {
                            if let Ok(val) = std::fs::read_to_string(&cpu_v1) {
                                cpu_usec = val.trim().parse::<u64>().unwrap_or(0) / 1_000_000;
                            }
                        }
                    }
                }
            }
        }

        Ok(WorkloadStats {
            id: id.to_string(),
            cpu_usage_percent: cpu_usec as f64 / 1_000_000.0, // cumulative CPU seconds from cgroup cpu.stat
            memory_bytes: mem_bytes,
            disk_bytes: 0,
            network_rx_bytes: 0,
            network_tx_bytes: 0,
        })
    }
}

/// Resolve the network namespace path of a running workload so another
/// workload can join it (pod model). The pid is only known to runc at
/// runtime, so this queries `runc state` and maps the container init
/// pid to `/proc/<pid>/ns/net`.
/// Detect a runc startup failure from its captured stderr. Container
/// workloads have their stdio redirected to /dev/null, so anything on
/// runc's own stderr is an error (bad config, cgroup, namespaces, ...).
fn runc_startup_error(stderr_path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(stderr_path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains("level=error") || trimmed.contains("runc:") || trimmed.contains("failed to")
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Read the container PID from `runc state`.
fn rootless_state_pid(
    runc_path: &std::path::Path,
    state_root: &std::path::Path,
    id: &str,
) -> Option<u32> {
    let output = std::process::Command::new(runc_path)
        .arg("--root")
        .arg(state_root)
        .arg("state")
        .arg(id)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    value.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32)
}

/// Resolve the outbound allowlist to concrete socket addresses.
fn resolve_egress_allow(
    cfg: &EgressConfig,
) -> Result<Vec<(std::net::IpAddr, u16, String)>, ExecError> {
    use std::net::ToSocketAddrs;
    let mut out = Vec::new();
    for (host, port, proto) in &cfg.allow {
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            out.push((ip, *port, proto.clone()));
            continue;
        }
        let addrs = (host.as_str(), *port)
            .to_socket_addrs()
            .map_err(|e| ExecError::ExecutionFailed(format!("resolve {host}:{port}: {e}")))?;
        for a in addrs {
            out.push((a.ip(), *port, proto.clone()));
        }
    }
    Ok(out)
}

/// Install a fail-closed nftables output policy inside the container's
/// network namespace. Any failure aborts startup (fail closed).
async fn apply_container_egress_policy(
    runc_path: &std::path::Path,
    id: &str,
    gateway: std::net::Ipv4Addr,
    cfg: &EgressConfig,
) -> Result<(), ExecError> {
    use std::process::Command as SyncCommand;
    let state = SyncCommand::new(runc_path)
        .args(["state", id])
        .output()
        .map_err(|e| ExecError::ExecutionFailed(format!("runc state: {e}")))?;
    if !state.status.success() {
        return Err(ExecError::ExecutionFailed(
            "runc state failed for egress policy".into(),
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(&state.stdout)
        .map_err(|e| ExecError::ExecutionFailed(format!("parse runc state: {e}")))?;
    let pid = json["pid"]
        .as_i64()
        .ok_or_else(|| ExecError::ExecutionFailed("no pid for egress policy".into()))?;
    apply_egress_rules(pid, gateway, cfg).await
}

async fn apply_egress_rules(
    pid: i64,
    gateway: std::net::Ipv4Addr,
    cfg: &EgressConfig,
) -> Result<(), ExecError> {
    use std::process::Command as SyncCommand;
    let allow = resolve_egress_allow(cfg)?;

    let nft = |args: &[&str]| -> Result<(), ExecError> {
        let out = SyncCommand::new("nsenter")
            .args(["-t", &pid.to_string(), "-n", "--", "nft"])
            .args(args)
            .output()
            .map_err(|e| ExecError::ExecutionFailed(format!("nsenter nft: {e}")))?;
        if !out.status.success() {
            return Err(ExecError::ExecutionFailed(format!(
                "nft {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    };

    // Best-effort reset of a stale table, then a drop-by-default chain.
    let _ = nft(&["delete", "table", "inet", "pullrun_egress"]);
    nft(&["add", "table", "inet", "pullrun_egress"])?;
    nft(&[
        "add",
        "chain",
        "inet",
        "pullrun_egress",
        "output",
        "{ type filter hook output priority 0 ; policy drop ; }",
    ])?;
    nft(&[
        "add",
        "rule",
        "inet",
        "pullrun_egress",
        "output",
        "oifname",
        "lo",
        "accept",
    ])?;
    nft(&[
        "add",
        "rule",
        "inet",
        "pullrun_egress",
        "output",
        "ct",
        "state",
        "established,related",
        "accept",
    ])?;
    let mut dns_servers = cfg.nameservers.clone();
    if !dns_servers.contains(&std::net::IpAddr::V4(gateway)) {
        dns_servers.push(std::net::IpAddr::V4(gateway));
    }
    for ns in dns_servers {
        for proto in ["udp", "tcp"] {
            nft(&[
                "add",
                "rule",
                "inet",
                "pullrun_egress",
                "output",
                "ip",
                "daddr",
                &ns.to_string(),
                proto,
                "dport",
                "53",
                "accept",
            ])?;
        }
    }
    for (ip, port, proto) in allow {
        let addr = ip.to_string();
        let port = port.to_string();
        nft(&[
            "add",
            "rule",
            "inet",
            "pullrun_egress",
            "output",
            "ip",
            "daddr",
            &addr,
            &proto,
            "dport",
            &port,
            "accept",
        ])?;
    }
    tracing::info!(
        workload_pid = pid,
        rules = cfg.allow.len(),
        "fail-closed egress policy applied"
    );
    Ok(())
}

fn netns_path_of(
    runc_path: &std::path::Path,
    state_root: Option<&std::path::Path>,
    target: &str,
) -> Result<String, ExecError> {
    use std::process::Command as SyncCommand;

    let mut cmd = SyncCommand::new(runc_path);
    if let Some(root) = state_root {
        cmd.arg("--root").arg(root);
    }
    let out = cmd
        .arg("state")
        .arg(target)
        .output()
        .map_err(|e| ExecError::ExecutionFailed(format!("runc state {target}: {e}")))?;
    if !out.status.success() {
        return Err(ExecError::ExecutionFailed(format!(
            "runc state {target}: target workload is not running (exit {:?})",
            out.status.code()
        )));
    }
    let state: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        ExecError::ExecutionFailed(format!("runc state {target}: invalid output: {e}"))
    })?;
    let pid = state.get("pid").and_then(|p| p.as_i64()).unwrap_or(0);
    if pid <= 0 {
        return Err(ExecError::ExecutionFailed(format!(
            "runc state {target}: workload is not running (pid {pid})"
        )));
    }
    Ok(format!("/proc/{pid}/ns/net"))
}

/// Patch the bundle's config.json so the container joins `netns_path`
/// instead of creating a fresh network namespace. `runc` has no
/// CLI flag for this; the join is expressed through the OCI spec.
fn join_netns_in_config(bundle_dir: &std::path::Path, netns_path: &str) -> Result<(), ExecError> {
    let config_path = bundle_dir.join("config.json");
    let data = std::fs::read(&config_path)
        .map_err(|e| ExecError::ExecutionFailed(format!("read {config_path:?}: {e}")))?;
    let mut cfg: serde_json::Value = serde_json::from_slice(&data)
        .map_err(|e| ExecError::ExecutionFailed(format!("parse {config_path:?}: {e}")))?;
    if let Some(namespaces) = cfg
        .get_mut("linux")
        .and_then(|l| l.get_mut("namespaces"))
        .and_then(|n| n.as_array_mut())
    {
        for entry in namespaces.iter_mut() {
            if entry.get("type").and_then(|t| t.as_str()) == Some("network") {
                entry["path"] = serde_json::Value::String(netns_path.to_string());
            }
        }
    }
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&cfg)
            .map_err(|e| ExecError::ExecutionFailed(format!("serialize {config_path:?}: {e}")))?,
    )
    .map_err(|e| ExecError::ExecutionFailed(format!("write {config_path:?}: {e}")))
}

/// Rootless container executor: uses user namespaces, no veth, no bridge.
/// Networking is provided by pasta or slirp4netns at start().
pub struct RootlessContainerExecutor {
    store: MmapStore,
    bundle_root: PathBuf,
    config: crate::rootless::RootlessConfig,
    uid: u32,
    use_pasta: bool,
    net_handles: Mutex<HashMap<String, crate::rootless::NetworkHandle>>,
    children: Mutex<HashMap<String, std::process::Child>>,
    base_store: Option<Arc<BaseStore>>,
}

impl RootlessContainerExecutor {
    pub fn new(store: MmapStore, bundle_root: PathBuf, uid: u32) -> Self {
        let config = crate::rootless::RootlessConfig::detect(uid);
        std::fs::create_dir_all(&bundle_root).ok();
        Self {
            store,
            bundle_root,
            config,
            uid,
            use_pasta: true,
            net_handles: Mutex::new(HashMap::new()),
            children: Mutex::new(HashMap::new()),
            base_store: None,
        }
    }

    pub fn with_base_store(mut self, base_store: Arc<BaseStore>) -> Self {
        self.base_store = Some(base_store);
        self
    }

    pub fn with_pasta(mut self, use_pasta: bool) -> Self {
        self.use_pasta = use_pasta;
        self
    }

    fn bundle_dir(&self, id: &str) -> PathBuf {
        self.bundle_root.join(id)
    }

    /// Public accessor so the `ExecutorRouter` can check if a workload
    /// belongs to this executor before dispatching `stop()`/`wait()`/`status()`.
    pub fn bundle_dir_for(&self, id: &str) -> PathBuf {
        self.bundle_dir(id)
    }
}

#[async_trait]
impl Executor for RootlessContainerExecutor {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        if spec.egress_deny {
            return Err(ExecError::ExecutionFailed(
                "fail-closed egress requires rootful container networking".into(),
            ));
        }

        info!(id = %spec.id, uid = self.uid, "creating rootless container");

        let bundle_dir = self.bundle_dir(&spec.id);
        if bundle_dir.exists() {
            std::fs::remove_dir_all(&bundle_dir)?;
        }

        let bundle = if let Some(base_store) = &self.base_store {
            let base = base_store
                .ensure_base(&spec.image_root)
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("prepare image base: {e}")))?;
            let (bundle, stats) =
                prepare_bundle_from_base(&self.store, &base, &spec.image_root, &bundle_dir)
                    .await
                    .map_err(|e| ExecError::ExecutionFailed(format!("clone image base: {e}")))?;
            debug!(
                id = %spec.id,
                cloned = stats.cloned,
                copied = stats.copied,
                "rootless rootfs cloned from base"
            );
            bundle
        } else {
            let materializer = OciMaterializer::new(&self.store);
            materializer.materialize_bundle(&spec.image_root, &bundle_dir)?
        };

        // Build the full OCI config from the request (command, env,
        // mounts, security), then apply the rootless user-namespace
        // patch. Previously this path used only the image config, so
        // `--cmd`, `-e`, `-v`, and limits were silently ignored.
        crate::oci_spec::write_oci_config(&spec, &bundle, true)?;
        crate::rootless::apply_rootless_config(&bundle_dir, self.uid)?;

        let network_join = match &spec.network_mode {
            crate::types::NetworkMode::Container(target) => Some(target.clone()),
            _ => None,
        };
        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: None,
            gateway: None,
            host_ports: if network_join.is_some() {
                Vec::new()
            } else {
                spec.network_rules
                    .iter()
                    .filter(|r| matches!(r.direction, pullrun_net::Direction::Inbound))
                    .map(|r| {
                        let host_p = if r.host_port != 0 {
                            r.host_port
                        } else {
                            r.port
                        };
                        (host_p, r.port)
                    })
                    .collect()
            },
            backend: "container-rootless".to_string(),
            bridge_name: None,
            network_join,
        };

        info!(id = %spec.id, "rootless container bundle ready");
        let _ = bundle;
        Ok(handle)
    }

    async fn start(&self, handle: &mut ProcessHandle) -> Result<(), ExecError> {
        info!(id = %handle.id, uid = self.uid, "starting rootless container");

        let bundle_dir = self.bundle_dir(&handle.id);
        let network_join = handle.network_join.clone();

        // Pod model: resolve the sandbox's netns and patch the OCI spec
        // so runc joins it instead of creating a fresh namespace.
        if let Some(ref target) = network_join {
            let netns = netns_path_of(
                &self.config.runc_path,
                Some(&self.config.state_root),
                target,
            )?;
            join_netns_in_config(&bundle_dir, &netns)?;
            info!(id = %handle.id, target = %target, netns = %netns, "rootless container joining network namespace");
        }

        // Clear stale runc state for this id before (re)starting.
        let _ = std::process::Command::new(&self.config.runc_path)
            .arg("--root")
            .arg(&self.config.state_root)
            .args(["delete", "--force"])
            .arg(&handle.id)
            .output();

        let stderr_path = bundle_dir.join("runc.stderr.log");
        let stderr_file = std::fs::File::create(&stderr_path).ok();
        let mut cmd = crate::rootless::rootless_runc_command(&self.config, &handle.id, &bundle_dir);
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        match stderr_file {
            Some(f) => {
                cmd.stderr(Stdio::from(f));
            }
            None => {
                cmd.stderr(Stdio::null());
            }
        }

        let mut child = cmd.spawn().map_err(|e| {
            ExecError::ExecutionFailed(format!("rootless runc spawn failed for {}: {e}", handle.id))
        })?;

        // `runc run` is now foreground: it stays alive as the
        // container's parent and exits with the workload's status.
        // Give it a moment to create the container, then decide
        // whether this is a long-running workload or an immediate
        // exit (echo, true, exit 42, ...).
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let early_status = child.try_wait().ok().flatten();

        let mut container_pid = None;
        for _ in 0..20 {
            container_pid =
                rootless_state_pid(&self.config.runc_path, &self.config.state_root, &handle.id);
            if container_pid.is_some() {
                break;
            }
            if early_status.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        if let Some(status) = early_status {
            // The process exited. If runc created a container, the
            // exit code is the workload's (including non-zero); if it
            // did not, this was a startup failure.
            if !status.success() {
                if let Some(err) = runc_startup_error(&stderr_path) {
                    let mut del = std::process::Command::new(&self.config.runc_path);
                    del.arg("--root")
                        .arg(&self.config.state_root)
                        .args(["delete", "--force"])
                        .arg(&handle.id);
                    let _ = del.output();
                    return Err(ExecError::ExecutionFailed(format!(
                        "rootless runc start failed for {}: {}",
                        handle.id,
                        err.lines().last().unwrap_or("unknown error")
                    )));
                }
            }
            info!(
                id = %handle.id,
                exit = ?status.code(),
                "rootless container exited immediately"
            );
        } else if network_join.is_none() {
            match container_pid {
                Some(pid) => {
                    match crate::rootless::setup_rootless_network(&handle.id, pid, self.use_pasta)
                        .await
                    {
                        Ok(net) => {
                            info!(id = %handle.id, "rootless networking set up");
                            self.net_handles
                                .lock()
                                .expect("net_handles lock poisoned")
                                .insert(handle.id.clone(), net);
                        }
                        Err(e) => {
                            warn!(id = %handle.id, "rootless networking failed: {e} - container may have no network");
                        }
                    }
                }
                None => {
                    warn!(id = %handle.id, "container pid unavailable; skipping rootless networking")
                }
            }
        }

        // Keep the foreground runc child so status() can report the
        // real exit code.
        self.children
            .lock()
            .expect("children lock poisoned")
            .insert(handle.id.clone(), child);

        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), ExecError> {
        info!(%id, "stopping rootless container");

        // Kill the runc child and the network process (pasta/slirp4netns).
        let maybe_child = self
            .children
            .lock()
            .expect("children lock poisoned")
            .remove(id);
        if let Some(mut child) = maybe_child {
            let _ = child.kill();
            tokio::task::spawn_blocking(move || child.wait()).await.ok();
        }
        if let Some(mut net) = self
            .net_handles
            .lock()
            .expect("net_handles lock poisoned")
            .remove(id)
        {
            net.kill().ok();
        }

        let _ = std::process::Command::new(&self.config.runc_path)
            .arg("delete")
            .arg("--root")
            .arg(&self.config.state_root)
            .arg("--force")
            .arg(id)
            .output();

        let bundle_dir = self.bundle_dir(id);
        if bundle_dir.exists() {
            tokio::fs::remove_dir_all(&bundle_dir).await.ok();
        }

        Ok(())
    }

    async fn wait(&self, id: &str) -> Result<ExitStatus, ExecError> {
        let maybe_child = self
            .children
            .lock()
            .expect("children lock poisoned")
            .remove(id);
        if let Some(mut child) = maybe_child {
            let status = tokio::task::spawn_blocking(move || child.wait())
                .await
                .map_err(|e| ExecError::ExecutionFailed(format!("join wait task for {id}: {e}")))?
                .map_err(|e| {
                    ExecError::ExecutionFailed(format!("wait for runc child {id}: {e}"))
                })?;
            let exit_code = status.code().unwrap_or(-1);
            use std::os::unix::process::ExitStatusExt;
            let signal = status.signal();
            return Ok(ExitStatus { exit_code, signal });
        }
        // Child not found — check if it was already stopped via stop()
        if !self.bundle_dir(id).exists() {
            return Ok(ExitStatus {
                exit_code: 0,
                signal: None,
            });
        }
        warn!(%id, "wait called before start; returning 0");
        Ok(ExitStatus {
            exit_code: 0,
            signal: None,
        })
    }

    async fn status(&self, id: &str) -> Result<String, ExecError> {
        // The foreground runc child is the authoritative source for
        // exited containers: its exit status is the workload's.
        {
            let mut children = self.children.lock().expect("children lock poisoned");
            if let Some(child) = children.get_mut(id) {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().unwrap_or(137);
                        return Ok(format!("exited({code})"));
                    }
                    Ok(None) => return Ok("running".to_string()),
                    Err(_) => {}
                }
            }
        }

        let output = Command::new(&self.config.runc_path)
            .args(["state", "--root"])
            .arg(&self.config.state_root)
            .arg(id)
            .output()
            .await?;

        if !output.status.success() {
            return Ok("stopped".to_string());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("\"running\"") {
            Ok("running".to_string())
        } else {
            Ok("stopped".to_string())
        }
    }

    async fn update(
        &self,
        id: &str,
        cpu_millicores: Option<u64>,
        memory_bytes: Option<u64>,
    ) -> Result<(), ExecError> {
        let mut args = vec![
            "update".to_string(),
            "--root".to_string(),
            self.config.state_root.to_string_lossy().to_string(),
            id.to_string(),
        ];

        if let Some(cpu) = cpu_millicores {
            args.push("--cpu-quota".to_string());
            args.push((cpu * 100).to_string());
            args.push("--cpu-period".to_string());
            args.push("100000".to_string());
            args.push("--cpu-shares".to_string());
            args.push((cpu * 1024 / 1000).to_string());
        }

        if let Some(mem) = memory_bytes {
            args.push("--memory".to_string());
            args.push(mem.to_string());
        }

        let output = Command::new(&self.config.runc_path)
            .args(&args)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ExecError::ExecutionFailed(format!(
                "runc update failed: {stderr}"
            )));
        }

        info!(%id, cpu_millicores = ?cpu_millicores, memory_bytes = ?memory_bytes, "updated rootless container resources");
        Ok(())
    }

    async fn exec(
        &self,
        id: &str,
        command: &[String],
        timeout_secs: u64,
    ) -> Result<ExecOutput, ExecError> {
        let mut args = vec![
            "exec".to_string(),
            "--root".to_string(),
            self.config.state_root.to_string_lossy().to_string(),
            id.to_string(),
        ];
        args.extend(command.iter().cloned());
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            Command::new(&self.config.runc_path).args(&args).output(),
        )
        .await
        .map_err(|_| {
            ExecError::ExecutionFailed(format!("runc exec timed out after {timeout_secs}s"))
        })?
        .map_err(|e| ExecError::ExecutionFailed(format!("runc exec failed: {e}")))?;
        Ok(ExecOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    async fn stats(&self, id: &str) -> Result<WorkloadStats, ExecError> {
        let output = Command::new(&self.config.runc_path)
            .args([
                "state",
                "--root",
                &self.config.state_root.to_string_lossy(),
                id,
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Err(ExecError::NotFound(id.to_string()));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let state: serde_json::Value = serde_json::from_str(&stdout)
            .map_err(|e| ExecError::ExecutionFailed(format!("parse runc state: {e}")))?;
        let pid = state["pid"]
            .as_i64()
            .ok_or_else(|| ExecError::ExecutionFailed("no pid in runc state".into()))?;

        let mut mem_bytes: u64 = 0;
        let mut cpu_usec: u64 = 0;

        let cgroup_path = std::path::PathBuf::from(format!("/proc/{pid}/cgroup"));
        if let Ok(data) = std::fs::read_to_string(&cgroup_path) {
            for line in data.lines() {
                if let Some(path) = line.split("::").nth(1) {
                    let path = path.trim();
                    if let Ok(val) =
                        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.current"))
                    {
                        mem_bytes = val.trim().parse().unwrap_or(0);
                    }
                    if let Ok(val) =
                        std::fs::read_to_string(format!("/sys/fs/cgroup{path}/cpu.stat"))
                    {
                        for line in val.lines() {
                            if let Some(rest) = line.strip_prefix("usage_usec ") {
                                cpu_usec = rest.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    break;
                }
            }
        }

        Ok(WorkloadStats {
            id: id.to_string(),
            cpu_usage_percent: cpu_usec as f64 / 1_000_000.0,
            memory_bytes: mem_bytes,
            disk_bytes: 0,
            network_rx_bytes: 0,
            network_tx_bytes: 0,
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod egress_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires root, nft, and unshare to build a throwaway netns"]
    async fn installs_drop_by_default_policy() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip: not root");
            return;
        }
        let nft = std::process::Command::new("nft").arg("--version").output();
        if nft.map(|o| !o.status.success()).unwrap_or(true) {
            eprintln!("skip: nft not available");
            return;
        }
        let mut child = match std::process::Command::new("unshare")
            .args(["-n", "sleep", "60"])
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skip: unshare: {e}");
                return;
            }
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        let pid = child.id() as i64;
        let cfg = EgressConfig {
            allow: vec![("1.1.1.1".to_string(), 80, "tcp".to_string())],
            nameservers: vec![],
        };
        let applied = apply_egress_rules(pid, "10.42.0.1".parse().unwrap(), &cfg).await;
        let output = std::process::Command::new("nsenter")
            .args([
                "-t",
                &pid.to_string(),
                "-n",
                "--",
                "nft",
                "list",
                "chain",
                "inet",
                "pullrun_egress",
                "output",
            ])
            .output()
            .expect("nft list");
        let text = String::from_utf8_lossy(&output.stdout);
        let _ = child.kill();
        let _ = child.wait();
        applied.expect("apply egress rules");
        assert!(text.contains("policy drop"), "chain: {text}");
        assert!(text.contains("1.1.1.1"), "allow rule missing: {text}");
        assert!(text.contains("dport 80"), "allow port missing: {text}");
    }
}
