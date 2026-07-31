// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub use pullrun_net::NetworkRule;
use pullrun_store::Digest;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum RestartPolicy {
    #[default]
    No,
    OnFailure,
    Always,
    UnlessStopped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mount {
    pub type_: String,        // "bind", "volume", "tmpfs"
    pub source: String,       // host path or volume name
    pub destination: String,  // container path
    pub options: Vec<String>, // e.g. ["rbind", "ro", "nosuid"]
}

impl Mount {
    pub fn bind(source: impl Into<String>, destination: impl Into<String>) -> Self {
        Self {
            type_: "bind".to_string(),
            source: source.into(),
            destination: destination.into(),
            options: vec!["rbind".to_string(), "rprivate".to_string()],
        }
    }

    pub fn with_option(mut self, opt: impl Into<String>) -> Self {
        self.options.push(opt.into());
        self
    }

    pub fn read_only(mut self) -> Self {
        self.options.push("ro".to_string());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub enum NetworkMode {
    #[default]
    Loopback,
    Bridge,
    Host,
    Slirp,
    /// Join the network namespace of another workload (pod model).
    /// Only valid for the container backends; VM backends cannot
    /// share a netns and must reject this mode.
    Container(String),
}

#[derive(Debug, Clone)]
pub enum Backend {
    Container,
    ContainerRootless,
    Vm,
    Sandbox,
}

impl Backend {
    pub fn as_str(&self) -> &str {
        match self {
            Backend::Container => "container",
            Backend::ContainerRootless => "container-rootless",
            Backend::Vm => "vm",
            Backend::Sandbox => "sandbox",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Result<Self, String> {
        s.parse()
    }
}

impl FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "container" => Ok(Backend::Container),
            "container-rootless" => Ok(Backend::ContainerRootless),
            "vm" | "firecracker" => Ok(Backend::Vm),
            "sandbox" => Ok(Backend::Sandbox),
            other => Err(format!("unknown backend: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkloadSpec {
    pub id: String,
    pub image_root: Digest,
    pub backend: Backend,
    pub command: Vec<String>,
    pub env: HashMap<String, String>,
    pub cpu_millicores: Option<u64>,
    pub memory_bytes: Option<u64>,
    pub network_mode: NetworkMode,
    pub network_rules: Vec<NetworkRule>,
    /// Optional host-side kernel path override (Firecracker only).
    /// When set, the executor uses this vmlinux instead of its
    /// default `kernel_path` config value.
    pub kernel_path: Option<PathBuf>,
    /// Per-project bridge name for network isolation.
    /// When set, the runtime creates a dedicated bridge
    /// instead of using the default "pullrun-br0".
    pub bridge_name: Option<String>,
    /// Volume/bind mount specifications.
    pub mounts: Vec<Mount>,
    /// Health check configuration.
    pub health_check: Option<HealthCheck>,
    /// Restart policy for this workload.
    pub restart_policy: RestartPolicy,
    /// Mount the rootfs read-only (no runtime tampering).
    pub readonly_rootfs: bool,
    /// Set `noNewPrivileges` in the OCI spec (blocks setuid/capset escalation).
    pub no_new_privileges: bool,
    /// Seccomp profile: "default" (built-in allowlist), "unconfined",
    /// or "pullrun:<json>" (inline runc seccomp spec).
    pub seccomp_profile: Option<String>,
    /// Explicit syscall allowlist for the seccomp profile.
    pub allowed_syscalls: Vec<String>,
    /// Privileged container: no seccomp, no no_new_privileges, read-write
    /// rootfs, and the full capability set (runc "ALL"). Overrides the
    /// three flags above. Only honored by the container backends.
    pub privileged: bool,
}

impl WorkloadSpec {
    pub fn builder(id: impl Into<String>, image_root: Digest) -> WorkloadSpecBuilder {
        WorkloadSpecBuilder {
            id: id.into(),
            image_root,
            backend: Backend::Container,
            command: vec![],
            env: HashMap::new(),
            cpu_millicores: None,
            memory_bytes: None,
            network_mode: NetworkMode::Loopback,
            network_rules: vec![],
            kernel_path: None,
            bridge_name: None,
            mounts: vec![],
            health_check: None,
            restart_policy: RestartPolicy::No,
            readonly_rootfs: false,
            no_new_privileges: false,
            seccomp_profile: None,
            allowed_syscalls: vec![],
            privileged: false,
        }
    }
}

pub struct WorkloadSpecBuilder {
    id: String,
    image_root: Digest,
    backend: Backend,
    command: Vec<String>,
    env: HashMap<String, String>,
    cpu_millicores: Option<u64>,
    memory_bytes: Option<u64>,
    network_mode: NetworkMode,
    network_rules: Vec<NetworkRule>,
    kernel_path: Option<PathBuf>,
    bridge_name: Option<String>,
    mounts: Vec<Mount>,
    health_check: Option<HealthCheck>,
    restart_policy: RestartPolicy,
    readonly_rootfs: bool,
    no_new_privileges: bool,
    seccomp_profile: Option<String>,
    allowed_syscalls: Vec<String>,
    privileged: bool,
}

impl WorkloadSpecBuilder {
    pub fn backend(mut self, backend: Backend) -> Self {
        self.backend = backend;
        self
    }

    pub fn command(mut self, cmd: Vec<String>) -> Self {
        self.command = cmd;
        self
    }

    pub fn env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn network_mode(mut self, mode: NetworkMode) -> Self {
        self.network_mode = mode;
        self
    }

    pub fn add_network_rule(mut self, rule: NetworkRule) -> Self {
        self.network_rules.push(rule);
        self
    }

    pub fn kernel_path(mut self, path: PathBuf) -> Self {
        self.kernel_path = Some(path);
        self
    }

    pub fn bridge_name(mut self, name: String) -> Self {
        self.bridge_name = Some(name);
        self
    }

    pub fn mounts(mut self, mounts: Vec<Mount>) -> Self {
        self.mounts = mounts;
        self
    }

    pub fn add_mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn health_check(mut self, hc: HealthCheck) -> Self {
        self.health_check = Some(hc);
        self
    }

    pub fn restart_policy(mut self, rp: RestartPolicy) -> Self {
        self.restart_policy = rp;
        self
    }

    pub fn readonly_rootfs(mut self, ro: bool) -> Self {
        self.readonly_rootfs = ro;
        self
    }

    pub fn no_new_privileges(mut self, nnp: bool) -> Self {
        self.no_new_privileges = nnp;
        self
    }

    pub fn seccomp_profile(mut self, profile: Option<String>) -> Self {
        self.seccomp_profile = profile;
        self
    }

    pub fn allowed_syscalls(mut self, syscalls: Vec<String>) -> Self {
        self.allowed_syscalls = syscalls;
        self
    }

    pub fn privileged(mut self, privileged: bool) -> Self {
        self.privileged = privileged;
        self
    }

    pub fn build(self) -> WorkloadSpec {
        WorkloadSpec {
            id: self.id,
            image_root: self.image_root,
            backend: self.backend,
            command: self.command,
            env: self.env,
            cpu_millicores: self.cpu_millicores,
            memory_bytes: self.memory_bytes,
            network_mode: self.network_mode,
            network_rules: self.network_rules,
            kernel_path: self.kernel_path,
            bridge_name: self.bridge_name,
            mounts: self.mounts,
            health_check: self.health_check,
            restart_policy: self.restart_policy,
            readonly_rootfs: self.readonly_rootfs,
            no_new_privileges: self.no_new_privileges,
            seccomp_profile: self.seccomp_profile,
            allowed_syscalls: self.allowed_syscalls,
            privileged: self.privileged,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessHandle {
    pub id: String,
    pub pid: Option<u32>,
    pub internal_ip: Option<String>,
    pub host_ports: Vec<(u16, u16)>,
    pub backend: String,
    /// Name of the Linux bridge this workload's veth is attached to.
    /// Populated in `create()` from the spec's `bridge_name`; used in
    /// `setup_container_network()` to attach the veth to the correct bridge.
    pub bridge_name: Option<String>,
    /// When set, the container joins the network namespace of the
    /// workload with this id (pod model, `runc --network container:<id>`).
    pub network_join: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct WorkloadStats {
    pub id: String,
    /// Cumulative CPU time consumed (in seconds, as a float from
    /// cgroup `usage_usec`). The field name matches the protobuf
    /// wire format (`cpu_usage_percent` in the proto) for backward
    /// compatibility, but the value is cumulative CPU seconds, NOT
    /// a percentage. Compute `(t2 - t1) / interval` client-side for
    /// a true percentage.
    pub cpu_usage_percent: f64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthCheck {
    pub test: Vec<String>,
    pub interval_seconds: u32,
    pub timeout_seconds: u32,
    pub retries: u32,
    pub start_period_seconds: u32,
}

#[derive(Debug, Clone)]
pub struct ExitStatus {
    pub exit_code: i32,
    /// Unix signal number if the workload was killed by a signal.
    /// Currently always `None` because runc does not expose the signal
    /// through `runc state`. Future: parse wait status from runc's
    /// per-container state file.
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("workload not found: {0}")]
    NotFound(String),
    #[error("backend not available: {0}")]
    BackendNotAvailable(String),
    #[error("execution failed: {0}")]
    ExecutionFailed(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Store error: {0}")]
    Store(#[from] pullrun_store::StoreError),
    #[error("OCI error: {0}")]
    Oci(#[from] pullrun_oci::OciError),
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError>;
    async fn start(&self, handle: &mut ProcessHandle) -> Result<(), ExecError>;
    async fn stop(&self, id: &str) -> Result<(), ExecError>;
    async fn wait(&self, id: &str) -> Result<ExitStatus, ExecError>;
    async fn status(&self, id: &str) -> Result<String, ExecError>;
    async fn update(
        &self,
        id: &str,
        cpu_millicores: Option<u64>,
        memory_bytes: Option<u64>,
    ) -> Result<(), ExecError>;
    async fn stats(&self, id: &str) -> Result<WorkloadStats, ExecError>;
    async fn exec(
        &self,
        id: &str,
        command: &[String],
        timeout_secs: u64,
    ) -> Result<ExecOutput, ExecError>;
}
