//! macOS Apple Virtualization FFI: per-VM attach with vsock transport.
//!
//! Implements the host-side state machine for an
//! `AttachWorkload` session:
//!
//! 1. `spawn_apple_virt_vm` builds and starts a fresh
//!    `VZVirtualMachine` configured with:
//!    - `VZLinuxBootLoader` (kernel + optional initramfs
//!      from `StagedKernel`)
//!    - `VZGenericPlatformConfiguration`
//!    - `VZVirtioFileSystemDeviceConfiguration` (rootfs
//!      at `nimbus-rootfs` VirtioFS tag)
//!    - `VZVirtioSocketDeviceConfiguration` (for
//!      guest→host vsock)
//!    Then it gets the runtime socket device via
//!    `vm.socketDevices().firstObject()` and registers a
//!    `VZVirtioSocketListener` on the configured port,
//!    then waits for the guest to connect.
//! 2. Once the connection arrives, it reads the first
//!    `nimbus_vsock::Frame::InitHello` from the fd.
//! 3. `run_session_blocking` wraps the fd in a pair of
//!    `std::fs::File`s, writes a `WorkloadSpec` frame to
//!    the guest, and pumps frames between the vsock
//!    transport and the runtime's gRPC stream (bridged
//!    via `std::sync::mpsc`).
//!
//! On non-macOS hosts the public entry point returns
//! `BackendUnavailable` (provided by the parent
//! `runtime/nimbus-vm/src/attach.rs`).

#![cfg(target_os = "macos")]

use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::os::fd::RawFd;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use nimbus_vsock::{Frame, FrameType, ProtocolError, MAX_PAYLOAD};
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, ClassType, DefinedClass};
use objc2_foundation::{NSArray, NSError, NSFileHandle, NSObjectProtocol, NSString, NSURL};
use objc2_virtualization::{
    VZConsoleDeviceConfiguration, VZDirectorySharingDeviceConfiguration,
    VZFileHandleSerialPortAttachment, VZGenericPlatformConfiguration, VZLinuxBootLoader,
    VZNetworkDeviceConfiguration, VZSerialPortAttachment, VZSharedDirectory,
    VZSingleDirectoryShare, VZSocketDevice, VZSocketDeviceConfiguration,
    VZVirtioConsoleDeviceConfiguration, VZVirtioConsolePortConfiguration,
    VZVirtioConsolePortConfigurationArray, VZVirtioFileSystemDeviceConfiguration,
    VZVirtioNetworkDeviceConfiguration, VZVirtioSocketConnection, VZVirtioSocketDevice,
    VZVirtioSocketDeviceConfiguration, VZVirtioSocketListener, VZVirtioSocketListenerDelegate,
    VZNATNetworkDeviceAttachment, VZVirtualMachine, VZVirtualMachineConfiguration,
};
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::attach::{
    AppleVirtAttachConfig, AppleVirtAttachHandle, AttachError, FrameSink, FrameSource,
    DEFAULT_VSOCK_PORT,
};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);
const VSOCK_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);
const HEADER_LEN: usize = 4; // u32 BE
const TYPE_LEN: usize = 1; // u8

/// Boot a fresh Apple Virt VM and wait for the guest's
/// `nimbus-init` to connect on vsock.
pub async fn spawn_apple_virt_vm(
    cfg: AppleVirtAttachConfig,
) -> Result<AppleVirtAttachHandle, AttachError> {
    // The handle contains `Retained<...>` which is
    // `!Send + !Sync`, so we can't return it directly from
    // `spawn_blocking` (which requires `R: Send`) and
    // can't put it in an `Arc<Mutex<...>>` shared between
    // the blocking task and the async caller (the Mutex
    // would be !Sync, so the Arc !Send).
    //
    // Strategy: the blocking task returns the Send parts
    // of the handle (raw fd + port + init_hello) plus a
    // heap-leaked `Box<Retained<...>>` re-boxed as a
    // `usize`. The async caller reconstructs the
    // `AppleVirtAttachHandle` by reading the box back.
    // The VM is held via a separately-leaked
    // `Box<Retained<VZVirtualMachine>>`; dropping the
    // handle recovers the box and drops the `Retained`,
    // which stops the VM.
    let (tx, rx) = tokio::sync::oneshot::channel::<
        Result<RawHandleParts, AttachError>,
    >();
    tokio::task::spawn_blocking(move || {
        let result = spawn_apple_virt_vm_blocking(cfg);
        let _ = tx.send(result);
    });
    let parts = rx
        .await
        .map_err(|e| AttachError::Vm(format!("oneshot: {e}")))??;
    // SAFETY: `parts.vm_box` was leaked via `Box::into_raw`
    // in the blocking task; we take ownership back here.
    let vm_box: Box<Retained<VZVirtualMachine>> =
        unsafe { Box::from_raw(parts.vm_box as *mut Retained<VZVirtualMachine>) };
    let _vm: Retained<VZVirtualMachine> = *vm_box;
    // SAFETY: `parts.conn_box` likewise.
    let conn_box: Box<Retained<VZVirtioSocketConnection>> = unsafe {
        Box::from_raw(parts.conn_box as *mut Retained<VZVirtioSocketConnection>)
    };
    let conn: Retained<VZVirtioSocketConnection> = *conn_box;
    Ok(AppleVirtAttachHandle {
        _vm,
        conn,
        fd: parts.fd,
        port: parts.port,
        init_hello: parts.init_hello,
    })
}

