//! Standalone Apple Virtualization "exec" tool.
//!
//! Boots a single Apple Virt VM with a Linux kernel + initramfs
//! containing `nimbus-init` (PID 1 inside the guest), then talks
//! to the guest over vsock to run a single command and stream
//! its stdio. This is the **minimal end-to-end** test for the
//! Apple Virt + nimbus-init + vsock transport path — no gRPC
//! service, no nimbus-runtime, no nimbusctl.
//!
//! ## What it does
//!
//! 1. Stages the kernel + initramfs (from `--kernel` and
//!    `--initramfs` paths, or pulled from an OCI image with
//!    `--kernel-image`).
//! 2. Builds an `AppleVirtAttachConfig` (kernel, rootfs
//!    VirtioFS share, command, env, working dir) and calls
//!    `nimbus_vm::run_session_blocking` to:
//!    - Boot the VM.
//!    - Register a `VZVirtioSocketListener` on port 42.
//!    - Wait for the `nimbus-init` guest to connect.
//!    - Send a `WorkloadSpec` frame (placeholder; the real
//!      spec is on the kernel command line).
//! 3. Pumps frames between vsock and the body thread:
//!    - `WorkloadStdout(Bytes)` from guest → process stdout
//!    - `WorkloadStderr(Bytes)` from guest → process stderr
//!    - bytes from process stdin → `WorkloadStdin` frame
//!    - EOF on stdin → `StdinEof` frame
//! 4. Waits for `WorkloadExit { exit_code, signal }` and
//!    propagates the exit code to the process exit status.
//!
//! ## Dispatch queue / main thread model
//!
//! Same pattern as `tools/apple-virt-smoke`:
//!
//! - **Main thread** is in `dispatch_main()`, pumping the main
//!   queue that the Apple Virt framework submits its async
//!   completion handlers to. Without an active runloop, start/
//!   pause/resume completions never fire.
//! - **Body thread** does the actual work. It calls
//!   `run_session_blocking` (which in turn dispatches VM
//!   operations to the main queue). When the workload exits
//!   or the session errors out, the body thread calls
//!   `libc::_exit(code)` — the main thread can't observe its
//!   return value because it's blocked in `dispatch_main()`.
//! - A **panic hook** catches body-thread panics and calls
//!   `_exit(1)`, otherwise a panic would leave the process
//!   stuck in `dispatch_main()` forever.
//!
//! ## macOS only
//!
//! The Apple Virt API is `#[cfg(target_os = "macos")]`-gated.
//! On other platforms this binary will fail to compile.
//! Sign with `com.apple.security.virtualization` entitlement
//! before running:
//!
//! ```text
//! codesign --force --sign - \
//!     --entitlements virt.entitlements \
//!     --options runtime \
//!     target/release/apple-virt-exec
//! ```
//!
//! ## Usage
//!
//! ```text
//! ./apple-virt-exec \
//!     --kernel     ~/.nimbus/kernels/vmlinux-3.31.0 \
//!     --initramfs  /tmp/nimbus-initramfs.cpio.gz \
//!     --rootfs     /tmp/alpine-rootfs \
//!     --store      /tmp/nimbus-store \
//!     --timeout    30 \
//!     --cmd        /bin/uname -- -a
//! ```
//!
//! Exit code:
//!   - workload's exit code on success
//!   - 124 on timeout
//!   - 1 on FFI/transport error
//!
//! ## What this does NOT do
//!
//! - **No warm pool.** Each invocation boots a fresh VM.
//!   `apple-virt-smoke` is for testing the pool FFI;
//!   `apple-virt-exec` is for testing the per-VM attach
//!   transport end-to-end.
//! - **No gRPC.** Direct vsock, not a runtime service.
//! - **No workload image pull.** The initramfs must already
//!   be built (`tools/build-initramfs/`); the rootfs is just
//!   a host dir shared via VirtioFS — contents are not
//!   inspected by the host.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use nimbus_vm::apple::AppleVirtError;
use nimbus_vm::oci_kernel::{OciKernelError, StagedKernel};
use nimbus_vm::{run_session_blocking, AppleVirtAttachConfig};
use nimbus_vsock::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, info};

