// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! Seccomp profile construction for the OCI runtime spec.
//!
//! Pullrun ships a `default` allowlist profile (the syscalls needed by
//! typical busybox/alpine/glibc workloads), supports `unconfined` (no
//! seccomp), and `pullrun:<json>` for inline runc seccomp specs. When
//! an explicit `allowed_syscalls` list is present it overrides the
//! built-in allowlist.

/// The syscalls in the `default` profile. The list is the union of
/// what busybox sh, coreutils, glibc/musl startup, and the kernel's
/// basic IPC/process machinery need; it is intentionally conservative
/// so existing workloads keep working.
pub const DEFAULT_ALLOWED_SYSCALLS: &[&str] = &[
    "accept4",
    "access",
    "arch_prctl",
    "bind",
    "brk",
    "capget",
    "capset",
    "chdir",
    "clock_getres",
    "clock_gettime",
    "clock_nanosleep",
    "clone",
    "clone3",
    "close",
    "connect",
    "dup",
    "dup2",
    "dup3",
    "epoll_create1",
    "epoll_ctl",
    "epoll_pwait",
    "epoll_wait",
    "eventfd2",
    "execve",
    "execveat",
    "exit",
    "exit_group",
    "faccessat",
    "faccessat2",
    "fchdir",
    "fchmod",
    "fchmodat",
    "fchown",
    "fchownat",
    "fcntl",
    "fdatasync",
    "flock",
    "fork",
    "fsync",
    "ftruncate",
    "futex",
    "getcwd",
    "getdents64",
    "getegid",
    "geteuid",
    "getgid",
    "getgroups",
    "getpeername",
    "getpid",
    "getppid",
    "getrandom",
    "getrlimit",
    "getsockname",
    "getsockopt",
    "gettid",
    "gettimeofday",
    "getuid",
    "ioctl",
    "kill",
    "link",
    "linkat",
    "listen",
    "lseek",
    "lstat",
    "madvise",
    "memfd_create",
    "mkdir",
    "mkdirat",
    "mknod",
    "mknodat",
    "mmap",
    "mprotect",
    "mq_getsetattr",
    "mq_notify",
    "mq_open",
    "mq_timedreceive",
    "mq_timedsend",
    "mq_unlink",
    "mremap",
    "msync",
    "munmap",
    "nanosleep",
    "newfstatat",
    "open",
    "openat",
    "pause",
    "pipe",
    "pipe2",
    "poll",
    "ppoll",
    "prctl",
    "pread64",
    "prlimit64",
    "pselect6",
    "pwrite64",
    "read",
    "readlink",
    "readlinkat",
    "readv",
    "recvfrom",
    "recvmsg",
    "rename",
    "renameat",
    "renameat2",
    "rmdir",
    "rt_sigaction",
    "rt_sigpending",
    "rt_sigprocmask",
    "rt_sigqueueinfo",
    "rt_sigreturn",
    "rt_sigsuspend",
    "rt_sigtimedwait",
    "sched_getaffinity",
    "sched_getparam",
    "sched_getscheduler",
    "sched_setaffinity",
    "sched_setparam",
    "sched_setscheduler",
    "sched_yield",
    "select",
    "sendfile",
    "sendmsg",
    "sendto",
    "set_robust_list",
    "set_tid_address",
    "setgid",
    "setgroups",
    "sethostname",
    "setitimer",
    "setpgid",
    "setresgid",
    "setresuid",
    "setrlimit",
    "setsid",
    "setsockopt",
    "setuid",
    "shutdown",
    "sigaltstack",
    "socket",
    "socketpair",
    "stat",
    "statfs",
    "statx",
    "symlink",
    "symlinkat",
    "sysinfo",
    "tee",
    "tgkill",
    "time",
    "timer_create",
    "timer_delete",
    "timer_settime",
    "tkill",
    "truncate",
    "umask",
    "uname",
    "unlink",
    "unlinkat",
    "utimensat",
    "wait4",
    "waitid",
    "write",
    "writev",
];

/// Build the runc `linux.seccomp` value for a profile, or `None` when
/// no seccomp should be applied (`None` profile, `unconfined`, or an
/// invalid inline JSON profile).
///
/// Returns `Ok(None)` for profiles that mean "no seccomp".
/// Returns `Err` for a malformed `pullrun:<json>` inline profile so the
/// caller can fail closed instead of silently running unconfined.
pub fn build_seccomp(
    profile: Option<&str>,
    allowed_syscalls: &[String],
) -> Result<Option<serde_json::Value>, String> {
    let profile = match profile {
        None => return Ok(None),
        Some(p) => p,
    };

    if profile == "unconfined" {
        return Ok(None);
    }

    if profile == "default" {
        let syscalls: Vec<String> = if allowed_syscalls.is_empty() {
            DEFAULT_ALLOWED_SYSCALLS
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            allowed_syscalls.to_vec()
        };
        return Ok(Some(seccomp_allowlist(&syscalls)));
    }

    if let Some(json) = profile.strip_prefix("pullrun:") {
        let value: serde_json::Value = serde_json::from_str(json)
            .map_err(|e| format!("invalid inline seccomp profile JSON: {e}"))?;
        if !value.is_object() {
            return Err("inline seccomp profile must be a JSON object".to_string());
        }
        return Ok(Some(value));
    }

    Err(format!("unknown seccomp profile: {profile}"))
}

/// Build a default-action-ERRNO allowlist profile for the given syscalls.
fn seccomp_allowlist(syscalls: &[String]) -> serde_json::Value {
    serde_json::json!({
        "defaultAction": "SCMP_ACT_ERRNO",
        "architectures": ["SCMP_ARCH_X86_64", "SCMP_ARCH_X86", "SCMP_ARCH_AARCH64", "SCMP_ARCH_ARM"],
        "syscalls": [
            {
                "names": syscalls,
                "action": "SCMP_ACT_ALLOW"
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_none_and_unconfined_produce_no_seccomp() {
        assert!(build_seccomp(None, &[]).unwrap().is_none());
        assert!(build_seccomp(Some("unconfined"), &[]).unwrap().is_none());
    }

    #[test]
    fn test_default_profile_is_allowlist() {
        let v = build_seccomp(Some("default"), &[]).unwrap().unwrap();
        assert_eq!(v["defaultAction"], "SCMP_ACT_ERRNO");
        assert_eq!(v["syscalls"][0]["action"], "SCMP_ACT_ALLOW");
        let names = v["syscalls"][0]["names"].as_array().unwrap();
        assert!(!names.is_empty());
        assert!(names.contains(&serde_json::json!("execve")));
        assert!(names.contains(&serde_json::json!("write")));
    }

    #[test]
    fn test_allowed_syscalls_overrides_default() {
        let v = build_seccomp(Some("default"), &["read".to_string()])
            .unwrap()
            .unwrap();
        let names = v["syscalls"][0]["names"].as_array().unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "read");
    }

    #[test]
    fn test_inline_json_profile_passthrough() {
        let v = build_seccomp(
            Some(r#"pullrun:{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[]}"#),
            &[],
        )
        .unwrap()
        .unwrap();
        assert_eq!(v["defaultAction"], "SCMP_ACT_ALLOW");
    }

    #[test]
    fn test_invalid_inline_profile_errors() {
        assert!(build_seccomp(Some("pullrun:not-json"), &[]).is_err());
        assert!(build_seccomp(Some("pullrun:\"string\""), &[]).is_err());
        assert!(build_seccomp(Some("bogus"), &[]).is_err());
    }
}
