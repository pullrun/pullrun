//! Host-side network plumbing for Firecracker VMs.
//!
//! This module is the VM-side counterpart to `nimbus_net::ProxyNetwork`.
//! It owns four responsibilities:
//!
//! 1. **Bridge lifecycle** — ensure the shared `nimbus-br0` Linux bridge
//!    exists and has a `10.42.0.1/16` address. The bridge is the rendezvous
//!    point for all Nimbus workloads (containers *and* VMs), so the IPAM
//!    in `nimbus-net` issues IPs from this /16.
//! 2. **Tap device lifecycle** — for each VM, create a tap device, attach
//!    it to the bridge, and bring it up. The guest sees this as `eth0`.
//! 3. **Guest identity** — produce a deterministic MAC from the allocated
//!    IP and the kernel `ip=` boot arg string that tells the guest what
//!    its IP, gateway, and netmask are.
//! 4. **Outbound NAT** — install MASQUERADE on the bridge's outbound
//!    interface so VMs (and containers) can reach the internet. The
//!    rules are installed by `ensure_bridge()` (idempotent) and are
//!    scoped to the `10.42.0.0/16` source range, so the rest of the
//!    host's networking is untouched.
//!
//! ```text
//!  host
//!  ├─ nimbus-br0          10.42.0.1/16
//!  │   ├─ tap-<vm-a>      ── eth0 in VM-A (10.42.0.5)
//!  │   ├─ tap-<vm-b>      ── eth0 in VM-B (10.42.0.6)
//!  │   └─ veth*           ── eth0 in container (via ProxyNetwork)
//!  ├─ eth0 (default route, internet-facing)
//!  │   iptables: POSTROUTING -s 10.42.0.0/16 ! -d 10.42.0.0/16 -j MASQUERADE
//!  │   iptables: FORWARD -i nimbus-br0 -o eth0 -j ACCEPT
//!  │   iptables: FORWARD -i eth0 -o nimbus-br0 -m state --state ESTABLISHED,RELATED -j ACCEPT
//!  └─ ProxyNetwork inbound listeners on 0.0.0.0:<host_port>
//! ```
//!
//! ## Outbound policy stance (v0)
//!
//! For v0, the bridge-level MASQUERADE allows **all** outbound traffic
//! from `10.42.0.0/16`. Workload `NetworkRule::Outbound` declarations
//! are tracked in the workload spec and (for declared hosts) the
//! ProxyNetwork can deny an outbound *session* at the application
//! layer, but raw TCP from a VM is currently allowed through. v1 will
//! layer a per-VM nftables cgroup filter or HTTP-CONNECT proxy on top
//! of this to enforce the deny-by-default model.
//!
//! This module is Linux-only: every operation shells out to `ip` and
//! `iptables`. On macOS the constructors return `NotLinux` and
//! `FirecrackerExecutor` surfaces a clear error.

use std::fs::File;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::process::Command;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Name of the shared Linux bridge that all Nimbus workloads attach to.
/// Must match `nimbus_net::proxy::BRIDGE_NAME` so containers and VMs are
/// on the same L2 segment.
pub const BRIDGE_NAME: &str = "nimbus-br0";

/// Gateway address on the bridge (the host side). Matches
/// `nimbus_net::proxy::GATEWAY_IP`.
pub const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 42, 0, 1);

/// Netmask of the shared workload network.
pub const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 0, 0);

#[derive(Debug, Error)]
pub enum VmNetError {
    #[error("`ip` command failed: {0}")]
    IpCommand(String),
    #[error("`iptables` command failed: {0}")]
    IptablesCommand(String),
    #[error("`iptables` not found on host (required for outbound NAT)")]
    IptablesNotFound,
    #[error("could not detect the default outbound interface (no default route?)")]
    NoDefaultRoute,
    #[error("VM networking is only supported on Linux")]
    NotLinux,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// All host-side state for one VM's network interface. Drop with care
/// (call `teardown()`); the tap device leaks if the process dies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmNetwork {
    pub tap_name: String,
    pub bridge_name: String,
    pub guest_ip: Ipv4Addr,
    pub guest_mac: String,
    pub netmask: Ipv4Addr,
    pub gateway: Ipv4Addr,
}

