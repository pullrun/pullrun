use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, info, warn};

use nimbus_net::{Direction, Ipam, NetworkRule, ProxyNetwork};
use nimbus_oci::OciMaterializer;
use nimbus_store::MmapStore;

use crate::types::{ExecError, Executor, ExitStatus, ProcessHandle, WorkloadSpec};

const DEFAULT_BRIDGE_NAME: &str = "nimbus-br0";
const DEFAULT_GATEWAY: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 1);

pub struct LinuxContainerExecutor {
    store: MmapStore,
    runc_path: PathBuf,
    bundle_root: PathBuf,
    ipam: Option<Arc<Ipam>>,
    proxy: Option<Arc<ProxyNetwork>>,
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
        }
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
        spec.bridge_name.is_some() || matches!(spec.network_mode, crate::types::NetworkMode::Bridge)
    }

    fn ensure_bridge_exists(bridge_name: &str) -> Result<(), ExecError> {
        use std::process::Command as SyncCommand;
        let check = SyncCommand::new("ip")
            .args(["link", "show", bridge_name])
            .output()?;
        if !check.status.success() {
            info!(bridge = bridge_name, "creating bridge");
            let prefix = "16";
            SyncCommand::new("ip")
                .args(["link", "add", bridge_name, "type", "bridge"])
                .output()?;
            SyncCommand::new("ip")
                .args(["link", "set", bridge_name, "up"])
                .output()?;
            SyncCommand::new("ip")
                .args([
                    "addr",
                    "add",
                    &format!("{}/{prefix}", DEFAULT_GATEWAY),
                    "dev",
                    bridge_name,
                ])
                .output()?;
        }
        Ok(())
    }

    async fn setup_container_network(
        &self,
        id: &str,
        ip: Ipv4Addr,
        host_ports: &[u16],
        bridge: &str,
    ) -> Result<(), ExecError> {
        use std::process::Command as SyncCommand;

        // Get container PID from runc state
        let output = Command::new(&self.runc_path)
            .args(["state", id])
            .output()
            .await?;
        if !output.status.success() {
            return Err(ExecError::ExecutionFailed("runc state failed after start".into()));
        }
        let state: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| ExecError::ExecutionFailed(format!("parse runc state: {e}")))?;
        let pid = state["pid"].as_i64()
            .ok_or_else(|| ExecError::ExecutionFailed("no pid in runc state".into()))?;

        let veth_host = format!("v{}", &id[..id.len().min(12)]);

        info!(id = %id, veth = veth_host, bridge = bridge, container_ip = %ip, pid = pid, "setting up container bridge network");

        // Create veth pair with one end in the container's netns
        let status = SyncCommand::new("ip")
            .args([
                "link", "add", &veth_host, "type", "veth", "peer", "name", "eth0",
                "netns", &pid.to_string(),
            ])
            .status()
            .map_err(|e| ExecError::ExecutionFailed(format!("ip link add veth: {e}")))?;
        if !status.success() {
            return Err(ExecError::ExecutionFailed("ip link add veth pair failed".into()));
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
            .args(["-t", &pid.to_string(), "-n", "--", "ip", "addr", "add", &cidr, "dev", "eth0"])
            .status()?;
        SyncCommand::new("nsenter")
            .args(["-t", &pid.to_string(), "-n", "--", "ip", "link", "set", "eth0", "up"])
            .status()?;
        SyncCommand::new("nsenter")
            .args([
                "-t", &pid.to_string(), "-n", "--", "ip", "route", "add", "default",
                "via", &DEFAULT_GATEWAY.to_string(),
            ])
            .status()?;

        // Register with proxy for port forwarding
        if let Some(ref proxy) = self.proxy {
            if !host_ports.is_empty() {
                let rules: Vec<NetworkRule> = host_ports
                    .iter()
                    .map(|&port| NetworkRule::inbound(port))
                    .collect();
                proxy.register_endpoint(id, ip.to_string(), &rules).await
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
                    "runc not found at {}: {e}",
                    self.runc_path.display()
                ))
            })?;

        if !output.status.success() {
            return Err(ExecError::BackendNotAvailable(
                "runc returned non-zero exit code".into(),
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        debug!(runc_version = %stdout.trim(), "runc found");
        Ok(())
    }
}

#[async_trait]
impl Executor for LinuxContainerExecutor {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        self.check_runc().await?;

        info!(id = %spec.id, image_root = %spec.image_root, "creating container");

        let bundle_dir = self.bundle_dir(&spec.id);
        if bundle_dir.exists() {
            std::fs::remove_dir_all(&bundle_dir)?;
        }

        let materializer = OciMaterializer::new(&self.store);
        let bundle = materializer.materialize_bundle(&spec.image_root, &bundle_dir)?;

        // Fix args composition: ENTRYPOINT is always prepended, CMD/spec.command
        // is the default args. OCI spec: process.args = entrypoint + command.
        let mut args = if !bundle.entrypoint.is_empty() {
            let cmd = if !spec.command.is_empty() {
                &spec.command
            } else {
                &bundle.cmd
            };
            [bundle.entrypoint.as_slice(), cmd.as_slice()].concat()
        } else if !spec.command.is_empty() {
            spec.command.clone()
        } else {
            bundle.cmd
        };

        if args.is_empty() {
            args = vec!["/bin/sh".to_string()];
        }

        // Allocate IP for bridge networking before spec.env is consumed
        let mut internal_ip: Option<Ipv4Addr> = None;
        if self.should_setup_bridge(&spec) {
            let bridge_name = spec.bridge_name.as_deref().unwrap_or(DEFAULT_BRIDGE_NAME);
            Self::ensure_bridge_exists(bridge_name)?;
            if let Some(ref ipam) = self.ipam {
                if let Some(ip) = ipam.allocate() {
                    internal_ip = Some(Ipv4Addr::from(ip));
                    info!(id = %spec.id, bridge = bridge_name, ip = %internal_ip.unwrap(), "allocated bridge IP");
                }
            }
        }

        let mut env_vars: Vec<String> = bundle.env.clone();
        {
            let spec_env: HashMap<String, String> = spec.env.into_iter().collect();
            for kv in env_vars.iter_mut() {
                if let Some((key, _)) = kv.split_once('=') {
                    if let Some(val) = spec_env.get(key) {
                        *kv = format!("{}={}", key, val);
                    }
                }
            }
            let existing_keys: HashSet<String> =
                env_vars.iter().filter_map(|kv| kv.split_once('=').map(|(k, _)| k.to_string())).collect();
            for (k, v) in spec_env.iter() {
                if !existing_keys.contains(k.as_str()) {
                    env_vars.push(format!("{k}={v}"));
                }
            }
        }

        let oci_spec = serde_json::json!({
            "ociVersion": "1.1.0",
            "process": {
                "terminal": false,
                "user": {
                    "uid": 0,
                    "gid": 0
                },
                "capabilities": {
                    "bounding": [
                        "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FSETID", "CAP_FOWNER",
                        "CAP_MKNOD", "CAP_NET_RAW", "CAP_SETGID", "CAP_SETUID",
                        "CAP_SETFCAP", "CAP_SETPCAP", "CAP_NET_BIND_SERVICE",
                        "CAP_SYS_CHROOT", "CAP_KILL", "CAP_AUDIT_WRITE"
                    ],
                    "effective": [
                        "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FSETID", "CAP_FOWNER",
                        "CAP_MKNOD", "CAP_NET_RAW", "CAP_SETGID", "CAP_SETUID",
                        "CAP_SETFCAP", "CAP_SETPCAP", "CAP_NET_BIND_SERVICE",
                        "CAP_SYS_CHROOT", "CAP_KILL", "CAP_AUDIT_WRITE"
                    ],
                    "permitted": [
                        "CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FSETID", "CAP_FOWNER",
                        "CAP_MKNOD", "CAP_NET_RAW", "CAP_SETGID", "CAP_SETUID",
                        "CAP_SETFCAP", "CAP_SETPCAP", "CAP_NET_BIND_SERVICE",
                        "CAP_SYS_CHROOT", "CAP_KILL", "CAP_AUDIT_WRITE"
                    ]
                },
                "args": args,
                "env": env_vars,
                "cwd": bundle.working_dir.unwrap_or_else(|| "/".to_string()),
            },
            "root": {
                "path": bundle.rootfs_path.to_string_lossy(),
                "readonly": false
            },
            "mounts": [
                {"destination": "/proc", "type": "proc", "source": "proc"},
                {"destination": "/dev", "type": "tmpfs", "source": "tmpfs"},
                {"destination": "/dev/pts", "type": "devpts", "source": "devpts"},
                {"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue"},
                {"destination": "/sys", "type": "sysfs", "source": "sysfs"}
            ],
            "linux": {
                "namespaces": [
                    {"type": "pid"},
                    {"type": "network"},
                    {"type": "ipc"},
                    {"type": "uts"},
                    {"type": "mount"}
                ]
            }
        });

        std::fs::write(
            &bundle.config_path,
            serde_json::to_string_pretty(&oci_spec).unwrap(),
        )?;

        let bridge_name = spec.bridge_name.clone();
        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: internal_ip.map(|ip| ip.to_string()),
            host_ports: spec
                .network_rules
                .iter()
                .filter(|r| matches!(r.direction, Direction::Inbound))
                .map(|r| r.port)
                .collect(),
            backend: "container".to_string(),
            bridge_name,
        };

        info!(id = %spec.id, "container created");
        Ok(handle)
    }

    async fn start(&self, handle: &ProcessHandle) -> Result<(), ExecError> {
        info!(id = %handle.id, "starting container");

        let bundle_dir = self.bundle_dir(&handle.id);

        let status = Command::new(&self.runc_path)
            .args(["run", "-d", "-b"])
            .arg(&bundle_dir)
            .arg(&handle.id)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if !status.success() {
            return Err(ExecError::ExecutionFailed(format!(
                "runc run failed for {} (check runc logs)",
                handle.id
            )));
        }

        // Set up bridge networking if an IP was allocated in create()
        if let Some(ref ip_str) = handle.internal_ip {
            let ip: Ipv4Addr = ip_str.parse().map_err(|e| {
                ExecError::ExecutionFailed(format!("invalid internal_ip in handle: {e}"))
            })?;
            let bridge = handle.bridge_name.as_deref().unwrap_or(DEFAULT_BRIDGE_NAME);
            self.setup_container_network(&handle.id, ip, &handle.host_ports, bridge).await?;
        }

        info!(id = %handle.id, "container started");
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), ExecError> {
        info!(%id, "stopping container");

        let output = Command::new(&self.runc_path)
            .args(["delete", "-f", id])
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!(%id, "runc delete warning: {stderr}");
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

        let state: RuncState = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|_| RuncState {
                status: "unknown".to_string(),
            });

        Ok(state.status)
    }
}

