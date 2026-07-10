// Copyright 2026 Mohammed Boukaba.
// SPDX-License-Identifier: Apache-2.0

//! OCI image-based kernel staging for the Apple Virtualization pool.
//!
//! This module bridges the gap between the OCI artifact pipeline
//! (`pullrun-oci` + `MmapStore`) and the Apple Virtualization
//! framework's expectation of a host filesystem path for the
//! kernel and initramfs.
//!
//! # Why this module exists
//!
//! `VZLinuxBootLoader::initWithKernelURL:` takes an `NSURL`
//! pointing at a host file. The kernel *content*, however, is
//! just another OCI artifact in pullrun's content-addressed
//! store. The right design is: pull the kernel image via the
//! same pipeline that pulls every other image, then materialize
//! just the two files we need (`/boot/vmlinux` and
//! `/boot/initramfs.cpio.gz`) into a temp directory and hand
//! the host paths to the framework.
//!
//! # Kernel image layout
//!
//! A pullrun kernel image is a regular OCI image with a
//! well-known shape. The layer tarball(s) contain:
//!
//! ```text
//! /boot/vmlinux
//! /boot/initramfs.cpio.gz    (optional)
//! /usr/lib/pullrun/pullrun-runtime  (optional, for the runtime; not consumed by v0)
//! ```
//!
//! The image's OCI config labels SHOULD include
//! `org.pullrun.image.kind=kernel` so the policy engine can
//! distinguish kernel images from container images.
//!
//! # Lifecycle
//!
//! `StagedKernel` either:
//!   - Owns a `tempfile::TempDir` (when built from an OCI image
//!     via `from_image`). The temp dir is cleaned up on drop.
//!   - Holds paths owned by the caller (when built via
//!     `from_paths`). No cleanup happens on drop.
//!
//! In both cases, the `StagedKernel` is held inside the
//! `AppleVirtPool` (via `Arc<PoolInner>`), so the staged files
//! live as long as the pool — and no longer.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pullrun_oci::{OciMaterializer, OciPuller, OciToDagConverter};
use pullrun_store::MmapStore;
use tempfile::TempDir;
use thiserror::Error;
use tracing::{debug, info};

/// Well-known path inside a pullrun kernel image where the
/// uncompressed kernel ELF lives.
pub const KERNEL_VMLINUX_PATH: &str = "boot/vmlinux";

/// Well-known path inside a pullrun kernel image where the
/// (optional) initramfs lives.
pub const KERNEL_INITRAMFS_PATH: &str = "boot/initramfs.cpio.gz";

