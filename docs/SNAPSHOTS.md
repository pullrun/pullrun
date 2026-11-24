# VM memory snapshots

Pullrun can capture a running Firecracker VM (guest memory + device
state + rootfs) and restore it into a fresh workload. This is the
foundation for suspend/resume actors: idle agents can be snapshotted,
their compute released, and resumed later from memory.

## Usage

```bash
# Start a VM workload (it must stay running).
pullrun run alpine:3.18 --backend vm --net bridge \
  --cmd /bin/sh --cmd -c --cmd 'while true; do date; sleep 1; done'

# MEMORY fidelity (default): guest RAM + disk; stops the source VM.
pullrun snapshot create <workload-id>
# ROOTFS fidelity: disk only; source keeps running, restore cold-boots.
pullrun snapshot create <workload-id> --fidelity rootfs

# Inspect / remove / GC.
pullrun snapshot ls
pullrun snapshot rm <snapshot-id>
pullrun snapshot gc --keep 1

# Resume once, or fork into an isolated network namespace.
pullrun snapshot restore <snapshot-id>
pullrun snapshot restore --fork <snapshot-id>
```

Snapshots live under `<store>/snapshots/<snapshot-id>/`:

| file | contents |
|---|---|
| `state` | Firecracker VM state (devices, vCPUs), AES-256-GCM; MEMORY only |
| `mem` | `PRPAGE1` index of 64 KiB page hashes; page blobs live once under `<store>/snapshot-pages/<sha256>` (AES-GCM) |
| `rootfs.ext4` | disk image at snapshot time, AES-256-GCM |
| `meta.json` | workload id, drive path, sizes, timestamp, parent, hashes, network intent |

Fidelity levels:

| fidelity | capture | source VM | restore |
|---|---|---|---|
| `memory` | RAM + disk + device state | stopped (single-resume) | snapshot load, resumes in-memory process |
| `rootfs` | disk only | keeps running | cold boot from the recorded kernel |
| `volumes` | not implemented yet | — | — |

## Verified behaviour

On the Linux KVM host (Firecracker 1.18.0, x86_64, 512 MiB guest):

- A guest running an in-memory counter was snapshotted at `tick 2`
  (memory file 256 MiB in the test; 512 MiB for a 512 MiB VM), the
  original VMM killed, and the snapshot restored into a new process.
  The restored guest continued at `tick 3`.
- Through the runtime, `snapshot create` produced encrypted `state`,
  a paged `mem` index, and `rootfs.ext4`; `snapshot restore` registered a
  new running workload. With page dedup, a 512 MiB guest memory snapshot
  stores 441 unique pages (27.6 MiB).
- With `--fidelity rootfs`, capture took **20 ms** (disk copy), the
  source VM stayed running, and `snapshot restore` cold-booted a new VM
  in **199 ms** whose guest restarted the counter from `tick 1`.

## Latency

`hack/snapshot-bench.sh` captures one snapshot, then restores it N times
and reports the executor-side wall times (`snapshot_ms` / `restore_ms`).

Linux KVM host, Firecracker 1.18.0, 512 MiB guest (release runtime;
page-dedup + encryption enabled):

| metric | measured |
|---|---|
| snapshot capture | 176–394 ms |
| normal restore | **153 ms** |
| fork restore (9 concurrent) | **142–483 ms** |
| page sharing, second snapshot | **97.6%** (8192 logical, 194 new) |
| stored memory, 512 MiB guest | **441 unique pages / 27.6 MiB** |

The page store collapses zero and duplicate 64 KiB pages; the same
snapshot can be forked repeatedly. Earlier figures (3.8 s capture,
63–119 ms restore) predate encryption and page dedup.

## Limitations

- **Firecracker / Linux only.** Apple Virtualization and containers are
  not supported by this API yet.
- **Network re-attach works for bridged and slirp restores.** The TAP is
  recreated before `load_snapshot`; inbound proxy rules are not
  re-registered yet.
- **Forked guests do not get a new identity.** IP, MAC, entropy, and clock
  are inherited from the snapshot; each fork gets its own isolated L2
  bridge/TAP, but guest identity re-seeding is not implemented.
- **Fork requires Firecracker ≥1.18** for snapshot `network_overrides`.
  Snapshot state also remains bound to the Firecracker format version.
- **Encrypted, but the key is local.** AES-256-GCM with
  `<store>/snapshots.key` (0600); a stolen disk includes the key.
  External KMS/HSM is not implemented.
- **Dedup is storage-level.** Pages are content-addressed and shared, but
  there is no capture-time delta or parent-diff snapshot API.
- **VOLUMES fidelity is not implemented.** `commit` remains
  filesystem-only.
- **ROOTFS restore is a cold boot.** It does not preserve processes or
  memory; guest startup scripts run again.
## Next

- Guest identity re-seeding on fork (IP/MAC/entropy/clock).
- Capture-time delta snapshots.
- VOLUMES fidelity and Apple VM snapshots.
- Actor templates built on golden snapshots.
