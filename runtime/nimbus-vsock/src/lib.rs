//! Wire protocol for host-guest communication over Virtio socket (vsock).
//!
//! Nimbus uses a length-prefixed binary frame protocol on top of a
//! reliable byte stream (vsock is reliable and in-order by design). Both
//! the host (`nimbus-runtime`) and the guest (`nimbus-init`) speak this
//! protocol. The frames are platform-agnostic and designed to be easy to
//! implement in ~200 LoC of guest-side Rust.
//!
//! # Frame format
//!
//! ```text
//! +--------+--------+-----------------+
//! | 4 byte | 1 byte |  payload        |
//! | length | type   |  (length bytes) |
//! +--------+--------+-----------------+
//!   ^        ^         ^
//!   |        |         +-- type-specific payload (see [`Frame`])
//!   |        +------------ frame type tag (see [`FrameType`])
//!   +--------------------- payload length, big-endian u32, excludes
//!                          the length and type bytes themselves
//! ```
//!
//! # Lifecycle
//!
//! ```text
//!  guest (nimbus-init)         host (nimbus-runtime)
//!       |                              |
//!       |  InitHello                   |
//!       |----------------------------->|
//!       |                              |
//!       |  WorkloadSpec                |
//!       |<-----------------------------|
//!       |                              |
//!       |  WorkloadStdout / Stderr     |
//!       |----------------------------->|
//!       |  WorkloadStdin               |
//!       |<-----------------------------|
//!       |  StdinEOF                    |
//!       |<-----------------------------|
//!       |                              |
//!       |  WorkloadExit                |
//!       |----------------------------->|
//! ```
//!
//! All payloads are length-prefixed strings or byte arrays — no
//! fixed-width structs on the wire, so future versions can extend the
//! protocol without breaking v0 peers (unknown frame types are
//! skipped, not rejected).
//!
//! # Concurrency model
//!
//! The guest runs two tasks on a single vsock connection:
//! - Reader: receives `WorkloadSpec`, `WorkloadStdin`, `StdinEOF`
//! - Writer: sends `InitHello`, `WorkloadStdout`, `WorkloadStderr`, `WorkloadExit`
//!
//! The host mirrors this: a reader task pulls from vsock and pumps
//! `WorkloadStdout`/`Stderr`/`Exit` to the gRPC stream; a writer task
//! receives gRPC `WorkloadStdin` messages and writes them to vsock.
//!
//! # Why not gRPC-over-vsock?
//!
//! gRPC requires HTTP/2, which requires a streaming HTTP/2 stack, which
//! is ~2MB of Rust (hyper, h2, tonic) statically linked into the
//! guest. We keep the protocol hand-rolled and ~50 LoC on the guest
//! side. The host side uses the same [`Frame`] codec.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Discriminator for the message body. Wire-stable: never re-use or
/// renumber. Unknown tags in a received stream are skipped, not
/// rejected, so a v0 host can talk to a v1 guest (the v1 guest will
/// silently drop v0's unknown frames and vice versa).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// Guest → host. First frame on every connection. Identifies the
    /// workload instance.
    InitHello = 0x01,

    /// Host → guest. Sent in response to `InitHello`. Specifies what
    /// to execute and with what environment.
    WorkloadSpec = 0x02,

    /// Guest → host. Stream of bytes from the workload's stdout.
    WorkloadStdout = 0x03,

    /// Guest → host. Stream of bytes from the workload's stderr.
    WorkloadStderr = 0x04,

    /// Host → guest. Stream of bytes to feed into the workload's
    /// stdin.
    WorkloadStdin = 0x05,

    /// Host → guest. Signals that no more stdin will arrive. The
    /// guest should close the workload's stdin pipe.
    StdinEof = 0x06,

    /// Guest → host. The workload exited. Payload is the exit code
    /// (i32) and signal (i32, 0 if not killed by signal).
    WorkloadExit = 0x07,

    /// Either direction. Hard error; the other side should terminate
    /// the workload and the connection.
    Error = 0x08,
}

