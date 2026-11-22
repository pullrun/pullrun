// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Firecracker memory snapshot/restore test.
//!
//! Boots a VM whose `/init` keeps a counter in shell memory and prints
//! `tick N` every second. After a few ticks it pauses the VM, writes a
//! full snapshot, kills the original Firecracker process, starts a fresh
//! one, loads the snapshot, and asserts the guest keeps counting from a
//! value greater than the pre-snapshot tick. That proves guest memory and
//! process state were restored, not just the disk.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use pullrun_vm::snapshot::{wait_for_api, FirecrackerApi};

fn skip_if_unavailable() -> Option<(PathBuf, PathBuf)> {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skip: /dev/kvm missing");
        return None;
    }
    let fc = match std::env::var("PULLRUN_FC_BIN") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            eprintln!("skip: PULLRUN_FC_BIN unset");
            return None;
        }
    };
    let kernel = match std::env::var("PULLRUN_FC_VMLINUX") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            eprintln!("skip: PULLRUN_FC_VMLINUX unset");
            return None;
        }
    };
    Some((fc, kernel))
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

fn run(argv: &[&str]) -> Result<(), String> {
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
}

fn build_rootfs(dir: &Path) -> Result<PathBuf, String> {
    let rootfs = dir.join("rootfs");
    std::fs::create_dir_all(&rootfs).map_err(|e| e.to_string())?;
    let url = std::env::var("PULLRUN_FC_ROOTFS_TAR").unwrap_or_else(|_| {
        "https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz".into()
    });
    eprintln!("[firecracker_snapshot] fetching rootfs: {url}");
    let bytes = if url.starts_with("http") {
        fetch_url(&url)?
    } else {
        std::fs::read(&url).map_err(|e| e.to_string())?
    };
    let decoder = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(&rootfs).map_err(|e| format!("untar: {e}"))?;

    // Keep the counter in shell memory; only the serial console is written.
    let init = "#!/bin/sh\ni=0\nwhile true; do\n  i=$((i+1))\n  echo \"tick $i\" >/dev/ttyS0\n  sleep 1\ndone\n";
    std::fs::write(rootfs.join("init"), init).map_err(|e| e.to_string())?;
    run(&["chmod", "+x", rootfs.join("init").to_str().unwrap()])?;

    let image = dir.join("rootfs.ext4");
    let _ = std::fs::remove_file(&image);
    run(&["truncate", "-s", "128M", image.to_str().unwrap()])?;
    run(&[
        "mkfs.ext4",
        "-F",
        "-q",
        "-d",
        rootfs.to_str().unwrap(),
        image.to_str().unwrap(),
    ])?;
    Ok(image)
}

fn last_tick(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("tick "))
        .filter_map(|n| n.trim().parse::<u64>().ok())
        .last()
}

fn wait_for_tick(path: &Path, min: u64, timeout: Duration) -> Option<u64> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(n) = last_tick(path) {
            if n >= min {
                return Some(n);
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn kill(mut child: Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires /dev/kvm, firecracker, a vmlinux, and network for the rootfs"]
async fn snapshot_restores_guest_memory() {
    let Some((fc, kernel)) = skip_if_unavailable() else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let image = build_rootfs(dir.path()).expect("build rootfs");

    let cfg1 = dir.path().join("vm1.json");
    let cfg = serde_json::json!({
        "boot-source": {
            "kernel_image_path": kernel.to_string_lossy(),
            "boot_args": "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw init=/init"
        },
        "drives": [{
            "drive_id": "rootfs",
            "path_on_host": image.to_string_lossy(),
            "is_root_device": true,
            "is_read_only": false
        }],
        "machine-config": { "vcpu_count": 1, "mem_size_mib": 256 },
        "network-interfaces": []
    });
    std::fs::write(&cfg1, serde_json::to_vec(&cfg).unwrap()).expect("write config");

    let sock1 = dir.path().join("fc1.sock");
    let console1 = dir.path().join("console1.log");
    let console1_err = dir.path().join("console1.err");
    let child1 = Command::new(&fc)
        .arg("--api-sock")
        .arg(&sock1)
        .arg("--config-file")
        .arg(&cfg1)
        .stdout(Stdio::from(std::fs::File::create(&console1).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&console1_err).unwrap()))
        .spawn()
        .expect("spawn firecracker");

    wait_for_api(&sock1, Duration::from_secs(10))
        .await
        .expect("fc1 api");
    let before = wait_for_tick(&console1, 2, Duration::from_secs(30)).expect("guest ticks");
    eprintln!("[firecracker_snapshot] pre-snapshot tick={before}");

    let api1 = FirecrackerApi::new(&sock1);
    api1.pause().await.expect("pause");
    let state = dir.path().join("snapshot.state");
    let mem = dir.path().join("snapshot.mem");
    api1.create_full_snapshot(&state, &mem)
        .await
        .expect("create snapshot");
    assert!(state.is_file() && mem.is_file(), "snapshot files exist");
    eprintln!(
        "[firecracker_snapshot] snapshot state={} mem={} bytes",
        state.metadata().unwrap().len(),
        mem.metadata().unwrap().len()
    );

    // Secure usage: the original VM is terminated and only the restored
    // clone resumes from this state.
    kill(child1);

    let sock2 = dir.path().join("fc2.sock");
    let console2 = dir.path().join("console2.log");
    let console2_err = dir.path().join("console2.err");
    let child2 = Command::new(&fc)
        .arg("--api-sock")
        .arg(&sock2)
        .stdout(Stdio::from(std::fs::File::create(&console2).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&console2_err).unwrap()))
        .spawn()
        .expect("spawn restore firecracker");
    wait_for_api(&sock2, Duration::from_secs(10))
        .await
        .expect("fc2 api");
    FirecrackerApi::new(&sock2)
        .load_snapshot(&state, &mem, true)
        .await
        .expect("load snapshot");

    let after =
        wait_for_tick(&console2, before + 1, Duration::from_secs(20)).unwrap_or_else(|| {
            panic!(
                "restored guest did not continue counting (before={before}); console: {}",
                std::fs::read_to_string(&console2).unwrap_or_default()
            )
        });
    eprintln!("[firecracker_snapshot] post-restore tick={after}");
    assert!(after > before, "guest memory/process state was restored");
    kill(child2);
}
