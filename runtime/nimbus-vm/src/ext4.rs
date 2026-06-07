use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use nimbus_oci::OciMaterializer;
use nimbus_store::{Digest, MmapStore};
use tempfile::TempDir;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::network::VmNetwork;

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
    #[error("mount failed: {0}")]
    MountFailed(String),
    #[error("umount failed: {0}")]
    UmountFailed(String),
    #[error("loop device setup failed: {0}")]
    LoopSetupFailed(String),
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
/// The flow:
/// 1. Create a sparse file of `size_mb` MB
/// 2. Format it as ext4
/// 3. Mount via loop device
/// 4. Use the existing OciMaterializer to copy the userland files
/// 5. Unmount
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

    create_sparse_file(output_path, options.size_mb)?;
    format_ext4(output_path, options.label.as_deref())?;

    let mount_dir = TempDir::new()?;
    mount_loop(output_path, mount_dir.path())?;

    let materialize_result = OciMaterializer::new(store)
        .materialize_into(root_digest, mount_dir.path())
        .await;

    if let Err(e) = materialize_result {
        return Err(Ext4Error::Materialization(format!("{e}")));
    }

    // Inject a default /init if the OCI image doesn't have one.
    // Container images (alpine, ubuntu, etc.) have ENTRYPOINT/CMD
    // but no /init executable. The kernel boot args pass init=/init,
    // so we need one present for the VM to boot successfully.
    let init_path = mount_dir.path().join("init");
    if !init_path.exists() {
        info!("OCI image has no /init, injecting default");
        std::fs::write(
            &init_path,
            b"#!/bin/sh\nexec /bin/sh\n",
        )?;
        // Make it executable (S_IRWXU | S_IRGRP | S_IXGRP | S_IROTH | S_IXOTH)
        std::fs::set_permissions(&init_path, PermissionsExt::from_mode(0o755))?;
    }

    let umount_result = umount(mount_dir.path());
    umount_result?;

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

