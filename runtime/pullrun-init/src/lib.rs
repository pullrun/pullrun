// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! # pullrun-init
//!
//! Guest-side init daemon for Pullrun microVMs.
//!
//! ## Lifecycle
//!
//! 1. The kernel boots, mounts the rootfs (9p/virtiofs), and
//!    `exec`s `/sbin/pullrun-init` as PID 1.
//! 2. `pullrun-init` opens a vsock connection to the host on
//!    port `DEFAULT_VSOCK_PORT` (42).
//! 3. It sends `Frame::InitHello { workload_id, init_pid }` so
//!    the host knows which workload this is and which PID is
//!    init (not the workload's PID).
//! 4. It receives `Frame::WorkloadSpec { command, env, cwd }`
//!    from the host. (The host may also send `Frame::Error` if
//!    the workload was rejected.)
//! 5. It `fork()`+`exec()`s the workload command, wiring up
//!    stdin/stdout/stderr to the vsock stream.
//! 6. It pumps bytes between the workload's stdio and vsock
//!    frames until the workload exits.
//! 7. It sends `Frame::WorkloadExit { code, signal }` and exits.
//!
//! ## stdio wiring
//!
//! We use a `tokio` runtime to multiplex three async tasks:
//!
//! - **vsock → workload stdin**: takes `Frame::WorkloadStdin`
//!   from the host, writes to the child's stdin pipe. A
//!   `Frame::StdinEof` causes the pipe to close.
//! - **workload stdout → vsock**: reads from the child's stdout
//!   pipe, wraps in `Frame::WorkloadStdout`.
//! - **workload stderr → vsock**: reads from the child's stderr
//!   pipe, wraps in `Frame::WorkloadStderr`.
//!
//! The child's exit is awaited with `waitpid`; the exit code
//! is sent as `Frame::WorkloadExit`.
//!
//! ## Error reporting
//!
//! If something goes wrong (vsock connect fails, exec fails,
//! etc.), we send `Frame::Error { message }` to the host
//! before exiting with a non-zero status. The host surfaces
//! the message to the user via the workload's `reason` field
//! in `pullrun workload inspect`.
//!
//! ## Why a separate crate
//!
//! `pullrun-init` is statically linked into the initramfs. It
//! must NOT depend on anything that requires dynamic linking
//! (no `dlopen`, no `glibc` symbols). Using a separate
//! workspace member lets us build it with
//! `RUSTFLAGS="-C target-feature=+crt-static"` independently
//! of the host binaries.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]

pub mod vsock_client;

pub use vsock_client::VsockClient;

use tracing::info;

/// Default port for the host-side vsock listener.
pub use pullrun_vsock::DEFAULT_VSOCK_PORT as DEFAULT_HOST_VSOCK_PORT;

