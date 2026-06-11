//! Standalone Firecracker boot smoke test.
//!
//! Builds a tiny ext4 rootfs whose `/init` script writes a marker
//! string to the serial console, then boots a real Firecracker
//! microVM. Exits 0 on success, non-zero on failure.
//!
//! Required env:
//!   PULLRUN_FC_BIN     — path to firecracker binary
//!   PULLRUN_FC_VMLINUX — path to an uncompressed vmlinux ELF
//!
//! Optional:
//!   PULLRUN_FC_TIMEOUT — seconds (default 30)
//!
//! Reproduce on Ubuntu 24.04:
//!
//!   apt-get install -y --no-install-recommends linux-image-virtual libssl-dev pkg-config
//!   curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain stable --profile minimal
//!   . ~/.cargo/env
//!
//!   vmlinuz=/boot/vmlinuz-$(uname -r)
//!   /usr/src/linux-headers-$(uname -r)/scripts/extract-vmlinux "$vmlinuz" > /tmp/vmlinux
//!
//!   curl -sSL https://github.com/firecracker-microvm/firecracker/releases/download/v1.10.1/firecracker-v1.10.1-x86_64.tgz \
//!     | tar -xz -C /tmp
//!   install -m 755 /tmp/release-v1.10.1-x86_64/firecracker-v1.10.1-x86_64 /usr/local/bin/firecracker
//!
//!   cd tools/firecracker-smoke
//!   PULLRUN_FC_BIN=/usr/local/bin/firecracker \
//!   PULLRUN_FC_VMLINUX=/tmp/vmlinux \
//!   cargo run --release

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Command as TokioCommand;

const SMOKE_MARKER: &str = "pullrun-firecracker-smoke OK";

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let exit_code = runtime.block_on(run());
    std::process::exit(exit_code);
}

async fn run() -> i32 {
    if let Some(reason) = skip_reason() {
        eprintln!("[firecracker-smoke] SKIPPED: {reason}");
        return 0;
    }

    let bin = std::env::var("PULLRUN_FC_BIN").expect("PULLRUN_FC_BIN set");
    let vmlinux = std::env::var("PULLRUN_FC_VMLINUX").expect("PULLRUN_FC_VMLINUX set");
    let timeout_secs: u64 = std::env::var("PULLRUN_FC_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let timeout = Duration::from_secs(timeout_secs);
    eprintln!("[firecracker-smoke] bin={bin} vmlinux={vmlinux} timeout={timeout_secs}s");

    let stage_dir = std::env::var("PULLRUN_FC_STAGE").unwrap_or_else(|_| "/tmp/fc-smoke-stage".into());
    let _ = std::fs::remove_dir_all(&stage_dir);
    std::fs::create_dir_all(&stage_dir).expect("stage dir");
    eprintln!("[firecracker-smoke] stage_dir={}", stage_dir);
    let tmp_path = PathBuf::from(&stage_dir);
    let api_sock = tmp_path.join("fc.sock");
    let log_path = tmp_path.join("fc.log");
    let cfg_path = tmp_path.join("vm-config.json");
    let rootfs_path = tmp_path.join("smoke.ext4");
    // Pre-create the log file. Firecracker requires --log-path target to
    // exist (it opens the file directly; missing file is a fatal error).
    std::fs::write(&log_path, b"").expect("create log file");

    if let Err(e) = build_smoke_rootfs(&rootfs_path) {
        eprintln!("[firecracker-smoke] build_smoke_rootfs failed: {e}");
        return 2;
    }
    if let Err(e) = write_fc_config(&cfg_path, Path::new(&vmlinux), &rootfs_path) {
        eprintln!("[firecracker-smoke] write_fc_config failed: {e}");
        return 2;
    }

    let mut child = match TokioCommand::new(&bin)
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
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[firecracker-smoke] spawn failed: {e}");
            return 3;
        }
    };

    // Firecracker writes the process log to --log-path, but the guest
    // VM's serial console is on stdout/stderr. We merge everything into
    // one view and tail both sources for the smoke marker.
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let out_path = tmp_path.join("serial.out");
    let err_path = tmp_path.join("serial.err");
    let _ = std::fs::write(&out_path, b"");
    let _ = std::fs::write(&err_path, b"");
    let mut out_file = std::fs::OpenOptions::new()
        .append(true)
        .open(&out_path)
        .expect("open out file");
    let mut err_file = std::fs::OpenOptions::new()
        .append(true)
        .open(&err_path)
        .expect("open err file");

    let pump_out = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    use std::io::Write;
                    let _ = out_file.write_all(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });
    let pump_err = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    use std::io::Write;
                    let _ = err_file.write_all(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });

    let start = Instant::now();
    let mut marker_seen = false;
    let mut last_line = String::new();
    let mut total_lines = 0usize;

    while start.elapsed() < timeout {
        // Check all three sources for the marker.
        for path in [&log_path, &out_path, &err_path] {
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
        // Bail early if firecracker exited cleanly without writing the marker.
        match child.try_wait() {
            Ok(Some(status)) if status.success() && start.elapsed() > Duration::from_secs(2) => break,
            Ok(Some(_)) if start.elapsed() > Duration::from_secs(2) => break,
            _ => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let _ = tokio::time::timeout(Duration::from_secs(2), pump_out).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), pump_err).await;

    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;

    eprintln!(
        "[firecracker-smoke] lines={} elapsed={:?} marker_seen={} last={}",
        total_lines,
        start.elapsed(),
        marker_seen,
        last_line
    );

    // Always dump captured output to /tmp for postmortem when failing.
    let dump_path = std::env::var("PULLRUN_FC_DUMP_PATH").unwrap_or_else(|_| "/tmp/fc-smoke.dump".into());
    let _ = std::fs::write(&dump_path, format!(
        "=== firecracker-smoke debug dump ===\nlines={} marker_seen={}\nlast_line={:?}\n",
        total_lines, marker_seen, last_line
    ));

    if marker_seen {
        eprintln!("[firecracker-smoke] PASS: marker '{SMOKE_MARKER}' observed");
        0
    } else {
        eprintln!("[firecracker-smoke] FAIL: marker '{SMOKE_MARKER}' not seen within {timeout:?}");
        1
    }
}