/// An error from kernel staging.
#[derive(Debug, Error)]
pub enum OciKernelError {
    #[error("OCI pull failed: {0}")]
    Pull(#[from] pullrun_oci::OciError),

    #[error("kernel image missing required file: {0}")]
    MissingFile(String),

    #[error("kernel image file is empty: {0}")]
    EmptyFile(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// A kernel that has been materialized onto the host
/// filesystem, ready to be passed to `VZLinuxBootLoader`.
#[derive(Debug)]
pub struct StagedKernel {
    vmlinux: PathBuf,
    initramfs: Option<PathBuf>,
    /// Owns the temp dir if we created one. None if the caller
    /// owns the underlying directory (e.g. for tests).
    _temp_dir: Option<TempDir>,
    /// DAG root digest of the kernel image, populated when the
    /// kernel was pulled from OCI. Used by GC to pin kernel
    /// layers for running/stopped VMs. None for local kernels.
    root_digest: Option<String>,
}

impl StagedKernel {
    /// Construct a `StagedKernel` from explicit host paths.
    /// The caller owns the underlying directory; nothing is
    /// cleaned up on drop.
    ///
    /// This is the right constructor for tests and for callers
    /// that already have a kernel staged on disk via some
    /// out-of-band mechanism.
    pub fn from_paths(
        vmlinux: PathBuf,
        initramfs: Option<PathBuf>,
    ) -> Result<Self, OciKernelError> {
        if !vmlinux.exists() {
            return Err(OciKernelError::MissingFile(format!(
                "{} (vmlinux)",
                vmlinux.display()
            )));
        }
        let meta = std::fs::metadata(&vmlinux)?;
        if meta.len() == 0 {
            return Err(OciKernelError::EmptyFile(format!(
                "{} (vmlinux)",
                vmlinux.display()
            )));
        }
        if let Some(initramfs) = &initramfs {
            if !initramfs.exists() {
                return Err(OciKernelError::MissingFile(format!(
                    "{} (initramfs)",
                    initramfs.display()
                )));
            }
        }
        Ok(Self {
            vmlinux,
            initramfs,
            _temp_dir: None,
            root_digest: None,
        })
    }

    /// Pull a kernel OCI image via the standard `pullrun-oci`
    /// pipeline, convert it to the DAG, materialize the bundle
    /// into a fresh temp dir, and return a `StagedKernel` that
    /// points at the materialized `/boot/vmlinux` and (if
    /// present) `/boot/initramfs.cpio.gz`.
    ///
    /// The returned `StagedKernel` owns the temp dir; dropping
    /// it cleans up the staged files.
    pub async fn from_image(
        image_ref: &str,
        store: &Arc<MmapStore>,
        registry: Option<&str>,
    ) -> Result<Self, OciKernelError> {
        Self::from_image_with_insecure(image_ref, store, registry, &[]).await
    }

    /// Like [`Self::from_image`], but allows specifying a list of
    /// registries that should be reached over plain HTTP.
    /// Pass an empty slice to require HTTPS for all
    /// registries (the default).
    pub async fn from_image_with_insecure(
        image_ref: &str,
        store: &Arc<MmapStore>,
        registry: Option<&str>,
        insecure_registries: &[String],
    ) -> Result<Self, OciKernelError> {
        info!(image = %image_ref, "pulling kernel image");

        // 1. Pull the OCI image (manifest + config + all layer
        //    blobs). The puller does the HTTP + digest
        //    verification. It returns a `PulledImage` that
        //    contains the parsed manifest and image config.
        let puller = OciPuller::with_insecure_registries(
            None,
            insecure_registries.iter().cloned().collect(),
        );
        let pulled = puller.pull(image_ref, registry).await?;

        // 2. Convert the OCI manifest into our content-
        //    addressed DAG. Layers are stored in `MmapStore`
        //    (zero-copy reads for the materializer).
        let converter = OciToDagConverter::new(store.clone());
        let root_digest = converter.convert(&pulled).await?;
        debug!(%root_digest, "kernel image converted to DAG");

        // 3. Materialize the DAG into a temp dir. This writes
        //    every file from every layer onto the host
        //    filesystem. For a kernel image the rootfs is
        //    tiny (~50-100 MB total) so this is fine for v0;
        //    a future optimization is to extract just the
        //    specific files we need via streaming tar walk.
        let temp_dir = tempfile::Builder::new()
            .prefix("pullrun-kernel-")
            .tempdir()?;
        let materializer = OciMaterializer::new(store);
        let bundle = materializer.materialize_bundle(&root_digest, temp_dir.path())?;
        let rootfs = &bundle.rootfs_path;

        let vmlinux = rootfs.join(KERNEL_VMLINUX_PATH);
        if !vmlinux.exists() {
            return Err(OciKernelError::MissingFile(format!(
                "{} (image is missing /boot/vmlinux — is this a pullrun kernel image?)",
                vmlinux.display()
            )));
        }

        let initramfs_path = rootfs.join(KERNEL_INITRAMFS_PATH);
        let initramfs = if initramfs_path.exists() {
            Some(initramfs_path)
        } else {
            debug!("kernel image has no initramfs at /boot/initramfs.cpio.gz");
            None
        };

        info!(
            vmlinux = %vmlinux.display(),
            initramfs = ?initramfs.as_ref().map(|p| p.display().to_string()),
            "kernel staged from OCI image"
        );

        Ok(Self {
            vmlinux,
            initramfs,
            _temp_dir: Some(temp_dir),
            root_digest: Some(root_digest.to_string()),
        })
    }

    /// Host filesystem path to the kernel ELF.
    pub fn vmlinux_path(&self) -> &Path {
        &self.vmlinux
    }

    /// Host filesystem path to the initramfs, if the image
    /// contained one.
    pub fn initramfs_path(&self) -> Option<&Path> {
        self.initramfs.as_deref()
    }

    /// DAG root digest of the kernel image, if the kernel was
    /// pulled from OCI. Returns None for locally-sourced kernels.
    pub fn root_digest(&self) -> Option<&str> {
        self.root_digest.as_deref()
    }

    /// Size of the vmlinux file in bytes, or 0 if the file
    /// was deleted between staging and this call.
    ///
    /// Useful for emitting metrics and for the runtime
    /// service to log "staged 20 MiB kernel" messages.
    pub fn vmlinux_size(&self) -> u64 {
        std::fs::metadata(&self.vmlinux)
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_paths` rejects a non-existent vmlinux.
    #[test]
    fn from_paths_rejects_missing_vmlinux() {
        let result = StagedKernel::from_paths(PathBuf::from("/nonexistent/vmlinux"), None);
        match result {
            Err(OciKernelError::MissingFile(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("should have rejected missing vmlinux"),
        }
    }

    /// `from_paths` accepts a real file and rejects an empty one.
    #[test]
    fn from_paths_rejects_empty_vmlinux() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // 0 bytes by default
        let result = StagedKernel::from_paths(tmp.path().to_path_buf(), None);
        match result {
            Err(OciKernelError::EmptyFile(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("should have rejected empty vmlinux"),
        }
    }

    /// `from_paths` accepts a non-empty vmlinux.
    #[test]
    fn from_paths_accepts_valid_vmlinux() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"\x7fELF dummy kernel").unwrap();
        let result = StagedKernel::from_paths(tmp.path().to_path_buf(), None);
        assert!(result.is_ok(), "expected Ok, got {:?}", result.err());
    }

    /// `from_paths` rejects a non-existent initramfs when
    /// provided.
    #[test]
    fn from_paths_rejects_missing_initramfs() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"\x7fELF dummy kernel").unwrap();
        let result = StagedKernel::from_paths(
            tmp.path().to_path_buf(),
            Some(PathBuf::from("/nonexistent/initramfs")),
        );
        match result {
            Err(OciKernelError::MissingFile(_)) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
            Ok(_) => panic!("should have rejected missing initramfs"),
        }
    }
}
