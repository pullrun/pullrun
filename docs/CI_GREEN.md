# CI Green — Lessons Learned

Collection of hard-won lessons to keep `cargo clippy --workspace -- -D warnings`,
`cargo fmt --all --check`, and `cargo test --workspace` green on CI (Ubuntu 24.04,
stable Rust, `RUSTFLAGS: -Dwarnings`).

## Platform-conditional code (macOS vs Linux)

The repo is developed on macOS but CI runs on Linux. Every `#[cfg(target_os = "linux")]`
block has a macOS counterpart — and vice versa.

### Always gate platform-specific imports

```rust
// WRONG — compiles on macOS, fails on Linux (or vice versa)
use std::os::fd::AsRawFd;     // unused on macOS → dead_code warning
use std::os::unix::io::OwnedFd; // unused on macOS → dead_code warning

// RIGHT
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::io::OwnedFd;
```

**Check:** `rg '#\[cfg'` to find all conditional compilation blocks, then verify
every symbol used inside them has its import gated the same way.

### Same type, different sizes

```rust
// On Linux:  libc::TIOCSCTTY is c_ulong (u64), matches ioctl arg
// On macOS:  libc::TIOCSCTTY is u32, needs .into() → u64
libc::ioctl(0, libc::TIOCSCTTY, 0);              // fails on macOS
libc::ioctl(0, libc::TIOCSCTTY.into(), 0);        // fails clippy on Linux (useless_conversion)

// RIGHT
#[cfg(not(target_os = "linux"))]
libc::ioctl(0, libc::TIOCSCTTY.into(), 0);
#[cfg(target_os = "linux")]
libc::ioctl(0, libc::TIOCSCTTY, 0);
```

## Using config(`target_os = "linux"`)

**Check:** `rg '\[cfg'` to find all conditional compilation blocks, then verify every symbol used inside them has its import gated the same way.

### Review what you changed

Use `git diff --stat` before pushing to see if the scope is reasonable.

## Always run `cargo clippy --workspace -- -D warnings`

CI runs with `RUSTFLAGS: -Dwarnings`. A local `cargo build` or `cargo check`
will NOT catch clippy lints. Always run the exact CI command:

```bash
cargo clippy --workspace -- -D warnings
```

## Format before committing

Run `cargo fmt --all` before every commit. The CI check is:

```bash
cargo fmt --all --check
```

One missed format will fail the pipeline. Make it a habit.

## Test on the Linux server before pushing

Some issues only reproduce on Linux:
- `UnixStream::from_std()` panics with "Registering a blocking socket with the
  tokio runtime" (tokio ≥ 1.38). Fix: set `O_NONBLOCK` via `fcntl` before
  wrapping a raw fd.

```bash
ssh root@51.159.130.114
```

## Tokio's blocking socket guard

When creating a `tokio::net::UnixStream` from a raw fd (via `from_raw_fd`),
tokio ≥ 1.38 panics at runtime with:

> Registering a blocking socket with the tokio runtime is unsupported.

**Fix:** set the fd to non-blocking **before** wrapping it:

```rust
let duped = unsafe { libc::dup(fd) };
let flags = unsafe { libc::fcntl(duped, libc::F_GETFL) };
unsafe { libc::fcntl(duped, libc::F_SETFL, flags | libc::O_NONBLOCK) };
let std_stream = unsafe { StdUnixStream::from_raw_fd(duped) };
UnixStream::from_std(std_stream)
```

## Dead code is dead — unless it's platform-specific

Functions and fields that look unused on your machine might be used behind
`#[cfg(target_os = "linux")]` or in tests. Before adding `#[allow(dead_code)]`,
verify the item really has no callers across all platforms.

## C-string literals

Prefer `c"..."` over `b"...\0".as_ptr() as *const libc::c_char` (available
since Rust 1.77, MSRV is 1.78+).

```rust
// Old
libc::mkdir("/dev/pts\0".as_ptr() as *const libc::c_char, 0o755);
// New
libc::mkdir(c"/dev/pts".as_ptr(), 0o755);
```

The `c"..."` literal returns `&CStr`; `.as_ptr()` gives `*const c_char`
directly — no cast needed.

## Redundant closures

```rust
// Old
.map_err(|e| InitError::Io(e))
// New
.map_err(InitError::Io)
```

## Useless `.into()`

If `.into()` converts a type to itself, remove it. When the type differs
per-platform, gate with `#[cfg]` (see "Same type, different sizes" above).
