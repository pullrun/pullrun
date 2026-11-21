// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Copy-on-write helpers for materialized image bases.
//!
//! A base tree is built once per image digest and then cloned per
//! sandbox. On filesystems with reflink support (APFS, btrfs, XFS) the
//! clone shares physical blocks and writes allocate new ones; on ext4
//! it falls back to a regular copy.
//!
//! Hardlinks are never used: a sandbox write must never mutate the
//! shared base.

#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CowMode {
    /// APFS `clonefile(2)`.
    Apfs,
    /// Linux `FICLONE` ioctl (btrfs/XFS with reflink).
    Ficlonable,
    /// No reflink support; copies are full copies.
    Copy,
}

impl CowMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Apfs => "reflink (apfs)",
            Self::Ficlonable => "reflink (ficlone)",
            Self::Copy => "copy",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CloneStats {
    pub cloned: u64,
    pub copied: u64,
    pub bytes: u64,
}

/// Probe whether `dir` supports reflinks.
pub fn probe(dir: &Path) -> CowMode {
    let _ = fs::create_dir_all(dir);
    let src = dir.join(".cow-probe-src");
    let dst = dir.join(".cow-probe-dst");
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    if fs::write(&src, b"pullrun cow probe").is_err() {
        return CowMode::Copy;
    }
    let mode = match try_clone_file(&src, &dst) {
        Ok(true) => {
            #[cfg(target_os = "macos")]
            {
                CowMode::Apfs
            }
            #[cfg(target_os = "linux")]
            {
                CowMode::Ficlonable
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            {
                CowMode::Copy
            }
        }
        _ => CowMode::Copy,
    };
    let _ = fs::remove_file(&src);
    let _ = fs::remove_file(&dst);
    mode
}

/// Clone a single file, returning `true` when a reflink was created.
pub fn try_clone_file(src: &Path, dst: &Path) -> io::Result<bool> {
    let _ = fs::remove_file(dst);

    #[cfg(target_os = "macos")]
    {
        let c_src = path_cstr(src)?;
        let c_dst = path_cstr(dst)?;
        let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        if rc == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        debug!(src = %src.display(), dst = %dst.display(), error = %err, "clonefile failed; falling back to copy");
    }

    #[cfg(target_os = "linux")]
    {
        let src_file = File::open(src)?;
        // O_CREAT|O_WRONLY without O_EXCL: destination was removed above.
        let dst_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(dst)?;
        const FICLONE: libc::c_ulong = 0x4004_9409;
        let rc = unsafe {
            libc::ioctl(
                dst_file.as_raw_fd(),
                FICLONE as _,
                src_file.as_raw_fd() as libc::c_int,
            )
        };
        if rc == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        debug!(src = %src.display(), dst = %dst.display(), error = %err, "FICLONE failed; falling back to copy");
    }

    // Fallback: full copy.
    fs::copy(src, dst)?;
    Ok(false)
}

#[cfg(target_os = "macos")]
fn path_cstr(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
}

/// Copy a directory tree, reflinking files where possible.
pub fn clone_tree(src: &Path, dst: &Path) -> io::Result<CloneStats> {
    let mut stats = CloneStats::default();

    // APFS can clone an entire directory in one call, preserving
    // metadata, symlinks, and hardlinks.
    #[cfg(target_os = "macos")]
    {
        if dst.exists() {
            fs::remove_dir_all(dst)?;
        }
        let c_src = path_cstr(src)?;
        let c_dst = path_cstr(dst)?;
        let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        if rc == 0 {
            let (files, bytes) = dir_stats(src)?;
            stats.cloned = files;
            stats.bytes = bytes;
            return Ok(stats);
        }
        debug!(src = %src.display(), dst = %dst.display(), error = %io::Error::last_os_error(), "directory clonefile failed; walking");
        if dst.exists() {
            fs::remove_dir_all(dst)?;
        }
    }

    clone_dir_walk(src, dst, &mut stats)?;
    Ok(stats)
}

#[cfg(target_os = "macos")]
fn dir_stats(root: &Path) -> io::Result<(u64, u64)> {
    let mut files = 0;
    let mut bytes = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                files += 1;
                bytes += meta.len();
            }
        }
    }
    Ok((files, bytes))
}

fn clone_dir_walk(src: &Path, dst: &Path, stats: &mut CloneStats) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    let meta = fs::metadata(src)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(meta.permissions().mode()))?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            clone_dir_walk(&path, &target, stats)?;
        } else if file_type.is_symlink() {
            let link = fs::read_link(&path)?;
            std::os::unix::fs::symlink(&link, &target)?;
        } else if file_type.is_file() {
            clone_one_file(&path, &target, stats)?;
        } else {
            warn!(path = %path.display(), "skipping special file in base clone");
        }
    }
    Ok(())
}

fn clone_one_file(src: &Path, dst: &Path, stats: &mut CloneStats) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    let cloned = try_clone_file(src, dst)?;
    let _ = fs::set_permissions(dst, fs::Permissions::from_mode(meta.permissions().mode()));
    // Best-effort mtime preservation.
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    let _ = filetime::set_file_mtime(dst, mtime);
    if !cloned {
        // try_clone_file already copied; nothing else to do.
    }
    copy_xattrs(src, dst);
    stats.bytes += meta.len();
    if cloned {
        stats.cloned += 1;
    } else {
        stats.copied += 1;
    }
    Ok(())
}

#[cfg(unix)]
fn copy_xattrs(src: &Path, dst: &Path) {
    if let Ok(names) = xattr::list(src) {
        for name in names {
            if let Ok(Some(value)) = xattr::get(src, &name) {
                let _ = xattr::set(dst, &name, &value);
            }
        }
    }
}

#[cfg(not(unix))]
fn copy_xattrs(_src: &Path, _dst: &Path) {}

/// Recursively delete an empty-able path (rootfs clones).
pub fn remove_tree(path: &Path) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    fs::remove_dir_all(path)
}

/// Copy a single file's contents when a clone is unavailable; used by
/// callers that do not want to go through `clone_tree`.
pub fn copy_file(src: &Path, dst: &Path) -> io::Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = try_clone_file(src, dst)?;
    Ok(())
}

/// Read a small file to a string (used by capability reporting).
pub fn read_to_string(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut out = String::new();
    file.read_to_string(&mut out)?;
    Ok(out)
}

/// Write a file atomically via a temp file in the same directory.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent: PathBuf = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    fs::create_dir_all(&parent)?;
    let tmp = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<CowMode>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_tree_copies_content_and_types() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("etc")).unwrap();
        std::fs::write(src.path().join("etc/passwd"), b"root:x:0:0\n").unwrap();
        std::os::unix::fs::symlink("etc/passwd", src.path().join("link")).unwrap();
        std::fs::set_permissions(
            src.path().join("etc/passwd"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        let dst = tempfile::tempdir().unwrap();
        let target = dst.path().join("clone");
        let stats = clone_tree(src.path(), &target).unwrap();
        assert!(stats.cloned + stats.copied >= 1);
        assert_eq!(
            std::fs::read(target.join("etc/passwd")).unwrap(),
            b"root:x:0:0\n"
        );
        assert!(std::fs::symlink_metadata(target.join("link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::metadata(target.join("etc/passwd"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn probe_returns_a_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mode = probe(dir.path());
        assert!(matches!(
            mode,
            CowMode::Apfs | CowMode::Ficlonable | CowMode::Copy
        ));
    }
}
