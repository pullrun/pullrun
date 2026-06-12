// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end Firecracker + network smoke test.
//!
//! This test boots a real Firecracker microVM with a tiny Alpine ext4
//! rootfs configured to:
//!   1. Bring up `eth0` using the kernel `ip=` boot arg
//!   2. Start a single-shot `nc -l` HTTP server on port 8080
//!   3. Reply with a marker string and exit
//!
//! On the host, we connect directly to the guest's IP:8080 over the
//! bridge (the same path the ProxyNetwork inbound listeners will use
//! in production) and assert we get the marker back.
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
//! See `firecracker_boot.rs` for the full staging recipe. The same
//! `PULLRUN_FC_BIN`, `PULLRUN_FC_VMLINUX`, and `PULLRUN_FC_ROOTFS_TAR`
//! env vars are read.
//!
//! Run with: `cargo test -p pullrun-vm --test firecracker_network -- --include-ignored --nocapture`
//! (the test is `#[ignore]` by default so plain `cargo test` skips it).

#![cfg(target_os = "linux")]

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;

use pullrun_vm::{create_tap, ensure_bridge, teardown_tap, GATEWAY_IP, NETMASK};

const SMOKE_MARKER: &str = "pullrun-vm-net OK";
const HTTP_PORT: u16 = 8080;
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
    // Probe tap creation; if we can't, we're in a sandbox without
    // CAP_NET_ADMIN.
    let probe = Command::new("ip")
        .args(["tuntap", "add", "tap-np", "mode", "tap"])
        .output();
    match probe {
        Ok(o) if o.status.success() => {
            let _ = Command::new("ip").args(["link", "del", "tap-np"]).output();
        }
        Ok(o) => {
            return Some(format!(
                "cannot create tap (no CAP_NET_ADMIN?): {}",
                String::from_utf8_lossy(&o.stderr)
            ));
        }
        Err(e) => return Some(format!("ip not available: {e}")),
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
            return Some(PathBuf::from(bin));
        }
    }
    None
}

async fn build_net_rootfs(
    target: &std::path::Path,
    stage_dir: &std::path::Path,
    guest_ip: Ipv4Addr,
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
    run(&[
        "mkfs.ext4",
        "-F",
        "-L",
        "pullrun-net",
        target.to_str().unwrap(),
    ])?;
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
    eprintln!("[firecracker_network] fetching rootfs tarball: {tar_url}");
    let bytes = if tar_url.starts_with("http") {
        fetch_url(&tar_url)?
    } else {
        std::fs::read(&tar_url).map_err(|e| format!("read {tar_url}: {e}"))?
    };
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&mount).map_err(|e| format!("untar: {e}"))?;

    // Init script: self-configure eth0 (in case kernel ip= arg didn't
    // fire), then start a one-shot HTTP server.
    let init_script = format!(
        r#"#!/bin/sh
set +e
mount -t proc none /proc 2>/dev/null
mount -t sysfs none /sys 2>/dev/null
mount -t devtmpfs none /dev 2>/dev/null
sleep 1
if ! ip -4 -o addr show eth0 2>/dev/null | grep -q inet; then
  echo "[guest] configuring eth0 manually"
  ifconfig eth0 {guest_ip} netmask 255.255.0.0 up 2>/dev/null \
    || busybox ifconfig eth0 {guest_ip} netmask 255.255.0.0 up
  route add default gw 10.42.0.1 2>/dev/null \
    || busybox route add default gw 10.42.0.1
fi
echo "[guest] eth0: $(ip -4 -o addr show eth0 2>/dev/null | awk '{{print $4}}')"
if command -v httpd >/dev/null 2>&1; then
  mkdir -p /var/www
  printf 'HTTP/1.0 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{marker}' > /var/www/index.html
  httpd -f -p 0.0.0.0:{port} -h /var/www 2>/dev/null
  sleep 8
else
  for i in 1 2 3 4 5 6 7 8 9 10; do
    {{ printf 'HTTP/1.0 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{marker}'; sleep 1; }} | nc -l -p {port} 2>/dev/null
  done
fi
echo "[guest] /init done"
sync
echo o > /proc/sysrq-trigger 2>/dev/null
sleep 1
"#,
        port = HTTP_PORT,
        len = SMOKE_MARKER.len(),
        marker = SMOKE_MARKER,
        guest_ip = guest_ip,
    );
    std::fs::write(mount.join("init"), &init_script).map_err(|e| e.to_string())?;
    let _ = Command::new("chmod")
        .args(["+x"])
        .arg(mount.join("init"))
        .status();

    // The init script needs the guest IP printed so we can see in
    // serial that the kernel ip= arg took effect.
    eprintln!("[firecracker_network] target guest IP: {guest_ip}");

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
    tap_name: &str,
    guest_ip: Ipv4Addr,
    mac: &str,
) -> Result<(), std::io::Error> {
    let boot_args = format!(
        "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init ip={}::{}:{}::eth0:off",
        guest_ip, GATEWAY_IP, NETMASK
    );
    let cfg = serde_json::json!({
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
            "vcpu_count": 1,
            "mem_size_mib": 256,
            "smt": false
        },
        "network-interfaces": [
            {
                "iface_id": "eth0",
                "guest_mac": mac,
                "host_dev_name": tap_name
            }
        ]
    });
    std::fs::write(config_path, serde_json::to_string_pretty(&cfg).unwrap())
}