/// Rootless container executor: uses user namespaces, no veth, no bridge.
/// Networking is provided by pasta or slirp4netns at start().
pub struct RootlessContainerExecutor {
    store: MmapStore,
    bundle_root: PathBuf,
    config: crate::rootless::RootlessConfig,
    uid: u32,
    use_pasta: bool,
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
        }
    }

    pub fn with_pasta(mut self, use_pasta: bool) -> Self {
        self.use_pasta = use_pasta;
        self
    }

    fn bundle_dir(&self, id: &str) -> PathBuf {
        self.bundle_root.join(id)
    }
}

#[async_trait]
impl Executor for RootlessContainerExecutor {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        info!(id = %spec.id, uid = self.uid, "creating rootless container");

        let bundle_dir = self.bundle_dir(&spec.id);
        if bundle_dir.exists() {
            std::fs::remove_dir_all(&bundle_dir)?;
        }

        let materializer = OciMaterializer::new(&self.store);
        let bundle = materializer.materialize_bundle(&spec.image_root, &bundle_dir)?;

        // Apply rootless patch (adds user namespace, sets noNewPrivileges, etc.)
        crate::rootless::apply_rootless_config(&bundle_dir, self.uid)?;

        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: None,
            host_ports: spec
                .network_rules
                .iter()
                .filter(|r| matches!(r.direction, nimbus_net::Direction::Inbound))
                .map(|r| r.port)
                .collect(),
            backend: "container-rootless".to_string(),
            bridge_name: None,
        };

        info!(id = %spec.id, "rootless container bundle ready");
        let _ = bundle;
        Ok(handle)
    }

    async fn start(&self, handle: &ProcessHandle) -> Result<(), ExecError> {
        info!(id = %handle.id, uid = self.uid, "starting rootless container");

        let bundle_dir = self.bundle_dir(&handle.id);

        let mut cmd = crate::rootless::rootless_runc_command(
            &self.config,
            &handle.id,
            &bundle_dir,
        );
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        let child = cmd.spawn()?;
        let pid = child.id();

        // Run pasta/slirp4netns in the container's network namespace
        match crate::rootless::setup_rootless_network(
            &handle.id,
            pid,
            self.use_pasta,
        )
        .await
        {
            Ok(_net) => {
                info!(id = %handle.id, "rootless networking set up");
                // In a real implementation, we'd keep the network handle
                // and kill it on stop()
            }
            Err(e) => {
                warn!(id = %handle.id, "rootless networking failed: {e} - container may have no network");
            }
        }

        // Detach the runc child
        std::mem::forget(child);

        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), ExecError> {
        info!(%id, "stopping rootless container");

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

    async fn wait(&self, _id: &str) -> Result<ExitStatus, ExecError> {
        warn!("rootless wait not implemented; container runs detached");
        Ok(ExitStatus {
            exit_code: 0,
            signal: None,
        })
    }

    async fn status(&self, id: &str) -> Result<String, ExecError> {
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
}