/// Default VM memory (MiB). 512 is the minimum that Linux
/// can boot with on arm64 with the Kata kernel.
const DEFAULT_VM_MEM_MIB: u32 = 512;

/// Default vCPUs. 1 is enough for a single workload exec.
const DEFAULT_VM_CPUS: u8 = 1;

/// Vsock port the guest connects to. Must match
/// `nimbus-init`'s `DEFAULT_VSOCK_PORT`.
const VSOCK_PORT: u32 = 42;

/// Default session timeout. The whole VM boot +
/// workload-exec round-trip is bounded by this. Workloads
/// that legitimately run longer should set `--timeout`.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

#[derive(Parser, Debug)]
#[command(
    name = "apple-virt-exec",
    about = "Boot an Apple Virt VM and run a single command in it via vsock"
)]
struct Args {
    /// Path to a Linux kernel compiled for the Apple Virt
    /// guest ABI. Mutually exclusive with `--kernel-image`.
    #[arg(long, conflicts_with = "kernel_image")]
    kernel: Option<PathBuf>,

    /// OCI image reference for a Nimbus kernel image. The
    /// image is pulled, materialized, and `/boot/vmlinux`
    /// (+ `/boot/initramfs.cpio.gz`) is staged into a temp
    /// dir. Mutually exclusive with `--kernel`.
    #[arg(long, conflicts_with = "kernel", value_name = "OCI_REF")]
    kernel_image: Option<String>,

    /// Path to an initramfs (cpio+gz) that contains
    /// `/sbin/nimbus-init` and an `/init` shell script that
    /// `exec`s it. Required when `--kernel` is used. Ignored
    /// when `--kernel-image` is used (initramfs is read
    /// from the OCI image instead).
    #[arg(long)]
    initramfs: Option<PathBuf>,

    /// Host directory exposed to the guest via VirtioFS as
    /// `nimbus-rootfs`. The guest sees this as the
    /// `nimbus-rootfs` mount (e.g. `/mnt/host` if the guest
    /// mounts it there). Required: the VM config will not
    /// build without a real directory.
    #[arg(long)]
    rootfs: PathBuf,

    /// Host directory for a second VirtioFS share tagged
    /// `nimbus-store`. Optional; used by some workloads to
    /// pull artifacts at runtime. If omitted, the
    /// `nimbus-store` share is configured to point at the
    /// same path as `--rootfs`.
    #[arg(long)]
    store: Option<PathBuf>,

    /// vCPUs per VM.
    #[arg(long, default_value_t = DEFAULT_VM_CPUS)]
    cpus: u8,

    /// Memory per VM in MiB.
    #[arg(long, default_value_t = DEFAULT_VM_MEM_MIB)]
    mem_mib: u32,

    /// Whole-session timeout (VM boot + workload exec). The
    /// process exits with 124 if the workload hasn't
    /// finished by then.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    timeout: u64,

    /// Working directory for the workload, inside the
    /// guest's filesystem. Empty means `/`.
    #[arg(long, default_value = "")]
    cwd: String,

    /// Environment variable for the workload, in `KEY=VALUE`
    /// form. Repeatable.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    envs: Vec<String>,

    /// Command to run inside the VM. Everything after `--`
    /// is treated as argv (argv[0] is the executable, the
    /// rest are arguments). Example:
    /// `--cmd /bin/uname -- -a`
    #[arg(last = true, required = true)]
    cmd: Vec<String>,

    /// Suppress info-level output; only show warnings and
    /// errors.
    #[arg(long, short = 'q')]
    quiet: bool,

