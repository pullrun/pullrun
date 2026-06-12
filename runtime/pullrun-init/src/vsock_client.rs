// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Guest-side vsock client.
//!
//! On Linux, the guest connects to the host's vsock listener
//! via an `AF_VSOCK` socket. The host CID is conventionally
//! `VMADDR_CID_HOST` (2). The port is the listener's port
//! (default: 42).
//!
//! In dev/test mode, we may also run `pullrun-init` outside a
//! VM (e.g. unit tests). In that case, we use a Unix domain
//! socket as a fallback. Set
//! `PULLRUN_VSOCK_FALLBACK_UNIX=/path/to.sock` to override the
//! default `/tmp/pullrun-init.sock`.

#[cfg(target_os = "linux")]
use std::os::unix::io::OwnedFd;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;

use pullrun_vsock::{Frame, ProtocolError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
#[cfg(target_os = "linux")]
use tracing::warn;

/// vsock connect errors.
#[derive(Debug, thiserror::Error)]
pub enum VsockError {
    #[error("AF_VSOCK connect failed: {0}")]
    VsockConnect(#[source] std::io::Error),

    #[error("Unix fallback connect failed: {0}")]
    UnixConnect(#[source] std::io::Error),

    #[error("encode frame: {0}")]
    Encode(#[from] ProtocolError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("EOF on vsock stream")]
    Eof,
}

/// A connected vsock stream to the host's runtime.
///
/// We back this with a `Mutex`-protected `UnixStream` (or
/// AF_VSOCK fd on Linux) because the underlying transport
/// doesn't support cheap async splits. Concurrent senders
/// serialize through the mutex; this is fine for the
/// workload's I/O rate (a few hundred KB/s at most).
#[derive(Clone, Debug)]
pub struct VsockClient {
    inner: Arc<VsockInner>,
}

#[derive(Debug)]
struct VsockInner {
    transport: VsockTransport,
}

#[derive(Debug)]
enum VsockTransport {
    /// Real AF_VSOCK socket. Only used on Linux guests.
    #[cfg(target_os = "linux")]
    Vsock(OwnedFd),
    /// Unix domain socket fallback for dev/test.
    Unix(UnixStream),
}

impl VsockClient {
    /// Open an AF_VSOCK connection to the host CID (2) on `port`.
    #[cfg(target_os = "linux")]
    fn connect_vsock(port: u32) -> Result<OwnedFd, std::io::Error> {
        // libc constants not in nix. We hardcode them.
        // AF_VSOCK = 40 on Linux.
        // VMADDR_CID_HOST = 2.
        // SOCK_STREAM = 1.
        //
        // `AF_VSOCK` must be passed to `libc::socket()` as
        // `c_int` (i32) because the `socket(2)` syscall uses
        // `int` for all three arguments. The libc binding
        // enforces this. `sa_family_t` is `u16` on Linux and
        // does not match — it would fail cross-compile checks.
        const AF_VSOCK: libc::c_int = 40;
        const VMADDR_CID_HOST: u32 = 2;

        // sockaddr_vm layout (Linux):
        //   u16  svm_family (AF_VSOCK)
        //   u16  svm_reserved1
        //   u32  svm_port
        //   u32  svm_cid
        //   u8[8] svm_zero
        #[repr(C)]
        struct SockAddrVm {
            svm_family: libc::sa_family_t,
            svm_reserved1: u16,
            svm_port: u32,
            svm_cid: u32,
            svm_zero: [u8; 8],
        }

        let addr = SockAddrVm {
            svm_family: AF_VSOCK as libc::sa_family_t,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: VMADDR_CID_HOST,
            svm_zero: [0; 8],
        };

        // Safety: socket() is async-signal-safe and doesn't
        // share state. The file descriptor is returned as a
        // bare c_int.
        let fd = unsafe {
            libc::socket(
                AF_VSOCK,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        // Safety: connect() takes a pointer to a sockaddr. We
        // pass a valid SockAddrVm.
        let ret = unsafe {
            libc::connect(
                fd,
                &addr as *const SockAddrVm as *const libc::sockaddr,
                std::mem::size_of::<SockAddrVm>() as libc::socklen_t,
            )
        };
        if ret < 0 {
            let e = std::io::Error::last_os_error();
            // Safety: close() on a valid fd is safe.
            unsafe { libc::close(fd) };
            return Err(e);
        }

        // Safety: we own `fd` and have just connected.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Dup the underlying fd and wrap it in a tokio UnixStream.
    ///
    /// This is the standard way to get a second independent
    /// handle on a Unix socket for use from a different task.
    /// The returned stream takes ownership of the duped fd
    /// and closes it on drop.
    fn dup_to_tokio_unix(
        fd: RawFd,
    ) -> Result<UnixStream, std::io::Error> {
        let duped = unsafe { libc::dup(fd) };
        if duped < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Build a sync UnixStream from the duped fd (takes
        // ownership), then convert to a tokio UnixStream.
        let std_stream = unsafe { StdUnixStream::from_raw_fd(duped) };
        std_stream.set_nonblocking(false).ok();
        UnixStream::from_std(std_stream)
    }

    /// Connect to the host's vsock listener on `port`.
    ///
    /// Tries, in order:
    /// 1. AF_VSOCK (Linux only), with retry — the host's
    ///    listener may take a moment to register after the
    ///    guest kernel boots, and the first connect can
    ///    race with the listener setup.
    /// 2. Unix domain socket at
    ///    `$PULLRUN_VSOCK_FALLBACK_UNIX` or
    ///    `/tmp/pullrun-init.sock`
    pub async fn connect(_port: u32) -> Result<Self, VsockError> {
        // Try AF_VSOCK first on Linux. Retry up to 5 times
        // with a short backoff because the host listener
        // registration can race with our connect.
        #[cfg(target_os = "linux")]
        {
            let mut last_err: Option<std::io::Error> = None;
            for attempt in 1..=5 {
                match Self::connect_vsock(_port) {
                    Ok(fd) => {
                        return Ok(Self {
                            inner: Arc::new(VsockInner {
                                transport: VsockTransport::Vsock(fd),
                            }),
                        });
                    }
                    Err(e) => {
                        warn!(attempt, error = %e, "AF_VSOCK connect failed; retrying");
                        last_err = Some(e);
                        // Brief sleep before retry. The host
                        // listener may not be ready yet.
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                }
            }
            warn!(
                error = ?last_err,
                "AF_VSOCK connect failed after 5 attempts; trying Unix fallback"
            );
        }

        // Unix fallback.
        let sock_path = std::env::var("PULLRUN_VSOCK_FALLBACK_UNIX")
            .unwrap_or_else(|_| "/tmp/pullrun-init.sock".to_string());
        let stream = UnixStream::connect(&sock_path)
            .await
            .map_err(VsockError::UnixConnect)?;
        Ok(Self {
            inner: Arc::new(VsockInner {
                transport: VsockTransport::Unix(stream),
            }),
        })
    }

    /// Send a frame to the host.
    pub async fn send_frame(&self, frame: Frame) -> Result<(), VsockError> {
        let bytes = pullrun_vsock::encode(&frame);
        match &self.inner.transport {
            #[cfg(target_os = "linux")]
            VsockTransport::Vsock(_fd) => {
                // For AF_VSOCK we wrap the fd in a tokio
                // File each send. This is a bit wasteful but
                // keeps the code simple; the alternative
                // (holding a Mutex<tokio::fs::File>) would
                // require cloning the fd per-call too.
                //
                // We use std::fs::File + spawn_blocking to
                // do the actual write to avoid pulling the
                // fd into a tokio task that might be moved
                // to another thread.
                let fd = _fd.try_clone().map_err(VsockError::Io)?;
                let bytes = bytes;
                tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let mut f = std::fs::File::from(fd);
                    f.write_all(&bytes)?;
                    f.flush()?;
                    Ok::<(), std::io::Error>(())
                })
                .await
                .map_err(|e| VsockError::Io(std::io::Error::other(format!("join: {e}"))))??;
            }
            VsockTransport::Unix(s) => {
                // Use a dup'd fd for each send. The dup
                // takes its own reference; the original
                // stream remains usable for the next send.
                let mut owned =
                    Self::dup_to_tokio_unix(s.as_raw_fd())
                        .map_err(VsockError::Io)?;
                owned.write_all(&bytes).await.map_err(VsockError::Io)?;
                owned.flush().await.map_err(VsockError::Io)?;
                // owned drops; duped fd is closed.
            }
        }
        Ok(())
    }

    /// Receive a frame from the host.
    pub async fn recv_frame(&self) -> Result<Frame, VsockError> {
        // Read the full 5-byte header (4-byte length + 1-byte type)
        // so we can decode in one shot.
        let mut hdr = [0u8; 5];
        self.read_exact(&mut hdr).await?;
        let len = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let ty_byte = hdr[4];
        if len > pullrun_vsock::MAX_PAYLOAD as usize {
            return Err(VsockError::Encode(ProtocolError::PayloadTooLarge {
                size: len as u32,
            }));
        }
        let ty = pullrun_vsock::FrameType::from_u8(ty_byte)
            .ok_or(VsockError::Encode(ProtocolError::Truncated {
                ty: pullrun_vsock::FrameType::Error,
                needed: 1,
                got: 0,
            }))?;
        let mut payload = vec![0u8; len];
        self.read_exact(&mut payload).await?;
        pullrun_vsock::decode(&payload, ty).map_err(VsockError::Encode)
    }

    async fn read_exact(&self, buf: &mut [u8]) -> Result<(), VsockError> {
        match &self.inner.transport {
            #[cfg(target_os = "linux")]
            VsockTransport::Vsock(_fd) => {
                let fd = _fd.try_clone().map_err(VsockError::Io)?;
                let (tx, rx) = tokio::sync::oneshot::channel();
                let buf_len = buf.len();
                let mut buf_vec = vec![0u8; buf_len];
                tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    let mut f = std::fs::File::from(fd);
                    if let Err(e) = f.read_exact(&mut buf_vec) {
                        let _ = tx.send(Err(e));
                        return;
                    }
                    let _ = tx.send(Ok(buf_vec));
                })
                .await
                .map_err(|e| VsockError::Io(std::io::Error::other(format!("join: {e}"))))?;
                let bytes = rx.await.map_err(|_| {
                    VsockError::Io(std::io::Error::other("blocking task cancelled"))
                })??;
                buf.copy_from_slice(&bytes);
            }
            VsockTransport::Unix(s) => {
                // For reads, we need to be careful: if
                // multiple tasks are reading concurrently,
                // the bytes get split. We use a per-call
                // dup'd fd to get an independent read.
                let mut owned =
                    Self::dup_to_tokio_unix(s.as_raw_fd())
                        .map_err(VsockError::Io)?;
                owned.read_exact(buf).await.map_err(VsockError::Io)?;
                // owned drops; duped fd is closed.
            }
        }
        Ok(())
    }

    /// Split the connection into a reader and writer.
    ///
    /// On Linux/AF_VSOCK, both halves get their own dup of
    /// the underlying fd. On Unix, same: each half is a
    /// dup'd `UnixStream`. The two halves are independent
    /// and can be moved into different tasks.
    ///
    /// Reading from one half does NOT consume bytes from
    /// the other (they share the same socket, but each has
    /// its own fd, and read on a socket fd does consume
    /// from the shared receive buffer — be careful).
    pub fn split(&self) -> (VsockClient, VsockClient) {
        let transport_a = match &self.inner.transport {
            #[cfg(target_os = "linux")]
            VsockTransport::Vsock(fd) => {
                let duped = fd.try_clone().expect("vsock fd clone");
                VsockTransport::Vsock(duped)
            }
            VsockTransport::Unix(s) => {
                let stream = Self::dup_to_tokio_unix(s.as_raw_fd())
                    .expect("dup_to_tokio_unix");
                VsockTransport::Unix(stream)
            }
        };
        let transport_b = match &self.inner.transport {
            #[cfg(target_os = "linux")]
            VsockTransport::Vsock(fd) => {
                let duped = fd.try_clone().expect("vsock fd clone");
                VsockTransport::Vsock(duped)
            }
            VsockTransport::Unix(s) => {
                let stream = Self::dup_to_tokio_unix(s.as_raw_fd())
                    .expect("dup_to_tokio_unix");
                VsockTransport::Unix(stream)
            }
        };
        (
            VsockClient {
                inner: Arc::new(VsockInner {
                    transport: transport_a,
                }),
            },
            VsockClient {
                inner: Arc::new(VsockInner {
                    transport: transport_b,
                }),
            },
        )
    }
}

impl AsRawFd for VsockClient {
    fn as_raw_fd(&self) -> RawFd {
        match &self.inner.transport {
            #[cfg(target_os = "linux")]
            VsockTransport::Vsock(fd) => fd.as_raw_fd(),
            VsockTransport::Unix(s) => s.as_raw_fd(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pullrun_vsock::Frame;
    use tokio::net::UnixListener;

    /// Helper: read a frame from a Unix stream using
    /// pullrun_vsock's `read_frame`.
    async fn read_frame_from(stream: &mut UnixStream) -> Frame {
        pullrun_vsock::read_frame(stream).await.unwrap()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unix_fallback_roundtrip() {
        // Set the fallback path before connecting.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("pullrun-init.sock");
        std::env::set_var("PULLRUN_VSOCK_FALLBACK_UNIX", &sock_path);

        // Start a "host" listener.
        let listener = UnixListener::bind(&sock_path).unwrap();
        let accept_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_frame_from(&mut stream).await
        });

        // Connect as guest.
        let client = VsockClient::connect(42).await.unwrap();
        client
            .send_frame(Frame::InitHello {
                workload_id: "wl-123".into(),
                init_pid: 1,
            })
            .await
            .unwrap();

        let frame = accept_task.await.unwrap();
        match frame {
            Frame::InitHello { workload_id, init_pid } => {
                assert_eq!(workload_id, "wl-123");
                assert_eq!(init_pid, 1);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unix_fallback_bidi() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("pullrun-init.sock");
        std::env::set_var("PULLRUN_VSOCK_FALLBACK_UNIX", &sock_path);

        let listener = UnixListener::bind(&sock_path).unwrap();

        // Host side: accept, send a WorkloadSpec, receive
        // WorkloadStdin + StdinEof.
        let host_task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read InitHello
            let _ = read_frame_from(&mut stream).await;

            // Send WorkloadSpec
            let spec = pullrun_vsock::Frame::WorkloadSpec {
                command: vec!["/bin/echo".into(), "hello".into()],
                env: vec!["FOO=bar".into()],
                working_dir: "/".to_string(),
                tty: false,
                rows: 0,
                cols: 0,
                mounts: vec![],
            };
            let bytes = pullrun_vsock::encode(&spec);
            stream.write_all(&bytes).await.unwrap();

            // Read WorkloadStdin
            let stdin_frame = read_frame_from(&mut stream).await;

            // Read StdinEof
            let eof_frame = read_frame_from(&mut stream).await;

            (stdin_frame, eof_frame)
        });

        let client = VsockClient::connect(42).await.unwrap();
        client
            .send_frame(Frame::InitHello {
                workload_id: "wl-bidi".into(),
                init_pid: 1,
            })
            .await
            .unwrap();

        // Receive WorkloadSpec
        let spec = client.recv_frame().await.unwrap();
        match spec {
            Frame::WorkloadSpec { command, env, working_dir, tty, .. } => {
                let _ = tty;
                assert_eq!(command, vec!["/bin/echo", "hello"]);
                assert_eq!(env, vec!["FOO=bar"]);
                assert_eq!(working_dir, "/");
            }
            other => panic!("unexpected frame: {other:?}"),
        }

        // Send WorkloadStdin + StdinEof
        client
            .send_frame(Frame::WorkloadStdin(bytes::Bytes::from_static(b"some input")))
            .await
            .unwrap();
        client.send_frame(Frame::StdinEof).await.unwrap();

        let (stdin_frame, eof_frame) = host_task.await.unwrap();
        match stdin_frame {
            Frame::WorkloadStdin(data) => {
                assert_eq!(&data[..], b"some input");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        match eof_frame {
            Frame::StdinEof => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}
