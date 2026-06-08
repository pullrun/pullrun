use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;

pub use nimbus_net::NetworkRule;
use nimbus_store::Digest;

#[derive(Debug, Clone)]
pub enum NetworkMode {
    Loopback,
    Bridge,
    Host,
}

impl Default for NetworkMode {
    fn default() -> Self {
        Self::Loopback
    }
}

#[derive(Debug, Clone)]
pub enum Backend {
    Container,
    Vm,
    Sandbox,
}

impl Backend {
    pub fn as_str(&self) -> &str {
        match self {
            Backend::Container => "container",
            Backend::Vm => "vm",
            Backend::Sandbox => "sandbox",
        }
    }

    pub fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "container" => Ok(Backend::Container),
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
    /// instead of using the default "nimbus-br0".
    pub bridge_name: Option<String>,
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProcessHandle {
    pub id: String,
    pub pid: Option<u32>,
    pub internal_ip: Option<String>,
    pub host_ports: Vec<u16>,
    pub backend: String,
    /// Name of the Linux bridge this workload's veth is attached to.
    /// Populated in `create()` from the spec's `bridge_name`; used in
    /// `setup_container_network()` to attach the veth to the correct bridge.
    pub bridge_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExitStatus {
    pub exit_code: i32,
    pub signal: Option<i32>,
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
    Store(#[from] nimbus_store::StoreError),
    #[error("OCI error: {0}")]
    Oci(#[from] nimbus_oci::OciError),
}

#[async_trait]
pub trait Executor: Send + Sync {
    async fn create(&self, spec: WorkloadSpec) -> Result<ProcessHandle, ExecError>;
    async fn start(&self, handle: &ProcessHandle) -> Result<(), ExecError>;
    async fn stop(&self, id: &str) -> Result<(), ExecError>;
    async fn wait(&self, id: &str) -> Result<ExitStatus, ExecError>;
    async fn status(&self, id: &str) -> Result<String, ExecError>;
}