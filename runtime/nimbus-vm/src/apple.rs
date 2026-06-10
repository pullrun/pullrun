//! Apple Virtualization backend for Nimbus.
//!
//! This module implements a `VZVirtualMachine`-based executor for
//! macOS. It uses Apple's Virtualization framework (via the
//! `objc2-virtualization` Rust bindings) to launch Linux VMs
//! directly on Apple Silicon hosts, without needing Docker
//! Desktop, QEMU, or any other emulation layer.
//!
//! This module implements a `VZVirtualMachine`-based executor for
//! macOS. It uses Apple's Virtualization framework (via the
//! `objc2-virtualization` Rust bindings) to launch Linux VMs
//! directly on Apple Silicon hosts, without needing Docker
//! Desktop, QEMU, or any other emulation layer.
//!
//! # Design
//!
//! The `AppleVirtPool` pre-creates a small set of VMs at startup,
//! boots them to the point of running an initramfs, and pauses
//! them. Acquiring a VM from the pool is a `resume` call (~10s of
//! milliseconds, vs. 1+ second for a cold start). Releasing is a
//! `pause`.
//!
//! # What v0 of this module does
//!
//! - Construct a `VZVirtualMachineConfiguration` with a Linux
//!   kernel + initramfs, a single VirtioFS share of the host's
//!   DAG store, a NAT'd Virtio network device, and a single
//!   vCPU + 512 MiB of RAM.
//! - Construct a pool of N paused VMs at construction time.
//! - `acquire` returns a resumed VM; `release` pauses and
//!   returns it to the pool.
//! - Bridge Apple's completion-handler API to async Rust via
//!   `std::sync::mpsc` and `block2::RcBlock`.
//!
//! # What v0 of this module does NOT do (yet)
//!
//! - **No vsock-based workload spawning.** The VM's runtime
//!   would be reached via a `VZVirtioSocketDevice` on port 42.
//!   v0 only knows how to boot the VM and verify the FFI
//!   round-trip works; actually executing a workload requires
//!   a static `nimbus-runtime` binary inside the initramfs,
//!   plus a Linux kernel compiled for the Apple Virtualization
//!   guest ABI.
//! - **No serial console capture.** Booting a Linux kernel
//!   without a console means panics are invisible. Future work
//!   will use `VZFileHandleSerialPortAttachment` to tee the
//!   kernel's console to a file for diagnostics.

pub mod attach;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{NSObject, ProtocolObject};
use objc2::{define_class, msg_send, ClassType};
use objc2_foundation::{NSArray, NSError, NSObjectProtocol, NSString, NSURL};
use objc2_virtualization::{
    VZGenericPlatformConfiguration, VZLinuxBootLoader, VZNATNetworkDeviceAttachment,
    VZSharedDirectory, VZSingleDirectoryShare,
    VZVirtioFileSystemDeviceConfiguration, VZVirtioNetworkDeviceConfiguration,
    VZVirtualMachine, VZVirtualMachineConfiguration, VZVirtualMachineDelegate,
    VZVirtualMachineState,
};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, error, info, warn};

use crate::oci_kernel::StagedKernel;

/// Default number of VMs to pre-create in the warm pool.
pub const DEFAULT_POOL_SIZE: usize = 3;

/// Default RAM per VM, in MiB.
pub const DEFAULT_VM_MEM_MIB: u32 = 512;

/// Default vCPUs per VM.
pub const DEFAULT_VM_CPUS: u8 = 1;

