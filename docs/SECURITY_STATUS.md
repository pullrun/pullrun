# Security status

This page tracks what Pullrun actually implements, in the same spirit as
Agent Substrate's security overview. "Met" means verified in this
repository; "In Progress" means code exists but the end-to-end property
is not proven; "Future" means not implemented.

| Principle | Status | Evidence / gap |
|---|---|---|
| Confined OCI extraction | **Met** | fd-relative `RootDir`, absolute/`..` rejection, symlink re-rooting; regression tests |
| VM isolation (Firecracker) | **Met** | one kernel per VM; boot, command execution, exit codes verified on KVM |
| VM isolation (Apple Virtualization) | **Met** | per-workload APFS clone, separate VM; macOS verified |
| Container isolation | **Trusted tier only** | namespaces on the host kernel; do not run untrusted code in containers |
| Safe VM default | **Met** | `run --backend vm` starts isolated; `--net slirp`/`--trusted` opt in |
| Daemon panic surface | **Partial** | non-test `unwrap()` denied in store/runtime; refcount lock recovers poison; no full audit |
| Fuzzing | **Met (parser)** | `runtime/pullrun-oci/fuzz` RelPath target; 23,728,225 runs in 121 s on Linux CI, no crash |
| Sandbox groups | **In Progress** | one VM, per-sandbox mount/net/UTS/IPC + tmpfs overlay; shared guest kernel |
| Immutable image bases | **Met** | one flattened base per digest; CoW clones; no hardlinks; GC |
| VM memory snapshot | **Met** (Linux) | state + memory + rootfs; restore verified (`tick 2 → 3`) |
| Snapshot fidelity | **Partial** | `MEMORY` and `ROOTFS` implemented; `VOLUMES` returns unsupported |
| Snapshot latency | **Met target** | capture 3.8 s; restore 63 / 84 / 119 ms (best/avg/worst, 512 MiB guest) |
| Actor templates / suspend/resume | **Met** (single-node) | golden memory snapshot; resume 99–110 ms; suspend writes a new snapshot |
| Request parking | **Partial** | per-actor mutex serializes resume/suspend; no HTTP ingress queue |
| Fail-closed egress | **Met** (rootful containers) | nftables `policy drop` + allowlist verified in a netns and with real traffic: allowed TCP connect exit 0, denied exit 1. VM/rootless reject the flag; no egress audit yet |
| Credential injection | **Met** (rootful containers) | HTTPS MITM proxy injects headers from the secret store; verified `Authorization: Bearer …` with no sandbox secret |
| No ambient credentials | **Partial** | egress-credential workloads hold no secret; explicit `--secret` mounts still place secrets in the sandbox; snapshots contain guest memory |
| Snapshot encryption / integrity | **Not met** | files are plaintext on the store; Firecracker state has a CRC, not authentication |
| Workload identity | **Future** | no SPIFFE/JWT issuance |
| Egress audit events | **Future** | policy is enforced by nftables; no per-flow audit stream |
| Snapshot GC / size accounting | **Partial** | manual `snapshot rm`; no automatic retention |
| Multi-tenancy | **Single-node only** | no cluster scheduler, quotas, or tenant isolation |
| Metrics | **Met (basic)** | Prometheus endpoint now includes snapshot/actor latency counters |

## North-star metrics

`pullrun-runtime --metrics-addr` exposes:

| Metric | Type | Meaning |
|---|---|---|
| `pullrun_snapshot_capture_duration_seconds` | histogram | snapshot capture wall time |
| `pullrun_snapshot_restore_duration_seconds` | histogram | restore request → running workload |
| `pullrun_snapshots_created_total` | counter (`fidelity`) | snapshots captured |
| `pullrun_snapshots_restored_total` | counter | snapshot restores |
| `pullrun_actor_resume_duration_seconds` | histogram | actor resume latency |
| `pullrun_actors_resumed_total` | counter (`resumed`) | resume calls |
| `pullrun_actors_suspended_total` | counter | actor suspends |

Current measured targets:

| Metric | Target | Current |
|---|---|---|
| actor resume p95 | < 500 ms | 99–110 ms (single-node test) |
| snapshot restore | < 500 ms | 63–119 ms |
| snapshot capture | no target | 3.8 s for 512 MiB |
| egress enforcement | default deny | mechanism verified; traffic path pending |

## Claims we do **not** make

- "No escape": VMs are strongly isolated, but there is no formal
  guarantee; Firecracker snapshot files and host kernel bugs remain
  part of the attack surface.
- "Zero ambient credentials": secrets mounts and snapshots still carry
  credentials.
- "Millions of agents": single-node tested at tens of actors; no fleet
  scale evidence.
- "Network-isolated by default": the default remains allow-all unless
  `--egress-deny` is set.
