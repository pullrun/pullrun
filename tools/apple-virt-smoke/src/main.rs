//! Standalone Apple Virtualization FFI smoke test.
//!
//! This binary exercises the `nimbus_vm::apple` module end-to-end on
//! macOS. It is a *real* binary (not a test in the nimbus-vm crate)
//! so it can be deployed to CI or a developer Mac and run without
//! the rest of the nimbus workspace needing to be present.
//!
//! ## What it tests (v0)
//!
//! 1. `VZVirtualMachine::isSupported()` — the framework is usable on
//!    this host (rejects e.g. a guest-VM-on-VM scenario).
//! 2. `AppleVirtPool::new` — constructs a pool of paused VMs from
//!    a `StagedKernel` (either pre-staged on disk or pulled from an
//!    OCI image) + host store path. Validates the FFI surface:
//!    config construction, `validateWithError`, kernel URL,
//!    VirtioFS share, NAT network device, framework callbacks for
//!    start/pause.
//! 3. `AppleVirtPool::acquire` — pops a VM from the pool and resumes
//!    it. Confirms the warm-path resume callback works.
//! 4. `AcquiredVm::state` / `is_running` — confirms the VM is in
//!    the `Running` state after acquire.
//! 5. `AcquiredVm::release().await` — pauses the VM and pushes it
//!    back into the pool.
//!
//! ## What it does NOT test (yet)
//!
//! - **Workload execution.** Running an actual binary inside the
//!   guest requires a Linux kernel *with* userspace (initramfs or
//!   disk), a static `nimbus-runtime` binary in that userspace, and
//!   a vsock listener on port 42. The `apple.rs` module is not
//!   wired with the vsock transport yet; that is the next session.
//! - **End-to-end guest boot to login prompt.** Requires a kernel
//!   compiled for the Apple Virtualization guest ABI (the Asahi
//!   `linux` tree). See `tools/build-kernel-image/` for how to
//!   build a kernel image and publish it to a registry.
//!
//! ## macOS only
//!
//! The `apple` module is `#[cfg(target_os = "macos")]`-gated. On
//! other platforms this binary will fail to build. We do not
//! provide a no-op fallback: a developer trying to run this on
//! Linux should get a clear compile error, not a silent skip.
//!
//! ## Usage
//!
//! ```text
//! # Pass a pre-staged kernel on disk
//! ./apple-virt-smoke \
//!   --kernel ~/.local/share/nimbus/vms/vmlinux \
//!   --store  ~/.local/share/nimbus/store
//!
//! # Or pull a kernel image from a registry (auto-materializes
//! # /boot/vmlinux + /boot/initramfs.cpio.gz via OCI pull)
//! ./apple-virt-smoke \
//!   --kernel-image ghcr.io/nimbus/kernel-asahi:6.19.14 \
//!   --store  ~/.local/share/nimbus/store
//!
//! # With initramfs and a custom pool size
//! ./apple-virt-smoke \
//!   --kernel     ~/.local/share/nimbus/vms/vmlinux \
//!   --initramfs  ~/.local/share/nimbus/initramfs.cpio.gz \
//!   --store      ~/.local/share/nimbus/store \
//!   --pool-size  3
//! ```
//!
//! Exit code 0 on PASS, 1 on FAIL. The FFI error (if any) is
//! printed to stderr in human-readable form.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use nimbus_vm::apple::{AppleVirtError, AppleVirtPool, AppleVirtPoolConfig};
use nimbus_vm::oci_kernel::{OciKernelError, StagedKernel};
use nimbus_store::MmapStore;
use tracing::{error, info};

/// Default VM memory (MiB). Matches the kernel's compiled-in
/// `CONFIG_VM_SIZE` for the Asahi 6.8 default.
const DEFAULT_VM_MEM_MIB: u32 = 512;

/// Default vCPUs. One is enough to validate FFI; raise this
/// later when actually running workloads.
const DEFAULT_VM_CPUS: u8 = 1;

#[derive(Parser, Debug)]
#[command(name = "apple-virt-smoke", about = "Apple Virtualization FFI smoke test")]
struct Args {
    /// Path to a Linux kernel compiled for the Apple
    /// Virtualization guest ABI. Mutually exclusive with
    /// `--kernel-image`.
    #[arg(long, conflicts_with = "kernel_image")]
    kernel: Option<PathBuf>,

    /// OCI image reference for a Nimbus kernel image (e.g.
    /// `ghcr.io/nimbus/kernel-asahi:6.19.14`). The image is
    /// pulled, materialized, and `/boot/vmlinux` (plus
    /// optional `/boot/initramfs.cpio.gz`) is staged into a
    /// temp directory. Mutually exclusive with `--kernel`.
    #[arg(long, conflicts_with = "kernel", value_name = "OCI_REF")]
    kernel_image: Option<String>,

    /// Path to an initramfs. Optional — without one, the
    /// kernel will panic on init= failure after FFI boot.
    /// Ignored when `--kernel-image` is used (initramfs is
    /// read from the OCI image instead).
    #[arg(long)]
    initramfs: Option<PathBuf>,

