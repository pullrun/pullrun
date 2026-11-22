# VM memory snapshots

Pullrun can capture a running Firecracker VM (guest memory + device
state + rootfs) and restore it into a fresh workload. This is the
foundation for suspend/resume actors: idle agents can be snapshotted,
their compute released, and resumed later from memory.

## Usage

```bash
# Start a VM workload (it must stay running).
pullrun run alpine:3.18 --backend vm --net none \
  --cmd /bin/sh --cmd -c --cmd 'while true; do date; sleep 1; done'

# Snapshot it. This stops the source VM (single-resume, secure usage).
pullrun snapshot create <workload-id>

# Inspect / remove.
pullrun snapshot ls
pullrun snapshot rm <snapshot-id>

# Resume into a new workload.
pullrun snapshot restore <snapshot-id>
```

Snapshots live under `<store>/snapshots/<snapshot-id>/`:

| file | contents |
|---|---|
| `state` | Firecracker VM state (devices, vCPUs) |
| `mem` | guest memory |
| `rootfs.ext4` | disk image at snapshot time |
| `meta.json` | workload id, drive path, sizes, timestamp |

## Verified behaviour

On the Linux KVM host (Firecracker 1.10.1, x86_64, 512 MiB guest):

- A guest running an in-memory counter was snapshotted at `tick 2`
  (memory file 256 MiB in the test; 512 MiB for a 512 MiB VM), the
  original VMM killed, and the snapshot restored into a new process.
  The restored guest continued at `tick 3`.
- Through the runtime, `snapshot create` produced `state` (27 KB) +
  `mem` (512 MiB) + `rootfs.ext4` (256 MiB), and `snapshot restore`
  registered a new running workload whose console continued the counter
  at `tick 64-66`.

## Latency

`hack/snapshot-bench.sh` captures one snapshot, then restores it N times
and reports the executor-side wall times (`snapshot_ms` / `restore_ms`).

Linux KVM host, Firecracker 1.10.1, 512 MiB guest, 256 MiB rootfs
(cold-ish page cache, debug runtime):

| metric | measured |
|---|---|
| snapshot capture | 3,895 ms |
| restore, best | **227 ms** |
| restore, average | 1,619 ms |
| restore, worst | 2,300 ms |

The first restore met the <500 ms target; later restores were slower
because each one rewrites the 256 MB rootfs copy before loading the
memory file. Reducing that copy (reflink, overlay-backed rootfs, or
snapshot-owned drive) is required to make resume consistently
sub-500 ms.

## Limitations

- **Firecracker / Linux only.** Apple Virtualization and containers are
  not supported by this API yet.
- **Net-less snapshots only.** The restore path currently assumes
  `--net none`; network devices and taps are not re-established.
- **Single restore.** Creating a snapshot stops the source VM; the
  snapshot is intended to be resumed once (secure usage). Restoring the
  same snapshot multiple times is possible but clones guest uniqueness
  state.
- **No encryption or dedup.** Snapshot files contain full guest memory
  and may contain secrets; protect the store accordingly.
- **Firecracker-version bound.** State files are tied to the
  Firecracker snapshot format version.
- **No fidelity levels yet.** `commit` remains filesystem-only;
  `snapshot` is always MEMORY+ROOTFS.

## Next

- `ROOTFS`/`VOLUMES` fidelity levels and `commit --memory`.
- Snapshot encryption, size accounting, and GC.
- Restore latency benchmark (target <500 ms).
- Actor templates built on golden snapshots.