/// Maximum time we will wait for a single Apple Virt lifecycle
/// callback (`start`, `stop`, `pause`, `resume`) before giving
/// up. Apple docs do not document a hard deadline; in practice
/// 10s is enough for cold start on M-series hardware. Tunable
/// via the `NIMBUS_APPLE_VIRT_CALLBACK_TIMEOUT_SECS` env var
/// for hosts where the framework's first-time setup
/// (firmware signing, memory zeroing) takes longer.
fn callback_timeout_secs() -> u64 {
    static OVERRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *OVERRIDE.get_or_init(|| {
        std::env::var("NIMBUS_APPLE_VIRT_CALLBACK_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(10)
    })
}

/// All configuration needed to build a pool.
#[derive(Debug)]
pub struct AppleVirtPoolConfig {
    /// A kernel that has been materialized onto the host
    /// filesystem, ready to be passed to `VZLinuxBootLoader`.
    ///
    /// This is typically built by
    /// [`crate::oci_kernel::StagedKernel::from_image`] (which
    /// pulls a nimbus kernel image via the standard OCI
    /// pipeline) or by
    /// [`crate::oci_kernel::StagedKernel::from_paths`] (for
    /// tests or out-of-band staging).
    ///
    /// The pool takes ownership: dropping the pool cleans up
    /// the staged kernel (including the temp dir, if any).
    pub kernel: StagedKernel,

    /// Host path that will be exposed to the guest via
    /// VirtioFS. In production this is the `MmapStore` root
    /// (`/var/lib/nimbus`). In tests it can be any directory.
    pub host_store_path: PathBuf,

    /// Number of VMs to pre-create. The default is
    /// `DEFAULT_POOL_SIZE` (3).
    pub pool_size: usize,

    /// vCPUs per VM.
    pub cpus: u8,

    /// Memory per VM, in MiB.
    pub mem_mib: u32,
}

impl AppleVirtPoolConfig {
    /// Build a config from a staged kernel + host store path
    /// with defaults for everything else.
    pub fn new(kernel: StagedKernel, host_store_path: PathBuf) -> Self {
        Self {
            kernel,
            host_store_path,
            pool_size: DEFAULT_POOL_SIZE,
            cpus: DEFAULT_VM_CPUS,
            mem_mib: DEFAULT_VM_MEM_MIB,
        }
    }

    /// Override the number of VMs to pre-create in the warm pool.
    pub fn with_pool_size(mut self, pool_size: usize) -> Self {
        self.pool_size = pool_size;
        self
    }

    /// Override the number of vCPUs per VM.
    pub fn with_cpus(mut self, cpus: u8) -> Self {
        self.cpus = cpus;
        self
    }

    /// Override the memory per VM, in MiB.
    pub fn with_mem_mib(mut self, mem_mib: u32) -> Self {
        self.mem_mib = mem_mib;
        self
    }
}

/// Errors from the Apple Virt pool. The wrapped `String` is the
/// `NSError.localizedDescription` from the framework.
#[derive(Debug, Error)]
pub enum AppleVirtError {
    #[error("Apple Virt config validation failed: {0}")]
    InvalidConfig(String),

    #[error("Apple Virt {operation} callback failed: {reason}")]
    Callback { operation: &'static str, reason: String },

    #[error("Apple Virt state error: {0}")]
    InvalidState(String),

    #[error("Pool is empty and could not create a new VM: {0}")]
    PoolExhausted(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Path error: {0}")]
    Path(String),
}

impl From<Retained<NSError>> for AppleVirtError {
    fn from(err: Retained<NSError>) -> Self {
        let description = err.localizedDescription();
        AppleVirtError::Callback {
            operation: "unknown",
            reason: description.to_string(),
        }
    }
}

/// A pool of pre-warmed `VZVirtualMachine`s. Cheap to clone
/// (the `Arc` is shared).
#[derive(Clone)]
pub struct AppleVirtPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    semaphore: Semaphore,
    paused_vms: Mutex<Vec<Retained<VZVirtualMachine>>>,
    config: AppleVirtPoolConfig,
    /// The dispatch queue used for all VM lifecycle operations
    /// (start, pause, resume, stop, property reads). The Apple
    /// Virtualization framework invokes the completion handlers
    /// for these operations on this queue, and requires that all
    /// property reads and other calls also happen on it.
    ///
    /// We use the **main queue** for two reasons:
    ///
    /// 1. **It's the framework's documented default.** Apple's
    ///    docs say "if no queue was passed, the default queue is
    ///    the main queue." Using the main queue matches what
    ///    Apple's own code (e.g. `containerization`) does.
    ///
    /// 2. **Custom queues trigger a libdispatch assertion
    ///    crash** when the framework invokes the completion
    ///    block from an XPC service thread. The signature is:
    ///    `BUG IN CLIENT OF LIBDISPATCH: Assertion failed:
    ///    Block was expected to execute on queue [...]`. The
    ///    framework wraps the user-supplied block in a
    ///    `dispatch_block_t` and submits it to the configured
    ///    queue from the framework's own thread; libdispatch
    ///    then asserts because the actual invocation thread
    ///    differs from the queue's target. The main queue does
    ///    not have this problem (presumably because the
    ///    framework's XPC plumbing is wired to invoke the main
    ///    queue directly).
    ///
    /// The caller is responsible for ensuring the main queue is
    /// actively pumped. In a typical CLI tool, this means
    /// running `dispatch2::dispatch_main()` on the main thread
    /// (or otherwise processing the main run loop) so the
    /// framework's XPC service can deliver completion messages.
    /// A process that doesn't pump the main queue will see VM
    /// state transitions to `Running` but completion handlers
    /// will never fire — the visible symptom is a 10s timeout in
    /// `start_vm` with `state=4` (Running) and `canStart=true`.
    vm_queue: &'static DispatchQueue,
}

impl AppleVirtPool {
    /// Create a new pool. This **synchronously** constructs and
    /// starts `pool_size` VMs, then pauses each one. Each VM
    /// creation may take a few hundred milliseconds (the
    /// framework's `init` + `start` overhead); expect the total
    /// construction time to scale linearly with the pool size.
    pub fn new(config: AppleVirtPoolConfig) -> Result<Self, AppleVirtError> {
        // `isSupported` is a class method on `VZVirtualMachine`.
        // The `unsafe` requirement is documented in the
        // generated bindings; the operation is safe to call
        // (no shared mutable state), but the binding marks it
        // unsafe because the underlying API is part of the
        // system library.
        if unsafe { VZVirtualMachine::isSupported() } {
            // ok
        } else {
            return Err(AppleVirtError::InvalidConfig(
                "VZVirtualMachine is not supported on this host (check macOS version and \
                 whether you're running inside another VM)"
                    .into(),
            ));
        }
        if !Path::new(&config.host_store_path).exists() {
            return Err(AppleVirtError::Path(format!(
                "host store path not found at {}",
                config.host_store_path.display()
            )));
        }
        // The staged kernel paths were validated by
        // `StagedKernel::from_paths` (or already exist on disk
        // if the caller used `from_image`). We don't re-check
        // here — that would duplicate work.

        let mut paused_vms = Vec::with_capacity(config.pool_size);
        // Use the main dispatch queue for VM lifecycle operations.
        // See the `vm_queue` field doc on `PoolInner` for why we
        // don't use a custom queue (libdispatch assertion crash
        // when the framework submits the completion from its
        // own XPC thread). The caller is responsible for pumping
        // the main queue — see the field doc for details.
        let vm_queue: &'static DispatchQueue = DispatchQueue::main();

        for i in 0..config.pool_size {
            info!(index = i, total = config.pool_size, "creating Apple Virt VM");
            let vm = build_vm(&config, vm_queue)?;
            start_vm(&vm, vm_queue)?;
            paused_vms.push(vm);
        }

        for (i, vm) in paused_vms.iter().enumerate() {
            pause_vm(vm, vm_queue).map_err(|e| AppleVirtError::Callback {
                operation: "pre-pause",
                reason: format!("VM {i} failed to pause: {e}"),
            })?;
        }

        Ok(Self {
#[allow(clippy::arc_with_non_send_sync)]
            inner: Arc::new(PoolInner {
                semaphore: Semaphore::new(config.pool_size),
                paused_vms: Mutex::new(paused_vms),
                config,
                vm_queue,
            }),
        })
    }

    /// Acquire a VM from the pool, resuming it. If the pool is
    /// empty, a new VM is created on demand (slower).
    pub async fn acquire(&self) -> Result<AcquiredVm, AppleVirtError> {
        let _permit = self
            .inner
            .semaphore
            .acquire()
            .await
            .map_err(|e| AppleVirtError::PoolExhausted(format!("semaphore closed: {e}")))?;

        let vm = {
            let mut pool = self.inner.paused_vms.lock().await;
            pool.pop()
        };

        let vm = match vm {
            Some(vm) => {
                debug!("acquired warm VM from pool");
                resume_vm(&vm, self.inner.vm_queue).map_err(|e| AppleVirtError::Callback {
                    operation: "resume",
                    reason: e.to_string(),
                })?;
                vm
            }
            None => {
                warn!("pool exhausted; creating cold VM");
                let vm = build_vm(&self.inner.config, self.inner.vm_queue)?;
                start_vm(&vm, self.inner.vm_queue)?;
                vm
            }
        };

        Ok(AcquiredVm {
            vm: Some(vm),
            pool: self.clone(),
        })
    }

    /// Pool config (cloneable for diagnostics).
    pub fn config(&self) -> &AppleVirtPoolConfig {
        &self.inner.config
    }
}

/// RAII guard for an acquired VM. The VM is stopped on drop
/// via a blocking task (see `Drop` impl). To reuse the VM in
/// the pool, call `release().await?` explicitly instead of
/// dropping.
///
/// **IMPORTANT**: `AcquiredVm` is `!Send` because it contains
/// a `Retained<VZVirtualMachine>` (which is `!Send`). It must
/// not cross `.await` boundaries. The compiler enforces this,
/// but if you find yourself needing a `Send` handle, extract
/// the VM reference before the `.await`:
///
/// ```ignore
/// let running = vm.is_running();
/// some_async_fn().await;
/// if !running { /* ... */ }
/// ```
///
/// We do NOT implement `Send` or `Sync` explicitly; the
/// compiler's auto-derivation handles the `!Send` correctly.
pub struct AcquiredVm {
    vm: Option<Retained<VZVirtualMachine>>,
    pool: AppleVirtPool,
}

// SAFETY: `Retained<VZVirtualMachine>` is `!Send`, making
// `AcquiredVm` `!Send` by default. We explicitly assert the
// `!Send` marker to prevent accidental future changes that
// would unsafely force `Send`.
// SAFETY: `VZVirtualMachine` contains internal `PhantomPinned`
// and raw `UnsafeCell` storage that is not thread-safe.
// Retained<VZVirtualMachine> inherits this `!Send` property.
// We DO NOT implement Send or Sync here; the compiler derives
// them from the field types, which correctly yields `!Send`.
// This comment exists to make the `!Send` constraint obvious
// to future readers.

impl AcquiredVm {
    /// Pause the VM and return it to the pool. Idempotent.
    pub async fn release(mut self) -> Result<(), AppleVirtError> {
        let Some(vm) = self.vm.take() else {
            return Ok(());
        };
        pause_vm(&vm, self.pool.inner.vm_queue).map_err(|e| AppleVirtError::Callback {
            operation: "release-pause",
            reason: e.to_string(),
        })?;
        let mut pool = self.pool.inner.paused_vms.lock().await;
        if pool.len() < self.pool.inner.config.pool_size {
            pool.push(vm);
        }
        Ok(())
    }

    /// Borrow the underlying VM handle.
    pub fn vm(&self) -> &VZVirtualMachine {
        self.vm
            .as_ref()
            .expect("AcquiredVm::vm called after release")
    }

    /// Current `VZVirtualMachineState`. The raw `NSInteger` is
    /// one of: 0=Stopped, 1=Running, 2=Paused, 3=Error, and
    /// the intermediate `Starting/Pausing/Resuming/Stopping`
    /// states.
    pub fn state(&self) -> VZVirtualMachineState {
        unsafe { self.vm().state() }
    }

    /// True if the VM is currently in the `Running` state.
    pub fn is_running(&self) -> bool {
        self.state() == VZVirtualMachineState::Running
    }
}

impl Drop for AcquiredVm {
    fn drop(&mut self) {
        if let Some(vm) = self.vm.take() {
            let queue = self.pool.inner.vm_queue;
            // `Retained<VZVirtualMachine>` is `!Send`, so we
            // extract a raw address as usize (which IS Send)
            // and leak the retained count.
            let raw = &*vm as *const VZVirtualMachine as usize;
            std::mem::forget(vm);

            // Spawn a blocking task that reforms the Retained
            // and calls stop_vm (which blocks on the framework
            // completion handler). The closure only captures
            // the usize (Send) and the static queue ref.
            tokio::task::spawn_blocking(move || {
                let ptr = raw as *mut VZVirtualMachine;
                // SAFETY: We leaked the Retained above; this
                // re-claims ownership on the blocking thread.
                let vm: Retained<VZVirtualMachine> = unsafe {
                    Retained::from_raw(ptr)
                }.expect("null VM handle in AcquiredVm::drop");
                if let Err(e) = stop_vm(&vm, queue) {
                    warn!("AcquiredVm::drop stop_vm failed: {e}");
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// PoolStateDelegate: a minimal VZVirtualMachineDelegate that just logs
// lifecycle transitions. We attach one of these to every VM in the pool
// because Apple's framework appears to require a non-nil delegate to
// drive the `startWithCompletionHandler` callback reliably (without it,
// we observed the callback never firing — the VM boots fine, but our
// completion block is silently dropped).
//
// The delegate has no per-instance state, so the ivars are `()`.
// ---------------------------------------------------------------------------
define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = ()]
    struct PoolStateDelegate;

    // SAFETY: VZVirtualMachineDelegate inherits from NSObjectProtocol,
    // and we satisfy its methods by providing logging-only
    // implementations of the two events we care about. The third
    // method (`virtualMachine:networkDevice:attachmentWasDisconnectedWithError:`)
    // is also optional and we skip it; it requires the `VZNetworkDevice`
    // feature which we don't enable.
    unsafe impl NSObjectProtocol for PoolStateDelegate {}

    unsafe impl VZVirtualMachineDelegate for PoolStateDelegate {
        #[unsafe(method(guestDidStopVirtualMachine:))]
        fn guest_did_stop_vm(&self, _virtual_machine: &VZVirtualMachine) {
            info!("PoolStateDelegate: guest stopped VM (clean shutdown)");
        }

        #[unsafe(method(virtualMachine:didStopWithError:))]
        fn virtual_machine_did_stop_with_error(
            &self,
            _virtual_machine: &VZVirtualMachine,
            error: &NSError,
        ) {
            let description = error.localizedDescription();
            warn!(
                error = %description,
                "PoolStateDelegate: VM stopped with error"
            );
        }
    }
);

impl PoolStateDelegate {
    /// Construct a new `PoolStateDelegate`. Caller takes ownership
    /// of the returned `Retained`; once it is handed to
    /// `VZVirtualMachine::setDelegate`, the VM retains a weak
    /// reference to it (per Apple's property docs), but the
    /// `Retained` returned here keeps it alive for the lifetime
    /// of our pool handle regardless.
    fn new() -> Retained<Self> {
        // SAFETY: `[PoolStateDelegate new]` is `[[PoolStateDelegate
        // alloc] init]`. The default NSObject init is fine because
        // our ivars are `()` and require no initialization.
        let raw: *mut Self = unsafe { msg_send![Self::class(), new] };
        unsafe { Retained::from_raw(raw) }.expect("PoolStateDelegate::new returned null")
    }
}

// ---------------------------------------------------------------------------
// Internal helpers: build / start / pause / resume a VM.
// ---------------------------------------------------------------------------

/// Helper: allocate an Objective-C object of type `T`. The
/// `ClassType` trait gives us the class handle; `alloc()` on the
/// class is safe to call but returns an uninitialized pointer.
fn alloc_objc<T: ClassType>() -> Allocated<T> {
    let cls = T::class();
    // SAFETY: `alloc` is safe to call on any class; it returns
    // an uninitialized instance which we immediately wrap in
    // `Allocated<T>`.
    unsafe { msg_send![cls, alloc] }
}

/// Build a `VZVirtualMachine` from the config. Does NOT start
/// it.
///
/// The `vm_queue` is the dispatch queue that the framework will
/// use to invoke completion handlers for this VM. We pass it
/// explicitly to `initWithConfiguration:queue:` so that the
/// framework's async callbacks (start, pause, stop, etc.) have
/// a known target — without an explicit queue, the framework
/// uses the main queue, which a CLI tool like ours does not
/// pump, causing the completion handlers to never fire.
fn build_vm(
    config: &AppleVirtPoolConfig,
    vm_queue: &DispatchQueue,
) -> Result<Retained<VZVirtualMachine>, AppleVirtError> {
    // 1. Boot loader (Linux kernel + optional initramfs).
    //    The kernel and initramfs paths come from a
    //    `StagedKernel` (which was either pulled from an OCI
    //    image or staged by the caller). The framework just
    //    needs host file paths.
    let kernel_url = NSURL::fileURLWithPath(&NSString::from_str(
        config.kernel.vmlinux_path().to_string_lossy().as_ref(),
    ));
    let boot_loader = unsafe {
        VZLinuxBootLoader::initWithKernelURL(alloc_objc(), &kernel_url)
    };
    if let Some(initramfs) = config.kernel.initramfs_path() {
        let initramfs_url = NSURL::fileURLWithPath(&NSString::from_str(
            initramfs.to_string_lossy().as_ref(),
        ));
        unsafe { boot_loader.setInitialRamdiskURL(Some(&initramfs_url)) };
    }
    // The kernel command line. `reboot=t` makes a kernel
    // panic auto-reboot the VM rather than hang the
    // framework. `console=hvc0` is the virtio console
    // device.
    let cmdline = NSString::from_str("reboot=t panic=-1 console=hvc0");
    unsafe { boot_loader.setCommandLine(&cmdline) };

    // 2. Platform (required for `VZLinuxBootLoader`).
    let platform = unsafe { VZGenericPlatformConfiguration::new() };

    // 3. CPUs + memory.
    let vm_config = unsafe { VZVirtualMachineConfiguration::new() };
    unsafe { vm_config.setBootLoader(Some(&boot_loader)) };
    unsafe { vm_config.setPlatform(&platform) };
    unsafe { vm_config.setCPUCount(config.cpus as usize) };
    unsafe { vm_config.setMemorySize((config.mem_mib as u64) * 1024 * 1024) };

    // 4. VirtioFS: mount the host's DAG store. The tag
    // `nimbus-store` is what the guest will reference.
    let store_tag = NSString::from_str("nimbus-store");
    let fs_device = unsafe { VZVirtioFileSystemDeviceConfiguration::init(alloc_objc()) };
    unsafe { fs_device.setTag(&store_tag) };
    let store_url = NSURL::fileURLWithPath(&NSString::from_str(
        config.host_store_path.to_string_lossy().as_ref(),
    ));
    let shared_dir = unsafe { VZSharedDirectory::initWithURL_readOnly(alloc_objc(), &store_url, false) };
    let share = unsafe { VZSingleDirectoryShare::initWithDirectory(alloc_objc(), &shared_dir) };
    unsafe { fs_device.setShare(Some(&share)) };
    // `into_super` walks the class hierarchy to get a
    // `Retained<VZDirectorySharingDeviceConfiguration>`
    // (the parent of `VZVirtioFileSystemDeviceConfiguration`).
    let fs_array: Retained<NSArray<objc2_virtualization::VZDirectorySharingDeviceConfiguration>> =
        NSArray::from_retained_slice(&[fs_device.into_super()]);
    unsafe { vm_config.setDirectorySharingDevices(&fs_array) };

    // 5. Network: NAT. The framework handles host-side
    // NAT automatically; the guest sees a normal Ethernet
    // interface with a DHCP-assigned address in the
    // 192.168.64.0/24 range.
    let nat_attachment = unsafe { VZNATNetworkDeviceAttachment::new() };
    let net_device = unsafe { VZVirtioNetworkDeviceConfiguration::init(alloc_objc()) };
    unsafe { net_device.setAttachment(Some(&*nat_attachment)) };
    let net_array: Retained<NSArray<objc2_virtualization::VZNetworkDeviceConfiguration>> =
        NSArray::from_retained_slice(&[net_device.into_super()]);
    unsafe { vm_config.setNetworkDevices(&net_array) };

    // 6. Validate. The framework returns an NSError if
    // the config is invalid (e.g. no boot loader, missing
    // kernel, unsupported memory size).
    let validation: Result<(), Retained<NSError>> = unsafe { vm_config.validateWithError() };
    if let Err(err) = validation {
        let description = err.localizedDescription();
        return Err(AppleVirtError::InvalidConfig(description.to_string()));
    }

    // 7. Construct the VM. We pass the dispatch queue so the
    //    framework has a known target for its async completion
    //    handlers. Without this, the framework defaults to the
    //    main queue, which we do not pump — the start callback
    //    would never fire.
    let allocated: Allocated<VZVirtualMachine> = alloc_objc();
    let vm = unsafe { VZVirtualMachine::initWithConfiguration_queue(allocated, &vm_config, vm_queue) };
    // 8. Attach a logging delegate. The framework uses the
    //    delegate to surface state transitions; without one
    //    the start callback may not fire reliably on some
    //    macOS versions (we observed 60s+ timeouts without
    //    it). The delegate just logs state changes and the
    //    guest stop reason.
    let delegate = PoolStateDelegate::new();
    unsafe { vm.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
    Ok(vm)
}

/// Start a VM. Blocks (synchronously) until the framework's
/// `startWithCompletionHandler` callback fires.
///
/// The framework invokes the completion block on the queue
/// passed to `initWithConfiguration:queue:` (Apple's
/// documented behavior). We dispatch the start call onto
/// that same queue so the framework can serialize state
/// transitions and so the completion block has a known
/// thread to run on. The raw `*mut NSError` travels over
/// a `std::sync::mpsc` channel (the channel sender is
/// `Send`, the raw pointer is `Send`, so the closure
/// passed to `queue.exec_async` is `Send + 'static`). The
/// `Retained<VZVirtualMachine>` is `!Send`, so we pass it
/// to the closure as a raw pointer (the VM lives on the
/// heap via `Retained` reference counting — its lifetime
/// extends past the closure).
fn start_vm(vm: &VZVirtualMachine, queue: &DispatchQueue) -> Result<(), AppleVirtError> {
    // Skip the `canStart()` pre-flight check. The Apple docs
    // say property reads (including `canStart`) must happen on
    // the VM's configured queue. Reading from a non-queue
    // thread (the body thread here, since the main thread is
    // blocked in `dispatch_main()`) triggers a libdispatch
    // assertion trap on recent macOS. The framework will
    // report the same error via the completion handler if
    // the VM isn't in a startable state, so we just rely on
    // that.

    // The `*mut NSError` from the framework is `!Send`, so we
    // ferry it through the channel as a `usize` and cast it
    // back on the receiving side. The pointer is only ever
    // dereferenced on the receiving thread, where we hold the
    // sole reference.
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    // Both the VM pointer and the NSError pointer are `!Send`
    // raw pointers, so we ferry them through the channel as
    // `usize` and cast back on the receiving side. The VM
    // pointer is only ever dereferenced on the calling
    // thread, which holds the `Retained` (keeping the VM
    // alive for the duration of the closure's execution on
    // the queue).
    let vm_addr: usize = std::ptr::addr_of!(*vm) as usize;
    queue.exec_async(move || {
        // SAFETY: `vm_addr` is the address of a
        // `VZVirtualMachine` kept alive by the caller's
        // `Retained`. The closure runs on the serial queue
        // before the caller drops the `Retained`, so the VM
        // is guaranteed live for the duration of the call.
        let vm = unsafe { &*(vm_addr as *const VZVirtualMachine) };
        let block = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(err as usize);
        });
        unsafe { vm.startWithCompletionHandler(&block) };
    });

    match rx.recv_timeout(Duration::from_secs(callback_timeout_secs())) {
        Ok(0) => Ok(()),
        Ok(raw) => {
            let err_ptr = raw as *mut NSError;
            let retained = unsafe { Retained::retain(err_ptr) }
                .expect("non-null NSError from framework");
            Err(AppleVirtError::from(retained))
        }
        Err(_timeout) => {
            // Don't call `vm.state()` or `vm.canStart()` here
            // — the Apple docs say these property reads must
            // happen on the VM's configured queue, and we're
            // on a different thread. Reading from a non-queue
            // thread triggers a libdispatch assertion trap.
            // The framework's own delegate (PoolStateDelegate)
            // will have logged any state transitions up to
            // this point.
            error!("Apple Virt start callback timed out");
            Err(AppleVirtError::Callback {
                operation: "start",
                reason: format!(
                    "timed out after {}s waiting for completion handler",
                    callback_timeout_secs()
                ),
            })
        }
    }
}

/// Pause a VM. Dispatches the call to the VM's queue and
/// blocks until the framework's completion handler fires.
fn pause_vm(vm: &VZVirtualMachine, queue: &DispatchQueue) -> Result<(), AppleVirtError> {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let vm_addr: usize = std::ptr::addr_of!(*vm) as usize;
    queue.exec_async(move || {
        let vm = unsafe { &*(vm_addr as *const VZVirtualMachine) };
        let block = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(err as usize);
        });
        unsafe { vm.pauseWithCompletionHandler(&block) };
    });
    match rx
        .recv_timeout(Duration::from_secs(callback_timeout_secs()))
        .map_err(|_| AppleVirtError::Callback {
            operation: "pause",
            reason: format!("timed out after {}s", callback_timeout_secs()),
        })? {
        0 => Ok(()),
        raw => {
            let err_ptr = raw as *mut NSError;
            let retained = unsafe { Retained::retain(err_ptr) }
                .expect("non-null NSError from framework");
            Err(AppleVirtError::from(retained))
        }
    }
}

/// Resume a VM. Dispatches the call to the VM's queue and
/// blocks until the framework's completion handler fires.
fn resume_vm(vm: &VZVirtualMachine, queue: &DispatchQueue) -> Result<(), AppleVirtError> {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let vm_addr: usize = std::ptr::addr_of!(*vm) as usize;
    queue.exec_async(move || {
        let vm = unsafe { &*(vm_addr as *const VZVirtualMachine) };
        let block = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(err as usize);
        });
        unsafe { vm.resumeWithCompletionHandler(&block) };
    });
    match rx
        .recv_timeout(Duration::from_secs(callback_timeout_secs()))
        .map_err(|_| AppleVirtError::Callback {
            operation: "resume",
            reason: format!("timed out after {}s", callback_timeout_secs()),
        })? {
        0 => Ok(()),
        raw => {
            let err_ptr = raw as *mut NSError;
            let retained = unsafe { Retained::retain(err_ptr) }
                .expect("non-null NSError from framework");
            Err(AppleVirtError::from(retained))
        }
    }
}

/// Stop a VM. Dispatches the call to the VM's queue and
/// blocks until the framework's completion handler fires.
#[allow(dead_code)]
fn stop_vm(vm: &VZVirtualMachine, queue: &DispatchQueue) -> Result<(), AppleVirtError> {
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let vm_addr: usize = std::ptr::addr_of!(*vm) as usize;
    queue.exec_async(move || {
        let vm = unsafe { &*(vm_addr as *const VZVirtualMachine) };
        let block = RcBlock::new(move |err: *mut NSError| {
            let _ = tx.send(err as usize);
        });
        unsafe { vm.stopWithCompletionHandler(&block) };
    });
    match rx
        .recv_timeout(Duration::from_secs(callback_timeout_secs()))
        .map_err(|_| AppleVirtError::Callback {
            operation: "stop",
            reason: format!("timed out after {}s", callback_timeout_secs()),
        })? {
        0 => Ok(()),
        raw => {
            let err_ptr = raw as *mut NSError;
            let retained = unsafe { Retained::retain(err_ptr) }
                .expect("non-null NSError from framework");
            Err(AppleVirtError::from(retained))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: just construct a `AppleVirtPoolConfig` and
    /// check that the config struct is well-formed. The
    /// kernel validation lives in `StagedKernel::from_paths`
    /// (see `oci_kernel.rs`); the pool itself just stores the
    /// kernel alongside the other config fields.
    #[test]
    fn pool_config_construction() {
        // Build a minimal valid staged kernel (a 64-byte ELF
        // header is enough for from_paths to accept it).
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"\x7fELF dummy kernel").unwrap();
        let kernel = StagedKernel::from_paths(tmp.path().to_path_buf(), None).unwrap();
        let cfg = AppleVirtPoolConfig::new(kernel, PathBuf::from("/tmp"));
        assert_eq!(cfg.pool_size, DEFAULT_POOL_SIZE);
        assert_eq!(cfg.cpus, DEFAULT_VM_CPUS);
        assert_eq!(cfg.mem_mib, DEFAULT_VM_MEM_MIB);
    }
}
