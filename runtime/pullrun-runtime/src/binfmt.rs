#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use tracing::{debug, info};
use tracing::warn;

/// Register a binfmt_misc handler for the target architecture so
/// runc can transparently execute cross-arch binaries via QEMU.
///
/// This is a no-op (with a warning) on non-Linux platforms.
pub fn ensure_binfmt_for_arch(target_arch: &str) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        warn!("cross-arch execution is only supported on Linux (target={target_arch})");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        if !Path::new("/proc/sys/fs/binfmt_misc").exists() {
            return Err(
                "binfmt_misc not available; mount with: mount binfmt_misc -t binfmt_misc /proc/sys/fs/binfmt_misc"
                    .to_string(),
            );
        }

        let (handler_name, qemu_binary) = match target_arch {
            "arm64" | "aarch64" => ("qemu-aarch64", "/usr/bin/qemu-aarch64-static"),
            "arm" | "armv7" | "armv7l" => ("qemu-arm", "/usr/bin/qemu-arm-static"),
            "amd64" | "x86_64" => ("qemu-x86_64", "/usr/bin/qemu-x86_64-static"),
            "ppc64le" | "powerpc64" => ("qemu-ppc64le", "/usr/bin/qemu-ppc64le-static"),
            "s390x" => ("qemu-s390x", "/usr/bin/qemu-s390x-static"),
            "riscv64" => ("qemu-riscv64", "/usr/bin/qemu-riscv64-static"),
            _ => {
                debug!("no known binfmt handler for arch {target_arch}");
                return Ok(());
            }
        };

        let handler_path = Path::new("/proc/sys/fs/binfmt_misc").join(handler_name);
        if handler_path.exists() {
            debug!(%target_arch, "binfmt handler already registered");
            return Ok(());
        }

        if !Path::new(qemu_binary).exists() {
            warn!(
                "qemu-user-static binary not found at {qemu_binary}; \
                 install with: apt install qemu-user-static  (or equivalent)"
            );
            return Ok(());
        }

        let magic = match target_arch {
            "arm64" | "aarch64" => "\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\xb7\\x00",
            "arm" | "armv7" | "armv7l" => "\\x7fELF\\x01\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x28\\x00",
            "amd64" | "x86_64" => "\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x3e\\x00",
            "ppc64le" => "\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x15\\x00",
            "s390x" => "\\x7fELF\\x02\\x02\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x16\\x00",
            "riscv64" => "\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\xf3\\x00",
            _ => return Ok(()),
        };

        let mask = "\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff";

        let register_line = format!(
            ":{name}:M:{offset}:{magic}:{mask}:{interpreter}:F",
            name = handler_name,
            offset = 0,
            magic = magic,
            mask = mask,
            interpreter = qemu_binary,
        );

        match std::fs::write(
            Path::new("/proc/sys/fs/binfmt_misc").join("register"),
            register_line.as_bytes(),
        ) {
            Ok(_) => {
                info!(%target_arch, "registered binfmt handler for cross-arch execution");
                Ok(())
            }
            Err(e) => {
                warn!(
                    "failed to register binfmt handler for {target_arch}: {e}; \
                     try: docker run --privileged --rm tonistiigi/binfmt --install all"
                );
                Ok(())
            }
        }
    }
}

/// Register binfmt handlers for common architectures at daemon
/// startup. This makes cross-arch container execution work without
/// any manual setup step.
pub fn register_common_binfmts() {
    let common_archs = &["arm64", "arm", "ppc64le", "s390x", "riscv64"];
    for arch in common_archs {
        if let Err(e) = ensure_binfmt_for_arch(arch) {
            warn!("failed to register binfmt for {arch} at startup: {e}");
        }
    }
}
