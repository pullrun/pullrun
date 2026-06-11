//! End-to-end Firecracker boot smoke test.
//!
//! This test boots a real Firecracker microVM with a tiny Alpine ext4
//! rootfs whose `/init` script writes a marker string to the serial
//! console. We spawn `firecracker` directly with `--config-file` and
//! `--log-path`, pump its stdout/stderr to files, then tail all three
//! sources (log + serial.out + serial.err) for the marker.
//!
//! ## Skip conditions
//!
//! The test is automatically skipped (returns `Ok(())`) when:
//! - not on Linux (`/dev/kvm` is a Linux concept)
//! - `/dev/kvm` is not readable
//! - `firecracker` binary is not on PATH (or `PULLRUN_FC_BIN` env var unset)
//! - `PULLRUN_FC_VMLINUX` env var is unset (no vmlinux staged)
//!
//! ## Required staging
//!
//! The CI / dev environment must stage:
//! - `PULLRUN_FC_BIN`     — path to a firecracker v1.10+ binary
//! - `PULLRUN_FC_VMLINUX` — path to an uncompressed vmlinux ELF that
//!                         boots on this host (e.g. extracted from
//!                         `linux-image-virtual` or the host kernel)
//!
//! To reproduce on the staging host (Ubuntu 24.04):
//!
//! ```bash
//! apt-get install -y linux-image-virtual
//! vmlinuz=/boot/vmlinuz-$(uname -r)
//! vmlinux=/tmp/vmlinux
//! /usr/src/linux-headers-$(uname -r)/scripts/extract-vmlinux "$vmlinuz" > "$vmlinux"
//! curl -sSL https://github.com/firecracker-microvm/firecracker/releases/download/v1.10.1/firecracker-v1.10.1-x86_64.tgz | tar -xz
//! install -m 755 release-v1.10.1-x86_64/firecracker-v1.10.1-x86_64 /usr/local/bin/firecracker
//! export PULLRUN_FC_BIN=/usr/local/bin/firecracker
//! export PULLRUN_FC_VMLINUX=$vmlinux
//! ```
//!
//! Run with: `cargo test -p pullrun-vm --test firecracker_boot -- --include-ignored --nocapture`
//! (the test is `#[ignore]` by default so plain `cargo test` skips it).

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Command as TokioCommand;

const SMOKE_MARKER: &str = "pullrun-firecracker-smoke OK";
const BOOT_TIMEOUT: Duration = Duration::from_secs(60);

fn skip_if_unavailable() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("not Linux".into());
    }
    if !std::path::Path::new("/dev/kvm").exists() {
        return Some("/dev/kvm missing".into());
    }
    let bin = std::env::var("PULLRUN_FC_BIN").unwrap_or_else(|_| "firecracker".into());
    if !std::path::Path::new(&bin).exists() && which(&bin).is_none() {
        return Some(format!(
            "firecracker binary not found (PULLRUN_FC_BIN={bin})"
        ));
    }
    match std::env::var("PULLRUN_FC_VMLINUX") {
        Ok(p) if !p.is_empty() && std::path::Path::new(&p).exists() => {}
        Ok(p) => return Some(format!("PULLRUN_FC_VMLINUX={p} not found")),
        Err(_) => return Some("PULLRUN_FC_VMLINUX not set".into()),
    }
    None
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        return Some(PathBuf::from(bin));
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Build a tiny ext4 rootfs by fetching the Alpine minirootfs (which
/// contains busybox `/bin/sh`) and overwriting `/init` with our smoke
/// script. Override the source with `PULLRUN_FC_ROOTFS_TAR` to test
/// offline.
async fn build_smoke_rootfs(
    target: &std::path::Path,
    stage_dir: &std::path::Path,
) -> Result<(), String> {
    let mount = stage_dir.join("mnt");
    std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;

    let run = |argv: &[&str]| -> Result<(), String> {
        let out = Command::new(argv[0])
            .args(&argv[1..])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "{argv:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(())
    };

    run(&["truncate", "-s", "128M", target.to_str().unwrap()])?;
    run(&["mkfs.ext4", "-F", "-L", "pullrun", target.to_str().unwrap()])?;
    run(&[
        "mount",
        "-o",
        "loop",
        target.to_str().unwrap(),
        mount.to_str().unwrap(),
    ])?;

    let tar_url = std::env::var("PULLRUN_FC_ROOTFS_TAR").unwrap_or_else(|_|
        "https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz".into()
    );
    eprintln!("[firecracker_boot] fetching rootfs tarball: {tar_url}");
    let bytes = if tar_url.starts_with("http") {
        fetch_url(&tar_url)?
    } else {
        std::fs::read(&tar_url).map_err(|e| format!("read {tar_url}: {e}"))?
    };
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&mount).map_err(|e| format!("untar: {e}"))?;

    let init_script = r#"#!/bin/sh
mount -t proc none /proc 2>/dev/null
mount -t sysfs none /sys 2>/dev/null
mount -t devtmpfs none /dev 2>/dev/null
echo "pullrun-firecracker-smoke OK at $(date)" > /dev/ttyS0
echo "pullrun-firecracker-smoke OK at $(date)" > /dev/console
sync
echo o > /proc/sysrq-trigger 2>/dev/null
sleep 1
"#;
    std::fs::write(mount.join("init"), init_script).map_err(|e| e.to_string())?;
    let _ = Command::new("chmod")
        .args(["+x"])
        .arg(mount.join("init"))
        .status();

    run(&["umount", mount.to_str().unwrap()])?;
    Ok(())
}

