// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Per-VM attach primitives for microVM workloads.
//!
//! This module exposes the lower-level building blocks that
//! the runtime service uses to implement the gRPC
//! `AttachWorkload` RPC:
//!
//! 1. [`AppleVirtAttachConfig`] — the per-VM config (kernel,
//!    initramfs, rootfs, command, etc.)
//! 2. [`spawn_apple_virt_vm`] — boots a fresh Apple Virt VM
//!    and waits for the `pullrun-init` guest to connect on
//!    vsock. Returns an [`AppleVirtAttachHandle`] that owns
//!    the VM and the vsock connection. Used by tests and
//!    `tools/apple-virt-smoke`; the production path is
//!    [`run_session_blocking`] which folds the spawn into
//!    the session runner.
//! 3. [`run_session_blocking`] — production session runner.
//!    Spawns the VM, sends the `WorkloadSpec` frame to the
//!    guest, and pumps frames between vsock and two
//!    `std::sync::mpsc` adapters until the workload exits.
//!    Runs on a `tokio::task::spawn_blocking` task because
//!    the `!Send` `AppleVirtAttachHandle` can't cross the
//!    async/spawn_blocking boundary.
//!
//! The runtime service wires (3) up to the gRPC `AttachMessage`
//! stream. `pullrun-vm` itself stays unaware of gRPC or proto
//! types; the contract is just "two byte streams that produce
//! and consume length-prefixed [`pullrun_vsock::Frame`]s."
//!
//! On non-macOS hosts every entry point returns
//! [`AttachError::BackendUnavailable`]. The runtime service
//! surfaces that to the client as `failed_precondition`.

use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::atomic::AtomicU64;

use pullrun_vsock::Frame;
use thiserror::Error;

/// Configuration for an Apple Virt attach session.
///
/// One of these is built per `AttachWorkload` gRPC request
/// and passed to [`spawn_apple_virt_vm`].
#[derive(Debug)]
pub struct AppleVirtAttachConfig {
    /// Pre-staged kernel (from `StagedKernel::from_image` or
    /// `StagedKernel::from_paths`).
    pub kernel: crate::oci_kernel::StagedKernel,
    /// Path to the rootfs (the OCI image's materialized
    /// root, or an ext4 image). Must be a directory; the VM
    /// shares it with the guest over 9p/VirtioFS.
    pub rootfs_dir: PathBuf,
    /// What to run inside the VM. `argv[0]` is the
    /// executable, the rest are arguments.
    pub command: Vec<String>,
    /// `KEY=VALUE` environment variables.
    pub env: Vec<String>,
    /// Working directory inside the guest. Empty means
    /// `pullrun-init`'s default (`/`).
    pub working_dir: String,
    /// vCPUs per VM. 1 is enough for the FFI round-trip.
    pub cpus: u8,
    /// Memory in MiB. 512 is the minimum that Linux can
    /// boot with on arm64.
    pub mem_mib: u32,
    /// Vsock port the guest connects to. Defaults to
    /// [`DEFAULT_VSOCK_PORT`] if `None`.
    pub vsock_port: Option<u32>,
    /// Workload ID — used to name the VM's background
    /// thread for debuggability.
    pub workload_id: String,
    /// Path to a file the kernel+init console logs get
    /// written to. If `None`, no console device is
    /// configured and kernel/init logs go to /dev/null.
    pub console_log: Option<PathBuf>,
    /// If true, allocate a PTY for the workload so it sees
    /// a real terminal with prompt, job control, etc.
    pub tty: bool,
    /// Initial terminal rows for the PTY (0 = default 24).
    pub initial_rows: u32,
    /// Initial terminal columns for the PTY (0 = default 80).
    pub initial_cols: u32,
    /// Volume mounts: host directories to share into the VM via VirtioFS.
    pub mounts: Vec<pullrun_exec::Mount>,
}

/// The default port the `pullrun-init` guest connects to on
/// the host's vsock device. Conventional, but arbitrary; the
/// kernel command line and the host's listener must agree.
pub const DEFAULT_VSOCK_PORT: u32 = 42;

/// Commands sent to a persistent VM session.
#[derive(Debug)]
pub enum VmCommand {
    /// A client attaches: send output frames to this sender.
    AttachClient {
        session_id: u64,
        stdout_tx: std::sync::mpsc::Sender<Frame>,
    },
    /// The attached client has disconnected.
    DetachClient { session_id: u64 },
    /// Shut down the VM entirely.
    Shutdown,
}

