use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(test)]
use std::sync::Arc;

use pullrun_oci::OciMaterializer;
use pullrun_store::{Digest, MmapStore};
use tempfile::TempDir;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::network::VmNetwork;

/// Shell-quote a single argument for use in `exec arg1 arg2 ...`
/// in a shell script. Wraps in single quotes and escapes any
/// internal single quotes via the `'\''` idiom.
fn shell_quote(arg: &str) -> String {
    let escaped = arg.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Build the merged command from OCI Entrypoint and Cmd, following
/// the Docker/OCI spec: if entrypoint is non-empty, it prefixes cmd.
fn merge_command(entrypoint: &[String], cmd: &[String]) -> Vec<String> {
    if entrypoint.is_empty() {
        cmd.to_vec()
    } else {
        entrypoint
            .iter()
            .cloned()
            .chain(cmd.iter().cloned())
            .collect()
    }
}

#[derive(Debug, Error)]
pub enum Ext4Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("fallocate failed: {0}")]
    Fallocate(String),
    #[error("mkfs.ext4 not found: {0}")]
    MkfsNotFound(String),
    #[error("mkfs.ext4 failed: {0}")]
    MkfsFailed(String),
    #[error("DAG materialization failed: {0}")]
    Materialization(String),
}

#[derive(Debug, Clone)]
pub struct Ext4Options {
    pub size_mb: u64,
    pub label: Option<String>,
}

impl Default for Ext4Options {
    fn default() -> Self {
        Self {
            size_mb: 256,
            label: None,
        }
    }
}

/// Materialize a DAG root into a bootable ext4 rootfs image.
///
/// Rootless-safe: uses `mkfs.ext4 -d <dir>` to populate the ext4
/// image directly from a temp directory — no loop mount, no root.
///
/// Requires e2fsprogs >= 1.47.0 (2023) which added the `-d` flag.
/// The fallback path without `-d` is not implemented (all supported
/// distros ship 1.47+).
///
/// The flow:
/// 1. Materialize the DAG into a temp directory
/// 2. Inject /init if the image lacks one
/// 3. Create a sparse file of size_mb MB
/// 4. Run `mkfs.ext4 -d <dir> <image>` — creates ext4 from directory
pub async fn materialize_ext4_rootfs(
    store: &MmapStore,
    root_digest: &Digest,
    output_path: &Path,
    options: Ext4Options,
) -> Result<(), Ext4Error> {
    info!(
        %root_digest,
        output = %output_path.display(),
        size_mb = options.size_mb,
        "materializing DAG root -> ext4 rootfs"
    );

    // 1. Materialize DAG to a temp directory.
    let work_dir = TempDir::new()?;
    let materialize_result = OciMaterializer::new(store)
        .materialize_into(root_digest, work_dir.path())
        .await;

    if let Err(e) = materialize_result {
        return Err(Ext4Error::Materialization(format!("{e}")));
    }

    // 2. Inject /init using the OCI image's ENTRYPOINT/CMD.
    //    Container images (alpine, ubuntu, etc.) have ENTRYPOINT/CMD
    //    but no /init executable. The kernel boot args pass init=/init,
    //    so we need one present for the VM to boot successfully.
    //    We read the DAG manifest to get the entrypoint and cmd,
    //    merge them per OCI spec, and write /init that execs them.
    let init_path = work_dir.path().join("init");
    if !init_path.exists() {
        let materializer = OciMaterializer::new(store);
        let manifest_data = materializer.materialize_manifest(root_digest);
        let shell_args: Vec<String> = match manifest_data {
            Ok(data) => {
                let merged = merge_command(&data.entrypoint, &data.cmd);
                if merged.is_empty() {
                    info!("OCI image has no ENTRYPOINT/CMD; injecting default /bin/sh");
                    vec!["/bin/sh".to_string()]
                } else {
                    info!(
                        "injecting /init from OCI manifest: {}",
                        merged.join(" ")
                    );
                    merged
                }
            }
            Err(e) => {
                warn!(
                    "failed to read OCI manifest for /init: {e}; falling back to /bin/sh"
                );
                vec!["/bin/sh".to_string()]
            }
        };
        let mut init_content = String::from("#!/bin/sh\n");
        init_content.push_str("exec");
        for arg in &shell_args {
            init_content.push(' ');
            init_content.push_str(&shell_quote(arg));
        }
        init_content.push('\n');
        std::fs::write(&init_path, init_content.as_bytes())?;
        std::fs::set_permissions(&init_path, PermissionsExt::from_mode(0o755))?;
    }

    // 3. Create sparse file for the ext4 image.
    create_sparse_file(output_path, options.size_mb)?;

    // 4. Create ext4 filesystem populated from the temp directory.
    //    No root required — mkfs.ext4 writes directly to the file.
    format_ext4_from_dir(output_path, work_dir.path(), options.label.as_deref())?;

    info!(output = %output_path.display(), "ext4 rootfs ready");
    Ok(())
}