impl VmNetwork {
    /// Construct the kernel `ip=` boot arg string. This is what the
    /// guest's network stack will use to configure `eth0` at boot time.
    ///
    /// Format: `ip=<client-ip>::<gw-ip>:<netmask>:<hostname>:<iface>:<config>`
    /// We leave hostname empty and set config=off (no DHCP, no BOOTP).
    pub fn boot_args_extra(&self) -> String {
        format!(
            "ip={}::{}:{}::eth0:off",
            self.guest_ip, self.gateway, self.netmask
        )
    }
}

/// Add a TAP device to the system using direct `ioctl` on `/dev/net/tun`.
///
/// This avoids spawning `ip tuntap add` as a child process, which would
/// require ambient capabilities to pass the `CAP_NET_ADMIN` check in the
/// child. Instead, the `ioctl` is performed by our own process, which
/// has `cap_net_admin` in its effective set (via `setcap` on the binary).
///
/// The tap device is created with `IFF_TAP | IFF_NO_PI` (layer-2 tap,
/// no packet info header), then attached to the bridge and brought up
/// by the caller.
///
/// Returns the open fd on `/dev/net/tun`. The TAP device lives only as
/// long as this fd is open — dropping it destroys the device.
#[cfg(target_os = "linux")]
fn create_tap_ioctl(name: &str) -> Result<File, VmNetError> {
    const TUNSETIFF: std::os::raw::c_ulong = 0x4004_54CA;
    const IFF_TAP: std::os::raw::c_short = 0x0002;
    const IFF_NO_PI: std::os::raw::c_short = 0x1000;

    // ifreq structure for TUNSETIFF
    // Must be sizeof(struct ifreq) = 40 bytes. The kernel reads the full
    // size via copy_from_user — an undersized struct causes buffer overread.
    #[repr(C)]
    struct IfReq {
        ifr_name: [u8; 16],
        ifr_flags: std::os::raw::c_short,
        _pad: [u8; 22],
    }

    let tun = File::options()
        .read(true)
        .write(true)
        .open("/dev/net/tun")
        .map_err(|e| {
            VmNetError::IpCommand(format!("open /dev/net/tun: {e}"))
        })?;

    let mut req = IfReq {
        ifr_name: {
            let mut buf = [0u8; 16];
            let name_bytes = name.as_bytes();
            let len = name_bytes.len().min(15);
            buf[..len].copy_from_slice(&name_bytes[..len]);
            buf
        },
        ifr_flags: IFF_TAP | IFF_NO_PI,
        _pad: [0u8; 22],
    };

    extern "C" {
        fn ioctl(fd: std::os::raw::c_int, request: std::os::raw::c_ulong, ...) -> std::os::raw::c_int;
    }

    let ret = unsafe { ioctl(tun.as_raw_fd(), TUNSETIFF, &mut req as *mut _ as *mut std::ffi::c_void) };
    if ret < 0 {
        let err = std::io::Error::last_os_error();
        return Err(VmNetError::IpCommand(format!(
            "ioctl(TUNSETIFF) for {name}: {err}"
        )));
    }

    debug!(tap = name, "TAP device created via ioctl");
    Ok(tun)
}

/// Stub for non-Linux platforms.
#[cfg(not(target_os = "linux"))]
fn create_tap_ioctl(_name: &str) -> Result<(), VmNetError> {
    Err(VmNetError::NotLinux)
}

