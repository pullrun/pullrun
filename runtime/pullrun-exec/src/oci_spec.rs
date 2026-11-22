// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Shared OCI runtime config generation.
//!
//! Both the rootful and rootless container executors build the same
//! OCI `config.json` from a [`WorkloadSpec`]. Keeping it in one place
//! prevents the rootless path from silently dropping the command,
//! environment, mounts, limits, or security settings.

use std::collections::{HashMap, HashSet};

use pullrun_oci::MaterializedBundle;

use crate::types::{ExecError, WorkloadSpec};

/// Capabilities granted to unprivileged (rootless) workloads. These
/// work inside a user namespace without granting host capabilities.
pub const ROOTLESS_CAPABILITIES: &[&str] = &["CAP_AUDIT_WRITE", "CAP_KILL", "CAP_NET_BIND_SERVICE"];

fn default_capabilities() -> Vec<&'static str> {
    vec![
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_FSETID",
        "CAP_FOWNER",
        "CAP_MKNOD",
        "CAP_NET_RAW",
        "CAP_SETGID",
        "CAP_SETUID",
        "CAP_SETFCAP",
        "CAP_SETPCAP",
        "CAP_NET_BIND_SERVICE",
        "CAP_SYS_CHROOT",
        "CAP_KILL",
        "CAP_AUDIT_WRITE",
    ]
}

fn capabilities_json(caps: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "bounding": caps,
        "effective": caps,
        "permitted": caps,
    })
}

