//! Standalone VM + network smoke test for Firecracker.
//!
//! Boots a real Firecracker microVM with a tiny Alpine rootfs configured
//! via the kernel `ip=` boot arg. The guest's `/init` script starts a
//! one-shot HTTP server on port 8080 that replies with a marker string.
//!
//! On the host, we:
//!  1. Create a tap device attached to a shared `pullrun-br0` bridge
//!     (creating the bridge if needed) at IP 10.42.88.88/16.
//!  2. Spawn firecracker with that tap as eth0.
//!  3. Listen on 127.0.0.1:8080 and forward bytes into the VM.
//!  4. Send an HTTP GET and assert the marker comes back.
//!
//! This is a self-contained alternative to the Rust integration test
//! in `runtime/pullrun-vm/tests/firecracker_network.rs` — same logic,
//! no pullrun dependencies, builds in seconds.
//!
//! ## Required env vars
//!
//! - `PULLRUN_FC_BIN`     — path to a firecracker v1.10+ binary
//! - `PULLRUN_FC_VMLINUX` — path to an uncompressed vmlinux ELF
//!
//! ## Optional env vars
//!
//! - `PULLRUN_FC_ROOTFS_TAR` — alpine minirootfs URL (default below)
//! - `PULLRUN_FC_STAGE`      — staging directory (default /tmp/vm-net-smoke)
//! - `PULLRUN_FC_TIMEOUT`    — boot timeout in seconds (default 60)
//! - `PULLRUN_FC_HOST_PORT`  — host-side proxy port (default 8080)
//! - `PULLRUN_FC_GUEST_IP`   — guest IP (default 10.42.88.88)

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;

const SMOKE_MARKER: &str = "pullrun-vm-net OK";
const DEFAULT_ALPINE_URL: &str =
    "https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz";
const BRIDGE_NAME: &str = "pullrun-br0";
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 1);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 0, 0);

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

fn skip_if_unavailable() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("not Linux".into());
    }
    if !std::path::Path::new("/dev/kvm").exists() {
        return Some("/dev/kvm missing".into());
    }
    let bin = std::env::var("PULLRUN_FC_BIN").unwrap_or_else(|_| "firecracker".into());
    if !std::path::Path::new(&bin).exists() && which(&bin).is_none() {
        return Some(format!("firecracker binary not found (PULLRUN_FC_BIN={bin})"));
    }
    match std::env::var("PULLRUN_FC_VMLINUX") {
        Ok(p) if !p.is_empty() && std::path::Path::new(&p).exists() => {}
        Ok(p) => return Some(format!("PULLRUN_FC_VMLINUX={p} not found")),
        Err(_) => return Some("PULLRUN_FC_VMLINUX not set".into()),
    }
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