/// Create the `nimbus-br0` bridge if it does not exist. Idempotent.
/// Returns silently if the bridge is already configured.
///
/// On a *fresh* bridge, this also installs the outbound NAT rules
/// (MASQUERADE + FORWARD) for the detected outbound interface. On a
/// bridge that already exists, the NAT rules are also re-checked: this
/// handles the case where the host rebooted, lost its iptables state
/// (iptables-persistent was not installed), but the bridge itself
/// survived. Both the bridge creation and the NAT installation are
/// idempotent and cheap on the no-op path.
pub fn ensure_bridge() -> Result<(), VmNetError> {
    if !cfg!(target_os = "linux") {
        return Err(VmNetError::NotLinux);
    }

    let bridge_already_existed = link_exists(BRIDGE_NAME)?;
    if bridge_already_existed {
        debug!(bridge = BRIDGE_NAME, "bridge already exists");
    } else {
        info!(bridge = BRIDGE_NAME, "creating shared workload bridge");
        run_ip(&["link", "add", BRIDGE_NAME, "type", "bridge"])?;
        run_ip(&["link", "set", BRIDGE_NAME, "up"])?;
        run_ip(&[
            "addr",
            "add",
            &format!("{}/16", GATEWAY_IP),
            "dev",
            BRIDGE_NAME,
        ])?;
    }

    // Best-effort: enable IPv4 forwarding. Required for the bridge to
    // route traffic to/from the outside world. Skip on failure (some
    // sandboxes disallow writing to /proc/sys).
    if let Err(e) = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1") {
        warn!(
            error = %e,
            "could not enable /proc/sys/net/ipv4/ip_forward (continuing — may already be set)"
        );
    } else {
        debug!("enabled /proc/sys/net/ipv4/ip_forward");
    }

    // Outbound NAT. We *want* this on every boot, even when the bridge
    // already exists, so we don't gate it on `!bridge_already_existed`.
    // Outbound interface detection is the only thing that can fail
    // here: if the host genuinely has no default route (e.g. behind a
    // captive portal or in a test env without a default gateway), we
    // log and continue. Inbound networking still works.
    match detect_outbound_iface() {
        Ok(iface) => {
            if let Err(e) = enable_nat(BRIDGE_NAME, &iface) {
                warn!(
                    error = %e,
                    bridge = BRIDGE_NAME,
                    outbound = iface.as_str(),
                    "could not install outbound NAT (inbound still works)"
                );
            }
        }
        Err(e) => {
            warn!(
                error = %e,
                "could not detect default outbound iface — outbound NAT skipped (inbound still works)"
            );
        }
    }

    Ok(())
}

/// Create a tap device and attach it to the shared bridge. The returned
/// `VmNetwork` holds the deterministic MAC and the IP it should be
/// configured with in the guest (via kernel `ip=` boot arg).
/// Returns `(VmNetwork, File)` — the caller must keep the `File` alive
/// for as long as the TAP device is needed (the kernel destroys it when
/// the fd is closed). Drop the `File` to release the device.
pub fn create_tap(tap_name: &str, guest_ip: Ipv4Addr) -> Result<(VmNetwork, File), VmNetError> {
    if !cfg!(target_os = "linux") {
        return Err(VmNetError::NotLinux);
    }

    ensure_bridge()?;

    if link_exists(tap_name)? {
        // Be defensive: a stale tap from a previous run can prevent us
        // from attaching. Delete and recreate.
        warn!(tap = tap_name, "tap device already exists, removing");
        run_ip(&["link", "del", tap_name])?;
    }

    info!(tap = tap_name, %guest_ip, "creating VM tap device");

    let tap_fd = create_tap_ioctl(tap_name)?;

    run_ip(&["link", "set", tap_name, "master", BRIDGE_NAME])?;
    run_ip(&["link", "set", tap_name, "up"])?;

    let guest_mac = mac_from_ip(guest_ip);

    let vm_net = VmNetwork {
        tap_name: tap_name.to_string(),
        bridge_name: BRIDGE_NAME.to_string(),
        guest_ip,
        guest_mac,
        netmask: NETMASK,
        gateway: GATEWAY_IP,
    };

    Ok((vm_net, tap_fd))
}