    /// Host path to expose to the guest via VirtioFS as
    /// `/mnt/nimbus-store` (guest-side tag: `nimbus-store`).
    /// Must exist on disk. Defaults to a temp dir if absent
    /// (the framework requires the path to exist, but the
    /// contents are not validated).
    #[arg(long)]
    store: Option<PathBuf>,

    /// Number of VMs to pre-create in the warm pool.
    #[arg(long, default_value_t = 3)]
    pool_size: usize,

    /// vCPUs per VM.
    #[arg(long, default_value_t = DEFAULT_VM_CPUS)]
    cpus: u8,

    /// Memory per VM in MiB.
    #[arg(long, default_value_t = DEFAULT_VM_MEM_MIB)]
    mem_mib: u32,

    /// Suppress info-level output; only show warnings and errors.
    #[arg(long, short = 'q')]
    quiet: bool,
}

fn main() -> ! {
    // Initialize tracing. Honor RUST_LOG if set, default to info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();

    // Install a panic hook that calls `std::process::exit(1)`.
    // The body thread (see below) does the actual work, but the
    // main thread is blocked in `dispatch_main()` and can't
    // observe the panic. Without this hook, a panic in the body
    // thread would leave the process stuck in `dispatch_main()`
    // forever.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        // Use `_exit` to skip running destructors (we're in a
        // panic, so the world is in an unknown state).
        unsafe { libc::_exit(1) };
    }));

    // Spawn the body thread. This thread does all the actual
    // work: kernel staging, pool construction, acquire/release.
    // The reason we don't run it on the main thread is that the
    // main thread must be the one calling `dispatch_main()` —
    // that's the only way to pump the main dispatch queue, which
    // is the queue the Apple Virtualization framework uses to
    // deliver its completion handlers.
    std::thread::Builder::new()
        .name("nimbus-smoke-body".into())
        .spawn(move || {
            let code = run_body(args);
            // The body thread is the only place that knows
            // when the work is done. It exits the process
            // directly because the main thread is blocked in
            // `dispatch_main()` and can't observe our return
            // value. Using `_exit` skips normal destructors
            // (we don't need them for a smoke test).
            unsafe { libc::_exit(code as i32) };
        })
        .expect("spawn body thread");

    // Main thread: pump the main dispatch queue forever. The
    // Apple Virtualization framework's XPC service submits
    // completion handlers to this queue, so it must be
    // actively drained for VM start/stop/pause/resume callbacks
    // to fire. `dispatch_main()` never returns; the body
    // thread exits the process when the work is done.
    dispatch2::dispatch_main()
}

/// Run the actual smoke test. Returns 0 on PASS, 1 on FAIL.
///
/// This runs on the body thread (not the main thread). The main
/// thread is blocked in `dispatch_main()`, which pumps the main
/// queue that the Apple Virt framework submits its completions
/// to. The framework's async API round-trip thus works: the body
/// thread dispatches a `start` call to the main queue, the main
/// thread processes it, the framework's XPC service boots the
/// VM, and the completion handler fires back on the main queue.
/// The main thread runs the completion handler, which sends
/// through a channel the body thread is blocked on.
fn run_body(args: Args) -> u8 {
    // Stage the kernel: either from a pre-staged path on
    // disk, or by pulling an OCI image and materializing the
    // vmlinux + initramfs into a temp dir. The pool only
    // cares about the resulting `StagedKernel`.
    let kernel = match stage_kernel(&args) {
        Ok(k) => k,
        Err(e) => {
            error!(error = %e, "FAIL: kernel staging failed");
            return 1;
        }
    };

    // Resolve the host store path. If the user did not pass one,
    // create a temp dir — the framework only needs the path to
    // exist; we are not exercising content delivery here.
    let store_path = match args.store {
        Some(p) if p.exists() => p,
        Some(p) => {
            error!(path = %p.display(), "store path provided but does not exist");
            return 1;
        }
        None => {
            let tmp = std::env::temp_dir().join("apple-virt-smoke-store");
            if let Err(e) = std::fs::create_dir_all(&tmp) {
                error!(path = %tmp.display(), error = %e, "failed to create temp store path");
                return 1;
            }
            tmp
        }
    };

    let config = AppleVirtPoolConfig::new(kernel, store_path)
        .with_pool_size(args.pool_size)
        .with_cpus(args.cpus)
        .with_mem_mib(args.mem_mib);

    let pool_result = time("AppleVirtPool::new", || AppleVirtPool::new(config));
    let pool = match pool_result {
        Ok(p) => {
            info!(pool_size = args.pool_size, "pool created");
            p
        }
        Err(e) => {
            error!(error = %e, "FAIL: pool construction failed");
            return 1;
        }
    };

    // Run the async portion (acquire/release) on a current-thread
    // runtime. acquire is the only async operation in the v0
    // surface; multi-thread is unnecessary.
    let rt = match build_runtime("tokio runtime") {
        Ok(rt) => rt,
        Err(code) => {
            return match code {
                ExitCode::SUCCESS => 0,
                _ => 1,
            };
        }
    };

    let result = rt.block_on(async {
        // 1. Acquire a warm VM.
        let acquired = match time_async("pool.acquire", pool.acquire()).await {
            Ok(vm) => {
                info!(state = ?vm.state(), "acquired warm VM");
                vm
            }
            Err(e) => {
                error!(error = %e, "FAIL: acquire failed");
                return Err(AppleVirtError::from(e));
            }
        };

        // 2. Verify the VM is in the Running state. The
        //    framework's `startWithCompletionHandler` returns
        //    before the VM is fully booted — `state()` may
        //    briefly report `Starting` instead of `Running`.
        //    We poll for a short window to handle that race.
        //
        //    NOTE: `vm.state()` reads the framework's state
        //    property, which the Apple docs say must be
        //    accessed on the VM's configured queue (the main
        //    queue). Reading from the body thread works in
        //    practice on recent macOS versions (the framework
        //    is lenient about reads), but the canonical fix
        //    would be to dispatch the read to the main queue
        //    via `DispatchQueue::main().exec_async(...)` and
        //    wait on a channel. v0 takes the lenient path
        //    for simplicity; v1 will move to the dispatch
        //    path.
        if !wait_for_running(&acquired, std::time::Duration::from_secs(2)) {
            error!(
                state = ?acquired.state(),
                "FAIL: VM did not enter Running state within 2s of acquire"
            );
            return Err(AppleVirtError::InvalidState(format!(
                "expected Running, got {:?}",
                acquired.state()
            )));
        }
        info!("VM is Running");

        // 3. Release back to the pool.
        match time_async("AcquiredVm::release", acquired.release()).await {
            Ok(()) => {
                info!("released VM back to pool");
                Ok(())
            }
            Err(e) => {
                error!(error = %e, "FAIL: release failed");
                Err(e)
            }
        }
    });

    match result {
        Ok(()) => {
            info!("PASS: Apple Virt FFI round-trip succeeded");
            0
        }
        Err(e) => {
            error!(error = %e, "FAIL: FFI round-trip failed");
            1
        }
    }
}