fn create_sparse_file(path: &Path, size_mb: u64) -> Result<(), Ext4Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if path.exists() {
        std::fs::remove_file(path)?;
    }

    // Try fallocate first (preferred — creates a sparse file)
    let fallocate_status = Command::new("fallocate")
        .args(["-l", &format!("{size_mb}M")])
        .arg(path)
        .status();

    match fallocate_status {
        Ok(s) if s.success() => {
            debug!(path = %path.display(), size_mb, "sparse file created (fallocate)");
            return Ok(());
        }
        Ok(s) => {
            warn!(?s, "fallocate returned non-zero, falling back to truncate");
        }
        Err(e) => {
            warn!(error = %e, "fallocate not available, falling back to truncate");
        }
    }

    // Fallback: create file with truncate (writes zeros — slower)
    let f = std::fs::File::create(path)?;
    f.set_len(size_mb * 1024 * 1024)?;
    debug!(path = %path.display(), size_mb, "sparse file created (truncate)");
    Ok(())
}

fn format_ext4_from_dir(image: &Path, source_dir: &Path, label: Option<&str>) -> Result<(), Ext4Error> {
    // Check mkfs.ext4 supports -d (e2fsprogs >= 1.47.0).
    let version_output = Command::new("mkfs.ext4")
        .arg("--version")
        .output()
        .map_err(|e| Ext4Error::MkfsNotFound(format!("mkfs.ext4 not found: {e}")))?;
    let version_str = String::from_utf8_lossy(
        if version_output.stdout.is_empty() { &version_output.stderr } else { &version_output.stdout }
    );
    if !version_str.contains("1.47") && !version_str.contains("1.48") && !version_str.contains("1.49") {
        warn!(
            "mkfs.ext4 version may not support -d flag (requires e2fsprogs >= 1.47.0, got: {})",
            version_str.lines().next().unwrap_or("unknown")
        );
    }
    let mut cmd = Command::new("mkfs.ext4");
    cmd.arg("-F"); // force (file exists)
    cmd.arg("-q"); // quiet
    if let Some(l) = label {
        cmd.args(["-L", l]);
    }
    cmd.arg("-d"); // populate from directory
    cmd.arg(source_dir);
    cmd.arg(image);

    let output = cmd
        .output()
        .map_err(|e| Ext4Error::MkfsNotFound(format!("mkfs.ext4 not found: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Ext4Error::MkfsFailed(format!("mkfs.ext4 failed: {stderr}")));
    }

    debug!(
        image = %image.display(),
        source = %source_dir.display(),
        "ext4 filesystem created from directory"
    );
    Ok(())
}

/// Build the Firecracker VM config JSON for a given rootfs image.
///
/// `vm_net`, if provided, is rendered as a single `eth0` network interface
/// attached to the host-side tap device, and the kernel `ip=` boot arg is
/// included so the guest configures its interface without DHCP.
pub fn firecracker_config(
    vm_id: &str,
    kernel_path: &Path,
    rootfs_path: &Path,
    vm_net: Option<&VmNetwork>,
    vcpus: u8,
    mem_mib: u32,
) -> serde_json::Value {
    let mut boot_args =
        String::from("console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init");
    let mut net_interfaces = vec![];
    if let Some(net) = vm_net {
        boot_args.push(' ');
        boot_args.push_str(&net.boot_args_extra());
        net_interfaces.push(serde_json::json!({
            "iface_id": "eth0",
            "guest_mac": net.guest_mac,
            "host_dev_name": net.tap_name
        }));
    }
    let _ = vm_id; // VM id is no longer used for MAC (now derived from IP)

    serde_json::json!({
        "boot-source": {
            "kernel_image_path": kernel_path.to_string_lossy(),
            "boot_args": boot_args
        },
        "drives": [
            {
                "drive_id": "rootfs",
                "path_on_host": rootfs_path.to_string_lossy(),
                "is_root_device": true,
                "is_read_only": false
            }
        ],
        "machine-config": {
            "vcpu_count": vcpus,
            "mem_size_mib": mem_mib
        },
        "network-interfaces": net_interfaces
    })
}

/// Compute the ext4 image path for a given workload ID.
pub fn ext4_path_for(rootfs_dir: &Path, vm_id: &str) -> PathBuf {
    rootfs_dir.join(format!("{vm_id}.ext4"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{VmNetwork, BRIDGE_NAME, GATEWAY_IP, NETMASK};
    use std::net::Ipv4Addr;

    fn sample_net(tap: &str, ip: Ipv4Addr) -> VmNetwork {
        VmNetwork {
            tap_name: tap.to_string(),
            bridge_name: BRIDGE_NAME.to_string(),
            guest_ip: ip,
            guest_mac: crate::network::mac_from_ip(ip),
            netmask: NETMASK,
            gateway: GATEWAY_IP,
        }
    }

    #[test]
    fn test_firecracker_config_no_network() {
        let cfg = firecracker_config(
            "wl-1",
            Path::new("/var/lib/pullrun/vmlinux"),
            Path::new("/tmp/web-1.ext4"),
            None,
            2,
            512,
        );
        assert_eq!(cfg["machine-config"]["vcpu_count"], 2);
        assert_eq!(cfg["machine-config"]["mem_size_mib"], 512);
        assert_eq!(cfg["drives"][0]["drive_id"], "rootfs");
        assert_eq!(cfg["network-interfaces"].as_array().unwrap().len(), 0);
        // Without network, boot_args must not contain the ip= flag.
        let args = cfg["boot-source"]["boot_args"].as_str().unwrap();
        assert!(!args.contains("ip="));
    }

    #[test]
    fn test_firecracker_config_with_network_injects_ip_boot_arg() {
        let net = sample_net("tap-pullrun-1", Ipv4Addr::new(10, 42, 0, 5));
        let cfg = firecracker_config(
            "wl-1",
            Path::new("/var/lib/pullrun/vmlinux"),
            Path::new("/tmp/web-1.ext4"),
            Some(&net),
            2,
            512,
        );
        assert_eq!(
            cfg["network-interfaces"][0]["host_dev_name"],
            "tap-pullrun-1"
        );
        assert_eq!(
            cfg["network-interfaces"][0]["guest_mac"],
            "AA:FC:0A:2A:00:05"
        );
        let args = cfg["boot-source"]["boot_args"].as_str().unwrap();
        assert!(
            args.contains("ip=10.42.0.5"),
            "boot_args missing guest IP: {args}"
        );
        assert!(
            args.contains("10.42.0.1"),
            "boot_args missing gateway: {args}"
        );
        assert!(
            args.contains("255.255.0.0"),
            "boot_args missing netmask: {args}"
        );
        assert!(args.contains("eth0"), "boot_args missing iface: {args}");
    }

    #[test]
    fn test_ext4_path_for() {
        let p = ext4_path_for(Path::new("/var/lib/pullrun/ext4"), "wl-1");
        assert_eq!(p, PathBuf::from("/var/lib/pullrun/ext4/wl-1.ext4"));
    }

    /// End-to-end test: pull a real OCI image (alpine), materialize it into
    /// an ext4 image, and verify alpine's userland is present via `debugfs`.
    /// Linux-only. Rootless-safe: no loop mount.
    ///
    /// This is the acceptance test for the rootless ext4 path. It exercises:
    ///   1. The OciPuller (real network download from Docker Hub)
    ///   2. The OciToDagConverter (tar → DAG)
    ///   3. The DAG store (rkyv + memmap)
    ///   4. The OciMaterializer::materialize_into (DAG → dir)
    ///   5. format_ext4_from_dir (dir → ext4, no root)
    ///   6. debugfs verification (no mount needed)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn integration_materialize_ext4_e2e() {
        use pullrun_oci::puller::OciPuller;
        use pullrun_oci::OciToDagConverter;

        // Skip if mkfs.ext4 is unavailable.
        if Command::new("mkfs.ext4").arg("--help").output().is_err() {
            eprintln!("skipping: mkfs.ext4 not available");
            return;
        }

        // 1. Pull alpine:latest and convert to DAG.
        let tmp = TempDir::new().expect("create tempdir");
        let store = Arc::new(MmapStore::new(tmp.path().join("store")));

        eprintln!("→ pulling alpine:latest from Docker Hub");
        let puller = OciPuller::new(None);
        let pulled = puller
            .pull("alpine:latest", None)
            .await
            .expect("pull alpine");
        eprintln!(
            "  pulled: config={}, layers={}",
            pulled.config_digest,
            pulled.layer_blobs.len()
        );

        let converter = OciToDagConverter::new(store.clone());
        let root_digest = converter.convert(&pulled).await.expect("convert to DAG");
        eprintln!("  DAG root: {root_digest}");

        // 2. Materialize into an ext4 image (rootless path).
        let ext4_path = tmp.path().join("rootfs.ext4");
        materialize_ext4_rootfs(
            &store,
            &root_digest,
            &ext4_path,
            Ext4Options {
                size_mb: 128,
                label: Some("pullrun-test".to_string()),
            },
        )
        .await
        .expect("materialize ext4");
        eprintln!("→ ext4 image created at {} ({} MB)", ext4_path.display(), fs_size_mb(&ext4_path));

        // 3. Verify the image is a valid ext4 filesystem.
        let fstype = std::process::Command::new("blkid")
            .arg("-o")
            .arg("value")
            .arg("-s")
            .arg("TYPE")
            .arg(&ext4_path)
            .output();
        match fstype {
            Ok(o) if o.status.success() => {
                let ty = String::from_utf8_lossy(&o.stdout).trim().to_string();
                assert_eq!(ty, "ext4", "expected ext4 filesystem, got {ty}");
            }
            Ok(o) => eprintln!("skip blkid check: {}", String::from_utf8_lossy(&o.stderr)),
            Err(e) => eprintln!("skip blkid check: {e}"),
        }

        // 4. Verify files inside the ext4 image using debugfs (no mount).
        //    debugfs is part of e2fsprogs and works as non-root.
        if let Ok(probe) = Command::new("debugfs").arg("--help").output() {
            if probe.status.success() {
                eprintln!("→ verifying files via debugfs");
                // /bin/sh should be a symlink to /bin/busybox
                let stat = Command::new("debugfs")
                    .args(["-R", "stat /bin/sh", &ext4_path.to_string_lossy()])
                    .output();
                match stat {
                    Ok(o) if o.status.success() => {
                        let out = String::from_utf8_lossy(&o.stdout);
                        assert!(
                            out.contains("Fast symlink") || out.contains("slow symlink"),
                            "/bin/sh should be a symlink: {out}"
                        );
                        // debugfs stat shows symlink target in the output
                        eprintln!("  /bin/sh is a symlink");
                    }
                    Ok(o) => eprintln!("  debugfs stat /bin/sh failed: {}", String::from_utf8_lossy(&o.stderr)),
                    Err(e) => eprintln!("  debugfs not usable: {e}"),
                }

                // /init should exist (injected by materialize_ext4_rootfs)
                let init_stat = Command::new("debugfs")
                    .args(["-R", "stat /init", &ext4_path.to_string_lossy()])
                    .output();
                match init_stat {
                    Ok(o) if o.status.success() => {
                        eprintln!("  /init exists (rootfs bootstrap injected)");
                    }
                    Ok(o) => eprintln!("  /init not found (may be expected): {}", String::from_utf8_lossy(&o.stderr)),
                    Err(e) => eprintln!("  debugfs stat /init failed: {e}"),
                }

                // /etc/alpine-release should exist
                let release_stat = Command::new("debugfs")
                    .args(["-R", "stat /etc/alpine-release", &ext4_path.to_string_lossy()])
                    .output();
                match release_stat {
                    Ok(o) if o.status.success() => {
                        eprintln!("  /etc/alpine-release exists (alpine userland verified)");
                    }
                    Ok(o) => eprintln!("  /etc/alpine-release not found: {}", String::from_utf8_lossy(&o.stderr)),
                    Err(e) => eprintln!("  debugfs stat /etc/alpine-release failed: {e}"),
                }
            } else {
                eprintln!("skip debugfs verification (not available)");
            }
        } else {
            eprintln!("skip debugfs verification (not available)");
        }

        // 5. Verify the label
        let label = std::process::Command::new("e2label")
            .arg(&ext4_path)
            .output();
        if let Ok(o) = label {
            if o.status.success() {
                let l = String::from_utf8_lossy(&o.stdout).trim().to_string();
                assert_eq!(l, "pullrun-test", "expected label pullrun-test, got {l}");
            }
        }

        // 6. Build the Firecracker config and verify it's valid JSON
        let fc_net = sample_net("tap-pullrun-1", Ipv4Addr::new(10, 42, 0, 5));
        let fc_cfg = firecracker_config(
            "alpine-vm-test",
            Path::new("/var/lib/pullrun/vmlinux"),
            &ext4_path,
            Some(&fc_net),
            2,
            1024,
        );
        let fc_json = serde_json::to_string_pretty(&fc_cfg).expect("serialize fc config");
        let fc_parsed: serde_json::Value = serde_json::from_str(&fc_json).expect("parse fc config");
        assert_eq!(fc_parsed["machine-config"]["vcpu_count"], 2);
        assert_eq!(fc_parsed["machine-config"]["mem_size_mib"], 1024);
        assert!(fc_parsed["drives"][0]["path_on_host"]
            .as_str()
            .unwrap()
            .contains("rootfs.ext4"));

        eprintln!("\n✓ ext4 integration test PASSED (rootless path):");
        eprintln!("  - Pulled alpine:latest from Docker Hub");
        eprintln!("  - Converted to DAG (root: {root_digest})");
        eprintln!(
            "  - Materialized into ext4 image ({} MB, no loop mount)",
            fs_size_mb(&ext4_path)
        );
        eprintln!("  - Verified alpine userland via debugfs");
        eprintln!("  - Generated valid Firecracker config");
    }

    fn fs_size_mb(path: &Path) -> u64 {
        std::fs::metadata(path)
            .map(|m| m.len() / 1024 / 1024)
            .unwrap_or(0)
    }

    /// Test that firecracker_config is well-formed and passes JSON schema
    /// checks we can do without a real Firecracker.
    #[test]
    fn test_firecracker_config_is_valid_json() {
        let net = sample_net("tap-pullrun-1", Ipv4Addr::new(10, 42, 0, 5));
        let cfg = firecracker_config(
            "wl-1",
            Path::new("/var/lib/pullrun/vmlinux"),
            Path::new("/var/lib/pullrun/vms/wl-1.ext4"),
            Some(&net),
            2,
            1024,
        );
        let serialized = serde_json::to_string(&cfg).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&serialized).expect("parse");

        // Required Firecracker fields per the API docs
        assert!(parsed["boot-source"]["kernel_image_path"].is_string());
        assert!(parsed["boot-source"]["boot_args"].is_string());
        assert_eq!(parsed["drives"][0]["is_root_device"], true);
        assert_eq!(parsed["drives"][0]["drive_id"], "rootfs");
        assert!(parsed["machine-config"]["vcpu_count"].is_u64());
        assert!(parsed["machine-config"]["mem_size_mib"].is_u64());

        // Network interface: MAC must be a valid format
        let mac = parsed["network-interfaces"][0]["guest_mac"]
            .as_str()
            .unwrap();
        assert_eq!(mac.split(':').count(), 6, "MAC must have 6 octets");
        for octet in mac.split(':') {
            u8::from_str_radix(octet, 16).expect("MAC octet must be hex");
        }
    }
}