    /// Path to a file the guest's kernel+init console
    /// output gets written to. Truncated on each run.
    /// Defaults to `/tmp/nimbus-exec-console.log`.
    #[arg(long, default_value = "/tmp/nimbus-exec-console.log")]
    console_log: PathBuf,
}

fn main() -> ! {
    // Initialize tracing. Honor RUST_LOG if set, default to
    // info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = match Args::try_parse_from(std::env::args_os()) {
        Ok(a) => a,
        Err(e) => {
            // clap's error pretty-prints to stderr and
            // exits with 2. We can't do that normally
            // because the main thread will be in
            // `dispatch_main()`. Print and `_exit(2)`.
            eprintln!("{}", e);
            unsafe { libc::_exit(2) };
        }
    };

    // Install panic hook. Body thread panics must exit
    // the process directly because the main thread is
    // stuck in `dispatch_main()`.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        unsafe { libc::_exit(1) };
    }));

    // Spawn body thread. Main thread pumps the dispatch
    // queue; body thread does the work.
    std::thread::Builder::new()
        .name("nimbus-exec-body".into())
        .spawn(move || {
            let code = run_body(args);
            unsafe { libc::_exit(code as i32) };
        })
        .expect("spawn body thread");

    // Main thread: pump the main dispatch queue forever.
    // The Apple Virt framework's XPC service submits
    // completion handlers to this queue, so it must be
    // actively drained for VM start/pause/resume callbacks
    // to fire. `dispatch_main()` never returns; the body
    // thread exits the process when the work is done.
    dispatch2::dispatch_main()
}