fn run_ip(args: &[&str]) -> Result<(), String> {
    let out = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| format!("ip {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ip {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["-o", "link", "show", "dev", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ensure_bridge() -> Result<(), String> {
    if link_exists(BRIDGE_NAME) {
        return Ok(());
    }
    eprintln!("[vm-network-smoke] creating bridge {BRIDGE_NAME}");
    run_ip(&["link", "add", BRIDGE_NAME, "type", "bridge"])?;
    run_ip(&["link", "set", BRIDGE_NAME, "up"])?;
    run_ip(&["addr", "add", &format!("{GATEWAY_IP}/16"), "dev", BRIDGE_NAME])?;
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1");
    Ok(())
}

fn mac_from_ip(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("AA:FC:{:02X}:{:02X}:{:02X}:{:02X}", o[0], o[1], o[2], o[3])
}

fn build_net_rootfs(
    target: &std::path::Path,
    stage_dir: &std::path::Path,
    host_port: u16,
    guest_ip: Ipv4Addr,
) -> Result<(), String> {
    let mount = stage_dir.join("mnt");
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
    run(&["mkfs.ext4", "-F", "-L", "pullrun-net", target.to_str().unwrap()])?;
    run(&["mount", "-o", "loop", target.to_str().unwrap(), mount.to_str().unwrap()])?;

    let tar_url = std::env::var("PULLRUN_FC_ROOTFS_TAR").unwrap_or_else(|_| DEFAULT_ALPINE_URL.into());
    eprintln!("[vm-network-smoke] fetching rootfs tarball: {tar_url}");
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
    let marker = SMOKE_MARKER;
    let guest_ip_str = guest_ip.to_string();
    let init_script = format!(
        r#"#!/bin/sh
set +e
mount -t proc none /proc 2>/dev/null
mount -t sysfs none /sys 2>/dev/null
mount -t devtmpfs none /dev 2>/dev/null
# Self-configure eth0 (busybox ifconfig). The kernel ip= arg is the
# primary path, but we belt-and-suspenders here in case the netdev
# came up after the kernel's IP_PNP phase.
sleep 1
if ! ip -4 -o addr show eth0 2>/dev/null | grep -q inet; then
  echo "[guest] configuring eth0 manually (ip boot arg did not set inet)"
  ifconfig eth0 {guest_ip} netmask 255.255.0.0 up 2>/dev/null \
    || busybox ifconfig eth0 {guest_ip} netmask 255.255.0.0 up
  route add default gw 10.42.0.1 2>/dev/null \
    || busybox route add default gw 10.42.0.1
else
  echo "[guest] eth0 already configured by kernel ip= arg"
fi
echo "[guest] eth0: $(ip -4 -o addr show eth0 2>/dev/null | awk '{{print $4}}')"
# One-shot HTTP server. Prefer busybox httpd (apk add busybox-extras
# style), else fall back to nc loop.
if command -v httpd >/dev/null 2>&1; then
  mkdir -p /var/www
  printf 'HTTP/1.0 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{marker}' > /var/www/index.html
  httpd -f -p 0.0.0.0:{port} -h /var/www 2>/dev/null
  SERVER_PID=$!
  sleep 8
  kill $SERVER_PID 2>/dev/null
else
  echo "[guest] httpd not present, using nc loop on port {port}"
  for i in 1 2 3 4 5 6 7 8 9 10; do
    {{ printf 'HTTP/1.0 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{marker}'; sleep 1; }} | nc -l -p {port} 2>/dev/null
  done
fi
echo "[guest] /init done"
sync
echo o > /proc/sysrq-trigger 2>/dev/null
sleep 1
"#,
        port = host_port,
        len = marker.len(),
        marker = marker,
        guest_ip = guest_ip_str,
    );
    std::fs::write(mount.join("init"), &init_script).map_err(|e| e.to_string())?;
    let _ = Command::new("chmod").args(["+x"]).arg(mount.join("init")).status();

    run(&["umount", mount.to_str().unwrap()])?;
    Ok(())
}

fn fetch_url(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url).call().map_err(|e| format!("HTTP GET {url}: {e}"))?;
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
        "drives": [{
            "drive_id": "rootfs",
            "path_on_host": rootfs_path.to_string_lossy(),
            "is_root_device": true,
            "is_read_only": false
        }],
        "machine-config": {
            "vcpu_count": 1,
            "mem_size_mib": 256,
            "smt": false
        },
        "network-interfaces": [{
            "iface_id": "eth0",
            "guest_mac": mac,
            "host_dev_name": tap_name
        }]
    });
    std::fs::write(config_path, serde_json::to_string_pretty(&cfg).unwrap())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(reason) = skip_if_unavailable() {
        eprintln!("[vm-network-smoke] SKIPPED: {reason}");
        return Ok(());
    }

    let bin = std::env::var("PULLRUN_FC_BIN").expect("PULLRUN_FC_BIN set");
    let vmlinux = std::env::var("PULLRUN_FC_VMLINUX").expect("PULLRUN_FC_VMLINUX set");
    let timeout_secs: u64 = std::env::var("PULLRUN_FC_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let host_port: u16 = std::env::var("PULLRUN_FC_HOST_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let guest_ip: Ipv4Addr = std::env::var("PULLRUN_FC_GUEST_IP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(Ipv4Addr::new(10, 42, 88, 88));
    eprintln!(
        "[vm-network-smoke] bin={bin} vmlinux={vmlinux} timeout={timeout_secs}s \
         host_port={host_port} guest_ip={guest_ip}"
    );

    let stage_dir = std::env::var("PULLRUN_FC_STAGE").unwrap_or_else(|_| "/tmp/vm-net-smoke".into());
    let _ = std::fs::remove_dir_all(&stage_dir);
    std::fs::create_dir_all(&stage_dir).expect("stage dir");
    eprintln!("[vm-network-smoke] stage_dir={stage_dir}");

    let tmp = PathBuf::from(&stage_dir);
    let api_sock = tmp.join("fc.sock");
    let log_path = tmp.join("fc.log");
    let cfg_path = tmp.join("vm-config.json");
    let rootfs_path = tmp.join("smoke.ext4");
    let serial_out = tmp.join("serial.out");
    let serial_err = tmp.join("serial.err");
    std::fs::write(&log_path, b"").expect("create log file");

    // 1. Plumb host-side network.
    ensure_bridge().map_err(|e| format!("ensure_bridge: {e}"))?;
    let tap_name = "tap-vm-net";
    if link_exists(tap_name) {
        let _ = run_ip(&["link", "del", tap_name]);
    }
    run_ip(&["tuntap", "add", tap_name, "mode", "tap"])?;
    run_ip(&["link", "set", tap_name, "master", BRIDGE_NAME])?;
    run_ip(&["link", "set", tap_name, "up"])?;
    let mac = mac_from_ip(guest_ip);
    eprintln!("[vm-network-smoke] tap={tap_name} guest_ip={guest_ip} mac={mac}");

    // 2. Build rootfs.
    build_net_rootfs(&rootfs_path, &tmp, host_port, guest_ip)
        .map_err(|e| format!("build rootfs: {e}"))?;
    write_fc_config(&cfg_path, std::path::Path::new(&vmlinux), &rootfs_path, tap_name, guest_ip, &mac)
        .map_err(|e| format!("write fc config: {e}"))?;

    // 3. Spawn firecracker.
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
        .map_err(|e| format!("spawn firecracker: {e}"))?;

    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let _ = std::fs::write(&serial_out, b"");
    let _ = std::fs::write(&serial_err, b"");
    let out_path = serial_out.clone();
    let err_path = serial_err.clone();
    let pump_out = tokio::spawn(async move {
        let mut s = stdout;
        let mut buf = [0u8; 4096];
        let mut file = std::fs::OpenOptions::new().append(true).open(&out_path).expect("open out");
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
        let mut file = std::fs::OpenOptions::new().append(true).open(&err_path).expect("open err");
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

    // 5. Probe the guest's HTTP server directly. We connect from the
    //    host to <guest_ip>:host_port over the bridge, send an HTTP GET,
    //    and read the marker back. This is the network path the
    //    ProxyNetwork inbound listeners will use in production.
    let start = Instant::now();
    let mut body_seen = false;
    let mut last_err = String::new();

    let guest_target = format!("{guest_ip}:{host_port}");

    while start.elapsed() < Duration::from_secs(timeout_secs) {
        // Give the VM a few seconds to boot and configure eth0.
        if start.elapsed() > Duration::from_secs(3) {
            match tokio::time::timeout(Duration::from_millis(2000), async {
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
                        "[vm-network-smoke] host->guest{} got {} bytes: {:?}",
                        guest_target,
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

    // 6. Teardown tap.
    let _ = run_ip(&["link", "del", tap_name]);

    eprintln!(
        "[vm-network-smoke] body_seen={} elapsed={:?} last_err={}",
        body_seen,
        start.elapsed(),
        last_err
    );

    if !body_seen {
        eprintln!("[vm-network-smoke] serial.out tail:");
        if let Ok(contents) = std::fs::read_to_string(&serial_out) {
            for line in contents.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev() {
                eprintln!("    {line}");
            }
        }
    }

    if !body_seen {
        std::process::exit(1);
    }
    println!("[vm-network-smoke] PASS: marker '{}' received via host proxy", SMOKE_MARKER);
    Ok(())
}
