// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Minimal Firecracker snapshot/restore client.
//!
//! Talks the Firecracker HTTP API over its Unix socket. Used to pause a
//! VM, write a full memory+state snapshot, and load it into a fresh
//! Firecracker process. Disk backing files are managed by the caller.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Client for a single Firecracker API socket.
#[derive(Debug, Clone)]
pub struct FirecrackerApi {
    socket: PathBuf,
}

impl FirecrackerApi {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("connect {}: {e}", self.socket.display()))?;
        let body_bytes = body
            .map(|v| serde_json::to_vec(&v).map_err(|e| e.to_string()))
            .transpose()?
            .unwrap_or_default();
        let header = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body_bytes.len()
        );
        stream
            .write_all(header.as_bytes())
            .await
            .map_err(|e| format!("write header: {e}"))?;
        if !body_bytes.is_empty() {
            stream
                .write_all(&body_bytes)
                .await
                .map_err(|e| format!("write body: {e}"))?;
        }
        stream.flush().await.map_err(|e| e.to_string())?;

        let mut response = Vec::new();
        let mut header_end = None;
        let mut content_len = None;
        let mut status: Option<u16> = None;
        let read = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut chunk = [0u8; 4096];
                let n = stream
                    .read(&mut chunk)
                    .await
                    .map_err(|e| format!("read response: {e}"))?;
                if n == 0 {
                    break;
                }
                response.extend_from_slice(&chunk[..n]);
                if header_end.is_none() {
                    if let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos);
                        let head = String::from_utf8_lossy(&response[..pos]);
                        status = head
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .and_then(|s| s.parse::<u16>().ok());
                        for line in head.lines().skip(1) {
                            if let Some(v) = line
                                .to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                            {
                                content_len = Some(v);
                            }
                        }
                    }
                }
                if let (Some(pos), Some(st)) = (header_end, status) {
                    let body_len = response.len() - (pos + 4);
                    if st == 204 || st == 304 || body_len >= content_len.unwrap_or(0) {
                        break;
                    }
                }
            }
            Ok::<_, String>(())
        })
        .await;
        match read {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("timed out reading Firecracker response".to_string()),
        }
        let split = response
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(|| "malformed Firecracker HTTP response".to_string())?;
        let head = String::from_utf8_lossy(&response[..split]);
        let status = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| format!("malformed status line: {head}"))?;
        let body = &response[split + 4..];
        if !(200..300).contains(&status) {
            return Err(format!(
                "Firecracker {method} {path} -> {status}: {}",
                String::from_utf8_lossy(body).trim()
            ));
        }
        if body.is_empty() {
            Ok(Value::Null)
        } else {
            serde_json::from_slice(body).map_err(|e| format!("parse response: {e}"))
        }
    }

    /// Pause the VM (required before creating a snapshot).
    pub async fn pause(&self) -> Result<(), String> {
        self.request("PATCH", "/vm", Some(json!({"state": "Paused"})))
            .await
            .map(|_| ())
    }

    /// Resume a paused VM.
    pub async fn resume(&self) -> Result<(), String> {
        self.request("PATCH", "/vm", Some(json!({"state": "Resumed"})))
            .await
            .map(|_| ())
    }

    /// Write a full snapshot (state + memory) to the given paths.
    pub async fn create_full_snapshot(
        &self,
        state_path: &Path,
        mem_path: &Path,
    ) -> Result<(), String> {
        self.request(
            "PUT",
            "/snapshot/create",
            Some(json!({
                "snapshot_type": "Full",
                "snapshot_path": state_path.to_string_lossy(),
                "mem_file_path": mem_path.to_string_lossy(),
            })),
        )
        .await
        .map(|_| ())
    }

    /// Load a snapshot into a fresh Firecracker process and resume it.
    pub async fn load_snapshot(
        &self,
        state_path: &Path,
        mem_path: &Path,
        resume: bool,
    ) -> Result<(), String> {
        self.load_snapshot_with_tap(state_path, mem_path, resume, None)
            .await
    }

    /// Load a snapshot, optionally overriding the host TAP name.
    pub async fn load_snapshot_with_tap(
        &self,
        state_path: &Path,
        mem_path: &Path,
        resume: bool,
        tap_override: Option<&str>,
    ) -> Result<(), String> {
        let mut body = json!({
            "snapshot_path": state_path.to_string_lossy(),
            "mem_backend": {
                "backend_path": mem_path.to_string_lossy(),
                "backend_type": "File",
            },
            "enable_diff_snapshots": false,
            "resume_vm": resume,
        });
        if let Some(tap) = tap_override {
            body["network_overrides"] = json!([{
                "iface_id": "eth0",
                "host_dev_name": tap,
            }]);
        }
        self.request("PUT", "/snapshot/load", Some(body))
            .await
            .map(|_| ())
    }
}

