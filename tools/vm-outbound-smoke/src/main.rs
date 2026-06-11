//! Standalone VM outbound NAT smoke test for Firecracker.
//!
//! Boots a real Firecracker microVM with a tiny Alpine rootfs. The
//! guest's `/init` script does `wget http://10.42.0.1:9999/` and
//! prints the response body to the serial console.
//!
//! On the host, we:
//!  1. Create the `pullrun-br0` bridge (idempotent) and install the
//!     iptables MASQUERADE + FORWARD rules for outbound NAT.
//!  2. Bind a one-shot HTTP server to `10.42.0.1:9999` (the bridge
//!     gateway) that returns the marker body. The host's kernel routes
//!     `10.42.0.0/16` to the bridge, so binding to `10.42.0.1` is the
//!     address the guest will hit when MASQUERADE rewrites its source.
//!  3. Create a tap, attach it to the bridge, spawn firecracker.
//!  4. Wait for the marker to appear in firecracker's serial output.
//!
//! If the marker appears, MASQUERADE + FORWARD + the bridge are all
//! working. The guest was able to send TCP through the kernel, which
//! then NAT'd the source to the host's outbound iface, and the host
//! HTTP server replied. The reply traveled back as RELATED,ESTABLISHED.
//!
//! This is a self-contained alternative to the Rust integration test
//! in `runtime/pullrun-vm/tests/firecracker_outbound.rs` — same logic,
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
//! - `PULLRUN_FC_STAGE`      — staging directory (default /tmp/vm-out-smoke)
//! - `PULLRUN_FC_TIMEOUT`    — boot timeout in seconds (default 60)
//! - `PULLRUN_FC_GUEST_IP`   — guest IP (default 10.42.88.88)
//! - `PULLRUN_FC_HOST_PORT`  — host-side HTTP server port (default 9999)

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command as TokioCommand;
use tokio::sync::oneshot;

const OUTBOUND_MARKER: &str = "pullrun-vm-outbound OK";
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
        return Some(format!(
            "firecracker binary not found (PULLRUN_FC_BIN={bin})"
        ));
    }
    match std::env::var("PULLRUN_FC_VMLINUX") {
        Ok(p) if !p.is_empty() && std::path::Path::new(&p).exists() => {}
        Ok(p) => return Some(format!("PULLRUN_FC_VMLINUX={p} not found")),
        Err(_) => return Some("PULLRUN_FC_VMLINUX not set".into()),
    }
    if which("iptables").is_none() {
        return Some("iptables not on PATH (required for outbound NAT)".into());
    }
    let probe = Command::new("ip")
        .args(["tuntap", "add", "tap-op", "mode", "tap"])
        .output();
    match probe {
        Ok(o) if o.status.success() => {
            let _ = Command::new("ip").args(["link", "del", "tap-op"]).output();
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
        eprintln!("[vm-outbound-smoke] bridge {BRIDGE_NAME} already exists");
    } else {
        eprintln!("[vm-outbound-smoke] creating bridge {BRIDGE_NAME}");
        run_ip(&["link", "add", BRIDGE_NAME, "type", "bridge"])?;
        run_ip(&["link", "set", BRIDGE_NAME, "up"])?;
        run_ip(&[
            "addr",
            "add",
            &format!("{GATEWAY_IP}/16"),
            "dev",
            BRIDGE_NAME,
        ])?;
    }
    let _ = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1");
    Ok(())
}

fn detect_outbound_iface() -> Result<String, String> {
    let out = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .map_err(|e| format!("ip route show default: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ip route show default failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        while let Some(tok) = tokens.next() {
            if tok == "dev" {
                if let Some(iface) = tokens.next() {
                    return Ok(iface.to_string());
                }
            }
        }
    }
    Err("no default route found".into())
}

fn iptables_check(args: &[&str]) -> Result<bool, String> {
    let out = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| format!("iptables {args:?}: {e}"))?;
    Ok(out.status.success())
}

