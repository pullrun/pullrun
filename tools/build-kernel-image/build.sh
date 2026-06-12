#!/usr/bin/env bash
# Copyright 2026 Mohammed Boukaba.
# SPDX-License-Identifier: Apache-2.0

#
# build.sh — build a Pullrun kernel image (Asahi Apple-Virt ABI)
#             and produce an OCI image layout.
#
# This script is a thin wrapper around the Asahi kernel build,
# post-processing the output into the layout `StagedKernel::from_image`
# expects (see ../../runtime/pullrun-vm/src/oci_kernel.rs):
#
#   /boot/vmlinux                (required)
#   /boot/initramfs.cpio.gz      (optional)
#   /usr/lib/pullrun/pullrun-runtime (optional, future)
#
# The result is a tarball you can `docker load` or push to any
# OCI registry as a pullrun kernel image.
#
# Requirements:
#   - Linux host (Apple Silicon cross-compile is fine)
#   - aarch64-linux-gnu- toolchain in PATH
#   - The Asahi kernel tree at $ASAHI_TREE
#     (git clone https://github.com/AsahiLinux/linux $ASAHI_TREE)
#   - docker or skopeo for publishing the result
#
# Usage:
#   ASAHI_TREE=~/src/linux-asahi \
#   PULLRUN_RUNTIME_BIN=../pullrun-runtime/target/release/pullrun-runtime \
#   ./build.sh 6.19.14
#
# Output:
#   pullrun-kernel-asahi-<version>.tar
#       (load with `docker load -i pullrun-kernel-asahi-<version>.tar`)
#   pullrun-kernel-asahi-<version>.oci/
#       (raw OCI layout for skopeo/diff tools)

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "usage: $0 <kernel-version> [extra make args...]" >&2
    echo "  e.g. $0 6.19.14" >&2
    exit 1
fi

KERNEL_VERSION="$1"; shift
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK_DIR="$(mktemp -d -t pullrun-kernel-build.XXXXXX)"
trap 'rm -rf "$WORK_DIR"' EXIT

ASAHI_TREE="${ASAHI_TREE:?ASAHI_TREE must point to the Asahi kernel tree}"
PULLRUN_RUNTIME_BIN="${PULLRUN_RUNTIME_BIN:-}"

echo "==> Building Asahi kernel $KERNEL_VERSION in $ASAHI_TREE"
echo "    output staging: $WORK_DIR/staging"
echo "    final tarball: pullrun-kernel-asahi-$KERNEL_VERSION.tar"

# 1. Build the kernel + DTBs + modules.
#    We don't `make install` — we stage files manually into
#    the OCI image layout below.
make -C "$ASAHI_TREE" -j"$(nproc)" \
    ARCH=arm64 \
    CROSS_COMPILE=aarch64-linux-gnu- \
    LOCALVERSION="-400.asahi" \
    "$@" \
    Image modules dtbs

# 2. Stage the files into the OCI image layout.
STAGING="$WORK_DIR/staging"
mkdir -p "$STAGING/boot" "$STAGING/usr/lib/pullrun"

# Stripped uncompressed ELF (the format VZLinuxBootLoader
# accepts). The Asahi `make Image` target produces this.
aarch64-linux-gnu-strip -s \
    "$ASAHI_TREE/arch/arm64/boot/Image" \
    -o "$STAGING/boot/vmlinux"

# Empty initramfs if the caller didn't supply one. A real
# workload will bring its own initramfs (e.g. via a separate
# image); the kernel image alone is the v0 contract.
if [ -n "${PULLRUN_INITRAMFS:-}" ] && [ -f "$PULLRUN_INITRAMFS" ]; then
    cp "$PULLRUN_INITRAMFS" "$STAGING/boot/initramfs.cpio.gz"
else
    # /dev/null | gzip is the canonical "empty cpio.gz".
    gzip -c /dev/null > "$STAGING/boot/initramfs.cpio.gz"
fi

# Optional: drop the static runtime binary in. v0 doesn't
# actually use it from inside the guest (the vsock transport
# is not wired yet), but the file path is part of the
# documented image layout.
if [ -n "$PULLRUN_RUNTIME_BIN" ] && [ -f "$PULLRUN_RUNTIME_BIN" ]; then
    cp "$PULLRUN_RUNTIME_BIN" "$STAGING/usr/lib/pullrun/pullrun-runtime"
    chmod 0755 "$STAGING/usr/lib/pullrun/pullrun-runtime"