/// Run the actual exec session. Returns the exit code
/// (workload's exit code on success, 124 on timeout, 1 on
/// FFI/transport error).
fn run_body(args: Args) -> i32 {
    let started = Instant::now();
    info!(
        cmd = ?args.cmd,
        cpus = args.cpus,
        mem_mib = args.mem_mib,
        timeout_s = args.timeout,
        "starting apple-virt-exec"
    );

    // 1. Stage the kernel.
    let kernel = match stage_kernel(&args) {
        Ok(k) => k,
        Err(e) => {
            error!(error = %e, "FAIL: kernel staging failed");
            return 1;
        }
    };

    // 2. Validate the rootfs dir exists. The framework
    //    requires the path to exist on disk.
    if !args.rootfs.exists() {
        error!(path = %args.rootfs.display(), "rootfs path does not exist");
        return 1;
    }
    if !args.rootfs.is_dir() {
        error!(path = %args.rootfs.display(), "rootfs path is not a directory");
        return 1;
    }

    // 3. Build the attach config.
    let cfg = AppleVirtAttachConfig {
        kernel,
        rootfs_dir: args.rootfs.clone(),
        command: args.cmd.clone(),
        env: args.envs.clone(),
        working_dir: args.cwd.clone(),
        cpus: args.cpus,
        mem_mib: args.mem_mib,
        vsock_port: Some(VSOCK_PORT),
        console_log: Some(args.console_log.clone()),
    };

    // 4. Set up the channel adapters. `client_in` is what
    //    we send to the guest (WorkloadStdin + StdinEof);
    //    `server_out` is what we receive from the guest
    //    (InitHello, WorkloadStdout/Stderr, WorkloadExit).
    let (client_in_tx, client_in_rx) = std::sync::mpsc::channel::<Frame>();
    let (server_out_tx, server_out_rx) = std::sync::mpsc::channel::<Frame>();

    // 5. Spawn a tokio task to handle stdin → client_in.
    //    Tokio mpsc is not needed; we can just block on
    //    stdin in a dedicated thread, but using a
    //    single-thread runtime is cleaner. We use a
    //    current-thread runtime so we can poll stdin
    //    concurrently with the message pump.
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(error = %e, "FAIL: tokio runtime build failed");
            return 1;
        }
    };

    // 6. The stdio pump runs on the body thread itself
    //    (not in the tokio runtime) because the runloop
    //    is simple: read from `server_out_rx`, write to
    //    stdout/stderr; if EOF or `StdinEof`, send
    //    `StdinEof` on `client_in_tx` and stop.
    //
    //    The body thread also runs `run_session_blocking`
    //    on a separate thread because it's `!Send + !Sync`
    //    and would otherwise deadlock. Wait — actually
    //    `run_session_blocking` is fine to call from
    //    *this* thread; it internally blocks until the
    //    workload exits.
    //
    //    We need to pump server_out_rx *concurrently*
    //    with `run_session_blocking`. Use a
    //    dedicated thread for the session runner, and
    //    this thread for the message pump + stdin reader.
    let (session_result_tx, session_result_rx) =
        std::sync::mpsc::channel::<Result<(), nimbus_vm::AttachError>>();
    let session_thread = std::thread::Builder::new()
        .name("nimbus-exec-session".into())
        .spawn(move || {
            let r = run_session_blocking(cfg, client_in_rx, server_out_tx);
            let _ = session_result_tx.send(r);
        })
        .expect("spawn session thread");

    // 7. Run the stdio pump on the tokio runtime. This
    //    task:
    //    - Reads from server_out_rx (blocking mpsc) and
    //      forwards stdout/stderr frames to the process's
    //      stdout/stderr. Stops on WorkloadExit.
    //    - Reads from process stdin (async stdin) and
    //      sends WorkloadStdin/StdinEof frames to
    //      client_in_tx. Stops on EOF.
    let pump = rt.block_on(async move {
        run_stdio_pump(client_in_tx, server_out_rx).await
    });

    // 8. Wait for the session thread to finish (it should
    //    already be done because the pump closed the
    //    channels). If it errored, surface that.
    let session_result = match session_thread.join() {
        Ok(_) => {
            // Recv the result. The session thread sent
            // its result on `session_result_tx` before
            // exiting.
            session_result_rx
                .recv()
                .unwrap_or_else(|_| Ok(()))
        }
        Err(_) => {
            error!("session thread panicked");
            Err(nimbus_vm::AttachError::Vm(
                AppleVirtError::InvalidState("session thread panic".into()).to_string(),
            ))
        }
    };

    let elapsed = started.elapsed();
    match (&pump, &session_result) {
        (Ok(exit_code), Ok(())) => {
            info!(
                elapsed_ms = elapsed.as_millis() as u64,
                exit_code,
                "workload completed"
            );
            // Clamp the exit code into the i32 range that
            // `_exit` expects. Workload exit codes are
            // already i32.
            *exit_code
        }
        (Err(e), _) | (_, Err(e)) => {
            error!(
                elapsed_ms = elapsed.as_millis() as u64,
                error = %e,
                "FAIL: exec session failed"
            );
            1
        }
    }
}

