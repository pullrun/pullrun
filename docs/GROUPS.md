# Sandbox groups

A sandbox group is **one VM that hosts many sandboxes**. It is the
dense execution tier: all sandboxes share the guest kernel and its page
cache, while each one gets a private filesystem view over the same
read-only image base.

```
                 one Apple Virtualization VM
  ┌───────────────────────────────────────────────────────┐
  │  guest kernel + shared page cache                     │
  │                                                       │
  │  sandbox A          sandbox B          sandbox C      │
  │  mnt/net/uts/ipc    mnt/net/uts/ipc    mnt/net/uts/ipc│
  │  overlay upper      overlay upper      overlay upper  │
  └──────────────┬──────────────┬──────────────┬─────────┘
                 └──────────────┴──────────────┘
                        read-only image base
```

## Semantics

| Property | Value |
|---|---|
| Isolation boundary | one VM per group; namespaces per sandbox (mount, net, UTS, IPC) |
| Rootfs | read-only image base + private tmpfs overlay upper per sandbox |
| State | sandbox writes are discarded when it exits; the base is never modified |
| Concurrency | sandboxes run concurrently; output/exit frames are tagged by sandbox id |
| Networking | each sandbox gets an empty network namespace (no connectivity yet) |
| Resources | no per-sandbox CPU/memory limits yet |
| Platforms | macOS Apple Virtualization only in this build |

Use a group when many tasks share one image and the VM boundary is
enough isolation (same tenant, trusted-ish code). Use a per-task
microVM (`pullrun run --backend vm`) for untrusted code.

## CLI

```bash
pullrun group create --image alpine:3.18     # -> grp-...
pullrun group run <id> -- /bin/sh -c 'echo hi'
pullrun group run <id> -- /bin/sh -c 'exit 7'   # exits 7
pullrun group ls
pullrun group rm <id>
```

## Benchmark

`hack/group-bench.sh` boots one group and measures concurrency,
latency, and guest page-cache growth. Run it with a signed runtime:

```bash
PULLRUN_BIN=... PULLRUN_RUNTIME=... \
PULLRUN_KERNEL_PATH=~/.pullrun/kernels/vmlinux-3.31.0 \
PULLRUN_INITRAMFS_PATH=.../pullrun-initramfs.cpio.gz \
bash hack/group-bench.sh alpine:3.18 10
```

### Results (2026-11-21, Apple Silicon macOS, debug runtime)

`alpine:3.18`, 1 vCPU / 512 MiB guest, image pulled once per run.

| metric | 5 sandboxes | 10 sandboxes |
|---|---|---|
| group boot (one VM) | 481 ms | 450 ms |
| N × `true`, sequential | 588 ms | 1,171 ms |
| N × `true`, concurrent | **188 ms** | **185 ms** |
| N × `sleep 0.5`, concurrent | 764 ms | 897 ms |
| N concurrent image readers | 292 ms | 355 ms |
| N sequential image readers | 696 ms | 1,395 ms |
| guest `Cached`, idle | 3.3 MiB | 3.3 MiB |
| guest `Cached` after 1 reader | 9.3 MiB | 9.3 MiB |
| guest `Cached` after N concurrent readers | **9.3 MiB** | **9.3 MiB** |
| guest `Cached` after N sequential readers | 9.3 MiB | 9.3 MiB |

The page-cache reading is the important one: the first sandbox that
reads the whole image warms 6 MiB of guest cache, and running 5 or 10
more readers adds **zero** cached bytes. That is the shared-page-cache
claim, measured.

Concurrency scales: 10 sandboxes that each sleep 0.5 s finish in 897 ms
(one boot + overlap), and 10 trivial sandboxes cost 185 ms total versus
1,171 ms sequentially.

### Caveats

- Timings use a **debug** `pullrun-runtime`; release builds are faster.
- Guest memory is fixed at 512 MiB in this test; page-cache numbers are
  from `/proc/meminfo` inside the sandbox.
- `pull + daemon start` is excluded from the tables above; it was
  14–20 s dominated by DAG conversion in a debug build.
- Docker containers already share the host page cache; the group’s
  differentiator is the **VM boundary plus the isolation-tier choice**
  (per-task microVM or dense group) from the same image and store.
