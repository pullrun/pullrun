use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;
use tracing::{debug, info, warn};

use nimbus_net::Direction;
use nimbus_oci::OciMaterializer;
use nimbus_store::MmapStore;

use crate::types::{ExecError, Executor, ExitStatus, ProcessHandle, WorkloadSpec};

pub struct LinuxContainerExecutor {
    store: MmapStore,
    runc_path: PathBuf,
    bundle_root: PathBuf,
}

impl LinuxContainerExecutor {
    pub fn new(store: MmapStore, runc_path: Option<PathBuf>, bundle_root: PathBuf) -> Self {
        let runc_path = runc_path.unwrap_or_else(|| PathBuf::from("runc"));
        std::fs::create_dir_all(&bundle_root).ok();
        Self {
            store,
            runc_path,
            bundle_root,
        }
    }

    fn bundle_dir(&self, id: &str) -> PathBuf {
        self.bundle_root.join(id)
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

        let mut args = if !spec.command.is_empty() {
            spec.command.clone()
        } else if !bundle.entrypoint.is_empty() {
            [bundle.entrypoint, bundle.cmd].concat()
        } else {
            bundle.cmd
        };

        if args.is_empty() {
            args = vec!["/bin/sh".to_string()];
        }

        let oci_spec = serde_json::json!({
            "ociVersion": "1.1.0",
            "process": {
                "terminal": false,
                "user": {
                    "uid": 0,
                    "gid": 0
                },
                "args": args,
                "env": spec.env.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>(),
                "cwd": bundle.working_dir.unwrap_or_else(|| "/".to_string()),
            },
            "root": {
                "path": bundle.rootfs_path.to_string_lossy(),
                "readonly": false
            },
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

        let handle = ProcessHandle {
            id: spec.id.clone(),
            pid: None,
            internal_ip: None,
            host_ports: spec
                .network_rules
                .iter()
                .filter(|r| matches!(r.direction, Direction::Inbound))
                .map(|r| r.port)
                .collect(),
            backend: "container".to_string(),
        };

        info!(id = %spec.id, "container created");
        Ok(handle)
    }

    async fn start(&self, handle: &ProcessHandle) -> Result<(), ExecError> {
        info!(id = %handle.id, "starting container");

        let bundle_dir = self.bundle_dir(&handle.id);

        let child = Command::new(&self.runc_path)
            .args(["run", "-d", "-b"])
            .arg(&bundle_dir)
            .arg(&handle.id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ExecError::ExecutionFailed(format!(
                "runc run failed: {stderr}"
            )));
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