/// Library entry point: run the init daemon.
///
/// This is a no-op when called from a non-init process. The
/// real entry point is `main()` (in `main.rs`) which calls
/// this and converts the `Result` into an `ExitCode`.
///
/// Public so tests and embedders can call it directly.
pub async fn run() -> Result<(), InitError> {
    info!("pullrun-init starting");

    let workload_id =
        std::env::var("PULLRUN_WORKLOAD_ID").unwrap_or_else(|_| "(unset)".to_string());
    let init_pid = std::process::id();

    // 1. Connect to the host over vsock.
    let mut client = VsockClient::connect(DEFAULT_HOST_VSOCK_PORT)
        .await
        .map_err(InitError::VsockConnect)?;

    // 2. Send the init hello.
    client
        .send_frame(pullrun_vsock::Frame::InitHello {
            workload_id: workload_id.clone(),
            init_pid,
        })
        .await
        .map_err(InitError::VsockSend)?;
    info!(workload_id, init_pid, "sent init hello");

    // 3. Receive the workload spec (or error).
    let spec = match client.recv_frame().await.map_err(InitError::VsockRecv)? {
        pullrun_vsock::Frame::WorkloadSpec {
            command,
            env,
            working_dir,
            tty,
            rows,
            cols,
            mounts,
        } => {
            info!(?command, working_dir = %working_dir, env_count = env.len(), tty, rows, cols, mount_count = mounts.len(), "got workload spec");
            Workload {
                command,
                env,
                working_dir,
                tty,
                rows,
                cols,
                mounts,
            }
        }
        pullrun_vsock::Frame::Error(msg) => {
            return Err(InitError::HostRejected(msg));
        }
        other => {
            return Err(InitError::Protocol(format!(
                "expected WorkloadSpec, got {other:?}"
            )));
        }
    };

    // 4. Spawn the workload and pump I/O.
    let exit = spec.run(&mut client).await?;
    info!(code = ?exit.code, signal = ?exit.signal, "workload exited");

    // 5. Send the exit frame.
    client
        .send_frame(pullrun_vsock::Frame::WorkloadExit {
            exit_code: exit.code,
            signal: exit.signal,
        })
        .await
        .map_err(InitError::VsockSend)?;

    info!("pullrun-init exiting cleanly");
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("vsock connect failed: {0}")]
    VsockConnect(#[source] crate::vsock_client::VsockError),

    #[error("vsock send failed: {0}")]
    VsockSend(#[source] crate::vsock_client::VsockError),

    #[error("vsock recv failed: {0}")]
    VsockRecv(#[source] crate::vsock_client::VsockError),

    #[error("host rejected workload: {0}")]
    HostRejected(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("workload exec failed: {0}")]
    Exec(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A parsed `WorkloadSpec` ready to be executed.
#[derive(Debug)]
struct Workload {
    command: Vec<String>,
    env: Vec<String>,
    working_dir: String,
    tty: bool,
    rows: u32,
    cols: u32,
    #[allow(dead_code)]
    mounts: Vec<pullrun_vsock::VsockMount>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WorkloadExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Workload {
    /// Mount VirtioFS volume shares before spawning the workload.
    #[cfg(target_os = "linux")]
    fn mount_volumes(&self) -> Result<(), InitError> {
        for m in &self.mounts {
            std::fs::create_dir_all(&m.destination).map_err(InitError::Io)?;
            let tag_c = std::ffi::CString::new(m.tag.as_bytes())
                .map_err(|_| InitError::Exec(format!("NUL in mount tag: {}", m.tag)))?;
            let dest_c = std::ffi::CString::new(m.destination.as_bytes()).map_err(|_| {
                InitError::Exec(format!("NUL in mount destination: {}", m.destination))
            })?;
            let fs_type_c = std::ffi::CString::new("virtiofs")
                .map_err(|_| InitError::Exec("NUL in virtiofs".into()))?;
            let flags: libc::c_ulong = if m.read_only {
                libc::MS_RDONLY as libc::c_ulong
            } else {
                0
            };
            let ret = unsafe {
                libc::mount(
                    tag_c.as_ptr(),
                    dest_c.as_ptr(),
                    fs_type_c.as_ptr(),
                    flags,
                    std::ptr::null(),
                )
            };
            if ret != 0 {
                return Err(InitError::Io(std::io::Error::last_os_error()));
            }
            info!(tag = %m.tag, destination = %m.destination, read_only = m.read_only, "mounted VirtioFS volume");
        }
        Ok(())
    }

    /// Mount the OCI rootfs VirtioFS share and chroot into it, so
    /// the workload sees the OCI image's full userland (not just
    /// the minimal initramfs).  The VirtioFS tag is
    /// "pullrun-rootfs", configured by the AppleVM host in
    /// `apple/attach.rs`.
    #[cfg(target_os = "linux")]
    fn mount_rootfs(&self) -> Result<(), InitError> {
        const ROOTFS_TAG: &str = "pullrun-rootfs";
        const MOUNT_POINT: &str = "/mnt/root";

        std::fs::create_dir_all(MOUNT_POINT).map_err(InitError::Io)?;

        let tag_c =
            std::ffi::CString::new(ROOTFS_TAG).map_err(|_| InitError::Exec("NUL in tag".into()))?;
        let mnt_c = std::ffi::CString::new(MOUNT_POINT)
            .map_err(|_| InitError::Exec("NUL in mount point".into()))?;
        let fs_type_c = std::ffi::CString::new("virtiofs")
            .map_err(|_| InitError::Exec("NUL in virtiofs".into()))?;

        let ret = unsafe {
            libc::mount(
                tag_c.as_ptr(),
                mnt_c.as_ptr(),
                fs_type_c.as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        if ret != 0 {
            return Err(InitError::Io(std::io::Error::last_os_error()));
        }
        info!("mounted OCI rootfs via VirtioFS");

        // Mount /proc inside the new root so the workload sees a
        // proper /proc (needed by many tools).
        let proc_dest = std::ffi::CString::new(format!("{MOUNT_POINT}/proc"))
            .map_err(|_| InitError::Exec("NUL in proc path".into()))?;
        let proc_type =
            std::ffi::CString::new("proc").map_err(|_| InitError::Exec("NUL in proc".into()))?;
        let _ = unsafe {
            libc::mount(
                c"proc".as_ptr(),
                proc_dest.as_ptr(),
                proc_type.as_ptr(),
                0,
                std::ptr::null(),
            )
        };

        // chroot into the OCI rootfs.
        let ret = unsafe { libc::chroot(mnt_c.as_ptr()) };
        if ret != 0 {
            return Err(InitError::Io(std::io::Error::last_os_error()));
        }
        unsafe { libc::chdir(c"/".as_ptr()) };
        info!("chrooted into OCI rootfs");

        Ok(())
    }

    /// Configure the VM's network interface before the workload starts.
    ///
    /// The Apple Virtualization framework creates a virtio NIC with a
    /// `VZNATNetworkDeviceAttachment`, but the guest is responsible for
    /// bringing it up.  Without this step, `eth0` exists but is `DOWN`
    /// with no IP address, causing all network operations to fail with
    /// `ENETUNREACH`.
    ///
    /// This uses `busybox` applets from the initramfs (`/bin/ifconfig`,
    /// `/bin/route`).  These are NOT available after `mount_rootfs()`
    /// chroots into the OCI rootfs, so this must be called *before* the
    /// chroot.
    #[cfg(target_os = "linux")]
    /// Minimal script for `udhcpc` that applies the DHCP lease.
    const UDHCPC_SCRIPT: &str = r"#!/bin/sh
case $1 in
    bound|renew)
        /bin/ifconfig $interface $ip netmask $subnet
        /bin/route add default gw $router 2>/dev/null
        for d in $dns; do echo nameserver $d; done > /etc/resolv.conf
        ;;
esac
";

    #[cfg(target_os = "linux")]
    fn configure_network() -> Result<(), InitError> {
        use std::process::Command;

        fn run(args: &[&str]) -> Result<(), InitError> {
            let bin = args[0];
            let output = Command::new(bin)
                .args(&args[1..])
                .output()
                .map_err(|e| InitError::Exec(format!("spawn {bin}: {e}")))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(InitError::Exec(format!("{bin} failed: {stderr}")));
            }
            Ok(())
        }

        // 1. Loopback.
        let _ = run(&["/bin/ifconfig", "lo", "127.0.0.1", "up"]);

        // 2. Write the udhcpc script and make it executable.
        std::fs::write("/etc/udhcpc.script", Self::UDHCPC_SCRIPT)
            .map_err(|e| InitError::Exec(format!("write script: {e}")))?;
        let _ = run(&["/bin/chmod", "+x", "/etc/udhcpc.script"]);

        // 3. Bring eth0 up (needed for DHCP to send discovers).
        run(&["/bin/ifconfig", "eth0", "up"])?;

        // 4. Request a DHCP lease.  The Apple Virt NAT provides
        //    DHCP (192.168.64.0/24, gateway .1).  If DHCP fails
        //    (e.g. delayed Virtio init) we fall back to a static
        //    IP so the workload always gets a working network.
        //    -q:  quit after obtaining a lease
        //    -n:  exit with non-zero if no lease is obtained
        //    -T:  per-packet timeout (seconds)
        //    -t:  max discover attempts
        let dhcp_ok = run(&[
            "/bin/udhcpc",
            "-i",
            "eth0",
            "-s",
            "/etc/udhcpc.script",
            "-q",
            "-n",
            "-T",
            "2",
            "-t",
            "3",
        ]);
        if dhcp_ok.is_ok() {
            info!("network configured via DHCP");
        } else {
            tracing::warn!("DHCP failed, using static IP");
            run(&[
                "/bin/ifconfig",
                "eth0",
                "192.168.64.2",
                "netmask",
                "255.255.255.0",
            ])?;
            run(&["/bin/route", "add", "default", "gw", "192.168.64.1"])?;
            std::fs::write(
                "/etc/resolv.conf",
                "nameserver 192.168.64.1\nnameserver 8.8.8.8\n",
            )
            .ok();
        }
        Ok(())
    }

    /// Spawn the workload, wire stdio to vsock, and return when
    /// the workload exits.  Dispatches to the PTY or piped
    /// implementation based on `self.tty`.
    async fn run(self, client: &mut VsockClient) -> Result<WorkloadExit, InitError> {
        #[cfg(target_os = "linux")]
        {
            Self::configure_network()?;
            self.mount_volumes()?;
            self.mount_rootfs()?;
            // After chroot, overwrite /etc/resolv.conf so the OCI
            // image's baked-in DNS (often 1.1.1.1) is replaced with
            // the NAT gateway + a public fallback.
            let _ = std::fs::write(
                "/etc/resolv.conf",
                "nameserver 192.168.64.1\nnameserver 8.8.8.8\n",
            );
        }
        if self.tty {
            self.run_tty(client).await
        } else {
            self.run_piped(client).await
        }
    }

    /// PTY-based I/O path.
    ///
    /// When `tty=true` we allocate a pseudo-terminal, attach the
    /// workload to the slave side, and pump bytes between the
    /// master side and vsock.  This gives the workload a real
    /// TTY (isatty = true) so interactive programs like shells
    /// and editors work properly.
    async fn run_tty(self, client: &mut VsockClient) -> Result<WorkloadExit, InitError> {
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
        use tokio::io::unix::AsyncFd;

        if self.command.is_empty() {
            return Err(InitError::Exec("empty command".into()));
        }

        let (command, _rest) = self.command.split_first().unwrap();

        info!(
            command = %command,
            working_dir = %self.working_dir,
            tty = true,
            "spawning workload with PTY",
        );

        // Allocate a pseudo-terminal.  We use raw libc calls
        // directly instead of `openpty()` so this works with
        // musl-based toolchains (common for static initramfs
        // binaries).
        //
        // On Linux, ensure the devpts filesystem is available.
        // In a minimal initramfs there is no `/dev/pts` or
        // `/dev/ptmx`, so we create them on demand.
        #[cfg(target_os = "linux")]
        unsafe {
            let _ = libc::mkdir(c"/dev/pts".as_ptr(), 0o755);
            libc::mount(
                c"devpts".as_ptr(),
                c"/dev/pts".as_ptr(),
                c"devpts".as_ptr(),
                0,
                std::ptr::null(),
            );
            let _ = libc::unlink(c"/dev/ptmx".as_ptr());
            libc::symlink(c"pts/ptmx".as_ptr(), c"/dev/ptmx".as_ptr());
        }
        // On macOS the host provides devfs with ptmx already
        // available, so nothing extra is needed.

        let (master_fd, slave_fd) = unsafe {
            let master = libc::open(
                c"/dev/ptmx".as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            );
            if master < 0 {
                return Err(InitError::Exec(format!(
                    "open /dev/ptmx: {}",
                    std::io::Error::last_os_error(),
                )));
            }
            if libc::grantpt(master) != 0 {
                let e = std::io::Error::last_os_error();
                let _ = libc::close(master);
                return Err(InitError::Exec(format!("grantpt: {e}")));
            }
            if libc::unlockpt(master) != 0 {
                let e = std::io::Error::last_os_error();
                let _ = libc::close(master);
                return Err(InitError::Exec(format!("unlockpt: {e}")));
            }
            let slave_name = libc::ptsname(master);
            if slave_name.is_null() {
                let e = std::io::Error::last_os_error();
                let _ = libc::close(master);
                return Err(InitError::Exec(format!("ptsname: {e}")));
            }
            let slave = libc::open(slave_name, libc::O_RDWR | libc::O_CLOEXEC);
            if slave < 0 {
                let e = std::io::Error::last_os_error();
                let _ = libc::close(master);
                return Err(InitError::Exec(format!("open slave: {e}")));
            }

            // Put the slave in raw mode so the workload
            // behaves as a real terminal (no line-buffering,
            // no echo).  Re-enable ISIG so that ^C generates
            // SIGINT and ^\ generates SIGQUIT, just like a
            // real terminal would.
            //
            // cfmakeraw clears OPOST (output processing),
            // which means \n is no longer expanded to \r\n.
            // We restore OPOST|ONLCR so the PTY output
            // contains proper carriage returns — without
            // them the host terminal (itself in raw mode)
            // would move the cursor down but not back to
            // column 0, making text appear invisible.
            let mut termios: libc::termios = std::mem::zeroed();
            libc::tcgetattr(slave, &mut termios);
            libc::cfmakeraw(&mut termios);
            termios.c_iflag |= libc::ICRNL;
            termios.c_lflag |= libc::ISIG | libc::ECHO;
            termios.c_lflag &= !libc::ECHOCTL;
            termios.c_oflag |= libc::OPOST | libc::ONLCR;
            libc::tcsetattr(slave, libc::TCSANOW, &termios);

            // Set a default window size so the shell gets a
            // reasonable initial geometry.  The host will
            // send an updated size via WindowSize frames.
            let ws = libc::winsize {
                ws_row: if self.rows > 0 { self.rows as u16 } else { 24 },
                ws_col: if self.cols > 0 { self.cols as u16 } else { 80 },
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            libc::ioctl(slave, libc::TIOCSWINSZ, &ws);

            let master_owned = OwnedFd::from_raw_fd(master);
            let slave_owned = OwnedFd::from_raw_fd(slave);
            (master_owned, slave_owned)
        };

        // We do a manual fork + exec instead of
        // `Command::spawn()` so we can call `setsid()` and
        // `TIOCSCTTY` in the child between fork and exec.
        // This makes the PTY slave the controlling terminal
        // of a new session, enabling job control and
        // eliminating "can't access tty" warnings.
        //
        // The slave PTY name is passed to the child via a
        // stack-allocated C string.  The child opens the
        // slave by name *after* `setsid()`, which makes the
        // kernel auto-assign it as the controlling terminal.
        let slave_name_c = {
            let sn = unsafe { libc::ptsname(master_fd.as_raw_fd()) };
            if sn.is_null() {
                return Err(InitError::Exec("ptsname returned null".into()));
            }
            // SAFETY: ptsname returns a pointer to a static
            // buffer that is valid until the next call to
            // ptsname/ptsname_r.  We copy it immediately.
            unsafe { std::ffi::CString::from_raw(libc::strdup(sn)) }
        };
        info!("spawning workload now");
        let child_pid: i32;
        {
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                return Err(InitError::Exec(format!(
                    "fork: {}",
                    std::io::Error::last_os_error(),
                )));
            }
            if pid == 0 {
                // ---- child process ----
                // SAFETY: We are in a freshly forked child.
                // Only async-signal-safe libc calls are allowed
                // here.  No Rust allocation, no tokio, no mutex.

                // 1. Create a new session.  This detaches
                //    from any inherited controlling terminal
                //    and makes this process a session leader.
                unsafe {
                    if libc::setsid() < 0 {
                        libc::_exit(127);
                    }

                    // 2. Open the slave PTY by name.  Because we
                    //    are a session leader with no controlling
                    //    terminal, the kernel automatically
                    //    associates this device as the controlling
                    //    terminal.
                    let slave_fd = libc::open(slave_name_c.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
                    if slave_fd < 0 {
                        libc::_exit(127);
                    }

                    // 3. Duplicate the slave to fd 0, 1, 2.
                    libc::dup2(slave_fd, 0);
                    libc::dup2(slave_fd, 1);
                    libc::dup2(slave_fd, 2);
                    if slave_fd > 2 {
                        libc::close(slave_fd);
                    }

                    // 4. Set the controlling terminal explicitly
                    //    (belt-and-suspenders with step 2).
                    #[cfg(not(target_os = "linux"))]
                    libc::ioctl(0, libc::TIOCSCTTY.into(), 0);
                    #[cfg(target_os = "linux")]
                    libc::ioctl(0, libc::TIOCSCTTY, 0);

                    // 5. Set the working directory if specified.
                    if !self.working_dir.is_empty() {
                        let cwd = std::ffi::CString::new(self.working_dir.clone()).unwrap();
                        libc::chdir(cwd.as_ptr());
                    }

                    // 6. Clear and set environment variables.
                    // clearenv is not available on all platforms;
                    // environ is reset by the kernel on exec anyway.
                    // libc::clearenv();
                    for var in &self.env {
                        if let Some((k, v)) = var.split_once('=') {
                            let key = std::ffi::CString::new(k).unwrap();
                            let val = std::ffi::CString::new(v).unwrap();
                            libc::setenv(key.as_ptr(), val.as_ptr(), 1);
                        }
                    }

                    // For TTY workloads, ensure TERM is set so
                    // the shell knows it's connected to a real
                    // terminal (enables line-buffering, color,
                    // cursor movement, etc.).
                    if self.tty {
                        let term = std::ffi::CString::new("xterm-256color").unwrap();
                        libc::setenv(c"TERM".as_ptr(), term.as_ptr(), 1);
                        let cols_str = std::ffi::CString::new(
                            (if self.cols > 0 { self.cols } else { 80 }).to_string(),
                        )
                        .unwrap();
                        let rows_str = std::ffi::CString::new(
                            (if self.rows > 0 { self.rows } else { 24 }).to_string(),
                        )
                        .unwrap();
                        libc::setenv(c"COLUMNS".as_ptr(), cols_str.as_ptr(), 1);
                        libc::setenv(c"LINES".as_ptr(), rows_str.as_ptr(), 1);
                    }

                    // 7. Build the argv array for execvp.
                    let c_command = std::ffi::CString::new(command.clone()).unwrap();
                    let c_args: Vec<std::ffi::CString> = self
                        .command
                        .iter()
                        .map(|a| std::ffi::CString::new(a.clone()).unwrap())
                        .collect();
                    let mut arg_ptrs: Vec<*const libc::c_char> =
                        c_args.iter().map(|a| a.as_ptr()).collect();
                    arg_ptrs.push(std::ptr::null());

                    libc::execvp(c_command.as_ptr(), arg_ptrs.as_ptr());
                    libc::_exit(127);
                }
            }
            // ---- parent process ----
            // Drop the slave clones; the child inherited the
            // original slave_fd via fork and opened its own.
            drop(slave_fd);
            child_pid = pid;
        };
        info!("workload spawned ok");

        // Convert the master fd-files into tokio-compatible
        // async I/O handles.  We dup the master so each task
        // has an independent fd (reading and writing from the
        // same fd concurrently with epoll is fine, but having
        // separate fds avoids subtle issues).
        let master_dup = master_fd
            .try_clone()
            .map_err(|e| InitError::Exec(format!("dup master: {e}")))?;
        let master_read_file = unsafe { std::fs::File::from_raw_fd(master_fd.into_raw_fd()) };
        let master_write_file = unsafe { std::fs::File::from_raw_fd(master_dup.into_raw_fd()) };
        let read_async = AsyncFd::new(master_read_file)
            .map_err(|e| InitError::Exec(format!("AsyncFd read: {e}")))?;
        let write_async = AsyncFd::new(master_write_file)
            .map_err(|e| InitError::Exec(format!("AsyncFd write: {e}")))?;

        let client_a = client.clone();
        let client_b = client.clone();

        // Task 1: PTY master → vsock (workload output).
        //
        // Reads from the PTY master and sends the data to the
        // host as `WorkloadStdout` frames.  On a PTY both
        // stdout and stderr are combined into a single stream.
        let read_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                let mut guard = match read_async.readable().await {
                    Ok(g) => g,
                    Err(_) => break,
                };
                match guard.try_io(|inner| inner.get_ref().read(&mut buf)) {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        if client_a
                            .send_frame(pullrun_vsock::Frame::WorkloadStdout(
                                bytes::Bytes::copy_from_slice(&buf[..n]),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => {
                        // WouldBlock: spurious readiness,
                        // clear and retry.
                        guard.clear_ready();
                        continue;
                    }
                    _ => break,
                }
            }
        });

        // Task 2: vsock → PTY master (workload input).
        //
        // Receives `WorkloadStdin` frames from the host and
        // writes the data to the PTY master.  A `StdinEof`
        // frame closes the write side.
        let write_task = tokio::spawn(async move {
            loop {
                let frame = match client_b.recv_frame().await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match frame {
                    pullrun_vsock::Frame::WorkloadStdin(data) => {
                        let mut remaining = &data[..];
                        while !remaining.is_empty() {
                            let mut guard = match write_async.writable().await {
                                Ok(g) => g,
                                Err(_) => break,
                            };
                            match guard.try_io(|inner| {
                                use std::io::Write;
                                inner.get_ref().write(remaining)
                            }) {
                                Ok(Ok(0)) => {
                                    break;
                                }
                                Ok(Ok(n)) => {
                                    remaining = &remaining[n..];
                                }
                                Err(_) => {
                                    guard.clear_ready();
                                }
                                _ => break,
                            }
                        }
                    }
                    pullrun_vsock::Frame::StdinEof => break,
                    pullrun_vsock::Frame::WindowSize { rows, cols } => {
                        let fd = write_async.get_ref().as_raw_fd();
                        let ws = libc::winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        unsafe {
                            libc::ioctl(fd, libc::TIOCSWINSZ, &ws);
                        }
                    }
                    _ => {}
                }
            }
        });

        info!("waiting for child to exit");
        let status = tokio::task::spawn_blocking(move || {
            let mut status: libc::c_int = 0;
            unsafe {
                libc::waitpid(child_pid, &mut status, 0);
            }
            status
        })
        .await
        .map_err(|e| InitError::Exec(format!("join: {e}")))?;
        let exit_code = if libc::WIFEXITED(status) {
            Some(libc::WEXITSTATUS(status))
        } else {
            None
        };
        info!("child exited: code={exit_code:?}");

        read_task.abort();
        write_task.abort();
        let _ = read_task.await;
        let _ = write_task.await;

        Ok(WorkloadExit {
            code: exit_code,
            signal: None,
        })
    }

    /// Pipe-based I/O path (no PTY).
    ///
    /// Spawns the workload with piped stdio and runs three
    /// tokio tasks to shuttle bytes between the pipes and
    /// the vsock stream.  This is the default when
    /// `tty=false`.
    async fn run_piped(self, client: &mut VsockClient) -> Result<WorkloadExit, InitError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::process::Command;

        if self.command.is_empty() {
            return Err(InitError::Exec("empty command".into()));
        }

        let (command, rest) = self.command.split_first().unwrap();
        let args: Vec<&str> = rest.iter().map(String::as_str).collect();

        info!(
            command = %command,
            working_dir = %self.working_dir,
            "spawning workload"
        );

        // Set up stdio pipes. We use `stdin` piped so the host
        // can push bytes into the workload. stdout/stderr are
        // also piped.
        let mut cmd = Command::new(command);
        cmd.args(&args)
            .env_clear()
            .envs(self.env.iter().map(|s| {
                let (k, v) = s.split_once('=').unwrap_or((s.as_str(), ""));
                (k, v)
            }))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Only set current_dir if non-empty (an empty
        // current_dir confuses some exec wrappers).
        if !self.working_dir.is_empty() {
            cmd.current_dir(&self.working_dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| InitError::Exec(format!("spawn {}: {e}", command)))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| InitError::Exec("child stdin not captured".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| InitError::Exec("child stdout not captured".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| InitError::Exec("child stderr not captured".into()))?;

        // The vsock client is cheaply cloneable (Arc inside),
        // so each task gets its own handle. The underlying
        // transport is duped per-handle so each task can
        // read/write independently.
        let client_a = client.clone();
        let client_b = client.clone();
        let client_c = client.clone();

        // Task 1: workload stdout → vsock
        let stdout_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if client_a
                            .send_frame(pullrun_vsock::Frame::WorkloadStdout(
                                bytes::Bytes::copy_from_slice(&buf[..n]),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Task 2: workload stderr → vsock
        let stderr_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if client_b
                            .send_frame(pullrun_vsock::Frame::WorkloadStderr(
                                bytes::Bytes::copy_from_slice(&buf[..n]),
                            ))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Task 3: vsock → workload stdin
        let stdin_task = tokio::spawn(async move {
            loop {
                let frame = match client_c.recv_frame().await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match frame {
                    pullrun_vsock::Frame::WorkloadStdin(data) => {
                        if stdin.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    pullrun_vsock::Frame::StdinEof => {
                        let _ = stdin.shutdown().await;
                        break;
                    }
                    other => {
                        // Unexpected frame; log and continue.
                        tracing::warn!(?other, "unexpected frame from host during stdin pump");
                    }
                }
            }
        });

        // Wait for the child to exit.
        let status = child
            .wait()
            .await
            .map_err(|e| InitError::Exec(format!("waitpid: {e}")))?;

        // Cancel the I/O tasks (they'll exit when their pipes
        // close naturally).
        stdout_task.abort();
        stderr_task.abort();
        stdin_task.abort();
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let _ = stdin_task.await;

        Ok(WorkloadExit {
            code: status.code(),
            signal: None, // tokio::process::ExitStatus doesn't expose signal on all platforms
        })
    }
}