/// Build the OCI config for a workload.
///
/// `rootless` selects the namespace/capability shape expected inside a
/// user namespace; uid/gid mappings are added separately by
/// `rootless::apply_rootless_config`.
pub fn build_oci_config(
    spec: &WorkloadSpec,
    bundle: &MaterializedBundle,
    rootless: bool,
) -> Result<serde_json::Value, ExecError> {
    // ENTRYPOINT is always prepended; the spec command overrides the
    // image CMD.
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
        bundle.cmd.clone()
    };
    if args.is_empty() {
        args = vec!["/bin/sh".to_string()];
    }

    // Merge image env with request env (request wins).
    let mut env_vars: Vec<String> = bundle.env.clone();
    let spec_env: HashMap<String, String> = spec.env.clone();
    for kv in env_vars.iter_mut() {
        if let Some((key, _)) = kv.split_once('=') {
            if let Some(val) = spec_env.get(key) {
                *kv = format!("{key}={val}");
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

    let capabilities = if spec.privileged {
        serde_json::json!({
            "bounding": ["ALL"],
            "effective": ["ALL"],
            "permitted": ["ALL"]
        })
    } else if rootless {
        capabilities_json(ROOTLESS_CAPABILITIES)
    } else {
        capabilities_json(&default_capabilities())
    };

    let mut process = serde_json::json!({
        "terminal": false,
        // Inside a user namespace uid 0 maps to the invoking host user;
        // rootful containers keep the historical uid 0 default.
        "user": { "uid": 0, "gid": 0 },
        "capabilities": capabilities,
        "args": args,
        "env": env_vars,
        "cwd": bundle.working_dir.clone().unwrap_or_else(|| "/".to_string()),
    });
    if spec.no_new_privileges && !spec.privileged {
        if let Some(obj) = process.as_object_mut() {
            obj.insert("noNewPrivileges".to_string(), serde_json::Value::Bool(true));
        }
    }

    let mut namespaces = vec![
        serde_json::json!({"type": "pid"}),
        serde_json::json!({"type": "network"}),
        serde_json::json!({"type": "ipc"}),
        serde_json::json!({"type": "uts"}),
        serde_json::json!({"type": "mount"}),
    ];
    if rootless {
        namespaces.push(serde_json::json!({"type": "user"}));
    }

    let mut linux = serde_json::json!({
        "namespaces": namespaces,
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

    // Rootless cgroup limits are not delegated by default, so omit
    // `linux.resources` there instead of making runc fail.
    if !rootless && (spec.cpu_millicores.is_some() || spec.memory_bytes.is_some()) {
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

    if !spec.privileged {
        let seccomp =
            crate::seccomp::build_seccomp(spec.seccomp_profile.as_deref(), &spec.allowed_syscalls)
                .map_err(|e| ExecError::ExecutionFailed(format!("seccomp: {e}")))?;
        if let Some(seccomp) = seccomp {
            if let Some(obj) = linux.as_object_mut() {
                obj.insert("seccomp".to_string(), seccomp);
            }
        }
    }

    if !spec.sysctls.is_empty() {
        if let Some(obj) = linux.as_object_mut() {
            obj.insert(
                "sysctl".to_string(),
                serde_json::to_value(&spec.sysctls)
                    .map_err(|e| ExecError::ExecutionFailed(format!("sysctls: {e}")))?,
            );
        }
    }

    Ok(serde_json::json!({
        "ociVersion": "1.1.0",
        "process": process,
        "root": {
            "path": bundle.rootfs_path.to_string_lossy(),
            "readonly": spec.readonly_rootfs && !spec.privileged
        },
        "mounts": serde_json::Value::Array(mounts),
        "linux": linux
    }))
}

/// Write the OCI config next to the bundle rootfs.
pub fn write_oci_config(
    spec: &WorkloadSpec,
    bundle: &MaterializedBundle,
    rootless: bool,
) -> Result<(), ExecError> {
    let config = build_oci_config(spec, bundle, rootless)?;
    std::fs::write(
        &bundle.config_path,
        serde_json::to_string_pretty(&config)
            .map_err(|e| ExecError::ExecutionFailed(format!("serialize config: {e}")))?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mount, NetworkMode, RestartPolicy};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn bundle() -> MaterializedBundle {
        MaterializedBundle {
            rootfs_path: PathBuf::from("/tmp/rootfs"),
            config_path: PathBuf::from("/tmp/config.json"),
            entrypoint: vec!["/entry".to_string()],
            cmd: vec!["default".to_string()],
            env: vec!["A=image".to_string(), "B=image".to_string()],
            working_dir: Some("/work".to_string()),
        }
    }

    fn spec() -> WorkloadSpec {
        WorkloadSpec {
            id: "wl-test".to_string(),
            image_root: pullrun_store::Digest::ZERO,
            command: vec!["/bin/echo".to_string(), "hi".to_string()],
            env: HashMap::from([
                ("A".to_string(), "request".to_string()),
                ("C".to_string(), "new".to_string()),
            ]),
            mounts: vec![Mount {
                source: "/host".to_string(),
                destination: "/data".to_string(),
                type_: "bind".to_string(),
                options: vec!["ro".to_string()],
            }],
            cpu_millicores: Some(500),
            memory_bytes: Some(64 * 1024 * 1024),
            network_rules: vec![],
            network_mode: NetworkMode::Loopback,
            bridge_name: None,
            privileged: false,
            readonly_rootfs: false,
            no_new_privileges: true,
            seccomp_profile: None,
            allowed_syscalls: vec![],
            restart_policy: RestartPolicy::No,
            backend: crate::types::Backend::Container,
            kernel_path: None,
            health_check: None,
            dns: crate::types::DnsConfig::default(),
            egress_deny: false,
            egress_proxy: None,
            sysctls: Default::default(),
        }
    }

    #[test]
    fn rootless_config_keeps_spec_command_env_and_mounts() {
        let cfg = build_oci_config(&spec(), &bundle(), true).unwrap();
        let args = cfg["process"]["args"].as_array().unwrap();
        assert_eq!(args[0], "/entry");
        assert_eq!(args[1], "/bin/echo");
        assert_eq!(args[2], "hi");

        let env: Vec<String> = cfg["process"]["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(env.contains(&"A=request".to_string()));
        assert!(env.contains(&"C=new".to_string()));

        assert!(cfg["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["destination"] == "/data"));

        let ns: Vec<&str> = cfg["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap())
            .collect();
        assert!(ns.contains(&"user"));
        assert!(cfg["linux"].get("resources").is_none());
    }

    #[test]
    fn rootful_config_applies_limits() {
        let cfg = build_oci_config(&spec(), &bundle(), false).unwrap();
        assert!(cfg["linux"]["resources"]["cpu"]["quota"].is_number());
        assert!(cfg["linux"]["resources"]["memory"]["limit"].is_number());
    }
}