fn fetch_url(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| format!("HTTP GET {url}: {e}"))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes)
        .map_err(|e| format!("read body: {e}"))?;
    Ok(bytes)
}

fn write_fc_config(
    config_path: &std::path::Path,
    kernel_path: &std::path::Path,
    rootfs_path: &std::path::Path,
) -> Result<(), std::io::Error> {
    let cfg = serde_json::json!({
        "boot-source": {
            "kernel_image_path": kernel_path.to_string_lossy(),
            "boot_args": "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init"
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
            "vcpu_count": 1,
            "mem_size_mib": 256,
            "smt": false
        }
    });
    std::fs::write(config_path, serde_json::to_string_pretty(&cfg).unwrap())
}

#[tokio::test]
#[ignore = "requires /dev/kvm, firecracker binary, PULLRUN_FC_VMLINUX, and network for alpine rootfs"]
async fn firecracker_boot_smoke() {
    if let Some(reason) = skip_if_unavailable() {
        eprintln!("[firecracker_boot_smoke] SKIPPED: {reason}");
        return;
    }

    let bin = std::env::var("PULLRUN_FC_BIN").expect("PULLRUN_FC_BIN set");
    let vmlinux = std::env::var("PULLRUN_FC_VMLINUX").expect("PULLRUN_FC_VMLINUX set");
    let timeout_secs: u64 = std::env::var("PULLRUN_FC_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let timeout = Duration::from_secs(timeout_secs);
    eprintln!("[firecracker_boot_smoke] bin={bin} vmlinux={vmlinux} timeout={timeout_secs}s");

    let stage_dir =
        std::env::var("PULLRUN_FC_STAGE").unwrap_or_else(|_| "/tmp/fc-smoke-stage".into());
    let _ = std::fs::remove_dir_all(&stage_dir);
    std::fs::create_dir_all(&stage_dir).expect("stage dir");
    eprintln!("[firecracker_boot_smoke] stage_dir={stage_dir}");

    let tmp_path = PathBuf::from(&stage_dir);
    let api_sock = tmp_path.join("fc.sock");
    let log_path = tmp_path.join("fc.log");
    let cfg_path = tmp_path.join("vm-config.json");
    let rootfs_path = tmp_path.join("smoke.ext4");
    let serial_out = tmp_path.join("serial.out");
    let serial_err = tmp_path.join("serial.err");
    // Pre-create the log file. Firecracker requires --log-path target
    // to exist (it opens the file directly; missing file is fatal).
    std::fs::write(&log_path, b"").expect("create log file");

    build_smoke_rootfs(&rootfs_path, &tmp_path)
        .await
        .expect("build smoke rootfs");
    write_fc_config(&cfg_path, std::path::Path::new(&vmlinux), &rootfs_path)
        .expect("write fc config");

    let mut child = TokioCommand::new(&bin)
        .args([
            "--api-sock",
            api_sock.to_str().unwrap(),
            "--config-file",
            cfg_path.to_str().unwrap(),
            "--log-path",
            log_path.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn firecracker");

    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let _ = std::fs::write(&serial_out, b"");
    let _ = std::fs::write(&serial_err, b"");
    let out_file_path = serial_out.clone();
    let err_file_path = serial_err.clone();
    let pump_out = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf = [0u8; 4096];
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&out_file_path)
            .expect("open out file");
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    use std::io::Write;
                    let _ = file.write_all(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });
    let pump_err = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = [0u8; 4096];
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&err_file_path)
            .expect("open err file");
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    use std::io::Write;
                    let _ = file.write_all(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });

    let start = Instant::now();
    let mut marker_seen = false;
    let mut last_line = String::new();
    let mut total_lines = 0usize;

    while start.elapsed() < BOOT_TIMEOUT.min(timeout) {
        for path in [&log_path, &serial_out, &serial_err] {
            if let Ok(contents) = std::fs::read_to_string(path) {
                total_lines = contents.lines().count();
                if let Some(last) = contents.lines().last() {
                    last_line = last.to_string();
                }
                if contents.contains(SMOKE_MARKER) {
                    marker_seen = true;
                    break;
                }
            }
        }
        if marker_seen {
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() && start.elapsed() > Duration::from_secs(2) => {
                break
            }
            Ok(Some(_)) if start.elapsed() > Duration::from_secs(2) => break,
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), pump_out).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), pump_err).await;

    eprintln!(
        "[firecracker_boot_smoke] lines={} elapsed={:?} marker_seen={} last={}",
        total_lines,
        start.elapsed(),
        marker_seen,
        last_line
    );

    assert!(
        marker_seen,
        "firecracker boot smoke test failed: marker '{SMOKE_MARKER}' not seen within {BOOT_TIMEOUT:?}; last line: {last_line:?}"
    );
}
