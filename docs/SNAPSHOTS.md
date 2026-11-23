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

# MEMORY fidelity (default): guest RAM + disk; stops the source VM.
pullrun snapshot create <workload-id>
# ROOTFS fidelity: disk only; source keeps running, restore cold-boots.
pullrun snapshot create <workload-id> --fidelity rootfs

# Inspect / remove.
pullrun snapshot ls
pullrun snapshot rm <snapshot-id>

# Resume into a new workload.
pullrun snapshot restore <snapshot-id>
```

Snapshots live under `<store>/snapshots/<snapshot-id>/`:

| file | contents |
|---|---|
| `state` | Firecracker VM state (devices, vCPUs); MEMORY only |
| `mem` | guest memory; MEMORY only |
| `rootfs.ext4` | disk image at snapshot time |
| `meta.json` | workload id, drive path, sizes, timestamp |

Fidelity levels:

| fidelity | capture | source VM | restore |
|---|---|---|---|
| `memory` | RAM + disk + device state | stopped (single-resume) | snapshot load, resumes in-memory process |
| `rootfs` | disk only | keeps running | cold boot from the recorded kernel |
| `volumes` | not implemented yet | — | — |

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
- With `--fidelity rootfs`, capture took **20 ms** (disk copy), the
  source VM stayed running, and `snapshot restore` cold-booted a new VM
  in **199 ms** whose guest restarted the counter from `tick 1`.

## Latency

`hack/snapshot-bench.sh` captures one snapshot, then restores it N times
and reports the executor-side wall times (`snapshot_ms` / `restore_ms`).

Linux KVM host, Firecracker 1.10.1, 512 MiB guest, 256 MiB rootfs
(debug runtime, warm page cache):

| metric | measured |
|---|---|
| snapshot capture | 3,849 ms |
| restore, best | **63 ms** |
| restore, average | **84 ms** |
| restore, worst | **119 ms** |

All five restores were well under the 500 ms target. The key was copying
the mostly-sparse ext4 image with `cp --sparse=always` (60 ms vs 190 ms
for 256 MB); before that, the rootfs rewrite dominated and restores
averaged 1.6 s.

## Limitations

- **Firecracker / Linux only.** Apple Virtualization and containers are
  not supported by this API yet.
- **Network re-attach works for bridged and slirp restores.** The TAP is
  recreated before `load_snapshot`; inbound proxy rules are not
  re-registered yet.
- **Single restore.** Creating a snapshot stops the source VM; the
  snapshot is intended to be resumed once (secure usage). Restoring the
  same snapshot multiple times is possible but clones guest uniqueness
  state.
- **Encrypted, but the key is local.** Snapshot state/memory/rootfs are
  AES-256-GCM encrypted with `<store>/snapshots.key` (0600). A stolen
  disk includes the key; external KMS/HSM support is not implemented.
- **No dedup.** Snapshot files contain full guest memory
  and may contain secrets; protect the store accordingly.
- **Firecracker-version bound.** State files are tied to the
  Firecracker snapshot format version.
- **VOLUMES fidelity is not implemented.** `commit` remains
  filesystem-only; `snapshot --fidelity volumes` returns a clear error.
- **ROOTFS restore is a cold boot.** It does not preserve processes or
  memory; guest startup scripts run again.

## Next

- `ROOTFS`/`VOLUMES` fidelity levels and `commit --memory`.
- Snapshot size accounting and incremental capture.
- Restore latency benchmark (target <500 ms).
- Actor templates built on golden snapshots.
