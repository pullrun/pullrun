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

use pullrun_net::{Direction, Ipam, NetworkManager, NetworkRule, ProxyNetwork};
use pullrun_oci::OciMaterializer;
use pullrun_store::MmapStore;

use crate::types::{
    ExecError, ExecOutput, Executor, ExitStatus, ProcessHandle, WorkloadSpec, WorkloadStats,
};

const DEFAULT_BRIDGE_NAME: &str = "pullrun-br0";
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
            Command::new("ip")
                .args(["link", "set", bridge_name, "up"])
                .status()
                .map_err(|e| {
                    ExecError::ExecutionFailed(format!("ip link set {bridge_name} up: {e}"))
                })?;
        }
        // Assign the gateway IP to the bridge so the host kernel has a
        // route to the container subnet (10.42.0.0/16). Without this,
        // the proxy's TcpStream::connect(container_ip:port) would get
        // "No route to host" and reset the client connection.
        // Must run regardless of whether the bridge already existed.
        if bridge_name == DEFAULT_BRIDGE_NAME {
            let gateway_ip = DEFAULT_GATEWAY.to_string();
            // Ignore error — the address may already be assigned.
            let _ = Command::new("ip")
                .args([
                    "addr",
                    "add",
                    &format!("{gateway_ip}/16"),
                    "dev",
                    bridge_name,
                ])
                .status();
        }
        Ok(())
    }

    async fn setup_container_network(
        &self,
        id: &str,
        ip: Ipv4Addr,
        host_ports: &[(u16, u16)],
        bridge: &str,
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

        // Create veth pair with one end in the container's netns
        let status = SyncCommand::new("ip")
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
            .status()
            .map_err(|e| ExecError::ExecutionFailed(format!("ip link add veth: {e}")))?;
        if !status.success() {
            return Err(ExecError::ExecutionFailed(
                "ip link add veth pair failed".into(),
            ));
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
                &DEFAULT_GATEWAY.to_string(),
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
}

#[async_trait]
impl Executor for LinuxContainerExecutor {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError> {
        self.check_runc().await.map_err(|e| {
            tracing::error!("check_runc failed: {e}");
            e
        })?;

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
                    let ip = internal_ip.expect("just allocated above");
                    info!(id = %spec.id, bridge = bridge_name, ip = %ip, "allocated bridge IP");
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
            let existing_keys: HashSet<String> = env_vars
                .iter()
                .filter_map(|kv| kv.split_once('=').map(|(k, _)| k.to_string()))
                .collect();
            for (k, v) in spec_env.iter() {
                if !existing_keys.contains(k.as_str()) {
                    env_vars.push(format!("{k}={v}"));
                }
            }
        }

        let mut linux = serde_json::json!({
            "namespaces": [
                {"type": "pid"},
                {"type": "network"},
                {"type": "ipc"},
                {"type": "uts"},
                {"type": "mount"}
            ],
            "maskedPaths": [
                "/proc/acpi",
                "/proc/asound",
                "/proc/kcore",
                "/proc/keys",
                "/proc/latency_stats",
                "/proc/timer_list",
                "/proc/timer_stats",
                "/proc/sched_debug",
                "/proc/scsi",
                "/sys/firmware"
            ],
            "readonlyPaths": [
                "/proc/asound",
                "/proc/bus",
                "/proc/fs",
                "/proc/irq",
                "/proc/sys",
                "/proc/sysrq-trigger"
            ]
        });

        if spec.cpu_millicores.is_some() || spec.memory_bytes.is_some() {
            let mut resources = serde_json::Map::new();
            if let Some(cpu_millicores) = spec.cpu_millicores {
                resources.insert(
                    "cpu".to_string(),
                    serde_json::json!({
                        "shares": cpu_millicores * 1024 / 1000,
                        "quota": (cpu_millicores * 100) as i64,
                        "period": 100000
                    }),
                );
            }
            if let Some(mem_bytes) = spec.memory_bytes {
                resources.insert(
                    "memory".to_string(),
                    serde_json::json!({
                        "limit": mem_bytes as i64,
                        "swap": mem_bytes as i64
                    }),
                );
            }
            if let Some(obj) = linux.as_object_mut() {
                obj.insert(
                    "resources".to_string(),
                    serde_json::Value::Object(resources),
                );
            }
        }

        let mut mounts = vec![
            serde_json::json!({"destination": "/proc", "type": "proc", "source": "proc"}),
            serde_json::json!({"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]}),
            serde_json::json!({"destination": "/dev/pts", "type": "devpts", "source": "devpts"}),
            serde_json::json!({"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue"}),
            serde_json::json!({"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid", "nodev", "noexec", "ro"]}),
            serde_json::json!({"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=1777"]}),
            serde_json::json!({"destination": "/dev/shm", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid", "nodev", "mode=1777"]}),
        ];
        for m in &spec.mounts {
            let mut mount = serde_json::Map::new();
            mount.insert(
                "destination".to_string(),
                serde_json::Value::String(m.destination.clone()),
            );
            mount.insert(
                "type".to_string(),
                serde_json::Value::String(m.type_.clone()),
            );
            mount.insert(
                "source".to_string(),
                serde_json::Value::String(m.source.clone()),
            );
            // Bind mounts always need rbind + rprivate for runc to work.
            let mut opts: Vec<String> = Vec::new();
            if m.type_ == "bind" {
                opts.push("rbind".to_string());
                opts.push("rprivate".to_string());
            }
            for o in &m.options {
                if !opts.contains(o) {
                    opts.push(o.clone());
                }
            }
            if !opts.is_empty() {
                mount.insert(
                    "options".to_string(),
                    serde_json::Value::Array(
                        opts.iter()
                            .map(|o| serde_json::Value::String(o.clone()))
                            .collect(),
                    ),
                );
            }
            mounts.push(serde_json::Value::Object(mount));
        }

        let mut process = serde_json::json!({
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
        });
        if spec.no_new_privileges {
            if let Some(obj) = process.as_object_mut() {
                obj.insert("noNewPrivileges".to_string(), serde_json::Value::Bool(true));
            }
        }

        // Seccomp: fail closed on an invalid profile instead of silently
        // running the workload unconfined.
        let seccomp =
            crate::seccomp::build_seccomp(spec.seccomp_profile.as_deref(), &spec.allowed_syscalls)
                .map_err(|e| ExecError::ExecutionFailed(format!("seccomp: {e}")))?;
        if let Some(seccomp) = seccomp {
            if let Some(obj) = linux.as_object_mut() {
                obj.insert("seccomp".to_string(), seccomp);
            }
        }

        let oci_spec = serde_json::json!({
            "ociVersion": "1.1.0",
            "process": process,
            "root": {
                "path": bundle.rootfs_path.to_string_lossy(),
                "readonly": spec.readonly_rootfs
            },
            "mounts": serde_json::Value::Array(mounts),
            "linux": linux
        });

        std::fs::write(
            &bundle.config_path,
            serde_json::to_string_pretty(&oci_spec)
                .expect("oci_spec serialization should never fail"),
        )?;

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

        let bridge_name = spec.bridge_name.clone();
        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: internal_ip.map(|ip| ip.to_string()),
            host_ports: spec
                .network_rules
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
                .collect(),
            backend: "container".to_string(),
            bridge_name,
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

        tracing::debug!(
            "start: runc_path={:?}, bundle_dir={:?}, id={}",
            runc_path,
            bdir,
            id
        );

        let status = tokio::task::spawn_blocking(move || {
            tracing::debug!("spawn_blocking: spawning runc run -d -b {:?} {}", bdir, id);
            match SyncCommand::new(&runc_path)
                .args(["run", "-d", "-b"])
                .arg(&bdir)
                .arg(&id)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(s) => {
                    tracing::debug!("runc exited with status: {:?}", s);
                    Ok(s)
                }
                Err(e) => {
                    tracing::error!("runc spawn failed: {e}");
                    Err(e)
                }
            }
        })
        .await
        .map_err(|e| ExecError::ExecutionFailed(format!("spawn_blocking failed: {e}")))?
        .map_err(|e| ExecError::ExecutionFailed(format!("runc spawn failed: {e}")))?;

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
            self.setup_container_network(&handle.id, ip, &handle.host_ports, bridge)
                .await?;
        }

        info!(id = %handle.id, "container started");
        Ok(())
    }

    async fn stop(&self, id: &str) -> Result<(), ExecError> {
        info!(%id, "stopping container");

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
        }
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
                .filter(|r| matches!(r.direction, pullrun_net::Direction::Inbound))
                .map(|r| {
                    let host_p = if r.host_port != 0 {
                        r.host_port
                    } else {
                        r.port
                    };
                    (host_p, r.port)
                })
                .collect(),
            backend: "container-rootless".to_string(),
            bridge_name: None,
        };

        info!(id = %spec.id, "rootless container bundle ready");
        let _ = bundle;
        Ok(handle)
    }

    async fn start(&self, handle: &mut ProcessHandle) -> Result<(), ExecError> {
        info!(id = %handle.id, uid = self.uid, "starting rootless container");

        let bundle_dir = self.bundle_dir(&handle.id);

        let mut cmd = crate::rootless::rootless_runc_command(&self.config, &handle.id, &bundle_dir);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().map_err(|e| {
            ExecError::ExecutionFailed(format!("rootless runc spawn failed for {}: {e}", handle.id))
        })?;
        let pid = child.id();

        // Quick non-blocking check: did runc fail immediately?
        // runc run -d succeeds or fails fast, so try_wait catches bad flags/config
        // without blocking when runc is still starting the container.
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                return Err(ExecError::ExecutionFailed(format!(
                    "rootless runc start failed for {} (exit: {:?})",
                    handle.id,
                    status.code()
                )));
            }
            // runc exited successfully — container is running in background.
        }

        // Run pasta/slirp4netns in the container's network namespace
        match crate::rootless::setup_rootless_network(&handle.id, pid, self.use_pasta).await {
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

        // Store the runc child so we can wait() on it later.
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