/// Run the stdio pump. Returns the workload's exit code
/// (or an error if the session failed before the workload
/// exited).
async fn run_stdio_pump(
    client_in_tx: std::sync::mpsc::Sender<Frame>,
    server_out_rx: std::sync::mpsc::Receiver<Frame>,
) -> Result<i32, nimbus_vm::AttachError> {
    // Spawn the stdin → client_in task. We poll process
    // stdin asynchronously and forward each chunk to the
    // guest. On EOF, we send StdinEof and the task ends.
    let stdin_tx = client_in_tx.clone();
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = vec![0u8; 4096];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => {
                    // EOF on host stdin → tell guest.
                    let _ = stdin_tx.send(Frame::StdinEof);
                    break;
                }
                Ok(n) => {
                    let chunk = bytes::Bytes::copy_from_slice(&buf[..n]);
                    if stdin_tx.send(Frame::WorkloadStdin(chunk)).is_err() {
                        // Receiver dropped (session ended).
                        break;
                    }
                }
                Err(e) => {
                    error!(error = %e, "stdin read error");
                    let _ = stdin_tx.send(Frame::StdinEof);
                    break;
                }
            }
        }
    });

    // Pump server_out_rx → process stdout/stderr. The
    // receiver is `std::sync::mpsc`, so we can't `.await`
    // it directly. Use a `tokio::task::spawn_blocking` to
    // block on it, or wrap it in an async adapter.
    //
    // Simpler: use `recv_timeout` in a loop on a blocking
    // task, and forward results through a tokio mpsc.
    let (async_tx, mut async_rx) = tokio::sync::mpsc::channel::<Frame>(16);
    let pump_thread = std::thread::spawn(move || {
        while let Ok(frame) = server_out_rx.recv() {
            if async_tx.blocking_send(frame).is_err() {
                break;
            }
        }
    });

    // Pull frames from the async channel and dispatch.
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let mut exit_code: i32 = 1; // default to error if we never get WorkloadExit
    let mut got_exit = false;
    while let Some(frame) = async_rx.recv().await {
        match frame {
            Frame::WorkloadStdout(bytes) => {
                if let Err(e) = stdout.write_all(&bytes).await {
                    error!(error = %e, "stdout write error");
                }
                let _ = stdout.flush().await;
            }
            Frame::WorkloadStderr(bytes) => {
                if let Err(e) = stderr.write_all(&bytes).await {
                    error!(error = %e, "stderr write error");
                }
                let _ = stderr.flush().await;
            }
            Frame::WorkloadExit {
                exit_code: code,
                signal,
            } => {
                if let Some(c) = code {
                    exit_code = c;
                } else if let Some(s) = signal {
                    // Killed by signal: encode as 128 + sig
                    // (POSIX convention).
                    exit_code = 128 + s;
                }
                got_exit = true;
                break;
            }
            Frame::Error(msg) => {
                error!(message = %msg, "guest reported error");
                return Err(nimbus_vm::AttachError::Workload(msg));
            }
            Frame::InitHello { .. } => {
                // We don't care about InitHello at the
                // pump level; `run_session_blocking`
                // already consumed it. Ignore here.
            }
            other => {
                info!(frame = ?other, "unexpected frame from guest");
            }
        }
    }

    if !got_exit {
        return Err(nimbus_vm::AttachError::Workload(
            "session ended without WorkloadExit".into(),
        ));
    }

    // Best-effort: drop the client_in_tx so the session
    // thread's `recv()` returns Err and it can clean up.
    drop(client_in_tx);
    let _ = stdin_task.await;
    let _ = pump_thread.join();
    Ok(exit_code)
}

/// Stage the kernel: either from a pre-staged path on
/// disk, or by pulling from an OCI registry and
/// materializing into a temp dir.
fn stage_kernel(args: &Args) -> Result<StagedKernel, OciKernelError> {
    if let Some(image_ref) = &args.kernel_image {
        info!(image = %image_ref, "pulling kernel from OCI registry");
        let store_dir = std::env::temp_dir()
            .join("apple-virt-exec-store")
            .join("oci-store");
        std::fs::create_dir_all(&store_dir).map_err(OciKernelError::Io)?;
        let store = Arc::new(nimbus_store::MmapStore::new(store_dir));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(OciKernelError::Io)?;
        rt.block_on(StagedKernel::from_image(image_ref, &store, None))
    } else if let Some(kernel_path) = &args.kernel {
        info!(path = %kernel_path.display(), "staging pre-built kernel");
        StagedKernel::from_paths(kernel_path.clone(), args.initramfs.clone())
    } else {
        Err(OciKernelError::MissingFile(
            "either --kernel or --kernel-image is required".to_string(),
        ))
    }
}

// `ExitCode` is unused in the body-thread path (we always
// `_exit(code as i32)`), but we keep the import so the
// clap `try_parse_from` error path compiles cleanly.
#[allow(dead_code)]
fn _exitcode_marker(_: ExitCode, _: Duration) {}