fn skip_reason() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("not Linux".into());
    }
    if !Path::new("/dev/kvm").exists() {
        return Some("/dev/kvm missing".into());
    }
    let bin = std::env::var("PULLRUN_FC_BIN").unwrap_or_else(|_| "firecracker".into());
    if !Path::new(&bin).exists() && which(&bin).is_none() {
        return Some(format!("firecracker binary not found (PULLRUN_FC_BIN={bin})"));
    }
    match std::env::var("PULLRUN_FC_VMLINUX") {
        Ok(p) if !p.is_empty() && Path::new(&p).exists() => {}
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

fn build_smoke_rootfs(target: &Path) -> Result<(), String> {
    let stage = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mount = stage.path().join("mnt");
    std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;

    let run = |argv: &[&str]| -> Result<(), String> {
        let out = Command::new(argv[0]).args(&argv[1..]).output().map_err(|e| e.to_string())?;
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
    run(&["mount", "-o", "loop", target.to_str().unwrap(), mount.to_str().unwrap()])?;

    // Populate with alpine minirootfs (has /bin/sh via busybox).
    // Source: dl-cdn.alpinelinux.org. Override with PULLRUN_FC_ROOTFS_TAR
    // to test offline.
    let tar_url = std::env::var("PULLRUN_FC_ROOTFS_TAR").unwrap_or_else(|_|
        "https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz".into()
    );
    eprintln!("[firecracker-smoke] fetching rootfs tarball: {tar_url}");
    let bytes = if tar_url.starts_with("http") {
        fetch_url(&tar_url)?
    } else {
        std::fs::read(&tar_url).map_err(|e| format!("read {tar_url}: {e}"))?
    };
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&mount).map_err(|e| format!("untar: {e}"))?;

    // Replace /init with our smoke marker.
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
    let _ = Command::new("chmod").args(["+x"]).arg(mount.join("init")).status();

    run(&["umount", mount.to_str().unwrap()])?;
    Ok(())
}

fn fetch_url(url: &str) -> Result<Vec<u8>, String> {
    eprintln!("[firecracker-smoke] downloading {url}");
    let resp = ureq::get(url)
        .call()
        .map_err(|e| format!("HTTP GET {url}: {e}"))?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut bytes)
        .map_err(|e| format!("read body: {e}"))?;
    Ok(bytes)
}

fn write_fc_config(
    config_path: &Path,
    kernel_path: &Path,
    rootfs_path: &Path,
) -> Result<(), String> {
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
        .map_err(|e| e.to_string())
}
