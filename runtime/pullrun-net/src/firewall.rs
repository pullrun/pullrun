use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FirewallError {
    #[error("`{command}` command failed: {message}")]
    CommandFailed { command: String, message: String },
    #[error("`{command}` not found on host (required for outbound NAT)")]
    CommandNotFound { command: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// The three rules Pullrun needs for bridge-mode outbound NAT.
/// Each backend (iptables, nftables) implements this trait.
pub trait FirewallBackend: Send + Sync {
    /// Install MASQUERADE + FORWARD rules for the bridge.
    /// Idempotent — returns Ok(true) if any rule was newly installed,
    /// Ok(false) if all were already present.
    fn enable_nat(
        &self,
        bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<bool, FirewallError>;

    /// Remove all rules installed by `enable_nat`.
    /// Idempotent — missing rules are not errors.
    fn disable_nat(
        &self,
        bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<(), FirewallError>;

    /// Check whether all expected rules are present.
    /// Used by the idempotency check in `ensure_bridge`.
    fn rules_present(
        &self,
        bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<bool, FirewallError>;

    /// Human-readable name for logging ("iptables" or "nftables").
    fn name(&self) -> &'static str;
}

// ── IptablesBackend ────────────────────────────────────────────────

pub struct IptablesBackend;

impl IptablesBackend {
    pub fn new() -> Self {
        Self
    }

    fn check(&self, args: &[&str]) -> Result<bool, FirewallError> {
        let out = Command::new("iptables").args(args).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FirewallError::CommandNotFound {
                    command: "iptables".into(),
                }
            } else {
                FirewallError::CommandFailed {
                    command: format!("iptables {:?}", args),
                    message: e.to_string(),
                }
            }
        })?;
        Ok(out.status.success())
    }

    fn run(&self, args: &[&str]) -> Result<(), FirewallError> {
        let out = Command::new("iptables").args(args).output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                FirewallError::CommandNotFound {
                    command: "iptables".into(),
                }
            } else {
                FirewallError::CommandFailed {
                    command: format!("iptables {:?}", args),
                    message: e.to_string(),
                }
            }
        })?;
        if !out.status.success() {
            return Err(FirewallError::CommandFailed {
                command: format!("iptables {:?}", args),
                message: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        Ok(())
    }
}

impl Default for IptablesBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FirewallBackend for IptablesBackend {
    fn enable_nat(
        &self,
        bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<bool, FirewallError> {
        let mut installed = false;

        // 1. POSTROUTING MASQUERADE
        if self.check(&[
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-s",
            bridge_cidr,
            "!",
            "-d",
            bridge_cidr,
            "-o",
            outbound_iface,
            "-j",
            "MASQUERADE",
        ])? {
            tracing::debug!("MASQUERADE rule already present");
        } else {
            self.run(&[
                "-t",
                "nat",
                "-A",
                "POSTROUTING",
                "-s",
                bridge_cidr,
                "!",
                "-d",
                bridge_cidr,
                "-o",
                outbound_iface,
                "-j",
                "MASQUERADE",
            ])?;
            installed = true;
            tracing::info!(
                bridge = bridge_name,
                outbound = outbound_iface,
                "installed MASQUERADE rule"
            );
        }

        // 2. FORWARD bridge -> outbound
        if self.check(&[
            "-C",
            "FORWARD",
            "-i",
            bridge_name,
            "-o",
            outbound_iface,
            "-j",
            "ACCEPT",
        ])? {
            tracing::debug!("FORWARD bridge->outbound rule already present");
        } else {
            self.run(&[
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
            tracing::info!("installed FORWARD bridge->outbound rule");
        }

        // 3. FORWARD outbound -> bridge (established/related)
        if self.check(&[
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
            tracing::debug!("FORWARD outbound->bridge established rule already present");
        } else {
            self.run(&[
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
            tracing::info!("installed FORWARD outbound->bridge established rule");
        }

        Ok(installed)
    }

    fn disable_nat(
        &self,
        bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<(), FirewallError> {
        let _ = self.run(&[
            "-t",
            "nat",
            "-D",
            "POSTROUTING",
            "-s",
            bridge_cidr,
            "!",
            "-d",
            bridge_cidr,
            "-o",
            outbound_iface,
            "-j",
            "MASQUERADE",
        ]);
        let _ = self.run(&[
            "-D",
            "FORWARD",
            "-i",
            bridge_name,
            "-o",
            outbound_iface,
            "-j",
            "ACCEPT",
        ]);
        let _ = self.run(&[
            "-D",
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
        ]);
        Ok(())
    }

    fn rules_present(
        &self,
        _bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<bool, FirewallError> {
        self.check(&[
            "-t",
            "nat",
            "-C",
            "POSTROUTING",
            "-s",
            bridge_cidr,
            "!",
            "-d",
            bridge_cidr,
            "-o",
            outbound_iface,
            "-j",
            "MASQUERADE",
        ])
    }

    fn name(&self) -> &'static str {
        "iptables"
    }
}

// ── NftablesBackend ────────────────────────────────────────────────

pub struct NftablesBackend {
    table_name: String,
}

impl NftablesBackend {
    pub fn new() -> Self {
        Self {
            table_name: "pullrun".to_string(),
        }
    }

    fn run_transaction(&self, script: &str) -> Result<(), FirewallError> {
        use std::io::Write;
        let mut child = Command::new("nft")
            .arg("-f")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    FirewallError::CommandNotFound {
                        command: "nft".into(),
                    }
                } else {
                    FirewallError::Io(e)
                }
            })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(script.as_bytes())?;
        }
        drop(child.stdin.take());
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(FirewallError::CommandFailed {
                command: "nft".into(),
                message: String::from_utf8_lossy(&out.stderr).into(),
            });
        }
        Ok(())
    }

    fn rule_exists(&self, pattern: &str) -> Result<bool, FirewallError> {
        let out = Command::new("nft")
            .args(["list", "ruleset"])
            .output()
            .map_err(FirewallError::Io)?;
        let ruleset = String::from_utf8_lossy(&out.stdout);
        Ok(ruleset.contains(pattern))
    }
}

impl Default for NftablesBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl FirewallBackend for NftablesBackend {
    fn enable_nat(
        &self,
        bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<bool, FirewallError> {
        let masq_pattern =
            format!("oifname \"{outbound_iface}\" ip saddr {bridge_cidr} masquerade");
        if self.rule_exists(&masq_pattern)? {
            return Ok(false);
        }

        let script = format!(
            r#"
table inet {table} {{
    chain postrouting {{
        type nat hook postrouting priority 100; policy accept;
        oifname "{outbound}" ip saddr {cidr} masquerade
    }}
    chain forward {{
        type filter hook forward priority 0; policy accept;
        iifname "{bridge}" oifname "{outbound}" accept
        iifname "{outbound}" oifname "{bridge}" ct state established,related accept
    }}
}}
"#,
            table = self.table_name,
            bridge = bridge_name,
            cidr = bridge_cidr,
            outbound = outbound_iface,
        );
        self.run_transaction(&script)?;
        tracing::info!(
            backend = "nftables",
            bridge = bridge_name,
            outbound = outbound_iface,
            "installed NAT rules"
        );
        Ok(true)
    }

    fn disable_nat(
        &self,
        _bridge_name: &str,
        _bridge_cidr: &str,
        _outbound_iface: &str,
    ) -> Result<(), FirewallError> {
        let script = format!("delete table inet {}", self.table_name);
        let _ = self.run_transaction(&script);
        Ok(())
    }

    fn rules_present(
        &self,
        _bridge_name: &str,
        bridge_cidr: &str,
        outbound_iface: &str,
    ) -> Result<bool, FirewallError> {
        let masq_pattern =
            format!("oifname \"{outbound_iface}\" ip saddr {bridge_cidr} masquerade");
        self.rule_exists(&masq_pattern)
    }

    fn name(&self) -> &'static str {
        "nftables"
    }
}

// ── Auto-detection ─────────────────────────────────────────────────

/// Detect which firewall backend is available on the host.
/// Prefers nftables (modern) over iptables (legacy).
/// Returns None if neither is available.
pub fn detect_backend() -> Option<Box<dyn FirewallBackend>> {
    if command_exists("nft") {
        return Some(Box::new(NftablesBackend::new()));
    }
    if command_exists("iptables") {
        return Some(Box::new(IptablesBackend::new()));
    }
    None
}

fn command_exists(cmd: &str) -> bool {
    Command::new(cmd).arg("--version").output().is_ok()
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iptables_backend_name() {
        let b = IptablesBackend::new();
        assert_eq!(b.name(), "iptables");
    }

    #[test]
    fn test_nftables_backend_name() {
        let b = NftablesBackend::new();
        assert_eq!(b.name(), "nftables");
    }

    #[test]
    fn test_detect_backend_returns_on_ci() {
        let backend = detect_backend();
        if cfg!(target_os = "linux") {
            let has_nft = command_exists("nft");
            let has_iptables = command_exists("iptables");
            if has_nft || has_iptables {
                assert!(
                    backend.is_some(),
                    "expected a firewall backend when nft or iptables exists"
                );
            } else {
                assert!(
                    backend.is_none(),
                    "no backend expected when neither nft nor iptables is installed"
                );
            }
        } else {
            assert!(backend.is_none(), "no firewall backend on non-Linux");
        }
    }

    #[test]
    fn test_command_exists_positive() {
        // `true` is a command that exists on every Unix-like system.
        assert!(command_exists("true"));
    }

    #[test]
    fn test_command_exists_negative() {
        assert!(!command_exists("this-command-does-not-exist-12345"));
    }
}