fn format_ext4(path: &Path, label: Option<&str>) -> Result<(), Ext4Error> {
    let mut cmd = Command::new("mkfs.ext4");
    cmd.arg("-F"); // force (file exists)
    cmd.arg("-q"); // quiet
    if let Some(l) = label {
        cmd.args(["-L", l]);
    }
    cmd.arg(path);

    let output = cmd
        .output()
        .map_err(|e| Ext4Error::MkfsNotFound(format!("mkfs.ext4 not found: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Ext4Error::MkfsFailed(format!("mkfs.ext4 failed: {stderr}")));
    }

    debug!(path = %path.display(), "ext4 filesystem created");
    Ok(())
}

fn mount_loop(image: &Path, target: &Path) -> Result<(), Ext4Error> {
    let output = Command::new("mount")
        .args(["-o", "loop"])
        .arg(image)
        .arg(target)
        .output()
        .map_err(|e| Ext4Error::MountFailed(format!("mount invocation failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Ext4Error::MountFailed(format!("mount failed: {stderr}")));
    }

    debug!(image = %image.display(), target = %target.display(), "loop-mounted");
    Ok(())
}

fn umount(target: &Path) -> Result<(), Ext4Error> {
    let output = Command::new("umount")
        .arg(target)
        .output()
        .map_err(|e| Ext4Error::UmountFailed(format!("umount invocation failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Ext4Error::UmountFailed(format!("umount failed: {stderr}")));
    }

    debug!(target = %target.display(), "unmounted");
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
            Path::new("/var/lib/nimbus/vmlinux"),
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
        let net = sample_net("tap-nimbus-1", Ipv4Addr::new(10, 42, 0, 5));
        let cfg = firecracker_config(
            "wl-1",
            Path::new("/var/lib/nimbus/vmlinux"),
            Path::new("/tmp/web-1.ext4"),
            Some(&net),
            2,
            512,
        );
        assert_eq!(
            cfg["network-interfaces"][0]["host_dev_name"],
            "tap-nimbus-1"
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
        let p = ext4_path_for(Path::new("/var/lib/nimbus/ext4"), "wl-1");
        assert_eq!(p, PathBuf::from("/var/lib/nimbus/ext4/wl-1.ext4"));
    }

    /// End-to-end test: pull a real OCI image (alpine), materialize it into
    /// an ext4 image, mount the image, and verify alpine's userland is
    /// present. Linux-only (uses mkfs.ext4 + mount -o loop).
    ///
    /// This is the acceptance test for Phase 2. It exercises:
    ///   1. The OciPuller (real network download from Docker Hub)
    ///   2. The OciToDagConverter (tar → DAG)
    ///   3. The DAG store (rkyv + memmap)
    ///   4. The OciMaterializer::materialize_into (DAG → ext4)
    ///   5. Filesystem-level verification (blkid, mount, ls, stat)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn integration_materialize_ext4_e2e() {
        use nimbus_oci::puller::OciPuller;
        use nimbus_oci::OciToDagConverter;

        // Skip if mkfs.ext4 or mount is unavailable.
        if Command::new("mkfs.ext4").arg("--help").output().is_err() {
            eprintln!("skipping: mkfs.ext4 not available");
            return;
        }
        if Command::new("mount").arg("--help").output().is_err() {
            eprintln!("skipping: mount not available");
            return;
        }

        // 1. Pull alpine:latest and convert to DAG.
        let tmp = TempDir::new().expect("create tempdir");
        let store = MmapStore::new(tmp.path().join("store"));

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

        let converter = OciToDagConverter::new(&store);
        let root_digest = converter.convert(&pulled).await.expect("convert to DAG");
        eprintln!("  DAG root: {root_digest}");

        // 2. Materialize into an ext4 image.
        let ext4_path = tmp.path().join("rootfs.ext4");
        materialize_ext4_rootfs(
            &store,
            &root_digest,
            &ext4_path,
            Ext4Options {
                size_mb: 128,
                label: Some("nimbus-test".to_string()),
            },
        )
        .await
        .expect("materialize ext4");
        eprintln!("→ ext4 image created at {}", ext4_path.display());

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

        // 4. Mount the ext4 image and verify alpine's userland.
        let mount = TempDir::new().expect("mount tempdir");
        mount_loop(&ext4_path, mount.path()).expect("mount loop");
        eprintln!("→ mounted at {}", mount.path().display());

        // Alpine ships /bin/sh, /etc/alpine-release, /lib, /usr
        let bin_sh = mount.path().join("bin/sh");
        let sm = bin_sh.symlink_metadata().expect("bin/sh symlink_metadata");
        assert!(
            sm.is_symlink(),
            "/bin/sh should exist as a symlink in materialized alpine rootfs"
        );
        let sh_target = std::fs::read_link(&bin_sh).expect("read /bin/sh target");
        assert_eq!(
            sh_target.to_string_lossy(),
            "/bin/busybox",
            "/bin/sh should target /bin/busybox"
        );

        let busybox = mount.path().join("bin/busybox");
        let busybox_size = std::fs::metadata(&busybox).unwrap().len();
        eprintln!("  /bin/busybox size: {busybox_size} bytes");
        assert!(
            busybox_size > 100_000,
            "/bin/busybox should be the real busybox binary"
        );

        let etc_release = mount.path().join("etc/alpine-release");
        assert!(etc_release.exists(), "/etc/alpine-release should exist");
        let release = std::fs::read_to_string(&etc_release).expect("read alpine-release");
        eprintln!("  alpine-release: {release}");

        let etc_release = mount.path().join("etc/alpine-release");
        assert!(etc_release.exists(), "/etc/alpine-release should exist");
        let release = std::fs::read_to_string(&etc_release).expect("read alpine-release");
        eprintln!("  alpine-release: {release}");

        let lib_dir = mount.path().join("lib");
        assert!(lib_dir.is_dir(), "/lib should be a directory");

        // 5. Verify the label
        let label = std::process::Command::new("e2label")
            .arg(&ext4_path)
            .output();
        if let Ok(o) = label {
            if o.status.success() {
                let l = String::from_utf8_lossy(&o.stdout).trim().to_string();
                assert_eq!(l, "nimbus-test", "expected label nimbus-test, got {l}");
            }
        }

        // 6. Build the Firecracker config and verify it's valid JSON
        let fc_net = sample_net("tap-nimbus-1", Ipv4Addr::new(10, 42, 0, 5));
        let fc_cfg = firecracker_config(
            "alpine-vm-test",
            Path::new("/var/lib/nimbus/vmlinux"),
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

        // 7. Unmount
        umount(mount.path()).expect("umount");
        eprintln!("→ unmounted");

        eprintln!("\n✓ ext4 integration test PASSED:");
        eprintln!("  - Pulled alpine:latest from Docker Hub");
        eprintln!("  - Converted to DAG (root: {root_digest})");
        eprintln!(
            "  - Materialized into ext4 image ({} MB)",
            fs_size_mb(&ext4_path)
        );
        eprintln!("  - Mounted and verified /bin/sh + /etc/alpine-release");
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
        let net = sample_net("tap-nimbus-1", Ipv4Addr::new(10, 42, 0, 5));
        let cfg = firecracker_config(
            "wl-1",
            Path::new("/var/lib/nimbus/vmlinux"),
            Path::new("/var/lib/nimbus/vms/wl-1.ext4"),
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