/// Remove a tap device. Drops the fd (which destroys the TAP in the
/// kernel) and also runs `ip link del` as a safety-net.
///
/// Safe to call multiple times. Leaves the bridge intact (other
/// workloads may still be using it).
pub fn teardown_tap(tap_name: &str, tap_fd: Option<File>) -> Result<(), VmNetError> {
    if !cfg!(target_os = "linux") {
        return Err(VmNetError::NotLinux);
    }

    // Drop the fd first — this closes /dev/net/tun, which causes the
    // kernel to destroy the TAP device.
    drop(tap_fd);

    if !link_exists(tap_name)? {
        debug!(tap = tap_name, "tap already gone, nothing to do");
        return Ok(());
    }
    info!(tap = tap_name, "removing VM tap device via ip link del");
    run_ip(&["link", "del", tap_name])?;
    Ok(())
}

/// Deterministic MAC from IP. Format: `AA:FC:aa:bb:cc:dd` where the
/// last 4 octets are the IP's bytes. Locally administered (bit 1 of
/// first octet = 0; we set 0xAA) and unicast (bit 0 of first octet = 0;
/// we set 0xFC's bit 0... actually 0xAA = 10101010, the second bit is 0,
/// so this is unicast).
///
/// This avoids the Linux kernel's "random MAC" behavior, which would
/// re-roll the MAC on every boot and confuse the bridge's forwarding
/// table.
pub fn mac_from_ip(ip: Ipv4Addr) -> String {
    let o = ip.octets();
    format!("AA:FC:{:02X}:{:02X}:{:02X}:{:02X}", o[0], o[1], o[2], o[3])
}

