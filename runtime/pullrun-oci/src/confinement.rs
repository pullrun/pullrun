// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Root-confined, descriptor-relative filesystem operations for OCI
//! extraction.
//!
//! OCI layer tar entries are attacker-controlled. Every path we honour
//! therefore has to be resolved *inside* the rootfs: no absolute paths,
//! no `..`, and no symlink traversal that escapes the root. The rules
//! implemented here:
//!
//! - Paths are parsed into [`RelPath`]: components must be non-empty,
//!   must not be `.` or `..`, and must not start with `/`.
//! - Directory resolution is a component walk driven by `openat` with
//!   `O_NOFOLLOW`. Symlinks are followed *manually*: relative targets
//!   continue from the containing directory, absolute targets restart
//!   at the root (chroot semantics). `..` pops a directory but can
//!   never pop past the root. Resolution is bounded to 40 links.
//! - Final operations (`unlinkat`, `openat(O_CREAT|O_EXCL|O_NOFOLLOW)`,
//!   `symlinkat`, `mkdirat`) are performed relative to the resolved
//!   parent descriptor, never against a joined host path string.
//!
//! This makes symlink-based escapes (e.g. `escape -> /etc` followed by
//! `escape/.wh.passwd`) resolve inside the rootfs, where the target
//! simply does not exist.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A validation failure for an untrusted path.
#[derive(Debug, thiserror::Error)]
pub enum ConfineError {
    #[error("invalid path {0:?}: {1}")]
    InvalidPath(String, &'static str),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("path escapes the root: {0:?}")]
    Escape(String),
}

pub type Result<T> = std::result::Result<T, ConfineError>;

/// A path relative to the rootfs, guaranteed free of `.`, `..`,
/// empty components, and any absolute prefix.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RelPath {
    parts: Vec<OsString>,
}

impl RelPath {
    pub fn root() -> Self {
        Self::default()
    }

    pub fn parse(raw: &str) -> Result<Self> {
        if raw.starts_with('/') {
            return Err(ConfineError::InvalidPath(
                raw.to_string(),
                "absolute paths are not allowed",
            ));
        }
        let mut parts = Vec::new();
        for component in raw.split('/') {
            match component {
                "" | "." => continue,
                ".." => {
                    return Err(ConfineError::InvalidPath(
                        raw.to_string(),
                        "parent components are not allowed",
                    ))
                }
                other => parts.push(OsString::from(other)),
            }
        }
        Ok(Self { parts })
    }

    pub fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    pub fn push(&mut self, component: &str) -> Result<()> {
        if component.is_empty() || component == "." || component == ".." || component.contains('/') {
            return Err(ConfineError::InvalidPath(
                component.to_string(),
                "component must be a single normal path element",
            ));
        }
        self.parts.push(OsString::from(component));
        Ok(())
    }

    pub fn join(&self, other: &RelPath) -> RelPath {
        let mut parts = self.parts.clone();
        parts.extend(other.parts.iter().cloned());
        RelPath { parts }
    }

    pub fn with(&self, component: &str) -> Result<RelPath> {
        let mut out = self.clone();
        out.push(component)?;
        Ok(out)
    }

    pub fn display(&self) -> String {
        self.parts
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    }
}

/// An open directory descriptor used as the confinement boundary.
pub struct RootDir {
    fd: OwnedFd,
    path: PathBuf,
}