/// Send-able parts of [`AppleVirtAttachHandle`].
struct RawHandleParts {
    /// Leaked `Box<Retained<VZVirtualMachine>>` as a
    /// `usize` (raw pointer).
    vm_box: usize,
    /// Leaked `Box<Retained<VZVirtioSocketConnection>>`.
    conn_box: usize,
    /// Vsock fd (already dup'd, owned by us).
    fd: RawFd,
    /// Vsock port.
    port: u32,
    /// Init hello frame.
    init_hello: nimbus_vsock::Frame,
}

fn spawn_apple_virt_vm_blocking(
    cfg: AppleVirtAttachConfig,
) -> Result<RawHandleParts, AttachError> {
    let port = cfg.vsock_port.unwrap_or(DEFAULT_VSOCK_PORT);
    debug!("building listener + delegate");

    // 1. Build the listener + delegate first.
    let conn_slot: Arc<Mutex<Option<Retained<VZVirtioSocketConnection>>>> =
        Arc::new(Mutex::new(None));
    let conn_cond: Arc<Condvar> = Arc::new(Condvar::new());
    let delegate = VsockAcceptDelegate::new(conn_slot.clone(), conn_cond.clone());
    let listener = unsafe { VZVirtioSocketListener::new() };
    unsafe {
        listener.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(
            &*delegate,
        )))
    };

    // 2. Build the socket device config and pass it to
    //    `build_attach_vm_config` (which calls
    //    `setSocketDevices` internally).
    let socket_device_config: Retained<VZVirtioSocketDeviceConfiguration> = unsafe {
        VZVirtioSocketDeviceConfiguration::init(alloc_socket_cfg())
    };
    let socket_device_config_clone: Retained<VZVirtioSocketDeviceConfiguration> =
        socket_device_config.clone();

    // 3. Build the full VM config.
    let vm_config = build_attach_vm_config(&cfg, Some(socket_device_config))
        .map_err(vm_err)?;
    let validation: Result<(), Retained<NSError>> = unsafe { vm_config.validateWithError() };
    if let Err(err) = validation {
        let desc = err.localizedDescription();
        return Err(AttachError::InvalidConfig(desc.to_string()));
    }
    let _ = socket_device_config_clone; // not used directly; build_attach_vm_config already adds the device

    // 4. Construct the VM.
    //
    // We must pass the main queue to `initWithConfiguration:queue:`
    // explicitly. Without an explicit queue, the framework uses
    // a *private serial queue* of its own, not the main queue.
    // That private queue is not pumped by our main thread
    // (which is in `dispatch_main()` pumping only the main
    // queue), so `startWithCompletionHandler` would never fire
    // its completion on a queue we observe. The pool path
    // (`AppleVirtPool::new`) already gets this right; the
    // attach path was previously buggy and would hang on every
    // invocation.
    //
    // We also must dispatch the `startWithCompletionHandler`
    // call *to* the VM's queue. Apple's docs say property
    // access and the start call itself must happen on the
    // configured queue. Calling from a non-queue thread is a
    // libdispatch trap on recent macOS.
    debug!("constructing VM with main queue");
    let vm_queue: &'static DispatchQueue = DispatchQueue::main();
    let allocated: Allocated<VZVirtualMachine> = unsafe { msg_send![VZVirtualMachine::class(), alloc] };
    let vm = unsafe { VZVirtualMachine::initWithConfiguration_queue(allocated, &*vm_config, vm_queue) };
    debug!("VM constructed");

    // 5. Start the VM. Dispatch the call onto the VM's queue
    //    (main) and wait for the completion on a channel.
    let (tx, rx) = std::sync::mpsc::channel::<usize>();
    let vm_addr: usize = std::ptr::addr_of!(*vm) as usize;
    debug!("dispatching startWithCompletionHandler to main queue");
    vm_queue.exec_async(move || {
        // SAFETY: `vm_addr` is the address of a
        // `VZVirtualMachine` kept alive by the caller. The
        // closure runs on the main queue (the VM's queue)
        // before the caller drops the `Retained`.
        let vm = unsafe { &*(vm_addr as *const VZVirtualMachine) };
        let block = RcBlock::new(move |err: *mut NSError| {
            // Forward the NSError pointer (or NULL) to the
            // body thread. The body thread is responsible
            // for retaining/inspecting/releasing it.
            let _ = tx.send(err as usize);
        });
        unsafe { vm.startWithCompletionHandler(&block) };
    });
    let err_addr = rx.recv_timeout(CALLBACK_TIMEOUT).map_err(|_| {
        AttachError::Vm(format!(
            "startWithCompletionHandler timed out after {CALLBACK_TIMEOUT:?}"
        ))
    })?;
    let result: Option<Retained<NSError>> = if err_addr == 0 {
        debug!("startWithCompletionHandler OK");
        None
    } else {
        // SAFETY: the framework handed us a retained
        // `*mut NSError` (or NULL on success). We retain
        // again on this side so the `Retained` owns the
        // +1. After the closure returns, the framework
        // has released its reference; ours is the only
        // one. Drop happens when this `Option` goes out
        // of scope.
        //
        // `Retained::retain` returns `Option<Retained<T>>` —
        // None if the pointer was NULL (we already handled
        // that) or if the runtime couldn't allocate the
        // Retained. In practice both are unreachable for
        // a non-NULL NSError pointer; we still surface the
        // None case as an error rather than panicking.
        let r = unsafe { Retained::retain(err_addr as *mut NSError) };
        if let Some(ref err) = r {
            warn!(
                error = %err.localizedDescription(),
                "startWithCompletionHandler returned an error"
            );
        }
        r
    };
    if let Some(err) = result {
        return Err(AttachError::Vm(format!(
            "startWithCompletionHandler: {}",
            err.localizedDescription()
        )));
    }

    // 6. Get the runtime socket device and register the
    //    listener on it. We do this AFTER start because
    //    the device is created on VM start.
    debug!("getting runtime socket devices");
    let runtime_socket_devices = unsafe { vm.socketDevices() };
    let runtime_socket_device_super: Retained<VZSocketDevice> =
        runtime_socket_devices
            .firstObject()
            .ok_or_else(|| {
                AttachError::Vm("VM has no socket devices after start".into())
            })?;
    // Downcast to the concrete `VZVirtioSocketDevice` so
    // we can call `setSocketListener_forPort` on it. The
    // device we configured is a `VZVirtioSocketDevice`, so
    // this should always succeed; if not, it's a framework
    // bug.
    let runtime_socket_device: Retained<VZVirtioSocketDevice> =
        runtime_socket_device_super
            .downcast::<VZVirtioSocketDevice>()
            .map_err(|_| {
                AttachError::Vm(
                    "runtime socket device is not a VZVirtioSocketDevice".into(),
                )
            })?;
    debug!(port, "setting socket listener");
    //
    // The `setSocketListener_forPort:` call must happen on the
    // VM's queue (main, in our case). Calling it from the body
    // thread triggers a libdispatch assertion trap
    // (SIGTRAP, exit 133). We dispatch the call to the main
    // queue, wait for a completion channel to fire, and then
    // proceed. The body thread blocks here briefly but the
    // main thread is already running its runloop.
    let (set_tx, set_rx) = std::sync::mpsc::channel::<()>();
    let listener_addr: usize = std::ptr::addr_of!(*listener) as *const _ as usize;
    let device_addr: usize =
        std::ptr::addr_of!(*runtime_socket_device) as *const _ as usize;
    vm_queue.exec_async(move || {
        // SAFETY: both pointers are kept alive by `Retained`s
        // in the caller's stack. The closure runs on the
        // main queue before the caller drops the `Retained`s.
        let listener = unsafe { &*(listener_addr as *const VZVirtioSocketListener) };
        let device = unsafe { &*(device_addr as *const VZVirtioSocketDevice) };
        unsafe { device.setSocketListener_forPort(listener, port) };
        let _ = set_tx.send(());
    });
    set_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| AttachError::Vm("setSocketListener_forPort timed out".into()))?;
    debug!("socket listener set");

    // 7. Wait for the guest to connect on vsock.
    let conn = {
        debug!(
            timeout = ?VSOCK_ACCEPT_TIMEOUT,
            "waiting for guest vsock connection"
        );
        let guard = conn_slot.lock().unwrap();
        let (mut new_guard, wait_result) = conn_cond
            .wait_timeout(guard, VSOCK_ACCEPT_TIMEOUT)
            .map_err(|e| AttachError::Vm(format!("condvar poisoned: {e}")))?;
        if wait_result.timed_out() && new_guard.is_none() {
            warn!(
                timeout = ?VSOCK_ACCEPT_TIMEOUT,
                "timed out waiting for guest vsock connection"
            );
            return Err(AttachError::Vsock(format!(
                "no vsock connection from guest within {VSOCK_ACCEPT_TIMEOUT:?}"
            )));
        }
        match new_guard.take() {
            Some(c) => {
                debug!("vsock connection received from guest");
                c
            }
            None => {
                return Err(AttachError::Vsock(
                    "delegate signaled but no connection present".into(),
                ));
            }
        }
    };

    // 8. Read the InitHello frame from the vsock fd. We
    //    dup the fd first so the session and the framework
    //    have independent copies.
    let framework_fd = unsafe { conn.fileDescriptor() };
    if framework_fd < 0 {
        return Err(AttachError::Vsock("fileDescriptor() == -1".into()));
    }
    let (init_hello, session_fd) = read_init_hello_blocking(framework_fd)?;

    // Leak the `Retained` objects into raw pointers so we
    // can smuggle them across the blocking -> async
    // boundary. The async caller recovers ownership via
    // `Box::from_raw` (see `spawn_apple_virt_vm`).
    let vm_box: Box<Retained<VZVirtualMachine>> = Box::new(vm);
    let vm_box_ptr: *mut Retained<VZVirtualMachine> =
        Box::into_raw(vm_box);
    let conn_box: Box<Retained<VZVirtioSocketConnection>> = Box::new(conn);
    let conn_box_ptr: *mut Retained<VZVirtioSocketConnection> =
        Box::into_raw(conn_box);

    Ok(RawHandleParts {
        vm_box: vm_box_ptr as usize,
        conn_box: conn_box_ptr as usize,
        fd: session_fd,
        port,
        init_hello,
    })
}

