use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tracing::{debug, info};

use crate::ExecError;

/// Rootless runc invocation settings.
#[derive(Debug, Clone)]
pub struct RootlessConfig {
    pub runc_path: PathBuf,
    pub state_root: PathBuf,
    pub uid_map_base: u32,
    pub uid_map_size: u32,
    pub gid_map_base: u32,
    pub gid_map_size: u32,
}

impl RootlessConfig {
    pub fn detect(uid: u32) -> Self {
        // /etc/subuid-style: in real production, parse /etc/subuid
        // For now, use a default mapping: 100000 + uid for 65536 IDs
        let subuid_base = std::env::var("NIMBUS_SUBUID_BASE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000);
        let subuid_size = std::env::var("NIMBUS_SUBUID_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(65_536);

        let state_root = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{uid}")))
            .join("nimbus-runc");

        Self {
            runc_path: PathBuf::from("runc"),
            state_root,
            uid_map_base: subuid_base,
            uid_map_size: subuid_size,
            gid_map_base: subuid_base,
            gid_map_size: subuid_size,
        }
    }
}

pub fn rootless_runc_command(config: &RootlessConfig, id: &str, bundle: &Path) -> Command {
    let mut cmd = Command::new(&config.runc_path);
    cmd.arg("run")
        .arg("--bundle")
        .arg(bundle)
        .arg("--root")
        .arg(&config.state_root)
        .arg(id);
    cmd
}

pub fn rootless_delete_command(config: &RootlessConfig, id: &str) -> Command {
    let mut cmd = Command::new(&config.runc_path);
    cmd.arg("delete")
        .arg("--root")
        .arg(&config.state_root)
        .arg("--force")
        .arg(id);
    cmd
}

/// Build the OCI config.json modifications for rootless mode.
pub fn rootless_oci_config(uid: u32) -> serde_json::Value {
    serde_json::json!({
        "ociVersion": "1.0.2",
        "process": {
            "user": {
                "uid": uid,
                "gid": uid
            },
            "rlimits": [
                {"type": "RLIMIT_NOFILE", "hard": 1024, "soft": 1024}
            ],
            "noNewPrivileges": true,
            "capabilities": {
                "bounding": [
                    "CAP_AUDIT_WRITE",
                    "CAP_KILL",
                    "CAP_NET_BIND_SERVICE"
                ],
                "effective": [
                    "CAP_AUDIT_WRITE",
                    "CAP_KILL",
                    "CAP_NET_BIND_SERVICE"
                ],
                "permitted": [
                    "CAP_AUDIT_WRITE",
                    "CAP_KILL",
                    "CAP_NET_BIND_SERVICE"
                ]
            }
        },
        "root": {
            "path": "rootfs",
            "readonly": false
        },
        "mounts": [
            {
                "destination": "/proc",
                "type": "proc",
                "source": "proc"
            },
            {
                "destination": "/dev",
                "type": "tmpfs",
                "source": "tmpfs",
                "options": ["nosuid", "strictatime", "mode=755", "size=65536k"]
            },
            {
                "destination": "/sys",
                "type": "none",
                "source": "/sys",
                "options": ["rbind", "nosuid", "noexec", "nodev", "ro"]
            }
        ],
        "linux": {
            "uidMappings": [
                {
                    "containerID": 0,
                    "hostID": uid,
                    "size": 1
                }
            ],
            "gidMappings": [
                {
                    "containerID": 0,
                    "hostID": uid,
                    "size": 1
                }
            ],
            "namespaces": [
                {"type": "pid"},
                {"type": "network"},
                {"type": "ipc"},
                {"type": "uts"},
                {"type": "mount"},
                {"type": "user"}
            ],
            "maskPaths": [
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
        }
    })
}

/// Apply rootless OCI config to an existing config.json on disk.
pub fn apply_rootless_config(
    bundle_dir: &Path,
    uid: u32,
) -> Result<(), ExecError> {
    let config_path = bundle_dir.join("config.json");
    if !config_path.exists() {
        return Err(ExecError::ExecutionFailed(format!(
            "config.json not found in bundle: {}",
            bundle_dir.display()
        )));
    }

    let mut config: serde_json::Value = std::fs::read(&config_path)
        .map_err(ExecError::Io)
        .and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|e| ExecError::ExecutionFailed(format!("parse config.json: {e}")))
        })?;