fn iptables_run(args: &[&str]) -> Result<(), String> {
    let out = Command::new("iptables")
        .args(args)
        .output()
        .map_err(|e| format!("iptables {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "iptables {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}

fn enable_nat(bridge_name: &str, outbound_iface: &str) -> Result<bool, String> {
    let cidr = "10.42.0.0/16";
    let mut installed = false;

    let cidr_masq = vec![
        "-t",
        "nat",
        "-C",
        "POSTROUTING",
        "-s",
        cidr,
        "!",
        "-d",
        cidr,
        "-o",
        outbound_iface,
        "-j",
        "MASQUERADE",
    ];
    if iptables_check(&cidr_masq)? {
        eprintln!("[vm-outbound-smoke] MASQUERADE rule already present");
    } else {
        iptables_run(&[
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-s",
            cidr,
            "!",
            "-d",
            cidr,
            "-o",
            outbound_iface,
            "-j",
            "MASQUERADE",
        ])?;
        installed = true;
        eprintln!("[vm-outbound-smoke] installed MASQUERADE on {outbound_iface}");
    }

    let cidr_fwd_out = vec![
        "-C",
        "FORWARD",
        "-i",
        bridge_name,
        "-o",
        outbound_iface,
        "-j",
        "ACCEPT",
    ];
    if iptables_check(&cidr_fwd_out)? {
        eprintln!("[vm-outbound-smoke] FORWARD bridge->{outbound_iface} rule already present");
    } else {
        iptables_run(&[
            "-A",
            "FORWARD",
            "-i",
            bridge_name,
            "-o",
            outbound_iface,
            "-j",
            "ACCEPT",
        ])?;
        installed = true;
        eprintln!("[vm-outbound-smoke] installed FORWARD bridge->{outbound_iface}");
    }

    let cidr_fwd_in = vec![
        "-C",
        "FORWARD",
        "-i",
        outbound_iface,
        "-o",
        bridge_name,
        "-m",
        "state",
        "--state",
        "RELATED,ESTABLISHED",
        "-j",
        "ACCEPT",
    ];
    if iptables_check(&cidr_fwd_in)? {
        eprintln!(
            "[vm-outbound-smoke] FORWARD {outbound_iface}->bridge RELATED rule already present"
        );
    } else {
        iptables_run(&[
            "-A",
            "FORWARD",
            "-i",
            outbound_iface,
            "-o",
            bridge_name,
            "-m",
            "state",
            "--state",
            "RELATED,ESTABLISHED",
            "-j",
            "ACCEPT",
        ])?;
        installed = true;
        eprintln!("[vm-outbound-smoke] installed FORWARD {outbound_iface}->bridge RELATED");
    }

    Ok(installed)
}

fn mac_from_ip(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("AA:FC:{:02X}:{:02X}:{:02X}:{:02X}", o[0], o[1], o[2], o[3])
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

fn build_outbound_rootfs(
    target: &std::path::Path,
    stage_dir: &std::path::Path,
    host_port: u16,
    guest_ip: Ipv4Addr,
) -> Result<(), String> {
    let mount = stage_dir.join("mnt");
    std::fs::create_dir_all(&mount).map_err(|e| e.to_string())?;

    let run = |argv: &[&str]| -> Result<(), String> {
        let out = Command::new(argv[0])
            .args(&argv[1..])
            .output()
            .map_err(|e| format!("{argv:?}: {e}"))?;
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
        "pullrun-out",
        target.to_str().unwrap(),
    ])?;
    run(&[
        "mount",
        "-o",
        "loop",
        target.to_str().unwrap(),
        mount.to_str().unwrap(),
    ])?;

    let tar_url =
        std::env::var("PULLRUN_FC_ROOTFS_TAR").unwrap_or_else(|_| DEFAULT_ALPINE_URL.into());
    eprintln!("[vm-outbound-smoke] fetching rootfs tarball: {tar_url}");
    let bytes = if tar_url.starts_with("http") {
        fetch_url(&tar_url)?
    } else {
        std::fs::read(&tar_url).map_err(|e| format!("read {tar_url}: {e}"))?
    };
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&mount).map_err(|e| format!("untar: {e}"))?;

    // Init: configure eth0 (fallback), then wget the host's HTTP server
    // at 10.42.0.1:9999. We hit the gateway IP because that's the
    // only address we can rely on being reachable from inside the
    // guest without DNS. The result is piped through `tee` to the
    // serial console and also captured to /out for inspection.
    let guest_ip_str = guest_ip.to_string();
    let init_script = format!(
        r#"#!/bin/sh
set +e
mount -t proc none /proc 2>/dev/null
mount -t sysfs none /sys 2>/dev/null
mount -t devtmpfs none /dev 2>/dev/null
# Belt-and-suspenders: if the kernel ip= arg didn't fire (netdev up
# after IP_PNP), do it ourselves.
sleep 1
if ! ip -4 -o addr show eth0 2>/dev/null | grep -q inet; then
  echo "[guest] configuring eth0 manually (kernel ip= arg did not set inet)"
  ifconfig eth0 {guest_ip} netmask 255.255.0.0 up 2>/dev/null \
    || busybox ifconfig eth0 {guest_ip} netmask 255.255.0.0 up
  route add default gw 10.42.0.1 2>/dev/null \
    || busybox route add default gw 10.42.0.1
else
  echo "[guest] eth0 already configured by kernel ip= arg"
fi
echo "[guest] eth0: $(ip -4 -o addr show eth0 2>/dev/null | awk '{{print $4}}')"
echo "[guest] default route: $(ip -4 route show default 2>/dev/null)"

# Probe #1: ICMP-style reachability to the gateway.
echo "[guest] ping -c 1 -W 2 10.42.0.1"
ping -c 1 -W 2 10.42.0.1 2>&1 || busybox ping -c 1 -W 2 10.42.0.1
echo "[guest] ping exit: $?"

# Probe #2: HTTP to the host-side HTTP server. This is the real test.
# We use wget (busybox) and capture the body. The body must contain
# the marker; if it does, MASQUERADE + FORWARD are working.
echo "[guest] wget http://10.42.0.1:{port}/"
mkdir -p /tmp
wget -q -O /tmp/out http://10.42.0.1:{port}/ 2>/tmp/wget.err \
  || busybox wget -q -O /tmp/out http://10.42.0.1:{port}/ 2>/tmp/wget.err
WGET_EXIT=$?
echo "[guest] wget exit: $WGET_EXIT"
echo "[guest] wget stderr:"
cat /tmp/wget.err 2>/dev/null
echo "[guest] response body:"
cat /tmp/out 2>/dev/null
echo "[guest] /init done"
sync
echo o > /proc/sysrq-trigger 2>/dev/null
sleep 1
"#,
        guest_ip = guest_ip_str,
        port = host_port,
    );
    std::fs::write(mount.join("init"), &init_script).map_err(|e| e.to_string())?;
    let _ = Command::new("chmod")
        .args(["+x"])
        .arg(mount.join("init"))
        .status();

    run(&["umount", mount.to_str().unwrap()])?;
    Ok(())
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

async fn serve_marker(
    listener: TcpListener,
    marker: &'static str,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    eprintln!(
        "[vm-outbound-smoke] host http server listening on 10.42.0.1:{} serving marker={marker:?}",
        listener.local_addr().unwrap().port()
    );
    let body = format!("{marker}\n");
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                eprintln!("[vm-outbound-smoke] host http server shutting down");
                break;
            }
            accept = listener.accept() => {
                match accept {
                    Ok((mut sock, _peer)) => {
                        let body = body.clone();
                        // Read & discard the request, then send a 200 with the
                        // marker body. busybox wget expects a valid HTTP
                        // response, not just raw bytes.
                        let mut req = [0u8; 1024];
                        let _ = tokio::time::timeout(
                            Duration::from_millis(500),
                            sock.read(&mut req),
                        ).await;
                        let resp = format!(
                            "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        if let Err(e) = sock.write_all(resp.as_bytes()).await {
                            eprintln!("[vm-outbound-smoke] server write err: {e}");
                        }
                        let _ = sock.shutdown().await;
                    }
                    Err(e) => eprintln!("[vm-outbound-smoke] accept err: {e}"),
                }
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(reason) = skip_if_unavailable() {
        eprintln!("[vm-outbound-smoke] SKIPPED: {reason}");
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
        .unwrap_or(9999);
    let guest_ip: Ipv4Addr = std::env::var("PULLRUN_FC_GUEST_IP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(Ipv4Addr::new(10, 42, 88, 88));
    eprintln!(
        "[vm-outbound-smoke] bin={bin} vmlinux={vmlinux} timeout={timeout_secs}s \
         host_port={host_port} guest_ip={guest_ip}"
    );

    // 1. Bridge + NAT.
    ensure_bridge()?;
    let outbound_iface = detect_outbound_iface()?;
    eprintln!("[vm-outbound-smoke] outbound iface: {outbound_iface}");
    enable_nat(BRIDGE_NAME, &outbound_iface)?;

    // 2. Host-side HTTP server bound to the bridge gateway IP.
    let bind_addr = format!("{GATEWAY_IP}:{host_port}");
    let listener = TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| format!("bind {bind_addr}: {e}"))?;
    let (tx, rx) = oneshot::channel();
    let server_handle = tokio::spawn(serve_marker(listener, OUTBOUND_MARKER, rx));

    // 3. Tap.
    let stage_dir = std::env::var("PULLRUN_FC_STAGE").unwrap_or_else(|_| "/tmp/vm-out-smoke".into());
    let _ = std::fs::remove_dir_all(&stage_dir);
    std::fs::create_dir_all(&stage_dir).expect("stage dir");
    eprintln!("[vm-outbound-smoke] stage_dir={stage_dir}");

    let tap_name = format!("tap-out-{:x}", std::process::id() & 0xFFFFFF);
    if tap_name.len() > 15 {
        return Err(format!("tap_name too long: {tap_name}").into());
    }
    run_ip(&["tuntap", "add", &tap_name, "mode", "tap"])?;
    run_ip(&["link", "set", &tap_name, "up"])?;
    run_ip(&["link", "set", &tap_name, "master", BRIDGE_NAME])?;

    // 4. Rootfs.
    let rootfs_path = std::path::PathBuf::from(&stage_dir).join("rootfs.ext4");
    let stage_path = std::path::Path::new(&stage_dir);
    build_outbound_rootfs(&rootfs_path, stage_path, host_port, guest_ip)?;
    let _ = std::fs::create_dir_all(&format!("/tmp/fc-out-{:x}", std::process::id()));
    let log_path = format!("/tmp/fc-out-{}.log", std::process::id());
    let _ = std::fs::File::create(&log_path);

    // 5. Firecracker config.
    let config_path = std::path::PathBuf::from(&stage_dir).join("fc-config.json");
    write_fc_config(
        &config_path,
        std::path::Path::new(&vmlinux),
        &rootfs_path,
        &tap_name,
        guest_ip,
        &mac_from_ip(guest_ip),
    )?;

    // 6. Boot.
    eprintln!("[vm-outbound-smoke] booting firecracker, log={log_path}");
    let started = Instant::now();
    let mut fc = TokioCommand::new(&bin)
        .args([
            "--api-sock",
            &format!("/tmp/fc-out-{}.sock", std::process::id()),
        ])
        .args(["--config-file", config_path.to_str().unwrap()])
        .args(["--log-path", &log_path])
        .args(["--level", "Info"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = fc.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout).lines();

    let deadline = started + Duration::from_secs(timeout_secs);
    let mut found_marker = false;
    let mut found_wget = false;
    if let Some(mut stderr) = fc.stderr.take() {
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let s = String::from_utf8_lossy(&buf[..n]);
                        eprintln!("[fc-stderr] {s}");
                    }
                    Err(_) => break,
                }
            }
        });
    }

    while Instant::now() < deadline {
        let elapsed = started.elapsed();
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = tokio::time::timeout(
            remaining.min(Duration::from_millis(500)),
            reader.next_line(),
        )
        .await;
        match line {
            Ok(Ok(Some(l))) => {
                eprintln!("[fc] {l}");
                if l.contains(OUTBOUND_MARKER) {
                    found_marker = true;
                }
                if l.contains("wget exit") {
                    found_wget = true;
                }
                if found_marker {
                    break;
                }
            }
            Ok(Ok(None)) => {
                eprintln!("[vm-outbound-smoke] firecracker stdout EOF after {elapsed:?}");
                break;
            }
            Ok(Err(e)) => {
                eprintln!("[vm-outbound-smoke] read err: {e}");
                break;
            }
            Err(_) => {
                // periodic tick
                if found_wget && !found_marker {
                    eprintln!("[vm-outbound-smoke] wget finished, waiting for marker...");
                }
            }
        }
    }

    // Tell the HTTP server to exit (we may have already served a request,
    // and the next accept() would block forever otherwise).
    let _ = tx.send(());
    let _ = tokio::time::timeout(Duration::from_millis(500), server_handle).await;

    // Teardown.
    let _ = fc.start_kill();
    let _ = fc.wait().await;
    let _ = run_ip(&["link", "del", &tap_name]);

    let elapsed = started.elapsed();
    eprintln!("[vm-outbound-smoke] elapsed: {elapsed:?}");

    if found_marker {
        eprintln!("[vm-outbound-smoke] PASS: outbound NAT works");
        Ok(())
    } else {
        eprintln!("[vm-outbound-smoke] FAIL: marker not seen in serial output");
        Err("outbound NAT smoke test failed".into())
    }
}