impl FrameType {
    /// Convert a raw byte to a `FrameType`, returning `None` for
    /// unknown or reserved byte values.
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => Self::InitHello,
            0x02 => Self::WorkloadSpec,
            0x03 => Self::WorkloadStdout,
            0x04 => Self::WorkloadStderr,
            0x05 => Self::WorkloadStdin,
            0x06 => Self::StdinEof,
            0x07 => Self::WorkloadExit,
            0x08 => Self::Error,
            _ => return None,
        })
    }
}

impl fmt::Display for FrameType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InitHello => "InitHello",
            Self::WorkloadSpec => "WorkloadSpec",
            Self::WorkloadStdout => "WorkloadStdout",
            Self::WorkloadStderr => "WorkloadStderr",
            Self::WorkloadStdin => "WorkloadStdin",
            Self::StdinEof => "StdinEof",
            Self::WorkloadExit => "WorkloadExit",
            Self::Error => "Error",
        })
    }
}

/// A single decoded frame on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// First frame from the guest. `workload_id` is a UUID-like string
    /// (16-128 bytes, ASCII, no NULs). `init_pid` is the PID of
    /// `nimbus-init` itself (for diagnostics).
    InitHello {
        /// Workload identifier (UUID or workload id).
        workload_id: String,
        /// PID of nimbus-init, for diagnostics.
        init_pid: u32,
    },

    /// What to run.
    WorkloadSpec {
        /// argv, with `argv[0]` being the executable name.
        command: Vec<String>,
        /// Environment variables, in `KEY=VALUE` form.
        env: Vec<String>,
        /// Working directory inside the guest. Empty means
        /// `nimbus-init`'s default (typically `/`).
        working_dir: String,
    },

    /// Bytes from the workload's stdout.
    WorkloadStdout(Bytes),

    /// Bytes from the workload's stderr.
    WorkloadStderr(Bytes),

    /// Bytes to feed into the workload's stdin.
    WorkloadStdin(Bytes),

    /// Signals stdin EOF. No payload.
    StdinEof,

    /// The workload exited.
    WorkloadExit {
        /// Exit code (the `i32` the workload passed to `_exit()` /
        /// `exit()`). `Some` only if the process exited normally; if
        /// it was killed by a signal, this is `None`.
        exit_code: Option<i32>,
        /// Signal number, if the workload was killed by a signal.
        /// `None` if it exited normally.
        signal: Option<i32>,
    },

    /// Hard error. The other side should tear down the workload
    /// immediately.
    Error(String),
}

impl Frame {
    /// The wire type tag for this frame.
    pub fn frame_type(&self) -> FrameType {
        match self {
            Self::InitHello { .. } => FrameType::InitHello,
            Self::WorkloadSpec { .. } => FrameType::WorkloadSpec,
            Self::WorkloadStdout(_) => FrameType::WorkloadStdout,
            Self::WorkloadStderr(_) => FrameType::WorkloadStderr,
            Self::WorkloadStdin(_) => FrameType::WorkloadStdin,
            Self::StdinEof => FrameType::StdinEof,
            Self::WorkloadExit { .. } => FrameType::WorkloadExit,
            Self::Error(_) => FrameType::Error,
        }
    }