/// Stage the kernel: either pre-staged on disk, or pulled
/// from an OCI registry and materialized into a temp dir.
fn stage_kernel(args: &Args) -> Result<StagedKernel, OciKernelError> {
    if let Some(image_ref) = &args.kernel_image {
        info!(image = %image_ref, "pulling kernel from OCI registry");
        // OCI pull needs a backing MmapStore for the
        // materializer. Use a per-invocation temp dir — we
        // throw it away after the kernel is staged.
        let store_dir = std::env::temp_dir()
            .join("apple-virt-smoke-store")
            .join("oci-store");
        std::fs::create_dir_all(&store_dir).map_err(|e| OciKernelError::Io(e))?;
        let store = Arc::new(MmapStore::new(store_dir));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| OciKernelError::Io(e))?;
        rt.block_on(StagedKernel::from_image(image_ref, &store, None))
    } else if let Some(kernel_path) = &args.kernel {
        info!(path = %kernel_path.display(), "staging pre-built kernel");
        StagedKernel::from_paths(kernel_path.clone(), args.initramfs.clone())
    } else {
        // clap's `conflicts_with` + `Option` should have
        // caught this, but the borrow checker doesn't know
        // both fields are mutually exclusive.
        Err(OciKernelError::MissingFile(
            "either --kernel or --kernel-image is required".to_string(),
        ))
    }
}

/// Poll the VM state for up to `timeout` waiting for it to
/// reach `Running`. Returns true on success.
fn wait_for_running(
    vm: &nimbus_vm::apple::AcquiredVm,
    timeout: std::time::Duration,
) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if vm.is_running() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Wrap a synchronous FFI call with timing + log output.
fn time<T>(label: &str, f: impl FnOnce() -> Result<T, AppleVirtError>) -> Result<T, AppleVirtError> {
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    match &result {
        Ok(_) => info!(elapsed_ms = elapsed.as_millis() as u64, "{label} OK"),
        Err(e) => error!(elapsed_ms = elapsed.as_millis() as u64, error = %e, "{label} failed"),
    }
    result
}

/// Async variant of `time` for pool operations.
async fn time_async<T, E: std::fmt::Display>(
    label: &str,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let start = Instant::now();
    let result = fut.await;
    let elapsed = start.elapsed();
    match &result {
        Ok(_) => info!(elapsed_ms = elapsed.as_millis() as u64, "{label} OK"),
        Err(e) => error!(elapsed_ms = elapsed.as_millis() as u64, error = %e, "{label} failed"),
    }
    result
}

/// Helper to convert a tokio runtime build error into an
/// `ExitCode` for the main fn.
fn build_runtime(label: &str) -> Result<tokio::runtime::Runtime, ExitCode> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            error!(error = %e, "FAIL: failed to build {label}");
            ExitCode::from(1)
        })
}
