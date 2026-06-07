//! # build-initramfs
//!
//! Assemble a Nimbus microVM initramfs.
//!
//! ## What it produces
//!
//! A `cpio` archive (newc format) compressed with gzip,
//! containing:
//!
//! ```text
//! /init                  -> shell script that execs /sbin/nimbus-init
//! /sbin/nimbus-init      -> the static nimbus-init binary
//! /bin/busybox           -> busybox static binary (mount, sh, etc.)
//! /dev/console           -> device node (Linux 5.x+ allows this in cpio)
//! /dev/null              -> device node
//! /dev/tty               -> device node
//! /proc                  -> directory (mount target)
//! /sys                   -> directory (mount target)
//! /etc/resolv.conf       -> empty (DNS comes from the host via 9p)
//! ```
//!
//! ## Why busybox?
//!
//! The guest's nimbus-init binary is statically linked and
//! does not need busybox. But the kernel mounts `/proc` and
//! `/sys` BEFORE exec'ing `/init`, and the workload command
//! inside the VM may want to do basic things like
//! `cat /etc/resolv.conf` or `sh -c 'echo $PATH'`.
//!
//! Having busybox available as a backup shell + utilities
//! means the workload can rely on a basic POSIX environment
//! without us having to ship separate binaries for
//! `cat`, `sh`, `mount`, etc.
//!
//! ## Usage
//!
//! ```text
//! build-initramfs \
//!     --busybox /path/to/busybox-static \
//!     --nimbus-init /path/to/nimbus-init \
//!     --out /path/to/initramfs.cpio.gz
//! ```
//!
//! ## Wire format
//!
//! We use `cpio -o -H newc` (the SVR4 "new" portable format)
//! which all modern Linux kernels understand as an initramfs.
//! The archive is piped to gzip.
//!
//! We don't shell out to `cpio`; we generate the newc
//! format directly in Rust. This keeps the build
//! hermetic — no need for a host cpio binary.
//!
//! ## File mode bits
//!
//! In newc format, each entry is a single 110-byte header
//! ASCII line followed by the file data and 4-byte
//! alignment padding. We use mode 0755 for directories and
//! executables, 0644 for regular files, and the standard
//! device numbers for `/dev/console` etc. (5, 1) for
//! `/dev/console`, (1, 3) for `/dev/null`, (5, 0) for
//! `/dev/tty`.
//!
//! Inode numbers: we just count up from 1 per entry. The
//! kernel doesn't care about specific inode values.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clap::Parser;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(
    name = "build-initramfs",
    about = "Assemble a Nimbus microVM initramfs from busybox + nimbus-init",
    long_about = None,
)]
struct Args {
    /// Path to a busybox STATIC binary (e.g. from `brew install busybox`
    /// on macOS, or `apk add busybox-static` on Alpine).
    #[arg(long, value_name = "PATH")]
    busybox: PathBuf,

    /// Path to the nimbus-init STATIC binary. Build it with:
    ///   cargo build -p nimbus-init --target aarch64-unknown-linux-musl --release
    #[arg(long, value_name = "PATH")]
    nimbus_init: PathBuf,

    /// Output path for the initramfs (cpio.gz).
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Don't gzip the output (useful for debugging).
    #[arg(long)]
    no_gzip: bool,

    /// Path to a shell script to use as `/init` instead of
    /// the default. The default just execs /sbin/nimbus-init.
    #[arg(long, value_name = "PATH")]
    init_script: Option<PathBuf>,
}

const DEFAULT_INIT_SCRIPT: &str = "#!/bin/sh\n\
exec /sbin/nimbus-init\n";

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();

    match run(&args) {
        Ok(()) => {
            info!(out = %args.out.display(), "initramfs built");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "build failed");
            std::process::ExitCode::FAILURE
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum BuildError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("file not found: {0}")]
    NotFound(String),
    #[error("not a regular file: {0}")]
    NotRegular(String),
}