/// Handle to a persistent Apple Virt VM that stays alive
/// after the client detaches. Created by [`spawn_vm`] and
/// used by [`attach_to_vm`].
///
/// Dropping the handle shuts down the VM and joins its
/// internal thread.
#[cfg(target_os = "macos")]
pub struct VmPersistentHandle {
    pub(crate) inner: crate::apple::attach::VmPersistentHandleInner,
}

#[cfg(target_os = "macos")]
impl VmPersistentHandle {
    /// Returns true if the VM's internal thread is still running.
    pub fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }
}

#[cfg(not(target_os = "macos"))]
pub struct VmPersistentHandle;

#[cfg(not(target_os = "macos"))]
impl VmPersistentHandle {
    /// Stub: non-macOS handles are never alive.
    pub fn is_alive(&self) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
impl Drop for VmPersistentHandle {
    fn drop(&mut self) {
        let _ = self.inner.cmd_tx.send(VmCommand::Shutdown);
        if let Some(t) = self.inner.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(target_os = "macos")]
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(target_os = "macos")]
pub(crate) fn next_session_id() -> u64 {
    NEXT_SESSION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Spawn a persistent Apple Virt VM for the given config.
/// The VM boots, receives the [`WorkloadSpec`], and stays
/// running until the workload exits or [`VmPersistentHandle`]
/// is dropped. Clients attach/detach via [`attach_to_vm`].
///
/// `on_exit` is called on the VM's background thread when
/// the VM thread exits (after cleanup). Use it to update
/// workload state, emit events, or clean up registries.
#[cfg(target_os = "macos")]
pub fn spawn_vm(
    cfg: AppleVirtAttachConfig,
    on_exit: Option<Box<dyn FnOnce() + Send>>,
) -> Result<VmPersistentHandle, AttachError> {
    crate::apple::attach::spawn_vm_inner(cfg, on_exit).map(|inner| VmPersistentHandle { inner })
}

/// Stub on non-macOS.
#[cfg(not(target_os = "macos"))]
pub fn spawn_vm(
    _cfg: AppleVirtAttachConfig,
    _on_exit: Option<Box<dyn FnOnce() + Send>>,
) -> Result<VmPersistentHandle, AttachError> {
    Err(AttachError::BackendUnavailable(
        "Apple Virtualization is macOS-only".into(),
    ))
}

/// Attach a gRPC session to a running persistent VM.
/// Forwards stdin from `client_in` to the VM and stdout
/// from the VM to `server_out`. Blocks until the client
/// disconnects or the VM exits.
#[cfg(target_os = "macos")]
pub fn attach_to_vm(
    handle: &VmPersistentHandle,
    client_in: FrameSource,
    server_out: FrameSink,
) -> Result<(), AttachError> {
    crate::apple::attach::attach_to_vm_inner(&handle.inner, client_in, server_out)
}

/// Stub on non-macOS.
#[cfg(not(target_os = "macos"))]
pub fn attach_to_vm(
    _handle: &VmPersistentHandle,
    _client_in: FrameSource,
    _server_out: FrameSink,
) -> Result<(), AttachError> {
    Err(AttachError::BackendUnavailable(
        "Apple Virtualization is macOS-only".into(),
    ))
}

/// Errors from the attach path.
#[derive(Debug, Error)]
pub enum AttachError {
    /// The macOS Apple Virtualization framework is not
    /// available on this host (e.g. running on Linux or in
    /// CI without the entitlement).
    #[error("Apple Virtualization backend not available: {0}")]
    BackendUnavailable(String),

    /// The workload id is not known to the runtime.
    #[error("workload not found: {0}")]
    NotFound(String),

    /// The attach request was malformed.
    #[error("invalid attach config: {0}")]
    InvalidConfig(String),

    /// The VM layer (Apple Virt) returned an error.
    #[error("VM error: {0}")]
    Vm(String),

    /// The vsock transport (host side) returned an error.
    #[error("vsock error: {0}")]
    Vsock(String),

    /// The workload (guest side) returned an error.
    #[error("workload error: {0}")]
    Workload(String),
}

/// An Apple Virt VM that is running and has an open vsock
/// connection to its `pullrun-init` guest.
///
/// Drop the handle to stop the VM and release the connection.
/// The vsock fd is owned by the underlying
/// `VZVirtioSocketConnection`; closing it on drop is handled
/// by the framework.
#[cfg(target_os = "macos")]
pub struct AppleVirtAttachHandle {
    /// The VM. Held to keep the VM alive for the lifetime of
    /// the attach session.
    pub(crate) _vm: objc2::rc::Retained<objc2_virtualization::VZVirtualMachine>,
    /// The vsock connection. `fileDescriptor()` returns the
    /// fd we read/write.
    pub(crate) _conn: objc2::rc::Retained<objc2_virtualization::VZVirtioSocketConnection>,
    /// Vsock fd (already dup'd by the spawn function so
    /// the framework's copy is independent).
    pub(crate) fd: std::os::fd::RawFd,
    /// Vsock port (for diagnostics).
    pub(crate) port: u32,
    /// Init hello payload (workload_id + init_pid) so the
    /// runtime can include it in lifecycle events.
    pub init_hello: pullrun_vsock::Frame,
}

/// Placeholder on non-macOS hosts (Linux, Windows). The type
/// exists so `spawn_apple_virt_vm` and `run_session_blocking`
/// can be declared with a concrete return type; the stub
/// implementations always return `Err`, so this ZST is never
/// constructed.
#[cfg(not(target_os = "macos"))]
pub struct AppleVirtAttachHandle;

#[cfg(target_os = "macos")]
impl AppleVirtAttachHandle {
    /// The raw file descriptor of the vsock connection.
    /// This is a dup of the framework's fd, owned by us.
    pub fn fd(&self) -> std::os::fd::RawFd {
        self.fd
    }

    /// The vsock port the guest connected to.
    pub fn port(&self) -> u32 {
        self.port
    }
}

/// Spawn an Apple Virt VM and wait for the guest's
/// `pullrun-init` to connect on vsock.
///
/// On macOS this builds the VM, starts it, registers a
/// `VZVirtioSocketListener` on [`DEFAULT_VSOCK_PORT`], and
/// blocks (with a 30s timeout) until the guest connects.
/// On non-macOS hosts this returns
/// `BackendUnavailable`.
#[cfg(target_os = "macos")]
pub async fn spawn_apple_virt_vm(
    cfg: AppleVirtAttachConfig,
) -> Result<AppleVirtAttachHandle, AttachError> {
    crate::apple::attach::spawn_apple_virt_vm(cfg).await
}

/// Stub on non-macOS. Always returns
/// `BackendUnavailable`.
#[cfg(not(target_os = "macos"))]
pub async fn spawn_apple_virt_vm(
    _cfg: AppleVirtAttachConfig,
) -> Result<AppleVirtAttachHandle, AttachError> {
    Err(AttachError::BackendUnavailable(
        "Apple Virtualization framework is macOS-only".into(),
    ))
}

/// Type alias for the two callback-style adapters the
/// session uses to bridge the vsock transport and the
/// runtime's gRPC stream.
///
/// The runtime service implements these with a `tokio::sync::mpsc`
/// or `tokio::sync::broadcast` channel, or by reading from a
/// `tokio_stream::wrappers::ReceiverStream`. We use a
/// `FnMut` (not `Fn`) so the implementation can advance
/// internal cursors.
pub type FrameSink = std::sync::mpsc::Sender<Frame>;
pub type FrameSource = std::sync::mpsc::Receiver<Frame>;

/// Pump frames between the vsock connection and the two
/// adapters until either side closes the stream or the
/// workload exits.
///
/// `vsock_fd` is the raw file descriptor returned by
/// [`AppleVirtAttachHandle::fd`]. `client_in` is the stream
/// of frames coming from the gRPC client (`WorkloadStdin`
/// + `StdinEof`); `server_out` is the sink for frames going
/// to the gRPC client (`WorkloadStdout` + `Stderr` +
/// `Exit`).
///
/// This is the **production** entry point. It runs the
/// whole session — VM spawn, handle reconstruction,
/// `WorkloadSpec` write, and the read/write pump — on a
/// single blocking task. The `!Send + !Sync`
/// [`AppleVirtAttachHandle`] is owned by the blocking
/// thread; the gRPC handler never touches it.
///
/// On non-macOS hosts this is a stub that returns
/// `BackendUnavailable`.
#[cfg(target_os = "macos")]
pub fn run_session_blocking(
    cfg: AppleVirtAttachConfig,
    client_in: FrameSource,
    server_out: FrameSink,
) -> Result<(), AttachError> {
    crate::apple::attach::run_session_blocking(cfg, client_in, server_out)
}

#[cfg(not(target_os = "macos"))]
pub fn run_session_blocking(
    _cfg: AppleVirtAttachConfig,
    _client_in: FrameSource,
    _server_out: FrameSink,
) -> Result<(), AttachError> {
    Err(AttachError::BackendUnavailable(
        "Apple Virtualization framework is macOS-only".into(),
    ))
}