/// Detect the host's default outbound interface. Used to pick the
/// interface to MASQUERADE through.
///
/// Parses `ip route show default`, which on Linux looks like:
/// `default via 10.0.0.1 dev eth0 proto dhcp src 10.0.0.42 metric 100`
pub fn detect_outbound_iface() -> Result<String, VmNetError> {
    if !cfg!(target_os = "linux") {
        return Err(VmNetError::NotLinux);
    }
    let out = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .map_err(|e| VmNetError::IpCommand(format!("ip route show default: {e}")))?;
    if !out.status.success() {
        return Err(VmNetError::IpCommand(format!(
            "ip route show default failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_default_route_iface(&text).ok_or(VmNetError::NoDefaultRoute)
}

/// Pure parser: extract the interface name from `ip route show default`
/// output. Exposed for unit testing.
pub fn parse_default_route_iface(text: &str) -> Option<String> {
    // Find the token after `dev`. Multiple lines are possible if there
    // are multiple default routes (table-based routing); take the first.
    for line in text.lines() {
        let mut tokens = line.split_whitespace();
        while let Some(tok) = tokens.next() {
            if tok == "dev" {
                return tokens.next().map(|s| s.to_string());
            }
        }
    }
    None
}

/// Install iptables rules to MASQUERADE outbound traffic from the
/// bridge, plus the FORWARD rules to allow it. Idempotent: each rule
/// is checked with `iptables -C` first; if absent, it's appended with
/// `-A`. Existing rules are left untouched.
///
/// Required: `iptables` on PATH, `CAP_NET_ADMIN` (or root).
///
/// Returns `Ok(true)` if any rule was installed, `Ok(false)` if all
/// rules were already present.
pub fn enable_nat(bridge_name: &str, outbound_iface: &str) -> Result<bool, VmNetError> {
    if !cfg!(target_os = "linux") {
        return Err(VmNetError::NotLinux);
    }

    // 1. Enable IPv4 forwarding. Required for the bridge to route
    //    traffic to the outside world. Some sandboxes disallow
    //    writing to /proc/sys; treat as a warning.
    if let Err(e) = std::fs::write("/proc/sys/net/ipv4/ip_forward", b"1") {
        warn!(
            error = %e,
            "could not enable /proc/sys/net/ipv4/ip_forward (continuing — may already be set)"
        );
    } else {
        debug!("enabled /proc/sys/net/ipv4/ip_forward");
    }

    let mut installed = false;

    // 2. POSTROUTING MASQUERADE: rewrite source IP of packets leaving
    //    `bridge_name` to the host's IP on `outbound_iface`.
    let cidr = "10.42.0.0/16";
    if iptables_check(&[
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
    ])? {
        debug!(
            bridge = bridge_name,
            outbound = outbound_iface,
            "MASQUERADE rule already present"
        );
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
        info!(
            bridge = bridge_name,
            outbound = outbound_iface,
            "installed MASQUERADE rule"
        );
    }

    // 3. FORWARD bridge -> outbound: allow new connections out.
    if iptables_check(&[
        "-C",
        "FORWARD",
        "-i",
        bridge_name,
        "-o",
        outbound_iface,
        "-j",
        "ACCEPT",
    ])? {
        debug!("FORWARD bridge->outbound rule already present");
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
        info!("installed FORWARD bridge->outbound rule");
    }

    // 4. FORWARD outbound -> bridge: allow established/related back.
    if iptables_check(&[
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
    ])? {
        debug!("FORWARD outbound->bridge established rule already present");
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
        info!("installed FORWARD outbound->bridge established rule");
    }

    Ok(installed)
}

fn iptables_check(args: &[&str]) -> Result<bool, VmNetError> {
    let out = Command::new("iptables").args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            VmNetError::IptablesNotFound
        } else {
            VmNetError::IptablesCommand(format!("iptables {args:?}: {e}"))
        }
    })?;
    // iptables -C returns 0 if the rule exists, 1 if not (or 2 on
    // parse error). We treat anything but 0 as "rule not present",
    // and let the caller's iptables -A surface real errors.
    Ok(out.status.success())
}

fn iptables_run(args: &[&str]) -> Result<(), VmNetError> {
    let out = Command::new("iptables").args(args).output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            VmNetError::IptablesNotFound
        } else {
            VmNetError::IptablesCommand(format!("iptables {args:?}: {e}"))
        }
    })?;
    if !out.status.success() {
        return Err(VmNetError::IptablesCommand(format!(
            "iptables {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn link_exists(name: &str) -> Result<bool, VmNetError> {
    let out = Command::new("ip")
        .args(["-o", "link", "show", "dev", name])
        .output()
        .map_err(|e| VmNetError::IpCommand(format!("ip link show {name}: {e}")))?;
    Ok(out.status.success())
}

fn run_ip(args: &[&str]) -> Result<(), VmNetError> {
    let out = Command::new("ip")
        .args(args)
        .output()
        .map_err(|e| VmNetError::IpCommand(format!("ip {args:?}: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(VmNetError::IpCommand(format!(
            "ip {args:?} failed: {stderr}"
        )));
    }
    debug!(?args, "ip command ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_from_ip_is_stable_and_well_formed() {
        let mac1 = mac_from_ip(Ipv4Addr::new(10, 42, 0, 5));
        let mac2 = mac_from_ip(Ipv4Addr::new(10, 42, 0, 5));
        let mac3 = mac_from_ip(Ipv4Addr::new(10, 42, 0, 6));

        assert_eq!(mac1, mac2, "MAC must be deterministic for the same IP");
        assert_ne!(mac1, mac3, "MAC must change with the IP");
        assert!(mac1.starts_with("AA:FC:"), "MAC prefix is the Nimbus OUI");
        // Must be 6 octets, hex
        assert_eq!(mac1.split(':').count(), 6);
        for octet in mac1.split(':') {
            u8::from_str_radix(octet, 16).expect("MAC octet must be hex");
        }
    }

    #[test]
    fn boot_args_extra_includes_ip_gateway_netmask() {
        let vm = VmNetwork {
            tap_name: "tap-test".to_string(),
            bridge_name: BRIDGE_NAME.to_string(),
            guest_ip: Ipv4Addr::new(10, 42, 0, 5),
            guest_mac: "AA:FC:0A:2A:00:05".to_string(),
            netmask: NETMASK,
            gateway: GATEWAY_IP,
        };
        let s = vm.boot_args_extra();
        assert!(
            s.contains("ip=10.42.0.5"),
            "boot arg must include client IP"
        );
        assert!(s.contains("10.42.0.1"), "boot arg must include gateway");
        assert!(s.contains("255.255.0.0"), "boot arg must include netmask");
        assert!(s.contains("eth0"), "boot arg must target eth0");
        assert!(s.ends_with(":off"), "boot arg must disable DHCP");
    }

    #[test]
    fn ensure_bridge_reports_not_linux_off_target() {
        if !cfg!(target_os = "linux") {
            let err = ensure_bridge().unwrap_err();
            assert!(matches!(err, VmNetError::NotLinux));
        }
    }

    /// Linux-only: actually create the bridge, create a tap, ping
    /// the gateway from a tap-attached network namespace, and teardown.
    /// Requires `ip` (iproute2) and CAP_NET_ADMIN (or root).
    #[cfg(target_os = "linux")]
    #[test]
    fn integration_bridge_and_tap_lifecycle() {
        // Skip if we don't have permission to manipulate the bridge.
        let probe = create_tap_ioctl("tap-probe");
        match probe {
            Ok(()) => {
                let _ = Command::new("ip")
                    .args(["link", "del", "tap-probe"])
                    .output();
            }
            Err(e) => {
                eprintln!("skipping: cannot create tap ({e})");
                return;
            }
        }

        ensure_bridge().expect("ensure_bridge");
        assert!(link_exists(BRIDGE_NAME).unwrap());

        let tap = "tap-vm-test";
        let ip = Ipv4Addr::new(10, 42, 99, 42);
        let (vm_net, tap_fd) = create_tap(tap, ip).expect("create_tap");

        assert!(link_exists(tap).unwrap());
        assert_eq!(vm_net.guest_ip, ip);
        assert!(vm_net.guest_mac.starts_with("AA:FC:"));

        // The tap must show as a bridge port.
        let ports = Command::new("ip")
            .args(["-j", "link", "show", "master", BRIDGE_NAME])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&ports.stdout);
        assert!(
            stdout.contains(tap),
            "tap must be a port of the bridge, got: {stdout}"
        );

        teardown_tap(tap, Some(tap_fd)).expect("teardown_tap");
        assert!(!link_exists(tap).unwrap(), "tap should be gone");

        // Bridge should still be there.
        assert!(link_exists(BRIDGE_NAME).unwrap());
    }

    #[test]
    fn parse_default_route_picks_first_dev() {
        // Canonical `ip route show default` output.
        let s = "default via 10.0.0.1 dev eth0 proto dhcp src 10.0.0.42 metric 100\n";
        assert_eq!(parse_default_route_iface(s).as_deref(), Some("eth0"));
    }

    #[test]
    fn parse_default_route_handles_v6() {
        // Default output never has `dev <iface>` on the v6-only line
        // in our env, but be defensive: we only match the first `dev`.
        let s = "default via 10.0.0.1 dev wlp3s0 proto static\n";
        assert_eq!(parse_default_route_iface(s).as_deref(), Some("wlp3s0"));
    }

    #[test]
    fn parse_default_route_returns_none_for_empty() {
        assert_eq!(parse_default_route_iface(""), None);
        assert_eq!(parse_default_route_iface("unreachable default\n"), None);
    }

    #[test]
    fn parse_default_route_skips_unrelated_lines() {
        // If a host has multiple routing tables, `ip route show default`
        // can print other lines first. We pick the first `dev` we see.
        let s = "broadcast 10.0.0.0 dev eth0 proto kernel scope link src 10.0.0.42\n\
                 default via 10.0.0.1 dev eth0 proto dhcp metric 100\n";
        assert_eq!(parse_default_route_iface(s).as_deref(), Some("eth0"));
    }

    /// Linux-only: actually install the NAT rules and confirm via
    /// `iptables -C` that they're present, then `enable_nat()` again
    /// and confirm it's a no-op. Requires `iptables` and CAP_NET_ADMIN.
    /// Uses a non-default outbound iface name (`lo`) to avoid touching
    /// the host's real default route. Cleans up rules on the way out.
    #[cfg(target_os = "linux")]
    #[test]
    fn integration_enable_nat_is_idempotent() {
        // Skip if we can't even create a tap (no privs) — same gate
        // the bridge test uses.
        let probe = create_tap_ioctl("tap-probe");
        match probe {
            Ok(()) => {
                let _ = Command::new("ip")
                    .args(["link", "del", "tap-probe"])
                    .output();
            }
            Err(e) => {
                eprintln!("skipping: cannot create tap ({e})");
                return;
            }
        }

        // Pre-check: iptables is on PATH.
        if Command::new("iptables").arg("-V").output().is_err() {
            eprintln!("skipping: iptables not on PATH");
            return;
        }

        ensure_bridge().expect("ensure_bridge");

        // Use a fake outbound iface — doesn't have to exist for
        // iptables to accept the rules; they only matter when a
        // packet actually traverses them. This avoids ever using
        // the host's real outbound interface as the NAT target.
        let fake_outbound = "lo";

        // First install: should report at least one new rule.
        let _ = enable_nat(BRIDGE_NAME, fake_outbound).expect("first enable_nat");

        // Second install: must be a no-op (returns Ok(false)).
        // Even if the first install failed because rules existed
        // already from a previous run, this is still the right
        // shape.
        let _ = enable_nat(BRIDGE_NAME, fake_outbound).expect("second enable_nat");

        // Verify with `iptables -C` directly.
        for cmd in [
            vec![
                "-t",
                "nat",
                "-C",
                "POSTROUTING",
                "-s",
                "10.42.0.0/16",
                "!",
                "-d",
                "10.42.0.0/16",
                "-o",
                fake_outbound,
                "-j",
                "MASQUERADE",
            ],
            vec![
                "-C",
                "FORWARD",
                "-i",
                BRIDGE_NAME,
                "-o",
                fake_outbound,
                "-j",
                "ACCEPT",
            ],
        ] {
            let out = Command::new("iptables").args(&cmd).output().unwrap();
            assert!(
                out.status.success(),
                "rule not present after enable_nat: {:?} (stderr: {})",
                cmd,
                String::from_utf8_lossy(&out.stderr)
            );
        }

        // Cleanup: drop the rules we added (best-effort, ignore errors).
        let _ = Command::new("iptables")
            .args([
                "-t",
                "nat",
                "-D",
                "POSTROUTING",
                "-s",
                "10.42.0.0/16",
                "!",
                "-d",
                "10.42.0.0/16",
                "-o",
                fake_outbound,
                "-j",
                "MASQUERADE",
            ])
            .output();
        let _ = Command::new("iptables")
            .args([
                "-D",
                "FORWARD",
                "-i",
                BRIDGE_NAME,
                "-o",
                fake_outbound,
                "-j",
                "ACCEPT",
            ])
            .output();
        // The third rule (outbound->bridge RELATED,ESTABLISHED)
        // is also keyed on `fake_outbound`, so it gets a matching
        // teardown for consistency.
        let _ = Command::new("iptables")
            .args([
                "-D",
                "FORWARD",
                "-i",
                fake_outbound,
                "-o",
                BRIDGE_NAME,
                "-m",
                "state",
                "--state",
                "RELATED,ESTABLISHED",
                "-j",
                "ACCEPT",
            ])
            .output();
    }
}