fn run(args: &Args) -> Result<(), BuildError> {
    // Validate inputs
    validate_input(&args.busybox, "busybox")?;
    validate_input(&args.nimbus_init, "nimbus-init")?;

    // Open the output. We wrap in a gzip::Encoder unless
    // --no-gzip is set.
    let out_file = File::create(&args.out)?;
    let out_file_size = args.out.display().to_string();
    let mut writer: Box<dyn Write> = if args.no_gzip {
        Box::new(BufWriter::new(out_file))
    } else {
        Box::new(flate2::GzBuilder::new()
            .filename("initramfs.cpio")
            .write(BufWriter::new(out_file), flate2::Compression::default()))
    };

    // Inode counter. The kernel doesn't care, but newc
    // requires a unique number per entry.
    let mut ino: u32 = 1;
    // File mode constants (POSIX)
    const S_IFREG: u32 = 0o100000;
    const S_IFDIR: u32 = 0o040000;
    const S_IFCHR: u32 = 0o020000;
    const MODE_DIR: u32 = 0o755;
    const MODE_FILE: u32 = 0o644;
    const MODE_EXEC: u32 = 0o755;
    // Device numbers for /dev nodes
    const DEV_CONSOLE: (u32, u32) = (5, 1); // /dev/console
    const DEV_NULL: (u32, u32) = (1, 3); // /dev/null
    const DEV_TTY: (u32, u32) = (5, 0); // /dev/tty

    // 1. Directories first.
    for path in &["/proc", "/sys", "/dev", "/bin", "/sbin", "/etc"] {
        write_newc_entry(
            &mut writer,
            path,
            ino,
            S_IFDIR | MODE_DIR,
            0,
            0,
            b"",
        )?;
        ino += 1;
    }

    // 2. Device nodes.
    write_newc_entry(
        &mut writer,
        "/dev/console",
        ino,
        S_IFCHR | MODE_FILE,
        DEV_CONSOLE.0,
        DEV_CONSOLE.1,
        b"",
    )?;
    ino += 1;
    write_newc_entry(
        &mut writer,
        "/dev/null",
        ino,
        S_IFCHR | MODE_FILE,
        DEV_NULL.0,
        DEV_NULL.1,
        b"",
    )?;
    ino += 1;
    write_newc_entry(
        &mut writer,
        "/dev/tty",
        ino,
        S_IFCHR | MODE_FILE,
        DEV_TTY.0,
        DEV_TTY.1,
        b"",
    )?;
    ino += 1;

    // 3. /etc/resolv.conf (empty; DNS comes from the host).
    write_newc_entry(
        &mut writer,
        "/etc/resolv.conf",
        ino,
        S_IFREG | MODE_FILE,
        0,
        0,
        b"",
    )?;
    ino += 1;

    // 4. /init script.
    let init_data: Vec<u8> = if let Some(path) = &args.init_script {
        std::fs::read(path)?
    } else {
        DEFAULT_INIT_SCRIPT.as_bytes().to_vec()
    };
    write_newc_entry(
        &mut writer,
        "/init",
        ino,
        S_IFREG | MODE_EXEC,
        0,
        0,
        &init_data,
    )?;
    ino += 1;

    // 5. /sbin/nimbus-init binary.
    let nimbus_init_data = std::fs::read(&args.nimbus_init)?;
    info!(bytes = nimbus_init_data.len(), "nimbus-init read");
    write_newc_entry(
        &mut writer,
        "/sbin/nimbus-init",
        ino,
        S_IFREG | MODE_EXEC,
        0,
        0,
        &nimbus_init_data,
    )?;
    ino += 1;

    // 6. /bin/busybox binary.
    let busybox_data = std::fs::read(&args.busybox)?;
    info!(bytes = busybox_data.len(), "busybox read");
    write_newc_entry(
        &mut writer,
        "/bin/busybox",
        ino,
        S_IFREG | MODE_EXEC,
        0,
        0,
        &busybox_data,
    )?;
    ino += 1;

    // 7. Symlinks for busybox applets (cat, sh, mount, etc.)
    //    so the workload can use them transparently.
    for app in &[
        "cat", "sh", "mount", "umount", "ls", "echo", "env", "true", "false",
        "mkdir", "rm", "ln", "cp", "mv", "ps", "sleep", "test", "uname",
    ] {
        write_newc_symlink(
            &mut writer,
            &format!("/bin/{app}"),
            ino,
            "/bin/busybox",
        )?;
        ino += 1;
    }

    // 8. Trailer ("empty" entry with name "TRAILER!!!")
    write_newc_trailer(&mut writer)?;

    // Flush the gzip encoder.
    drop(writer);

    // Report final size.
    let final_size = std::fs::metadata(&args.out)?.len();
    info!(
        out = %out_file_size,
        bytes = final_size,
        "initramfs written"
    );

    Ok(())
}

fn validate_input(path: &Path, label: &str) -> Result<(), BuildError> {
    let meta = std::fs::metadata(path)
        .map_err(|_| BuildError::NotFound(format!("{}: {}", label, path.display())))?;
    if !meta.is_file() {
        return Err(BuildError::NotRegular(format!(
            "{}: {}",
            label,
            path.display()
        )));
    }
    // Check it's executable.
    let perms = meta.permissions();
    if perms.mode() & 0o111 == 0 {
        eprintln!(
            "warning: {} ({}) is not executable; nimbus-init won't be able to run it",
            label,
            path.display()
        );
    }
    Ok(())
}

