// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! macOS sandbox-group regression test.
//!
//! Runs the `apple-group-smoke` binary, which boots one VM and executes
//! several sandboxes sequentially in the same guest. Skipped unless the
//! kernel, initramfs, and a static aarch64 busybox are available.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

fn default_kernel() -> Option<PathBuf> {
    env_path("PULLRUN_KERNEL_PATH").or_else(|| {
        let home = std::env::var("HOME").ok()?;
        std::fs::read_dir(PathBuf::from(home).join(".pullrun/kernels"))
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.is_file())
    })
}

fn busybox() -> Option<PathBuf> {
    env_path("PULLRUN_GROUP_BUSYBOX").or_else(|| {
        let p = PathBuf::from("/tmp/busybox-aarch64");
        p.is_file().then_some(p)
    })
}

fn make_rootfs(busybox: &Path) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pullrun-group-rootfs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["bin", "dev", "proc", "tmp"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::copy(busybox, dir.join("bin/busybox")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        dir.join("bin/busybox"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    for applet in ["sh", "cat", "echo", "test", "ls", "sleep"] {
        std::os::unix::fs::symlink("/bin/busybox", dir.join("bin").join(applet)).unwrap();
    }
    dir
}

#[test]
fn group_runs_multiple_sandboxes_in_one_vm() {
    let Some(kernel) = default_kernel() else {
        eprintln!("skipping: no kernel (set PULLRUN_KERNEL_PATH)");
        return;
    };
    let Some(initramfs) = env_path("PULLRUN_INITRAMFS_PATH") else {
        eprintln!("skipping: set PULLRUN_INITRAMFS_PATH");
        return;
    };
    let Some(busybox) = busybox() else {
        eprintln!("skipping: set PULLRUN_GROUP_BUSYBOX");
        return;
    };

    let rootfs = make_rootfs(&busybox);
    let bin = env!("CARGO_BIN_EXE_apple-group-smoke");

    // Virtualization.framework requires the entitlement on the process.
    let signed = Command::new("codesign")
        .args([
            "-f",
            "-s",
            "-",
            "--entitlements",
            "../../tools/apple-virt-smoke/virt.entitlements",
        ])
        .arg(bin)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(signed, "failed to codesign {bin}");

    let output = Command::new(bin)
        .env("PULLRUN_KERNEL_PATH", &kernel)
        .env("PULLRUN_INITRAMFS_PATH", &initramfs)
        .env("PULLRUN_GROUP_ROOTFS", &rootfs)
        .output()
        .expect("run apple-group-smoke");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "group smoke failed: stdout={stdout} stderr={stderr}"
    );
    assert!(stdout.contains("sandbox1 exit=Some(0) stdout=\"one\\n\""));
    assert!(stdout.contains("sandbox2 exit=Some(0) stdout=\"two\\n\""));
    assert!(stdout.contains("sandbox3 isolation exit=Some(0)"));
    assert!(stdout.contains("sandbox4 exit=Some(42)"));

    let _ = std::fs::remove_dir_all(&rootfs);
}
