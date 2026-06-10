//! # nimbus-init
//!
//! Guest-side init daemon for Nimbus microVMs.
//!
//! ## Lifecycle
//!
//! 1. The kernel boots, mounts the rootfs (9p/virtiofs), and
//!    `exec`s `/sbin/nimbus-init` as PID 1.
//! 2. `nimbus-init` opens a vsock connection to the host on
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
//! in `nimbusctl workload inspect`.
//!
//! ## Why a separate crate
//!
//! `nimbus-init` is statically linked into the initramfs. It
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
pub use nimbus_vsock::DEFAULT_VSOCK_PORT as DEFAULT_HOST_VSOCK_PORT;

/// Library entry point: run the init daemon.
///
/// This is a no-op when called from a non-init process. The
/// real entry point is `main()` (in `main.rs`) which calls
/// this and converts the `Result` into an `ExitCode`.
///
/// Public so tests and embedders can call it directly.
pub async fn run() -> Result<(), InitError> {
    info!("nimbus-init starting");

    let workload_id = std::env::var("NIMBUS_WORKLOAD_ID")
        .unwrap_or_else(|_| "(unset)".to_string());
    let init_pid = std::process::id();

    // 1. Connect to the host over vsock.
    let mut client = VsockClient::connect(DEFAULT_HOST_VSOCK_PORT)
        .await
        .map_err(InitError::VsockConnect)?;

    // 2. Send the init hello.
    client
        .send_frame(nimbus_vsock::Frame::InitHello {
            workload_id: workload_id.clone(),
            init_pid,
        })
        .await
        .map_err(InitError::VsockSend)?;
    info!(workload_id, init_pid, "sent init hello");

    // 3. Receive the workload spec (or error).
    let spec = match client.recv_frame().await.map_err(InitError::VsockRecv)? {
        nimbus_vsock::Frame::WorkloadSpec {
            command,
            env,
            working_dir,
        } => {
            info!(?command, working_dir = %working_dir, env_count = env.len(), "got workload spec");
            Workload { command, env, working_dir }
        }
        nimbus_vsock::Frame::Error(msg) => {
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
        .send_frame(nimbus_vsock::Frame::WorkloadExit {
            exit_code: exit.code,
            signal: exit.signal,
        })
        .await
        .map_err(InitError::VsockSend)?;

    info!("nimbus-init exiting cleanly");
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
}

#[derive(Debug, Default, Clone, Copy)]
pub struct WorkloadExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl Workload {
    /// Spawn the workload command, wire up stdio to the vsock
    /// stream, and return when the workload exits.
    async fn run(
        self,
        client: &mut VsockClient,
    ) -> Result<WorkloadExit, InitError> {
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

        let mut stdin = child.stdin.take().ok_or_else(|| {
            InitError::Exec("child stdin not captured".into())
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            InitError::Exec("child stdout not captured".into())
        })?;
        let mut stderr = child.stderr.take().ok_or_else(|| {
            InitError::Exec("child stderr not captured".into())
        })?;

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
                            .send_frame(nimbus_vsock::Frame::WorkloadStdout(
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
                            .send_frame(nimbus_vsock::Frame::WorkloadStderr(
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
                    nimbus_vsock::Frame::WorkloadStdin(data) => {
                        if stdin.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    nimbus_vsock::Frame::StdinEof => {
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
        let status = child.wait().await.map_err(|e| {
            InitError::Exec(format!("waitpid: {e}"))
        })?;

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
