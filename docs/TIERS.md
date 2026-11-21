# Execution tiers

Pullrun runs the same OCI image in three tiers. They trade isolation
against density and boot cost:

| tier | isolation | memory | best for |
|---|---|---|---|
| **Sandbox group** | one VM; mount/net/UTS/IPC namespaces per sandbox | shared guest kernel + page cache | many tasks from one image, same tenant |
| **Per-task VM** | one VM/kernel per task | independent guest memory | untrusted code, strong isolation |
| **Container** | namespaces on the host kernel | shared host page cache | trusted code, Linux only |

## Measured comparison

`hack/tier-bench.sh` boots a runtime, pulls `alpine:3.18` once, warms a
run, then times N sandboxes sequentially and concurrently. All runs
execute `/bin/sh -c true`.

### macOS (Apple Silicon, 5 sandboxes)

| tier | sequential | concurrent |
|---|---:|---:|
| group (one VM, boot 518 ms) | 667 ms | **231 ms** |
| per-task Apple VM | 1,580 ms | **467 ms** |

### Linux (x86_64, 5 sandboxes, rootless runc)

| tier | sequential | concurrent |
|---|---:|---:|
| container (rootless runc) | 3,592 ms | **914 ms** |
| per-task Firecracker VM | boots & executes in ~1 s; attach pending | — |
| group | not implemented on Linux | — |

Interpretation:

- On macOS, a group is roughly **2× faster** than per-task VMs in both
  modes, and the 518 ms boot is paid once for all sandboxes.
- On Linux, five rootless containers complete concurrently in ~0.9 s.
  The measurement includes rootfs clone, `runc` startup, and exit
  detection; it is a debug runtime.
- Firecracker boots and executes workloads: the daemon started a
  Firecracker VM with `--net none`, the guest ran
  `/bin/sh -c 'echo fc-user-ok; exit 7'`, printed
  `PULLRUN_EXIT=7`, and powered off (`sysrq: Power Off`,
  `reboot: Power down`). Isolated VMs create no tap/bridge/NIC.
- Remaining gap: `run --attach` does not yet return on VM poweroff
  (`[pullrun] detached` then hang), and the CLI does not propagate the
  guest exit code. `status()` parses `PULLRUN_EXIT=` and kills the
  Firecracker VMM, but the attach lifecycle needs the exit frame.
- Re-run `hack/tier-bench.sh` on Linux with `/dev/kvm`,
  `PULLRUN_FC_BIN`, and `PULLRUN_FC_VMLINUX` once attach returns to
  fill the tier row.
- The boot test now builds its rootfs with `mkfs.ext4 -d` instead of a
  loop mount, so it runs as an unprivileged user with KVM access.

## Caveats

- Debug builds (`pullrun-runtime`); release binaries are faster.
- Image pull/DAG conversion is excluded (done once before warming).
- Container runs use `--net none`; pasta/slirp4netns is not installed on
  the Linux test host, so no network setup is included.
- macOS VMs use 1 vCPU / 512 MiB; groups use one such VM.
- N is small (5). The group advantage grows with N; see
  `docs/GROUPS.md` for 10-sandbox page-cache numbers.

## Reproduce

```bash
PULLRUN_BIN=... PULLRUN_RUNTIME=... \
PULLRUN_KERNEL_PATH=... PULLRUN_INITRAMFS_PATH=... \
bash hack/tier-bench.sh alpine:3.18 5
```

On Linux, omit the kernel/initramfs variables; Firecracker is skipped
unless `/dev/kvm` is readable.