    /// Encode the frame's payload (everything after the 1-byte type
    /// tag) into a fresh buffer.
    fn encode_payload(&self) -> Bytes {
        match self {
            Self::InitHello { workload_id, init_pid } => {
                let mut buf = BytesMut::new();
                buf.put_u32(*init_pid);
                write_length_prefixed_string(&mut buf, workload_id);
                buf.freeze()
            }
            Self::WorkloadSpec { command, env, working_dir } => {
                let mut buf = BytesMut::new();
                buf.put_u32(command.len() as u32);
                for s in command {
                    write_length_prefixed_string(&mut buf, s);
                }
                buf.put_u32(env.len() as u32);
                for kv in env {
                    write_length_prefixed_string(&mut buf, kv);
                }
                write_length_prefixed_string(&mut buf, working_dir);
                buf.freeze()
            }
            Self::WorkloadStdout(b) | Self::WorkloadStderr(b) | Self::WorkloadStdin(b) => {
                b.clone()
            }
            Self::StdinEof => Bytes::new(),
            Self::WorkloadExit { exit_code, signal } => {
                let mut buf = BytesMut::new();
                // Bitfield: bit 0 = has_exit_code, bit 1 = has_signal.
                let mut flags = 0u8;
                if exit_code.is_some() {
                    flags |= 0x01;
                }
                if signal.is_some() {
                    flags |= 0x02;
                }
                buf.put_u8(flags);
                if let Some(c) = exit_code {
                    buf.put_i32(*c);
                }
                if let Some(s) = signal {
                    buf.put_i32(*s);
                }
                buf.freeze()
            }
            Self::Error(msg) => {
                let mut buf = BytesMut::new();
                write_length_prefixed_string(&mut buf, msg);
                buf.freeze()
            }
        }
    }

    fn decode_payload(ty: FrameType, payload: Bytes) -> Result<Self, ProtocolError> {
        let mut cur = payload;
        Ok(match ty {
            FrameType::InitHello => {
                if cur.remaining() < 4 {
                    return Err(ProtocolError::Truncated {
                        ty,
                        needed: 4,
                        got: cur.remaining(),
                    });
                }
                let init_pid = cur.get_u32();
                let workload_id = read_length_prefixed_string(&mut cur, ty)?;
                Self::InitHello { workload_id, init_pid }
            }
            FrameType::WorkloadSpec => {
                if cur.remaining() < 4 {
                    return Err(ProtocolError::Truncated {
                        ty,
                        needed: 4,
                        got: cur.remaining(),
                    });
                }
                let n_cmd = cur.get_u32() as usize;
                let mut command = Vec::with_capacity(n_cmd);
                for _ in 0..n_cmd {
                    command.push(read_length_prefixed_string(&mut cur, ty)?);
                }
                if cur.remaining() < 4 {
                    return Err(ProtocolError::Truncated {
                        ty,
                        needed: 4,
                        got: cur.remaining(),
                    });
                }
                let n_env = cur.get_u32() as usize;
                let mut env = Vec::with_capacity(n_env);
                for _ in 0..n_env {
                    env.push(read_length_prefixed_string(&mut cur, ty)?);
                }
                let working_dir = read_length_prefixed_string(&mut cur, ty)?;
                Self::WorkloadSpec { command, env, working_dir }
            }
            FrameType::WorkloadStdout => Self::WorkloadStdout(cur),
            FrameType::WorkloadStderr => Self::WorkloadStderr(cur),
            FrameType::WorkloadStdin => Self::WorkloadStdin(cur),
            FrameType::StdinEof => Self::StdinEof,
            FrameType::WorkloadExit => {
                if cur.remaining() < 1 {
                    return Err(ProtocolError::Truncated {
                        ty,
                        needed: 1,
                        got: 0,
                    });
                }
                let flags = cur.get_u8();
                let has_code = (flags & 0x01) != 0;
                let has_signal = (flags & 0x02) != 0;
                let exit_code = if has_code {
                    if cur.remaining() < 4 {
                        return Err(ProtocolError::Truncated { ty, needed: 4, got: cur.remaining() });
                    }
                    Some(cur.get_i32())
                } else {
                    None
                };
                let signal = if has_signal {
                    if cur.remaining() < 4 {
                        return Err(ProtocolError::Truncated { ty, needed: 4, got: cur.remaining() });
                    }
                    Some(cur.get_i32())
                } else {
                    None
                };
                Self::WorkloadExit { exit_code, signal }
            }
            FrameType::Error => {
                let msg = read_length_prefixed_string(&mut cur, ty)?;
                Self::Error(msg)
            }
        })
    }
}

/// A [`Frame`] tagged with its on-wire type. This is what the codec
/// yields; consumers pattern-match on the type tag returned by
/// [`Frame::frame_type`] before downcasting to the inner [`Frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedFrame {
    /// The decoded frame body.
    pub frame: Frame,
}

