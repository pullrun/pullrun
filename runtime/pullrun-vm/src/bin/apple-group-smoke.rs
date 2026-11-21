// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Sandbox-group smoke driver for Apple Virtualization.
//!
//! Boots one VM in group mode and runs several sandboxes sequentially
//! in the same guest (shared page cache), printing their output and
//! exit codes. Requires the virtualization entitlement:
//!
//!   cargo build -p pullrun-vm --bin apple-group-smoke
//!   codesign -f -s - --entitlements tools/apple-virt-smoke/virt.entitlements \
//!     target/debug/apple-group-smoke
//!   PULLRUN_KERNEL_PATH=... PULLRUN_INITRAMFS_PATH=... \
//!   PULLRUN_GROUP_ROOTFS=... target/debug/apple-group-smoke

#[cfg(target_os = "macos")]
fn main() {
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    use pullrun_vm::attach::{run_session_blocking, FrameSink, FrameSource};
    use pullrun_vm::{AppleVirtAttachConfig, DEFAULT_VSOCK_PORT};
    use pullrun_vsock::{Frame, VsockMount};

    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let Some(kernel) = env("PULLRUN_KERNEL_PATH") else {
        eprintln!("PULLRUN_KERNEL_PATH is required");
        std::process::exit(2);
    };
    let Some(initramfs) = env("PULLRUN_INITRAMFS_PATH") else {
        eprintln!("PULLRUN_INITRAMFS_PATH is required");
        std::process::exit(2);
    };
    let Some(rootfs) = env("PULLRUN_GROUP_ROOTFS") else {
        eprintln!("PULLRUN_GROUP_ROOTFS is required");
        std::process::exit(2);
    };

    let staged =
        pullrun_vm::StagedKernel::from_paths(PathBuf::from(kernel), Some(PathBuf::from(initramfs)))
            .expect("stage kernel");

    let cfg = AppleVirtAttachConfig {
        kernel: staged,
        rootfs_dir: PathBuf::from(rootfs),
        rootfs_upper: None,
        group_mode: true,
        command: vec![],
        env: vec![],
        working_dir: "/".to_string(),
        cpus: 1,
        mem_mib: 512,
        vsock_port: Some(DEFAULT_VSOCK_PORT),
        console_log: Some(PathBuf::from("/tmp/pullrun-group-smoke-console.log")),
        workload_id: "group-smoke".to_string(),
        tty: false,
        initial_rows: 0,
        initial_cols: 0,
        mounts: Vec::new(),
    };

    // The driver runs on a side thread; the main thread must pump the
    // main dispatch queue for Virtualization.framework callbacks.
    std::thread::spawn(move || {
        let (client_in_tx, client_in_rx): (mpsc::Sender<Frame>, FrameSource) = mpsc::channel();
        let (server_out_tx, server_out_rx): (FrameSink, mpsc::Receiver<Frame>) = mpsc::channel();

        std::thread::spawn(move || {
            let result = run_session_blocking(cfg, client_in_rx, server_out_tx);
            eprintln!("session ended: {result:?}");
            std::process::exit(if result.is_ok() { 0 } else { 1 });
        });

        let send = |command: &[&str]| {
            client_in_tx
                .send(Frame::WorkloadSpec {
                    command: command.iter().map(|s| s.to_string()).collect(),
                    env: vec!["PATH=/bin:/usr/bin".to_string()],
                    working_dir: "/".to_string(),
                    tty: false,
                    rows: 0,
                    cols: 0,
                    mounts: Vec::<VsockMount>::new(),
                })
                .unwrap();
        };

        let run = |command: &[&str]| -> (String, Option<i32>) {
            send(command);
            let mut stdout = String::new();
            loop {
                match server_out_rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(Frame::WorkloadStdout(data)) => {
                        stdout.push_str(&String::from_utf8_lossy(&data))
                    }
                    Ok(Frame::WorkloadStderr(data)) => {
                        eprintln!("stderr: {}", String::from_utf8_lossy(&data))
                    }
                    Ok(Frame::WorkloadExit { exit_code, .. }) => return (stdout, exit_code),
                    Ok(Frame::Error(msg)) => panic!("guest error: {msg}"),
                    Ok(_) => {}
                    Err(e) => panic!("sandbox timeout: {e}"),
                }
            }
        };

        let (one, c1) = run(&["/bin/sh", "-c", "echo one"]);
        println!("sandbox1 exit={c1:?} stdout={one:?}");
        let (two, c2) = run(&["/bin/sh", "-c", "echo two > /probe; cat /probe"]);
        println!("sandbox2 exit={c2:?} stdout={two:?}");
        let (_, c3) = run(&["/bin/sh", "-c", "test ! -e /probe"]);
        println!("sandbox3 isolation exit={c3:?}");
        let (_, c4) = run(&["/bin/sh", "-c", "exit 42"]);
        println!("sandbox4 exit={c4:?}");

        let ok = c1 == Some(0)
            && one.contains("one")
            && c2 == Some(0)
            && two.contains("two")
            && c3 == Some(0)
            && c4 == Some(42);

        let _ = client_in_tx.send(Frame::GroupShutdown);
        drop(client_in_tx);
        std::process::exit(if ok { 0 } else { 3 });
    });

    extern "C" {
        fn dispatch_main();
    }
    unsafe { dispatch_main() };
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("apple-group-smoke is macOS-only");
    std::process::exit(2);
}
