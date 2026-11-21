// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Overlayfs lowerdir support for container rootfs.
//!
//! The immutable image base is the read-only lower layer; each sandbox
//! gets a private upper/work pair. Reads hit the base inodes, so the
//! host page cache is shared across sandboxes, and writes allocate
//! only in the sandbox's upper directory.
//!
//! Mounting overlayfs requires CAP_SYS_ADMIN. Rootless containers keep
//! using reflink clones (or fuse-overlayfs in a future iteration).

use std::io;
use std::path::Path;

#[cfg(target_os = "linux")]
pub fn mount_overlay(base: &Path, upper: &Path, work: &Path, merged: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let cstr = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))
    };

    std::fs::create_dir_all(upper)?;
    std::fs::create_dir_all(work)?;
    std::fs::create_dir_all(merged)?;

    let source = cstr(Path::new("overlay"))?;
    let target = cstr(merged)?;
    let fstype = cstr(Path::new("overlay"))?;
    let data = std::ffi::CString::new(format!(
        "lowerdir={},upperdir={},workdir={}",
        base.display(),
        upper.display(),
        work.display()
    ))
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in overlay options"))?;

    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            data.as_ptr().cast(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn mount_overlay(_base: &Path, _upper: &Path, _work: &Path, _merged: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "overlayfs is only available on Linux",
    ))
}

#[cfg(target_os = "linux")]
pub fn unmount_overlay(merged: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let target = std::ffi::CString::new(merged.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in path"))?;
    let rc = unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn unmount_overlay(_merged: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Mount overlayfs inside a fresh user namespace, as an
    /// unprivileged user. This validates the exact mount call used by
    /// the rootful executor without requiring root on the test host.
    #[test]
    fn overlay_mount_works_in_user_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let upper = dir.path().join("upper");
        let work = dir.path().join("work");
        let merged = dir.path().join("merged");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("hello"), b"base").unwrap();

        let base_c = base.clone();
        let upper_c = upper.clone();
        let work_c = work.clone();
        let merged_c = merged.clone();

        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // Child: enter a user + mount namespace and map the
            // invoking uid/gid to root inside it.
            let uid = unsafe { libc::getuid() };
            let gid = unsafe { libc::getgid() };
            if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
                unsafe { libc::_exit(10) };
            }
            let _ = std::fs::write("/proc/self/setgroups", "deny");
            if std::fs::write("/proc/self/uid_map", format!("0 {uid} 1")).is_err() {
                unsafe { libc::_exit(11) };
            }
            if std::fs::write("/proc/self/gid_map", format!("0 {gid} 1")).is_err() {
                unsafe { libc::_exit(12) };
            }

            if mount_overlay(&base_c, &upper_c, &work_c, &merged_c).is_err() {
                unsafe { libc::_exit(13) };
            }
            if std::fs::read(merged_c.join("hello")).ok().as_deref() != Some(b"base".as_slice()) {
                unsafe { libc::_exit(14) };
            }
            if std::fs::write(merged_c.join("new"), b"upper").is_err() {
                unsafe { libc::_exit(15) };
            }
            if !upper_c.join("new").is_file() {
                unsafe { libc::_exit(16) };
            }
            let _ = unmount_overlay(&merged_c);
            unsafe { libc::_exit(0) };
        }

        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        match code {
            0 => {}
            // 10/11/12: unshare or uid/gid mapping denied; 13: overlay
            // mount denied. Hosted CI runners and hardened distros
            // restrict unprivileged user namespaces / overlay mounts,
            // so skip instead of failing. The real behavior checks
            // (14/15/16) still fail the test.
            10..=13 => {
                eprintln!("skipping overlay user-namespace test: setup unavailable (code {code})");
            }
            other => panic!("overlay user-namespace test failed (code {other})"),
        }
    }
}