impl RootDir {
    /// Open (or create) the directory that will contain the extracted
    /// filesystem. The descriptor is the only way to reach children.
    pub fn open(path: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(path)?;
        let c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in root path"))?;
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn dup_root(&self) -> io::Result<OwnedFd> {
        let fd = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Resolve `rel` to a directory descriptor, optionally creating
    /// missing directories. Never escapes the root.
    pub fn open_dir(&self, rel: &RelPath, create: bool) -> io::Result<OwnedFd> {
        let mut stack: Vec<OwnedFd> = vec![self.dup_root()?];
        let mut queue: VecDeque<OsString> = rel.parts.iter().cloned().collect();
        let mut links = 0usize;

        while let Some(component) = queue.pop_front() {
            let bytes = component.as_bytes();
            if bytes == b"." {
                continue;
            }
            if bytes == b".." {
                if stack.len() > 1 {
                    stack.pop();
                }
                continue;
            }
            let name = CString::new(bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "NUL in path component")
            })?;
            let current = stack.last().expect("stack always has the root");

            match openat_dir(current.as_raw_fd(), &name) {
                Ok(fd) => stack.push(fd),
                Err(err)
                    if create
                        && matches!(
                            err.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                {
                    mkdirat(current.as_raw_fd(), &name, 0o755)?;
                    stack.push(openat_dir(current.as_raw_fd(), &name)?);
                }
                Err(err)
                    if matches!(
                        err.raw_os_error(),
                        Some(libc::ENOTDIR) | Some(libc::ELOOP)
                    ) =>
                {
                    // The component exists but is a symlink (or a
                    // non-directory). Follow it manually, staying inside
                    // the root.
                    links += 1;
                    if links > 40 {
                        return Err(io::Error::other("too many levels of symbolic links"));
                    }
                    let target = readlinkat(current.as_raw_fd(), &name)?;
                    let target_path = Path::new(OsStr::from_bytes(target.as_bytes()));
                    let mut target_parts: VecDeque<OsString> = target_path
                        .components()
                        .filter_map(|c| match c {
                            std::path::Component::Normal(part) => Some(part.to_os_string()),
                            std::path::Component::ParentDir => Some(OsString::from("..")),
                            std::path::Component::CurDir => None,
                            // Absolute targets are re-rooted (chroot
                            // semantics): drop the leading `/`.
                            std::path::Component::RootDir
                            | std::path::Component::Prefix(_) => None,
                        })
                        .collect();
                    if target_path.is_absolute() {
                        stack.truncate(1);
                    }
                    if target_parts.is_empty() {
                        return Err(io::Error::other("symlink target is empty"));
                    }
                    target_parts.append(&mut queue);
                    queue = target_parts;
                }
                Err(err) => return Err(err),
            }
        }

        Ok(stack.pop().expect("stack always contains the root"))
    }

    fn parent_of(&self, rel: &RelPath, create: bool) -> io::Result<(OwnedFd, CString)> {
        let Some((name, parents)) = rel.parts.split_last() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path has no final component",
            ));
        };
        let parent_rel = RelPath {
            parts: parents.to_vec(),
        };
        let parent = self.open_dir(&parent_rel, create)?;
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in name"))?;
        Ok((parent, name))
    }

    pub fn ensure_dir(&self, rel: &RelPath) -> io::Result<()> {
        let _ = self.open_dir(rel, true)?;
        Ok(())
    }

    /// Write a regular file, replacing whatever is currently at that
    /// path (including a symlink, which is unlinked rather than
    /// followed).
    pub fn write_file(&self, rel: &RelPath, data: &[u8], mode: u32) -> io::Result<()> {
        let (parent, name) = self.parent_of(rel, true)?;
        unlink_if_exists(parent.as_raw_fd(), &name)?;
        let fd = openat_file_create(parent.as_raw_fd(), &name, mode)?;
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(data)?;
        Ok(())
    }

    /// Copy a host file into the rootfs, streaming (no full-file buffer).
    pub fn copy_file_from(&self, rel: &RelPath, src: &Path, mode: u32) -> io::Result<()> {
        let (parent, name) = self.parent_of(rel, true)?;
        unlink_if_exists(parent.as_raw_fd(), &name)?;
        let fd = openat_file_create(parent.as_raw_fd(), &name, mode)?;
        let mut dst = unsafe { File::from_raw_fd(fd) };
        let mut src = File::open(src)?;
        io::copy(&mut src, &mut dst)?;
        Ok(())
    }

    /// Create a symlink, replacing any existing entry at that path.
    pub fn symlink(&self, rel: &RelPath, target: &OsStr) -> io::Result<()> {
        let (parent, name) = self.parent_of(rel, true)?;
        unlink_if_exists(parent.as_raw_fd(), &name)?;
        let target = CString::new(target.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in symlink target"))?;
        let rc = unsafe { libc::symlinkat(target.as_ptr(), parent.as_raw_fd(), name.as_ptr()) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Remove a file, symlink, or directory tree at `rel`. Missing
    /// paths are a no-op (whiteout semantics).
    pub fn remove_path(&self, rel: &RelPath) -> io::Result<()> {
        if rel.is_root() {
            return self.clear_dir(&RelPath::root());
        }
        let (parent, name) = match self.parent_of(rel, false) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        remove_entry_at(parent.as_raw_fd(), &name)
    }

    /// Empty a directory without removing the directory itself.
    pub fn clear_dir(&self, rel: &RelPath) -> io::Result<()> {
        let fd = match self.open_dir(rel, false) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        clear_dir_fd(fd.as_raw_fd())
    }

    fn open_target(&self, rel: &RelPath) -> io::Result<Option<OwnedFd>> {
        let (parent, name) = self.parent_of(rel, false)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ELOOP) {
                // Symlink: attributes are not applied to the link itself.
                return Ok(None);
            }
            return Err(err);
        }
        Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub fn set_mode(&self, rel: &RelPath, mode: u32) -> io::Result<()> {
        if let Some(fd) = self.open_target(rel)? {
            if unsafe { libc::fchmod(fd.as_raw_fd(), mode as libc::mode_t) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn set_owner(&self, rel: &RelPath, uid: u32, gid: u32) -> io::Result<()> {
        if let Some(fd) = self.open_target(rel)? {
            if unsafe { libc::fchown(fd.as_raw_fd(), uid, gid) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn set_mtime(&self, rel: &RelPath, mtime: i64) -> io::Result<()> {
        if mtime < 0 {
            return Ok(());
        }
        if let Some(fd) = self.open_target(rel)? {
            let times = [
                libc::timespec {
                    tv_sec: mtime as libc::time_t,
                    tv_nsec: 0,
                },
                libc::timespec {
                    tv_sec: mtime as libc::time_t,
                    tv_nsec: 0,
                },
            ];
            if unsafe { libc::futimens(fd.as_raw_fd(), times.as_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub fn set_xattr(&self, rel: &RelPath, name: &[u8], value: &[u8]) -> io::Result<()> {
        let Some(fd) = self.open_target(rel)? else {
            return Ok(());
        };
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in xattr name"))?;
        #[cfg(target_os = "linux")]
        let rc = unsafe {
            libc::fsetxattr(
                fd.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        #[cfg(target_os = "macos")]
        let rc = unsafe {
            libc::fsetxattr(
                fd.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let rc = 0;
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn openat_dir(dirfd: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn mkdirat(dirfd: RawFd, name: &CStr, mode: u32) -> io::Result<()> {
    if unsafe { libc::mkdirat(dirfd, name.as_ptr(), mode as libc::mode_t) } < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EEXIST) {
            return Err(err);
        }
    }
    Ok(())
}

fn openat_file_create(dirfd: RawFd, name: &CStr, mode: u32) -> io::Result<RawFd> {
    let fd = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn readlinkat(dirfd: RawFd, name: &CStr) -> io::Result<CString> {
    let mut buf = vec![0u8; libc::PATH_MAX as usize + 1];
    let n = unsafe {
        libc::readlinkat(
            dirfd,
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len() - 1,
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    CString::new(buf).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in link"))
}

fn fstatat(dirfd: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dirfd,
            name.as_ptr(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat)
}

fn is_dir(stat: &libc::stat) -> bool {
    (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR
}

fn unlink_if_exists(dirfd: RawFd, name: &CStr) -> io::Result<()> {
    match fstatat(dirfd, name) {
        Ok(stat) => {
            if is_dir(&stat) {
                remove_entry_at(dirfd, name)
            } else {
                if unsafe { libc::unlinkat(dirfd, name.as_ptr(), 0) } < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn remove_entry_at(dirfd: RawFd, name: &CStr) -> io::Result<()> {
    let stat = match fstatat(dirfd, name) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if is_dir(&stat) {
        let child = openat_dir(dirfd, name)?;
        clear_dir_fd(child.as_raw_fd())?;
        if unsafe { libc::unlinkat(dirfd, name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
            return Err(io::Error::last_os_error());
        }
    } else if unsafe { libc::unlinkat(dirfd, name.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn clear_dir_fd(dirfd: RawFd) -> io::Result<()> {
    let dup = unsafe { libc::fcntl(dirfd, libc::F_DUPFD_CLOEXEC, 3) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    let dir = unsafe { libc::fdopendir(dup) };
    if dir.is_null() {
        unsafe { libc::close(dup) };
        return Err(io::Error::last_os_error());
    }

    // Collect names first: recursive syscalls can invalidate the
    // readdir buffer.
    let mut names: Vec<CString> = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if let Ok(c) = CString::new(bytes) {
            names.push(c);
        }
    }
    unsafe { libc::closedir(dir) };

    for name in names {
        remove_entry_at(dirfd, &name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_absolute_and_parent_paths() {
        assert!(RelPath::parse("/etc/passwd").is_err());
        assert!(RelPath::parse("../escape").is_err());
        assert!(RelPath::parse("a/../../b").is_err());
        assert!(RelPath::parse("a/./b").is_ok());
    }

    #[test]
    fn writes_stay_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = RootDir::open(tmp.path()).unwrap();
        let rel = RelPath::parse("etc/passwd").unwrap();
        root.write_file(&rel, b"root:x:0:0\n", 0o644).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join("etc/passwd")).unwrap(),
            b"root:x:0:0\n"
        );
    }

    #[test]
    fn symlink_escape_is_resolved_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        std::fs::write(&sentinel, b"keep").unwrap();

        let root = RootDir::open(tmp.path()).unwrap();
        let link = RelPath::parse("escape").unwrap();
        root.symlink(&link, outside.path().as_os_str()).unwrap();

        // Absolute symlink targets are re-rooted: this resolves to
        // <root>/... and does not exist, so nothing is deleted.
        let target = RelPath::parse("escape/sentinel").unwrap();
        root.remove_path(&target).unwrap();
        assert!(sentinel.exists());
    }

    #[test]
    fn opaque_clears_children_not_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = RootDir::open(tmp.path()).unwrap();
        root.write_file(&RelPath::parse("a").unwrap(), b"a", 0o644)
            .unwrap();
        root.write_file(&RelPath::parse("d/b").unwrap(), b"b", 0o644)
            .unwrap();
        root.clear_dir(&RelPath::root()).unwrap();
        assert!(!tmp.path().join("a").exists());
        assert!(!tmp.path().join("d").exists());
        assert!(tmp.path().exists());
    }
}
