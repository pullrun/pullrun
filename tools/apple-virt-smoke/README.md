# `apple-virt-smoke` — Apple Virtualization FFI round-trip validator

A standalone binary that exercises the
[`pullrun_vm::apple`](../../runtime/pullrun-vm/src/apple.rs) module
end-to-end on macOS. It is **not** an integration test in the
pullrun-vm crate — it is a separate binary so it can be run on
CI or a developer Mac without the rest of the pullrun
workspace needing to be present.

## What it does

1. Calls `VZVirtualMachine::isSupported()` to confirm the
   Apple Virtualization framework is usable on this host.
2. Builds an `AppleVirtPool` of N paused `VZVirtualMachine`s
   from a kernel + host store path. This exercises the full
   FFI surface: `VZVirtualMachineConfiguration`,
   `VZLinuxBootLoader`, `VZVirtioFileSystemDeviceConfiguration`,
   `VZVirtioNetworkDeviceConfiguration`, the
   `startWithCompletionHandler` and `pauseWithCompletionHandler`
   callbacks.
3. Acquires a warm VM from the pool and confirms it enters the
   `Running` state within 2 seconds (polls because
   `startWithCompletionHandler` may return during the
   `Starting` state).
4. Releases the VM back to the pool (pause).

## What it does **not** do

- **Run a workload inside the guest.** That requires a Linux
  kernel with userspace, a static `pullrun-runtime` binary
  inside the initramfs, and a vsock transport. See
  [Apple Virt warm pool roadmap](../../PROGRESS.md).
- **Test end-to-end guest boot to login prompt.** Requires a
  kernel compiled for the Apple Virtualization guest ABI (the
  Asahi `linux` tree). Use
  `tools/fetch-asahi-kernel.sh` (not yet written) to stage
  one.

## Prerequisites

- **macOS on Apple Silicon.** Intel Macs are not supported
  by the Virtualization framework's Linux guest support.
- **A Linux kernel** compiled for the Apple Virtualization
  guest ABI (the Asahi `linux` tree, `asahi-6.8-1` branch or
  newer). Required config options: `CONFIG_VIRTIO=y`,
  `CONFIG_VIRTIO_FS=y`, `CONFIG_VIRTIO_NET=y`,
  `CONFIG_VIRTIO_VSOCKETS=y`. Without such a kernel, the FFI
  succeeds but the VM panics on `init=` failure.
- **Code signing entitlement** (`com.apple.security.virtualization`)
  for production use. Dev builds (unsigned) will pass the FFI
  round-trip but `validateWithError` will reject the config
  with `"The process doesn't have the entitlement"`.

## Build

```bash
cd tools/apple-virt-smoke
cargo build --release
```

The first build pulls in the `objc2` stack (~150 MiB of
`target/`); subsequent incremental builds are sub-second.

## Run

```bash
# Minimal — store path defaults to a temp dir
./target/release/apple-virt-smoke \
  --kernel ~/.local/share/pullrun/vms/vmlinux

# With initramfs + a custom store path
./target/release/apple-virt-smoke \
  --kernel     ~/.local/share/pullrun/vms/vmlinux \
  --initramfs  ~/.local/share/pullrun/initramfs.cpio.gz \
  --store      ~/.local/share/pullrun/store \
  --pool-size  3
```

## Exit codes

- `0` — FFI round-trip succeeded (pool created, acquire
  entered `Running`, release returned to pool).
- `1` — any failure (kernel missing, framework unsupported,
  config validation failed, callback timed out, etc.).
  Errors are logged with the Apple framework's
  `localizedDescription`.

## Example output (FFI works, entitlement missing)

```
INFO creating Apple Virt VM index=0 total=3
INFO creating Apple Virt VM index=1 total=3
INFO creating Apple Virt VM index=2 total=3
ERROR AppleVirtPool::new failed elapsed_ms=17 error=Apple Virt config
       validation failed: Invalid virtual machine configuration.
       The process doesn't have the "com.apple.security.virtualization"
       entitlement.
ERROR FAIL: pool construction failed
```

Exit code: 1.