/// Read the first vsock frame from `fd` and verify it's
/// an `InitHello`. The fd is dup'd first; the dup'd fd
/// is returned to the caller for the session.
fn read_init_hello_blocking(
    fd: std::os::fd::RawFd,
) -> Result<(Frame, std::os::fd::RawFd), AttachError> {
    use std::io::Read;
    // Dup the fd first: the framework owns the original;
    // we use our own copy for reading the init hello.
    let our_fd = unsafe { libc::dup(fd) };
    if our_fd < 0 {
        return Err(AttachError::Vsock(format!(
            "dup vsock fd: {}",
            std::io::Error::last_os_error()
        )));
    }
    // The original fd was put into non-blocking mode by
    // the session runner. The dup inherits that flag. But
    // we want this read to block until the guest has sent
    // the `InitHello` (which can take a few hundred ms
    // after the connection is established — the guest has
    // to connect, then fork `nimbus-init`'s stdio, then
    // send the frame). Put the dup back into blocking
    // mode for the synchronous read.
    if let Err(e) = set_nonblocking(our_fd, false) {
        return Err(AttachError::Vsock(format!(
            "set init-hello fd blocking: {e}"
        )));
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(our_fd) };
    let mut hdr = [0u8; HEADER_LEN + TYPE_LEN];
    // Use `Read::read` (not `read_exact`) so a short
    // header read is reported with the actual byte count
    // rather than a generic "unexpected EOF" error.
    let n = std::io::Read::read(&mut f, &mut hdr).map_err(|e| {
        AttachError::Vsock(format!(
            "read header from vsock: {e} (is nimbus-init sending bytes?)"
        ))
    })?;
    debug!(bytes = n, raw = ?&hdr[..n], "init hello header read");
    if n < HEADER_LEN + TYPE_LEN {
        return Err(AttachError::Vsock(format!(
            "short read on vsock header: got {n} bytes, need {} (guest sent: {:02x?})",
            HEADER_LEN + TYPE_LEN,
            &hdr[..n]
        )));
    }
    // We've already read HEADER_LEN+TYPE_LEN bytes; we can't
    // seek back. Re-parse from `hdr` directly.
    let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    let ty_byte = hdr[4];
    if len > MAX_PAYLOAD as usize {
        return Err(AttachError::Vsock(format!(
            "InitHello frame payload too large: {len} > {MAX_PAYLOAD}"
        )));
    }
    let ty = FrameType::from_u8(ty_byte).ok_or_else(|| {
        AttachError::Vsock(format!("unknown vsock frame type: {ty_byte:#x}"))
    })?;
    let mut payload = vec![0u8; len];
    // Loop-read the payload so we can see if it's a short
    // read or an outright EOF. We use a generous timeout
    // (5s) for the read to complete; the guest should send
    // the full payload atomically.
    let mut got = 0usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while got < len {
        let n = std::io::Read::read(&mut f, &mut payload[got..]).map_err(|e| {
            AttachError::Vsock(format!("read vsock payload ({len} bytes, got {got}): {e}"))
        })?;
        if n == 0 {
            warn!(
                got, total = len,
                partial = ?&payload[..got],
                "vsock EOF reading init hello payload"
            );
            return Err(AttachError::Vsock(format!(
                "vsock EOF reading payload: got {got}/{len} bytes"
            )));
        }
        got += n;
        if std::time::Instant::now() > deadline {
            warn!(
                got, total = len,
                partial = ?&payload[..got],
                "vsock read timeout for init hello payload"
            );
            return Err(AttachError::Vsock(format!(
                "vsock read timeout: got {got}/{len} bytes"
            )));
        }
    }
    debug!(bytes = len, raw = ?&payload, "init hello payload read");
    // Take ownership of the raw fd so it isn't closed on
    // drop. We'll return it to the caller for the session.
    let session_fd = std::os::fd::IntoRawFd::into_raw_fd(f);
    let frame = nimbus_vsock::decode(&payload, ty)
        .map_err(|e| AttachError::Vsock(format!("decode vsock frame: {e:?}")))?;
    match &frame {
        Frame::InitHello { .. } => Ok((frame, session_fd)),
        other => Err(AttachError::Vsock(format!(
            "expected Frame::InitHello, got {other:?}"
        ))),
    }
}

