// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

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
        let subuid_base = std::env::var("PULLRUN_SUBUID_BASE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000);
        let subuid_size = std::env::var("PULLRUN_SUBUID_SIZE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(65_536);

        let state_root = std::env::var("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{uid}")))
            .join("pullrun-runc");

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
    rootless_runc_command_with_network(config, id, bundle, None)
}

/// Same as [`rootless_runc_command`], but with an optional network namespace
/// to join (`Some(target)` produces `--network container:<target>`).
pub fn rootless_runc_command_with_network(
    config: &RootlessConfig,
    id: &str,
    bundle: &Path,
    network_join: Option<&str>,
) -> Command {
    // --root is a global runc flag and MUST come before the subcommand.
    let mut cmd = Command::new(&config.runc_path);
    cmd.arg("--root")
        .arg(&config.state_root)
        .arg("run")
        .arg("-d");
    if let Some(target) = network_join {
        // Join the sandbox's network namespace (pod model).
        cmd.arg("--network").arg(format!("container:{target}"));
    }
    cmd.arg("--bundle").arg(bundle).arg(id);
    cmd
}

pub fn rootless_delete_command(config: &RootlessConfig, id: &str) -> Command {
    let mut cmd = Command::new(&config.runc_path);
    cmd.arg("--root")
        .arg(&config.state_root)
        .arg("delete")
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
pub fn apply_rootless_config(bundle_dir: &Path, uid: u32) -> Result<(), ExecError> {
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

    // Apply linux.* patches — uidMappings and gidMappings are required
    // for rootless; without them runc rejects with "no uid mappings found".
    if let Some(patch_linux) = patch.get("linux").and_then(|v| v.as_object()) {
        if let Some(linux_obj) = config
            .as_object_mut()
            .and_then(|root| root.get_mut("linux"))
            .and_then(|v| v.as_object_mut())
        {
            for key in &["uidMappings", "gidMappings", "maskPaths", "readonlyPaths"] {
                if let Some(val) = patch_linux.get(*key) {
                    linux_obj.insert(key.to_string(), val.clone());
                }
            }
            // Ensure "user" namespace is present in the namespaces list.
            if let Some(namespaces) = linux_obj
                .get_mut("namespaces")
                .and_then(|v| v.as_array_mut())
            {
                let has_user = namespaces
                    .iter()
                    .any(|n| n.get("type").and_then(|v| v.as_str()) == Some("user"));
                if !has_user {
                    namespaces.push(serde_json::json!({"type": "user"}));
                }
            } else {
                linux_obj.insert(
                    "namespaces".to_string(),
                    serde_json::json!([{"type": "user"}]),
                );
            }
        }
    }

    // Apply process.* patches — capabilities and noNewPrivileges for
    // rootless safety, plus user mapping so the process runs as UID/GID
    // of the calling (non-root) user.
    if let Some(patch_process) = patch.get("process").and_then(|v| v.as_object()) {
        if let Some(process_obj) = config.get_mut("process").and_then(|v| v.as_object_mut()) {
            for key in &["capabilities", "noNewPrivileges", "user"] {
                if let Some(val) = patch_process.get(*key) {
                    process_obj.insert(key.to_string(), val.clone());
                }
            }
        }
    }

    // Replace /proc, /dev, /sys mounts with rootless-safe versions.
    // These must use proc/tmpfs/bind mounts that don't require privileges.
    if let Some(patch_mounts) = patch.get("mounts").and_then(|v| v.as_array()) {
        if let Some(mounts) = config.get_mut("mounts").and_then(|v| v.as_array_mut()) {
            let rootless_dests: std::collections::HashSet<&str> = ["/proc", "/dev", "/sys"].into();
            mounts.retain(|m| {
                m.get("destination")
                    .and_then(|v| v.as_str())
                    .map(|d| !rootless_dests.contains(d))
                    .unwrap_or(true)
            });
            mounts.extend(patch_mounts.iter().cloned());
        }
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
        (
            PathBuf::from("pasta"),
            args.drain(..).map(String::from).collect(),
        )
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
            ExecError::BackendNotAvailable(format!("{} not available: {e}", program.display()))
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
    // SAFETY: `libc::geteuid()` is async-signal-safe and returns a
    // trivial integer value. No shared state is accessed.
    unsafe { libc::geteuid() == 0 }
}

/// Returns the effective user ID of the current process.
pub fn current_euid() -> u32 {
    // SAFETY: `libc::geteuid()` is async-signal-safe, no invariants.
    unsafe { libc::geteuid() }
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
        let path = cfg.state_root.to_string_lossy();
        // When XDG_RUNTIME_DIR is set the uid may not appear in the
        // path; on systems where it is unset the uid should appear.
        assert!(
            path.contains("1000") || std::env::var("XDG_RUNTIME_DIR").is_ok(),
            "expected '1000' in state root, got: {path}"
        );
    }

    #[test]
    fn test_rootless_runc_command_args() {
        let cfg = RootlessConfig {
            runc_path: PathBuf::from("runc"),
            state_root: PathBuf::from("/tmp/pullrun-test"),
            uid_map_base: 100_000,
            uid_map_size: 65_536,
            gid_map_base: 100_000,
            gid_map_size: 65_536,
        };
        let cmd = rootless_runc_command(&cfg, "test", Path::new("/tmp/bundle"));
        let dbg = format!("{:?}", cmd);
        assert!(dbg.contains("run"));
        assert!(dbg.contains("test"));
        assert!(dbg.contains("/tmp/bundle"));
        assert!(dbg.contains("/tmp/pullrun-test"));
        assert!(!dbg.contains("--network"));
        let _ = cmd;

        let cmd = rootless_runc_command_with_network(
            &cfg,
            "test",
            Path::new("/tmp/bundle"),
            Some("sandbox-1"),
        );
        let dbg = format!("{:?}", cmd);
        assert!(dbg.contains("container:sandbox-1"), "got: {dbg}");
        let _ = cmd;
    }
}