    let patch = rootless_oci_config(uid);

    // Patch the user namespace
    if let Some(linux) = config.get_mut("linux") {
        if let Some(namespaces) = linux.get_mut("namespaces") {
            if let Some(arr) = namespaces.as_array_mut() {
                let has_user = arr.iter().any(|n| n.get("type").and_then(|v| v.as_str()) == Some("user"));
                if !has_user {
                    arr.push(serde_json::json!({"type": "user"}));
                }
            }
        }
    }

    // Set noNewPrivileges
    if let Some(process) = config.get_mut("process") {
        process["noNewPrivileges"] = serde_json::json!(true);
    }

    debug!(bundle = %bundle_dir.display(), %uid, "applied rootless config");

    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).unwrap_or_else(|_| patch.to_string()),
    )
    .map_err(ExecError::Io)?;

    Ok(())
}

/// Run pasta (preferred) or slirp4netns to set up networking for a rootless container.
pub async fn setup_rootless_network(
    workload_id: &str,
    container_pid: u32,
    use_pasta: bool,
) -> Result<NetworkHandle, ExecError> {
    let netns_path = format!("/proc/{container_pid}/ns/net");

    let (program, args) = if use_pasta {
        info!(%workload_id, %container_pid, "starting pasta for rootless networking");
        let mut args = vec!["--ns", &netns_path, "--config-net", "--mtu", "65520"];
        (PathBuf::from("pasta"), args.drain(..).map(String::from).collect())
    } else {
        info!(%workload_id, %container_pid, "starting slirp4netns for rootless networking");
        let args = vec![
            "--configure".to_string(),
            "--mtu=65520".to_string(),
            netns_path.clone(),
            "tap0".to_string(),
        ];
        (PathBuf::from("slirp4netns"), args)
    };

    let child = tokio::process::Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| {
            ExecError::BackendNotAvailable(format!(
                "{} not available: {e}",
                program.display()
            ))
        })?;

    Ok(NetworkHandle { child })
}

pub struct NetworkHandle {
    child: tokio::process::Child,
}

impl NetworkHandle {
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.start_kill()
    }
}

pub fn detect_rootless_available() -> bool {
    Command::new("runc")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn is_running_as_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rootless_oci_config_shape() {
        let cfg = rootless_oci_config(1000);
        assert_eq!(cfg["process"]["user"]["uid"], 1000);
        assert!(cfg["linux"]["uidMappings"].is_array());
        assert!(cfg["linux"]["namespaces"].is_array());
    }

    #[test]
    fn test_rootless_config_default_state_root() {
        let cfg = RootlessConfig::detect(1000);
        assert!(cfg.state_root.to_string_lossy().contains("1000"));
    }

    #[test]
    fn test_rootless_runc_command_args() {
        let cfg = RootlessConfig {
            runc_path: PathBuf::from("runc"),
            state_root: PathBuf::from("/tmp/nimbus-test"),
            uid_map_base: 100_000,
            uid_map_size: 65_536,
            gid_map_base: 100_000,
            gid_map_size: 65_536,
        };
        let mut cmd = rootless_runc_command(&cfg, "test", Path::new("/tmp/bundle"));
        let dbg = format!("{:?}", cmd);
        assert!(dbg.contains("run"));
        assert!(dbg.contains("test"));
        assert!(dbg.contains("/tmp/bundle"));
        assert!(dbg.contains("/tmp/nimbus-test"));
        let _ = cmd;
    }
}