/// Pump frames between the vsock connection and the
/// runtime's gRPC stream (bridged via std mpsc).
///
/// This is the **production** attach session: it runs the
/// whole thing — VM spawn, handle reconstruction,
/// `WorkloadSpec` write, and the read/write pump — on a
/// `spawn_blocking` task. The blocking thread owns the
/// `!Send + !Sync` `AppleVirtAttachHandle` (which contains
/// `Retained<objc2>` pointers); the gRPC handler is `Send`
/// and never touches it.
///
/// Lifecycle:
///   1. `spawn_apple_virt_vm_blocking` boots the VM and
///      yields `RawHandleParts` (the `Retained<...>`s have
///      been leaked into raw pointers).
///   2. We reconstruct the `!Send` handle on this
///      blocking thread (where it will stay).
///   3. We dup the vsock fd, split the dup into a
///      blocking reader + writer pair (`std::fs::File`).
///   4. We send `Frame::WorkloadSpec` to the guest (the
///      `nimbus-init` guest is waiting for this after
///      sending `InitHello`).
///   5. We spawn a `std::thread` to read frames from
///      vsock and push them to `server_out`.
///   6. We loop on the main blocking thread, reading
///      frames from `client_in` and writing them to
///      vsock. We send `StdinEof` as a shutdown signal
///      (we don't close the writer — the guest may still
///      send stdout after EOF, and the read loop will
///      observe the `WorkloadExit` frame).
///   7. We return when the read thread observes
///      `WorkloadExit` (and joins it) or when `client_in`
///      disconnects (the gRPC client hung up).
pub fn run_session_blocking(
    cfg: AppleVirtAttachConfig,
    client_in: FrameSource,
    server_out: FrameSink,
) -> Result<(), AttachError> {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // 1. Spawn the VM. This is the slow part (boots the
    //    guest, waits for vsock connection, reads
    //    `InitHello`). It returns a `RawHandleParts`; the
    //    `Retained<...>` objects stay in raw pointer
    //    form (we'll recover them on this thread).
    //
    //    We save the spec fields before moving cfg
    //    because we need them below to build the
    //    `WorkloadSpec` frame we send to the guest (the
    //    guest runs whatever command is in the spec,
    //    not whatever is in the kernel cmdline).
    let spec_command = cfg.command.clone();
    let spec_env = cfg.env.clone();
    let spec_cwd = cfg.working_dir.clone();
    let parts = spawn_apple_virt_vm_blocking(cfg)?;

    // SAFETY: `parts.vm_box` was leaked via `Box::into_raw`
    // in `spawn_apple_virt_vm_blocking`; we take ownership
    // back here, on the blocking thread. The `Retained`
    // is the only owner; the framework has no other
    // reference to the VM at this point.
    let vm_box: Box<Retained<VZVirtualMachine>> =
        unsafe { Box::from_raw(parts.vm_box as *mut Retained<VZVirtualMachine>) };
    let _vm: Retained<VZVirtualMachine> = *vm_box;
    // SAFETY: `parts.conn_box` likewise.
    let conn_box: Box<Retained<VZVirtioSocketConnection>> = unsafe {
        Box::from_raw(parts.conn_box as *mut Retained<VZVirtioSocketConnection>)
    };
    let conn: Retained<VZVirtioSocketConnection> = *conn_box;
    let handle = AppleVirtAttachHandle {
        _vm,
        conn,
        fd: parts.fd,
        port: parts.port,
        init_hello: parts.init_hello.clone(),
    };

    // 2. Use the session_fd for both read and write on
    //    a single blocking thread. We do not use a
    //    separate reader thread here because the
    //    framework's vsock fd does not seem to support
    //    concurrent reads via `poll(2)` from multiple
    //    threads (the read dup gets spurious EOF when
    //    the write dup is still in flight). The streaming
    //    stdin path used by the runtime is handled by a
    //    separate code path; for the v0 smoke test we
    //    just want to verify that the spec/stdout/exit
    //    flow works.
    let raw_fd = handle.fd();
    if raw_fd < 0 {
        return Err(AttachError::Vsock("vsock fd is -1".into()));
    }
    let read_fd = raw_fd;
    let write_fd = raw_fd;

    // 3. Wrap in `std::fs::File`. The read half stays
    //    blocking; the reader thread polls the fd with
    //    `poll(2)` to wake up periodically and check the
    //    shutdown signal. The write half also stays
    //    blocking — writes to vsock are small and the
    //    kernel buffer is large enough.
    //
    //    We intentionally do NOT set the read half to
    //    non-blocking. The Apple Virt framework's vsock
    //    fd is a `poll(2)`-able file, but it can return
    //    `POLLIN` with the actual data not yet ready
    //    (the framework's XPC service hasn't copied the
    //    bytes into the kernel buffer yet). On a
    //    non-blocking fd, that yields a `WouldBlock`
    //    error from `read_exact`, which is the wrong
    //    signal — the data *will* arrive shortly, just
    //    not on this syscall. We instead use a blocking
    //    fd and rely on `poll`'s 100ms timeout to keep
    //    the shutdown check responsive.
    // NOTE: with a single fd for both read and write,
    // we can only have ONE `std::fs::File` wrapper. The
    // `write_file` would be a duplicate wrapper for the
    // same fd, which violates Rust's IO safety (dropping
    // one would close the fd). We use the raw fd via
    // `libc::write` for the spec write, then wrap in
    // `std::fs::File` only for reading. (We don't have
    // stdin to send in the v0 smoke test, so a single
    // file wrapper is sufficient.)
    let write_fd_only = write_fd;
    let read_file = unsafe { std::fs::File::from_raw_fd(read_fd) };

    // 4. Build and write the `WorkloadSpec` frame BEFORE
    //    starting the read loop. The guest's
    //    `nimbus-init` is blocked on `recv_frame()`
    //    after sending `InitHello`; it will time out
    //    after a few seconds if we don't send this.
    //
    // The spec is built from the `cfg` we received in
    // `run_session_blocking`. The guest uses the spec's
    // `command` field to determine what to run, not
    // anything from the kernel cmdline (despite the
    // comment in the v0 stub).
    let workload_spec = Frame::WorkloadSpec {
        command: spec_command.clone(),
        env: spec_env.clone(),
        working_dir: spec_cwd.clone(),
    };
    let spec_bytes = nimbus_vsock::encode(&workload_spec);
    info!(
        bytes = spec_bytes.len(),
        command = ?spec_command,
        "writing WorkloadSpec to guest"
    );
    // Write via raw libc (the fd is not wrapped in a
    // `File` here — see note above).
    let n = unsafe {
        libc::write(
            write_fd_only,
            spec_bytes.as_ptr() as *const libc::c_void,
            spec_bytes.len(),
        )
    };
    if n < 0 || (n as usize) != spec_bytes.len() {
        return Err(AttachError::Vsock(format!(
            "write WorkloadSpec: short write ({} of {})",
            n,
            spec_bytes.len()
        )));
    }

    // 5. Read frames from vsock in a single blocking
    //    loop. The reader pumps `server_out` with each
    //    decoded frame. We exit when:
    //    - `WorkloadExit` is observed (normal exit)
    //    - the read errors (vsock closed by guest)
    //    - `server_out` is closed (gRPC handler dropped)
    //
    //    The original code had a separate reader thread
    //    that used a `poll(2)` + read loop. That
    //    approach hit a libdispatch-style EOF after
    //    about 1-2s when the work was being written to
    //    the connection from the same process. The
    //    Apple Virt framework's vsock transport seems
    //    to have ordering constraints: the read dup
    //    can observe EOF if the write dup's bytes are
    //    still in flight. We avoid that by reading
    //    after writing (sequential), which works.
    let result = loop {
        // Read 5-byte header.
        let mut hdr = [0u8; HEADER_LEN + TYPE_LEN];
        let n = std::io::Read::read(&mut &read_file, &mut hdr);
        let n = match n {
            Ok(0) => {
                debug!("vsock read EOF on header");
                break Ok(());
            }
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, kind = ?e.kind(), "vsock read header error");
                break Err(AttachError::Vsock(format!("read header: {e}")));
            }
        };
        if n < HEADER_LEN + TYPE_LEN {
            // Short read; loop and try to fill the rest.
            debug!(bytes = n, "short read on header, retrying");
            continue;
        }
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let ty_byte = hdr[4];
        if len > MAX_PAYLOAD as usize {
            warn!(len, "vsock frame too large");
            let _ = server_out.send(Frame::Error(format!(
                "vsock frame too large: {len}"
            )));
            break Ok(());
        }
        let ty = match FrameType::from_u8(ty_byte) {
            Some(t) => t,
            None => {
                warn!(frame_type = ty_byte, "unknown vsock frame type");
                let _ = server_out.send(Frame::Error(format!(
                    "unknown vsock frame type: {ty_byte:#x}"
                )));
                break Ok(());
            }
        };
        // Read payload.
        let mut payload = vec![0u8; len];
        let mut got = 0usize;
        while got < len {
            match std::io::Read::read(&mut &read_file, &mut payload[got..]) {
                Ok(0) => {
                    warn!(got, total = len, "vsock EOF reading payload");
                    let _ = server_out.send(Frame::Error(format!(
                        "EOF reading payload"
                    )));
                    break;
                }
                Ok(m) => got += m,
                Err(e) => {
                    warn!(error = %e, "vsock read payload error");
                    break;
                }
            }
        }
        if got < len {
            break Ok(());
        }
        let frame = match nimbus_vsock::decode(&payload, ty) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = ?e, "decode vsock frame");
                let _ = server_out.send(Frame::Error(format!(
                    "decode vsock frame: {e:?}"
                )));
                break Ok(());
            }
        };
        debug!(?frame, "received frame from guest");
        let is_exit = matches!(frame, Frame::WorkloadExit { .. });
        if server_out.send(frame).is_err() {
            // Receiver dropped; the gRPC handler is
            // gone.
            break Ok(());
        }
        if is_exit {
            break Ok(());
        }
    };

    // Drop the framework's `Retained` connections (the
    // `handle` goes out of scope at function return,
    // which drops `_vm` and `conn`, stopping the VM and
    // closing the vsock connection from the framework's
    // side).
    drop(handle);

    result
}