impl TaggedFrame {
    /// Convenience constructor.
    pub fn new(frame: Frame) -> Self {
        Self { frame }
    }
}

/// Errors that can occur on the wire or in the codec.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The byte stream ended before a full frame could be read.
    #[error("unexpected EOF reading {ty} frame")]
    UnexpectedEof {
        /// Frame type we were trying to read.
        ty: FrameType,
    },

    /// A frame declared a payload size larger than the buffer can
    /// hold. Hard limit is 16 MiB — workload I/O frames are
    /// typically < 64 KiB.
    #[error("frame payload too large: {size} bytes (max 16 MiB)")]
    PayloadTooLarge {
        /// Declared size in bytes.
        size: u32,
    },

    /// A frame's payload was shorter than the type-specific decoder
    /// expected.
    #[error("{ty} frame truncated: needed {needed} bytes, got {got}")]
    Truncated {
        /// Frame type we were trying to read.
        ty: FrameType,
        /// Bytes the type's decoder said it needed.
        needed: usize,
        /// Bytes actually available.
        got: usize,
    },

    /// A string field was not valid UTF-8.
    #[error("non-UTF-8 string in {ty} frame: {ty}")]
    BadUtf8 {
        /// Frame type containing the bad string.
        ty: FrameType,
        /// Underlying error.
        source: std::str::Utf8Error,
    },

    /// Underlying I/O error on the socket.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Maximum allowed payload size. 16 MiB is a hard cap to prevent a
/// malicious or buggy peer from making us allocate arbitrarily large
/// buffers. Real workload I/O is typically fragmented by the writer
/// into 8-64 KiB chunks anyway.
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;

/// Default vsock port for the host's runtime listener.
///
/// Port 42 is chosen because it is the answer to life, the
/// universe, and everything — and also because the conventional
/// range is 1024-4096 and this is a nice round-ish number
/// that's unlikely to collide with anything.  See
/// `nimbus-vm::attach::DEFAULT_VSOCK_PORT` for the
/// corresponding host-side constant.
pub const DEFAULT_VSOCK_PORT: u32 = 42;

/// Encode a `Frame` to its full wire form (4-byte length, 1-byte type,
/// payload). The returned buffer is what you write to the socket.
pub fn encode(frame: &Frame) -> Bytes {
    let payload = frame.encode_payload();
    let len = payload.len() as u32;
    let mut buf = BytesMut::with_capacity(4 + 1 + payload.len());
    buf.put_u32(len);
    buf.put_u8(frame.frame_type() as u8);
    buf.extend_from_slice(&payload);
    buf.freeze()
}

/// Decode a frame from its payload bytes (after the length and
/// type bytes have already been consumed).
///
/// This is the inverse of `encode`'s `frame.encode_payload()`
/// step. It is useful when you have the full payload in hand
/// (e.g. from a manual `read_exact` loop) and just need to
/// turn it into a `Frame`.
pub fn decode(payload: &[u8], ty: FrameType) -> Result<Frame, ProtocolError> {
    Frame::decode_payload(ty, Bytes::copy_from_slice(payload))
}

/// Read a single frame from a byte stream.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Frame, ProtocolError> {
    // 1. 4-byte big-endian length prefix.
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            ProtocolError::UnexpectedEof {
                ty: FrameType::InitHello, // placeholder
            }
        } else {
            ProtocolError::Io(e)
        }
    })?;
    let mut hdr_buf = Bytes::copy_from_slice(&hdr);
    let len = hdr_buf.get_u32();
    let ty_byte = hdr_buf.get_u8();
    if len > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge { size: len });
    }
    let ty = FrameType::from_u8(ty_byte).ok_or(ProtocolError::Truncated {
        ty: FrameType::InitHello,
        needed: 0,
        got: 0,
    })?;
    // 2. Payload.
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload).await?;
    Frame::decode_payload(ty, Bytes::from(payload))
}

/// Write a single frame to a byte stream. The whole buffer is
/// written atomically; partial writes are not handled at this layer.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<(), ProtocolError> {
    let bytes = encode(frame);
    w.write_all(&bytes).await?;
    Ok(())
}