/// Write a single newc-format cpio entry to `w`.
///
/// `mode` is the full mode word (file type bits OR'd with
/// permission bits). `rdev_major`/`rdev_minor` are 0 for
/// regular files; for device nodes they are the device
/// numbers. `data` is the file contents (empty for
/// directories and device nodes).
fn write_newc_entry<W: Write>(
    w: &mut W,
    name: &str,
    ino: u32,
    mode: u32,
    rdev_major: u32,
    rdev_minor: u32,
    data: &[u8],
) -> std::io::Result<()> {
    // Header format (newc), 110 bytes:
    //   "070701"  (6 bytes: magic)
    //   ino       (8 bytes: hex)
    //   mode      (8 bytes: hex)
    //   uid       (8 bytes: hex)
    //   gid       (8 bytes: hex)
    //   nlink     (8 bytes: hex)
    //   mtime     (8 bytes: hex)
    //   filesize  (8 bytes: hex)
    //   devmajor  (8 bytes: hex)
    //   devminor  (8 bytes: hex)
    //   rdevmajor (8 bytes: hex)
    //   rdevminor (8 bytes: hex)
    //   namesize  (8 bytes: hex)
    //   check     (8 bytes: hex)
    // Total: 6 + 13*8 = 110 bytes
    let header = format!(
        "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        ino,
        mode,
        0u32, // uid (root)
        0u32, // gid (root)
        1u32, // nlink
        0u32, // mtime
        data.len() as u32,
        0u32, // devmajor
        0u32, // devminor
        rdev_major,
        rdev_minor,
        // +1 for the trailing NUL
        (name.len() + 1) as u32,
        0u32, // check
    );
    debug_assert_eq!(header.len(), 110);
    w.write_all(header.as_bytes())?;
    w.write_all(name.as_bytes())?;
    w.write_all(&[0])?; // NUL terminator
    // Pad to 4-byte alignment.
    let total = 110 + name.len() + 1;
    let pad = (4 - (total % 4)) % 4;
    for _ in 0..pad {
        w.write_all(&[0])?;
    }
    // Data + padding.
    w.write_all(data)?;
    let data_pad = (4 - (data.len() % 4)) % 4;
    for _ in 0..data_pad {
        w.write_all(&[0])?;
    }
    Ok(())
}

/// Write a symlink entry. `target` is the symlink target
/// (the kernel resolves the path at archive-extract time).
fn write_newc_symlink<W: Write>(
    w: &mut W,
    name: &str,
    ino: u32,
    target: &str,
) -> std::io::Result<()> {
    // Symlink: mode = S_IFLNK (0120000) | 0777
    const S_IFLNK: u32 = 0o120000;
    const MODE_LNK: u32 = 0o777;
    let data = target.as_bytes();
    write_newc_entry(
        w,
        name,
        ino,
        S_IFLNK | MODE_LNK,
        0,
        0,
        data,
    )
}

/// Write the newc "TRAILER!!!" entry that marks the end of
/// the archive.
fn write_newc_trailer<W: Write>(w: &mut W) -> std::io::Result<()> {
    // The trailer is a regular file entry named
    // "TRAILER!!!". The kernel recognizes this name and
    // stops reading.
    const S_IFREG: u32 = 0o100000;
    let name = "TRAILER!!!";
    let header = format!(
        "070701{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        0u32,                    // ino (kernel doesn't care)
        S_IFREG,                 // mode
        0u32,                    // uid
        0u32,                    // gid
        1u32,                    // nlink
        0u32,                    // mtime
        0u32,                    // filesize
        0u32, 0u32,              // devmajor, devminor
        0u32, 0u32,              // rdevmajor, rdevminor
        (name.len() + 1) as u32, // namesize
        0u32,                    // check
    );
    debug_assert_eq!(header.len(), 110);
    w.write_all(header.as_bytes())?;
    w.write_all(name.as_bytes())?;
    w.write_all(&[0])?;
    let total = 110 + name.len() + 1;
    let pad = (4 - (total % 4)) % 4;
    for _ in 0..pad {
        w.write_all(&[0])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newc_entry_format_is_110_bytes() {
        let mut buf = Vec::new();
        write_newc_entry(
            &mut buf,
            "/init",
            1,
            0o100755,
            0,
            0,
            b"#!/bin/sh\necho hello\n",
        )
        .unwrap();
        // First 110 bytes are the header.
        assert_eq!(&buf[..6], b"070701");
        // Name "/init" is 5 chars + 1 NUL = 6 bytes.
        // Header (110) + name (6) = 116, already aligned to 4.
        // Data "#!/bin/sh\necho hello\n" = 22 bytes.
        // 22 % 4 = 2, so 2 bytes of padding.
        // Total: 116 + 22 + 2 = 140.
        assert_eq!(buf.len(), 140);
    }

    #[test]
    fn newc_entry_with_padding() {
        let mut buf = Vec::new();
        write_newc_entry(
            &mut buf,
            "/bin/cat",
            2,
            0o100755,
            0,
            0,
            b"abc", // 3 bytes -> 1 byte of padding
        )
        .unwrap();
        // Header (110) + name (8 chars + 1 NUL = 9) = 119, pad to 4 = 120.
        // Data (3) + 1 byte pad = 4.
        // Total: 120 + 4 = 124.
        assert_eq!(buf.len(), 124);
    }

    #[test]
    fn newc_trailer_format() {
        let mut buf = Vec::new();
        write_newc_trailer(&mut buf).unwrap();
        let s = std::str::from_utf8(&buf[110..]).unwrap();
        assert!(s.starts_with("TRAILER!!!"));
    }
}