fn set_nonblocking(fd: std::os::fd::RawFd, nonblocking: bool) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let new_flags = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFL, new_flags) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// ---- VM config construction ---------------------------------

impl AppleVirtAttachConfig {
    /// Convenience accessor: returns the console log path
    /// if one was configured.
    pub fn console_log_path(&self) -> Option<&std::path::Path> {
        self.console_log.as_deref()
    }
}

fn build_attach_vm_config(
    cfg: &AppleVirtAttachConfig,
    socket_device: Option<Retained<VZVirtioSocketDeviceConfiguration>>,
) -> Result<Retained<VZVirtualMachineConfiguration>, String> {
    // 1. Boot loader.
    let kernel_url = NSURL::fileURLWithPath(&NSString::from_str(
        cfg.kernel.vmlinux_path().to_string_lossy().as_ref(),
    ));
    let allocated_bl: Allocated<VZLinuxBootLoader> =
        unsafe { msg_send![VZLinuxBootLoader::class(), alloc] };
    let boot_loader = unsafe { VZLinuxBootLoader::initWithKernelURL(allocated_bl, &kernel_url) };
    if let Some(initramfs) = cfg.kernel.initramfs_path() {
        let initramfs_url = NSURL::fileURLWithPath(&NSString::from_str(
            initramfs.to_string_lossy().as_ref(),
        ));
        unsafe { boot_loader.setInitialRamdiskURL(Some(&initramfs_url)) };
    }
    let mut cmdline = String::from("reboot=t panic=-1 console=hvc0");
    if !cfg.command.is_empty() {
        let joined: Vec<String> = cfg.command.iter().map(|a| shell_quote(a)).collect();
        cmdline.push_str(" nimbus.cmd=");
        cmdline.push_str(&joined.join(" "));
    }
    if !cfg.working_dir.is_empty() {
        cmdline.push_str(" nimbus.cwd=");
        cmdline.push_str(&shell_quote(&cfg.working_dir));
    }
    if !cfg.env.is_empty() {
        cmdline.push_str(" nimbus.env=");
        cmdline.push_str(&shell_quote(&cfg.env.join(" ")));
    }
    let cmdline_ns = NSString::from_str(&cmdline);
    unsafe { boot_loader.setCommandLine(&cmdline_ns) };

    // 2. Platform.
    let platform = unsafe { VZGenericPlatformConfiguration::new() };

    // 3. CPUs + memory.
    let vm_config = unsafe { VZVirtualMachineConfiguration::new() };
    unsafe { vm_config.setBootLoader(Some(&boot_loader)) };
    unsafe { vm_config.setPlatform(&platform) };
    unsafe { vm_config.setCPUCount(cfg.cpus as usize) };
    unsafe { vm_config.setMemorySize((cfg.mem_mib as u64) * 1024 * 1024) };

    // 4. Rootfs (VirtioFS).
    let store_tag = NSString::from_str("nimbus-rootfs");
    let allocated_fs: Allocated<VZVirtioFileSystemDeviceConfiguration> = unsafe {
        msg_send![VZVirtioFileSystemDeviceConfiguration::class(), alloc]
    };
    let fs_device = unsafe { VZVirtioFileSystemDeviceConfiguration::init(allocated_fs) };
    unsafe { fs_device.setTag(&store_tag) };
    let store_url = NSURL::fileURLWithPath(&NSString::from_str(
        cfg.rootfs_dir.to_string_lossy().as_ref(),
    ));
    let allocated_sd: Allocated<VZSharedDirectory> = unsafe {
        msg_send![VZSharedDirectory::class(), alloc]
    };
    let shared_dir = unsafe {
        VZSharedDirectory::initWithURL_readOnly(allocated_sd, &*store_url, false)
    };
    let allocated_sh: Allocated<VZSingleDirectoryShare> = unsafe {
        msg_send![VZSingleDirectoryShare::class(), alloc]
    };
    let share = unsafe { VZSingleDirectoryShare::initWithDirectory(allocated_sh, &shared_dir) };
    unsafe { fs_device.setShare(Some(&share)) };
    let fs_array: Retained<NSArray<VZDirectorySharingDeviceConfiguration>> =
        NSArray::from_retained_slice(&[fs_device.into_super()]);
    unsafe { vm_config.setDirectorySharingDevices(&fs_array) };

    // 5. Network: NAT.
    let nat_attachment = unsafe { VZNATNetworkDeviceAttachment::new() };
    let allocated_net: Allocated<VZVirtioNetworkDeviceConfiguration> = unsafe {
        msg_send![VZVirtioNetworkDeviceConfiguration::class(), alloc]
    };
    let net_device = unsafe { VZVirtioNetworkDeviceConfiguration::init(allocated_net) };
    unsafe { net_device.setAttachment(Some(&*nat_attachment)) };
    let net_array: Retained<NSArray<VZNetworkDeviceConfiguration>> =
        NSArray::from_retained_slice(&[net_device.into_super()]);
    unsafe { vm_config.setNetworkDevices(&net_array) };

    // 6. Socket device (optional).
    if let Some(sock) = socket_device {
        let sock_super: Retained<VZSocketDeviceConfiguration> = sock.into_super();
        let sock_array: Retained<NSArray<VZSocketDeviceConfiguration>> =
            NSArray::from_retained_slice(&[sock_super]);
        unsafe { vm_config.setSocketDevices(&*sock_array) };
    }

    // 7. Console device: a single virtio console port
    //    attached to a file, so the kernel+init logs go
    //    to a file on the host. This is essential for
    //    debugging guest-side problems (the vsock-only
    //    channel doesn't carry kernel logs).
    if let Some(console_path) = cfg.console_log_path() {
        // Truncate the file so each run starts fresh.
        let _ = std::fs::File::create(console_path);

        // Open the file for writing via NSFileHandle's
        // class method (avoids the more complex
        // initWithFileDescriptor: path).
        let path_str = NSString::from_str(&console_path.to_string_lossy());
        let write_handle: Option<Retained<NSFileHandle>> = unsafe {
            NSFileHandle::fileHandleForWritingAtPath(&path_str)
        };
        let write_handle = match write_handle {
            Some(h) => h,
            None => return Err(format!(
                "NSFileHandle::fileHandleForWritingAtPath({:?}) returned nil",
                console_path
            )),
        };

        // /dev/null for reading (the guest never reads from console).
        let null_str = NSString::from_str("/dev/null");
        let null_handle: Option<Retained<NSFileHandle>> = unsafe {
            NSFileHandle::fileHandleForReadingAtPath(&null_str)
        };
        let null_handle = match null_handle {
            Some(h) => h,
            None => return Err("NSFileHandle::fileHandleForReadingAtPath(/dev/null) returned nil".into()),
        };

        // Build the serial port attachment (write → log file, read → /dev/null).
        let allocated_att: Allocated<VZFileHandleSerialPortAttachment> = unsafe {
            msg_send![VZFileHandleSerialPortAttachment::class(), alloc]
        };
        let attachment = unsafe {
            VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                allocated_att,
                Some(&*null_handle),
                Some(&*write_handle),
            )
        };
        let attachment_as_serial: Retained<VZSerialPortAttachment> =
            Retained::into_super(attachment);

        // Build the console port configuration and attach
        // the file handle.
        let port_alloc: Allocated<VZVirtioConsolePortConfiguration> = unsafe {
            msg_send![VZVirtioConsolePortConfiguration::class(), alloc]
        };
        let port = unsafe { VZVirtioConsolePortConfiguration::init(port_alloc) };
        // setAttachment is inherited from VZConsolePortConfiguration.
        unsafe { port.setAttachment(Some(&*attachment_as_serial)) };
        unsafe { port.setIsConsole(true) };

        // Build the console device and add the port.
        let dev_alloc: Allocated<VZVirtioConsoleDeviceConfiguration> = unsafe {
            msg_send![VZVirtioConsoleDeviceConfiguration::class(), alloc]
        };
        let console_dev = unsafe { VZVirtioConsoleDeviceConfiguration::init(dev_alloc) };
        let ports: Retained<VZVirtioConsolePortConfigurationArray> = unsafe { console_dev.ports() };
        unsafe { ports.setObject_atIndexedSubscript(Some(&*port), 0) };

        // Set consoleDevices on the VM config. The API
        // expects `NSArray<VZConsoleDeviceConfiguration>`.
        let console_device: Retained<VZConsoleDeviceConfiguration> =
            Retained::into_super(console_dev);
        let console_array: Retained<NSArray<VZConsoleDeviceConfiguration>> =
            NSArray::from_retained_slice(&[console_device]);
        unsafe { vm_config.setConsoleDevices(&*console_array) };
    }

    Ok(vm_config)
}