/// Wait for a Firecracker API socket to accept connections.
pub async fn wait_for_api(socket: &Path, timeout: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for {}", socket.display()));
        }
        match UnixStream::connect(socket).await {
            Ok(_) => return Ok(()),
            Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

fn default_fidelity() -> String {
    "memory".to_string()
}

/// Metadata persisted next to a snapshot.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SnapshotMeta {
    /// Workload the snapshot was taken from.
    #[serde(default)]
    pub workload_id: String,
    /// "memory", "rootfs", or "volumes".
    #[serde(default = "default_fidelity")]
    pub fidelity: String,
    /// Kernel used by the snapshot's VM (for ROOTFS cold boot).
    #[serde(default)]
    pub kernel_path: PathBuf,
    /// vCPU count of the snapshot's VM.
    #[serde(default)]
    pub vcpus: u8,
    /// Guest memory size in MiB.
    #[serde(default)]
    pub mem_mib: u32,
    /// Path to the Firecracker VM state file.
    pub state_path: PathBuf,
    /// Path to the guest memory file.
    pub mem_path: PathBuf,
    /// Rootfs image backing the snapshot (must remain at this path).
    pub rootfs_path: PathBuf,
    /// Path the snapshot's drive expects on restore.
    #[serde(default)]
    pub drive_path: PathBuf,
    /// Unix seconds when the snapshot was created.
    pub created_at: i64,
    /// Size of the memory file in bytes.
    pub mem_bytes: u64,
    /// Guest memory is stored as content-addressed pages.
    #[serde(default)]
    pub paged_mem: bool,
    /// Files are AES-256-GCM encrypted at rest.
    #[serde(default)]
    pub encrypted: bool,
    /// SHA-256 of the Firecracker state file (empty for legacy snapshots).
    #[serde(default)]
    pub state_sha256: String,
    /// SHA-256 of the guest memory file.
    #[serde(default)]
    pub mem_sha256: String,
    /// SHA-256 of the rootfs snapshot file.
    #[serde(default)]
    pub rootfs_sha256: String,
    /// Network mode the snapshot was captured with (`none`, `isolated`, ...).
    #[serde(default)]
    pub network_mode: String,
    /// Guest IP at capture time, if networked.
    #[serde(default)]
    pub internal_ip: String,
    /// Bridge the guest was attached to, if networked.
    #[serde(default)]
    pub bridge_name: String,
    /// TAP device name referenced by the Firecracker snapshot.
    #[serde(default)]
    pub tap_name: String,
    /// Guest MAC at capture time.
    #[serde(default)]
    pub guest_mac: String,
    /// Guest gateway at capture time.
    #[serde(default)]
    pub gateway: String,
    /// Guest netmask at capture time.
    #[serde(default)]
    pub netmask: String,
}

/// SHA-256 of a file, hex encoded.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

/// Verify a file against a stored SHA-256. Empty hashes are legacy snapshots.
pub fn verify_file(path: &Path, expected: &str) -> Result<(), String> {
    if expected.is_empty() {
        return Ok(());
    }
    let actual = sha256_file(path)?;
    if actual != expected {
        return Err(format!(
            "snapshot integrity check failed for {}: expected {}, got {}",
            path.display(),
            expected,
            actual
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_memory_file_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem");
        std::fs::write(&path, b"snapshot-bytes").unwrap();
        let digest = sha256_file(&path).unwrap();
        assert!(verify_file(&path, &digest).is_ok());

        std::fs::write(&path, b"tampered").unwrap();
        assert!(verify_file(&path, &digest).is_err());
    }

    #[test]
    fn empty_hash_is_legacy_compatible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::write(&path, b"legacy").unwrap();
        assert!(verify_file(&path, "").is_ok());
    }
}