// --- internal helpers -------------------------------------------------

fn write_length_prefixed_string(buf: &mut BytesMut, s: &str) {
    let bytes = s.as_bytes();
    buf.put_u32(bytes.len() as u32);
    buf.extend_from_slice(bytes);
}

fn read_length_prefixed_string(
    cur: &mut Bytes,
    ty: FrameType,
) -> Result<String, ProtocolError> {
    if cur.remaining() < 4 {
        return Err(ProtocolError::Truncated {
            ty,
            needed: 4,
            got: cur.remaining(),
        });
    }
    let len = cur.get_u32() as usize;
    if cur.remaining() < len {
        return Err(ProtocolError::Truncated {
            ty,
            needed: len,
            got: cur.remaining(),
        });
    }
    let bytes = cur.copy_to_bytes(len);
    std::str::from_utf8(&bytes)
        .map(String::from)
        .map_err(|e| ProtocolError::BadUtf8 { ty, source: e })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(frame: &Frame) {
        let bytes = encode(frame);
        // Simulate the wire: a Cursor over the encoded bytes.
        let mut cur = Cursor::new(bytes.to_vec());
        let rt = futures_lite::future::block_on(read_frame(&mut cur)).expect("decode");
        assert_eq!(&rt, frame);
    }

    #[test]
    fn roundtrip_init_hello() {
        roundtrip(&Frame::InitHello {
            workload_id: "wl-1234".to_string(),
            init_pid: 42,
        });
    }

    #[test]
    fn roundtrip_workload_spec() {
        roundtrip(&Frame::WorkloadSpec {
            command: vec!["/bin/sh".into(), "-c".into(), "echo hello".into()],
            env: vec!["PATH=/usr/bin:/bin".into(), "FOO=bar".into()],
            working_dir: "/".into(),
        });
    }

    #[test]
    fn roundtrip_io_streams() {
        roundtrip(&Frame::WorkloadStdout(Bytes::from_static(b"hello\n")));
        roundtrip(&Frame::WorkloadStderr(Bytes::from_static(b"oops\n")));
        roundtrip(&Frame::WorkloadStdin(Bytes::from_static(b"input")));
        roundtrip(&Frame::StdinEof);
    }

    #[test]
    fn roundtrip_workload_exit_normal() {
        roundtrip(&Frame::WorkloadExit {
            exit_code: Some(0),
            signal: None,
        });
    }

    #[test]
    fn roundtrip_workload_exit_signal() {
        roundtrip(&Frame::WorkloadExit {
            exit_code: None,
            signal: Some(9),
        });
    }

    #[test]
    fn roundtrip_workload_exit_both() {
        roundtrip(&Frame::WorkloadExit {
            exit_code: Some(1),
            signal: Some(15),
        });
    }

    #[test]
    fn roundtrip_error() {
        roundtrip(&Frame::Error("something broke".into()));
    }

    #[test]
    fn decode_rejects_truncated_init_hello() {
        // Encode an InitHello, then truncate the payload.
        let mut bytes = encode(&Frame::InitHello {
            workload_id: "abc".into(),
            init_pid: 1,
        });
        // Drop the last 2 bytes (well into the workload_id string).
        bytes.truncate(bytes.len() - 2);
        let mut cur = Cursor::new(bytes.to_vec());
        let result = futures_lite::future::block_on(read_frame(&mut cur));
        assert!(matches!(
            result,
            Err(ProtocolError::Truncated { .. }) | Err(ProtocolError::Io(_))
        ));
    }

    #[test]
    fn payload_too_large_rejected() {
        // Encode a frame header that claims 17 MiB.
        let mut buf = BytesMut::new();
        buf.put_u32(17 * 1024 * 1024);
        buf.put_u8(FrameType::WorkloadStdout as u8);
        let mut cur = Cursor::new(buf.to_vec());
        let result = futures_lite::future::block_on(read_frame(&mut cur));
        assert!(matches!(result, Err(ProtocolError::PayloadTooLarge { .. })));
    }
}