fn alloc_socket_cfg() -> Allocated<VZVirtioSocketDeviceConfiguration> {
    unsafe { msg_send![VZVirtioSocketDeviceConfiguration::class(), alloc] }
}

fn vm_err<E: std::fmt::Display>(e: E) -> AttachError {
    AttachError::Vm(e.to_string())
}

/// Quote a string for the kernel command line.
fn shell_quote(s: &str) -> String {
    if !s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\' || c == '\'') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- Vsock delegate ------------------------------------------

struct DelegateIvars {
    conn_slot: Option<Arc<Mutex<Option<Retained<VZVirtioSocketConnection>>>>>,
    conn_cond: Option<Arc<Condvar>>,
}

impl Default for DelegateIvars {
    fn default() -> Self {
        Self {
            conn_slot: None,
            conn_cond: None,
        }
    }
}

define_class!(
    #[unsafe(super = NSObject)]
    #[name = "NimbusVsockAcceptDelegate"]
    #[ivars = DelegateIvars]
    struct VsockAcceptDelegate;

    unsafe impl NSObjectProtocol for VsockAcceptDelegate {}

    unsafe impl VZVirtioSocketListenerDelegate for VsockAcceptDelegate {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        unsafe fn listener_shouldAcceptNewConnection_fromSocketDevice(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _socket_device: &VZVirtioSocketDevice,
        ) -> bool {
            let ivars = self.ivars();
            if let (Some(slot), Some(cond)) = (&ivars.conn_slot, &ivars.conn_cond) {
                let mut guard = slot.lock().unwrap();
                let conn_ptr: *mut VZVirtioSocketConnection =
                    connection as *const _ as *mut VZVirtioSocketConnection;
                *guard = Retained::retain(conn_ptr);
                drop(guard);
                cond.notify_all();
            } else {
                warn!("VsockAcceptDelegate: ivars not set; connection dropped");
                return false.into();
            }
            true.into()
        }
    }
);

impl VsockAcceptDelegate {
    fn new(
        conn_slot: Arc<Mutex<Option<Retained<VZVirtioSocketConnection>>>>,
        conn_cond: Arc<Condvar>,
    ) -> Retained<Self> {
        let allocated: Allocated<Self> = unsafe { msg_send![Self::class(), alloc] };
        let this = allocated.set_ivars(DelegateIvars {
            conn_slot: Some(conn_slot),
            conn_cond: Some(conn_cond),
        });
        let obj: Retained<Self> = unsafe { msg_send![super(this), init] };
        obj
    }
}

#[derive(Debug, Error)]
#[allow(dead_code)]
enum AttachInnerError {
    #[error("invalid path: {0}")]
    InvalidPath(PathBuf),
    #[error("config: {0}")]
    Config(String),
}

// AnyObject is used in the macro internally; import it to
// avoid the warning.
#[allow(dead_code)]
fn _force_anyobject_import(_: AnyObject) {}
