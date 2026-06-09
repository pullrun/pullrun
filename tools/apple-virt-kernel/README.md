# Pre-built Apple Virt Kernel Image (Asahi Linux)

This directory contains a pre-built Asahi Linux kernel image for the
Apple Virtualization framework (`VZLinuxBootLoader`). The image was
built from the [Asahi Linux kernel tree](https://github.com/AsahiLinux/linux)
and packaged as a Nimbus kernel OCI image.

The macOS VM backend (`nimbusctl run --backend=vm`) downloads the kernel
image from an OCI registry on first use and stages it automatically.
These files are provided so you can push the image to your own registry
or use them directly with the smoke-test tools.

## Files

| File | Size | Description |
|---|---|---|
| `nimbus-kata-3.31.0.tar` | 9.4 MB | OCI image tarball for `docker load` / `nimbusctl load` (tags `:v2` and `:v3`) |
| `vmlinux-3.31.0` | 17 MB | Uncompressed ARM64 kernel ELF (Linux 6.18.28) |
| `initramfs-3.31.0.cpio.gz` | 1.5 MB | Initramfs (original, contains `nimbus-init` + VirtioFS mount + vsock) |
| `initramfs-v3.cpio.gz` | 1.8 MB | Initramfs v3 — adds VirtioFS rootfs mount, DHCP (`udhcpc -i eth0`), chroot into Alpine rootfs, VM IP reporting in `InitHello.vm_ip`, 17 additional busybox applets, bundled `/usr/share/udhcpc/default.script` |

## Features (initramfs v3)

The v3 initramfs (`initramfs-v3.cpio.gz`) enables full host-to-VM
network connectivity:

- **VirtioFS rootfs mount** — mounts the shared rootfs from the host
- **DHCP networking** — `udhcpc -i eth0` obtains an IP in the
  192.168.64.x range via Apple VZ NAT
- **chroot** — copies `nimbus-init` and busybox into the rootfs, then
  `chroot()`s into the Alpine guest rootfs
- **VM IP reporting** — the assigned IP is sent to the host in
  `InitHello.vm_ip` (backward-compatible protocol extension)
- **Additional busybox applets** — chroot, udhcpc, ifconfig, route,
  grep, head, nc, ping, cut, sort, tr, sed, df, du, dmesg, insmod, lsmod
- **Bundled DHCP script** — `/usr/share/udhcpc/default.script` handles
  `bound`/`renew`/`deconfig` events

## How to use

### Option A: Push to a registry (for use with `nimbusctl run --kernel-image`)

```bash
docker load -i nimbus-kata-3.31.0.tar
docker tag nimbus/kata:3.31.0 <your-registry>/nimbus/kata:v3
docker push <your-registry>/nimbus/kata:v3

# Then run with nimbusctl:
nimbusctl run --backend=vm \
  --kernel-image <your-registry>/nimbus/kata:v3 \
  alpine:3.18 --cmd echo --cmd hello
```

The image is also tagged `:v2` — both tags work.

### Option B: Use with the Apple Virt smoke tool

```bash
# Direct paths (v3 initramfs):
../apple-virt-smoke/target/release/apple-virt-smoke \
  --kernel vmlinux-3.31.0 \
  --initramfs initramfs-v3.cpio.gz
```

### Option C: Use with `nimbus-runtime` directly (no registry)

Start the daemon with a local kernel image reference:

```bash
nimbus-runtime daemon --insecure-registry localhost:5000
nimbusctl run --backend=vm \
  --kernel-image localhost:5000/nimbus/kata:v3 \
  alpine:3.18 --cmd echo --cmd hello
```

### SSH into VM

With the v3 initramfs, the VM is reachable from the host via
Apple VZ NAT (192.168.64.x). Run any SSH-enabled OCI image:

```bash
nimbusctl run --attach --backend vm \
  --kernel-image localhost:5000/nimbus/kata:v3 \
  --registry localhost:5000 \
  custom/alpine-sshd:3.18 \
  --cmd /usr/sbin/sshd --cmd -D

# From another terminal:
sshpass -p password ssh root@192.168.64.x
```

## How to rebuild

See `../build-kernel-image/build.sh` for the full build process from
the Asahi kernel tree. The initramfs is built by
`../build-initramfs/` (`cargo run -p build-initramfs`).

## Source

- Kernel version: **Linux 6.18.28** (Asahi Linux `asahi-6.18` branch)
- Architecture: `arm64`
- Initramfs v3: Custom Nimbus initramfs with `nimbus-init` + VirtioFS +
  vsock + DHCP + chroot + 17 additional busybox applets
- Build toolchain: `aarch64-linux-gnu-` cross-compiler (Ubuntu Linux)