fi

# 3. Build the OCI image with a hand-rolled Dockerfile.
#    `docker buildx` is the only tool that does this
#    without external dependencies; `skopeo` and `umoci` are
#    alternatives but heavier.
echo "==> Building OCI image"
TARBALL="pullrun-kernel-asahi-$KERNEL_VERSION.tar"

# Write a temporary Dockerfile. Doing it inline keeps
# `build.sh` self-contained — no auxiliary files in
# the repo to keep in sync.
cat >"$WORK_DIR/Dockerfile" <<EOF
FROM scratch
LABEL org.pullrun.image.kind="kernel" \\
      org.pullrun.image.kernel.version="$KERNEL_VERSION" \\
      org.pullrun.image.kernel.vendor="asahi"
COPY boot/ /boot/
COPY usr/ /usr/
EOF

# We assemble the layer tarball ourselves rather than going
# through docker buildx, because we want a deterministic
# reproducible artifact (no build cache, no timestamps,
# rootless, no daemon).
LAYER_TAR="$WORK_DIR/layer.tar"
tar --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime='@0' \
    -C "$STAGING" \
    -cf "$LAYER_TAR" \
    boot usr

LAYER_DIGEST="sha256:$(sha256sum "$LAYER_TAR" | awk '{print $1}')"
LAYER_SIZE="$(stat -c%s "$LAYER_TAR" 2>/dev/null || stat -f%z "$LAYER_TAR")"

# OCI image manifest (v1.0.0, schema 2).
CONFIG_JSON="$WORK_DIR/config.json"
cat >"$CONFIG_JSON" <<EOF
{
  "architecture": "arm64",
  "os": "linux",
  "config": {
    "Env": [],
    "Cmd": null
  },
  "rootfs": {
    "type": "layers",
    "diff_ids": ["$LAYER_DIGEST"]
  },
  "history": [
    {
      "created": "1970-01-01T00:00:00Z",
      "comment": "Built by tools/build-kernel-image/build.sh"
    }
  ]
}
EOF

CONFIG_DIGEST="sha256:$(sha256sum "$CONFIG_JSON" | awk '{print $1}')"
CONFIG_SIZE="$(stat -c%s "$CONFIG_JSON" 2>/dev/null || stat -f%z "$CONFIG_JSON")"

MANIFEST_JSON="$WORK_DIR/manifest.json"
cat >"$MANIFEST_JSON" <<EOF
{
  "schemaVersion": 2,
  "mediaType": "application/vnd.oci.image.manifest.v1+json",
  "config": {
    "mediaType": "application/vnd.oci.image.config.v1+json",
    "digest": "$CONFIG_DIGEST",
    "size": $CONFIG_SIZE
  },
  "layers": [
    {
      "mediaType": "application/vnd.oci.image.layer.v1.tar",
      "digest": "$LAYER_DIGEST",
      "size": $LAYER_SIZE
    }
  ]
}
EOF

# 4. Package the final tarball. This is the format
#    `docker load` accepts: a single tar with a
#    `manifest.json` and a `repositories` file at the root.
REPOS_JSON="$WORK_DIR/repositories"
cat >"$REPOS_JSON" <<EOF
{"pullrun/kernel-asahi":{"$KERNEL_VERSION":"$CONFIG_DIGEST"}}
EOF

tar -C "$WORK_DIR" \
    --transform 's,^manifest.json,pullrun/kernel-asahi/$KERNEL_VERSION/manifest.json,' \
    --transform 's,^config.json,pullrun/kernel-asahi/$KERNEL_VERSION/config.json,' \
    --transform 's,^layer.tar,pullrun/kernel-asahi/$KERNEL_VERSION/layer.tar,' \
    --transform 's,^repositories,repositories,' \
    -cf "$TARBALL" \
    manifest.json config.json layer.tar repositories

echo
echo "==> Done."
echo "  Tarball:       $TARBALL"
echo "  Load with:     docker load -i $TARBALL"
echo "  Then run:      docker run --rm pullrun/kernel-asahi:$KERNEL_VERSION \\"
echo "                     ls /boot"
echo "  Or pull via:   apple-virt-smoke --kernel-image pullrun/kernel-asahi:$KERNEL_VERSION"