#[tokio::test]
#[ignore = "requires /dev/kvm, firecracker binary, PULLRUN_FC_VMLINUX, /dev/net/tun (CAP_NET_ADMIN), and network for alpine rootfs"]
async fn firecracker_network_smoke() {
    if let Some(reason) = skip_if_unavailable() {
        eprintln!("[firecracker_network] SKIPPED: {reason}");
        return;
    }

    let bin = std::env::var("PULLRUN_FC_BIN").expect("PULLRUN_FC_BIN set");
    let vmlinux = std::env::var("PULLRUN_FC_VMLINUX").expect("PULLRUN_FC_VMLINUX set");
    eprintln!("[firecracker_network] bin={bin} vmlinux={vmlinux}");

    let stage_dir =
        std::env::var("PULLRUN_FC_STAGE_NET").unwrap_or_else(|_| "/tmp/fc-net-stage".into());
    let _ = std::fs::remove_dir_all(&stage_dir);
    std::fs::create_dir_all(&stage_dir).expect("stage dir");
    eprintln!("[firecracker_network] stage_dir={stage_dir}");

    let tmp_path = PathBuf::from(&stage_dir);
    let api_sock = tmp_path.join("fc.sock");
    let log_path = tmp_path.join("fc.log");
    let cfg_path = tmp_path.join("vm-config.json");
    let rootfs_path = tmp_path.join("smoke.ext4");
    let serial_out = tmp_path.join("serial.out");
    let serial_err = tmp_path.join("serial.err");
    std::fs::write(&log_path, b"").expect("create log file");

    // 1. Plumb the host-side network before booting.
    ensure_bridge().expect("ensure_bridge");
    let tap_name = "tap-pullrun-net";
    let guest_ip = Ipv4Addr::new(10, 42, 88, 88);
    let (vm_net, _tap_fd) = create_tap(tap_name, guest_ip).expect("create_tap");
    eprintln!(
        "[firecracker_network] tap={} ip={} mac={}",
        vm_net.tap_name, vm_net.guest_ip, vm_net.guest_mac
    );

    // 2. Build a rootfs whose /init starts a one-shot HTTP server.
    build_net_rootfs(&rootfs_path, &tmp_path, guest_ip)
        .await
        .expect("build net rootfs");
    write_fc_config(
        &cfg_path,
        std::path::Path::new(&vmlinux),
        &rootfs_path,
        tap_name,
        guest_ip,
        &vm_net.guest_mac,
    )
    .expect("write fc config");

    // 4. Spawn firecracker.
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
        let mut s = stdout;
        let mut buf = [0u8; 4096];
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&out_file_path)
            .expect("open out file");
        loop {
            match s.read(&mut buf).await {
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
        let mut s = stderr;
        let mut buf = [0u8; 4096];
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&err_file_path)
            .expect("open err file");
        loop {
            match s.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    use std::io::Write;
                    let _ = file.write_all(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    });

    // 5. Wait for the VM to come up, then probe the guest's HTTP
    //    server directly. We connect from the host to <guest_ip>:8080
    //    over the bridge — the same path the ProxyNetwork inbound
    //    listeners will use in production.
    let start = Instant::now();
    let mut body_seen = false;
    let mut last_err = String::new();
    let guest_target = format!("{guest_ip}:{HTTP_PORT}");

    while start.elapsed() < BOOT_TIMEOUT {
        if start.elapsed() > Duration::from_secs(3) {
            match tokio::time::timeout(Duration::from_secs(2), async {
                let mut sock = tokio::net::TcpStream::connect(&guest_target)
                    .await
                    .map_err(|e| e.to_string())?;
                sock.write_all(b"GET / HTTP/1.0\r\nHost: x\r\n\r\n")
                    .await
                    .map_err(|e| e.to_string())?;
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.map_err(|e| e.to_string())?;
                Ok::<Vec<u8>, String>(buf[..n].to_vec())
            })
            .await
            {
                Ok(Ok(body)) => {
                    let s = String::from_utf8_lossy(&body);
                    eprintln!(
                        "[firecracker_network] host->guest{guest_target} got {} bytes: {:?}",
                        body.len(),
                        &s[..s.len().min(200)]
                    );
                    if s.contains(SMOKE_MARKER) {
                        body_seen = true;
                        break;
                    }
                    last_err = format!("no marker in body: {s}");
                }
                Ok(Err(e)) => last_err = e,
                Err(_) => last_err = "connect timeout".into(),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), pump_out).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), pump_err).await;

    // 6. Tear down host-side network.
    teardown_tap(tap_name, Some(_tap_fd)).ok();

    eprintln!(
        "[firecracker_network] body_seen={} elapsed={:?} last_err={}",
        body_seen,
        start.elapsed(),
        last_err
    );

    if !body_seen {
        eprintln!("[firecracker_network] serial.out tail:");
        if let Ok(contents) = std::fs::read_to_string(&serial_out) {
            for line in contents
                .lines()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                eprintln!("    {line}");
            }
        }
    }

    assert!(
        body_seen,
        "VM network smoke test failed: marker '{}' not seen via host proxy. last_err={}",
        SMOKE_MARKER, last_err
    );
}
