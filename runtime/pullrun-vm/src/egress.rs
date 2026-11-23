// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Fail-closed host-side egress for Firecracker TAPs.
//!
//! Applies per-VM nftables rules on the bridge forward/input hooks:
//! established traffic and explicit allowlist entries pass, everything
//! else from the guest IP is dropped. Requires root (Firecracker already
//! does).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::process::Command;

use pullrun_net::{Direction, NetworkRule, Protocol};

fn table_name(id: &str) -> String {
    let suffix: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("pullrun_vm_{suffix}")
}

fn nft(args: &[String]) -> Result<(), String> {
    let out = Command::new("nft")
        .args(args)
        .output()
        .map_err(|e| format!("spawn nft: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "nft {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

pub fn install(
    id: &str,
    guest_ip: Ipv4Addr,
    bridge: &str,
    rules: &[NetworkRule],
    nameservers: &[IpAddr],
    proxy: Option<SocketAddr>,
) -> Result<(), String> {
    let table = table_name(id);
    let _ = remove(id);
    nft(&["add".into(), "table".into(), "inet".into(), table.clone()])?;
    nft(&[
        "add".into(),
        "chain".into(),
        "inet".into(),
        table.clone(),
        "c_fwd".into(),
        "{ type filter hook forward priority 0 ; }".into(),
    ])?;
    nft(&[
        "add".into(),
        "chain".into(),
        "inet".into(),
        table.clone(),
        "c_inp".into(),
        "{ type filter hook input priority 0 ; }".into(),
    ])?;

    let push = |chain: &str, rule: Vec<String>| -> Result<(), String> {
        let mut args = vec![
            "add".into(),
            "rule".into(),
            "inet".into(),
            table.clone(),
            chain.into(),
        ];
        args.extend(rule);
        nft(&args)
    };

    push(
        "c_fwd",
        vec![
            "ct".into(),
            "state".into(),
            "established,related".into(),
            "accept".into(),
        ],
    )?;
    // DNS to the configured resolvers (or the gateway).
    let dns_ips: Vec<IpAddr> = if nameservers.is_empty() {
        vec![IpAddr::V4(crate::GATEWAY_IP)]
    } else {
        nameservers.to_vec()
    };
    for ns in dns_ips {
        for proto in ["udp", "tcp"] {
            push(
                "c_fwd",
                vec![
                    "ip".into(),
                    "saddr".into(),
                    guest_ip.to_string(),
                    "ip".into(),
                    "daddr".into(),
                    ns.to_string(),
                    proto.into(),
                    "dport".into(),
                    "53".into(),
                    "accept".into(),
                ],
            )?;
        }
    }
    // Host-side DNS for the gateway.
    push(
        "c_inp",
        vec![
            "iifname".into(),
            bridge.into(),
            "ip".into(),
            "saddr".into(),
            guest_ip.to_string(),
            "udp".into(),
            "dport".into(),
            "53".into(),
            "accept".into(),
        ],
    )?;
    push(
        "c_inp",
        vec![
            "iifname".into(),
            bridge.into(),
            "ip".into(),
            "saddr".into(),
            guest_ip.to_string(),
            "tcp".into(),
            "dport".into(),
            "53".into(),
            "accept".into(),
        ],
    )?;

    for rule in rules
        .iter()
        .filter(|r| matches!(r.direction, Direction::Outbound))
    {
        let Some(host) = &rule.to_host else { continue };
        let addrs: Vec<IpAddr> = match host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => (host.as_str(), rule.port)
                .to_socket_addrs()
                .map(|it| it.map(|s| s.ip()).collect())
                .unwrap_or_default(),
        };
        for ip in addrs {
            if !ip.is_ipv4() {
                continue;
            }
            let proto = match rule.protocol {
                Protocol::Udp => "udp",
                _ => "tcp",
            };
            for chain in ["c_fwd", "c_inp"] {
                let mut rule_vec = vec![
                    "ip".into(),
                    "saddr".into(),
                    guest_ip.to_string(),
                    "ip".into(),
                    "daddr".into(),
                    ip.to_string(),
                    proto.into(),
                    "dport".into(),
                    rule.port.to_string(),
                    "accept".into(),
                ];
                if chain == "c_inp" {
                    rule_vec.insert(0, bridge.into());
                    rule_vec.insert(0, "iifname".into());
                }
                push(chain, rule_vec)?;
            }
        }
    }
    if let Some(proxy) = proxy {
        for chain in ["c_fwd", "c_inp"] {
            let mut rule = vec![
                "ip".into(),
                "saddr".into(),
                guest_ip.to_string(),
                "ip".into(),
                "daddr".into(),
                proxy.ip().to_string(),
                "tcp".into(),
                "dport".into(),
                proxy.port().to_string(),
                "accept".into(),
            ];
            if chain == "c_inp" {
                rule.insert(0, bridge.into());
                rule.insert(0, "iifname".into());
            }
            push(chain, rule)?;
        }
    }

    // Default deny with a rate-limited audit log line per dropped flow.
    let prefix = format!("\"pullrun-audit {id} \"");
    push(
        "c_fwd",
        vec![
            "ip".into(),
            "saddr".into(),
            guest_ip.to_string(),
            "log".into(),
            "prefix".into(),
            prefix.clone(),
            "limit".into(),
            "rate".into(),
            "10/second".into(),
            "drop".into(),
        ],
    )?;
    push(
        "c_inp",
        vec![
            "iifname".into(),
            bridge.into(),
            "ip".into(),
            "saddr".into(),
            guest_ip.to_string(),
            "log".into(),
            "prefix".into(),
            prefix,
            "limit".into(),
            "rate".into(),
            "10/second".into(),
            "drop".into(),
        ],
    )?;
    Ok(())
}

pub fn remove(id: &str) -> Result<(), String> {
    let table = table_name(id);
    let out = Command::new("nft")
        .args(["delete", "table", "inet", &table])
        .output()
        .map_err(|e| format!("spawn nft: {e}"))?;
    if out.status.success()
        || String::from_utf8_lossy(&out.stderr).contains("No such file or directory")
    {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_names_are_bounded_and_distinct() {
        let a = table_name("wl-restored-18eac56b411f9380");
        let b = table_name("wl-restored-18eac5a4f4a1d163");
        assert!(a.len() <= 32);
        assert_ne!(a, b);
    }
}
