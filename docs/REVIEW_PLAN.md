# 2026 Review Roadmap

Source: independent review (Nov 2026) + repo verification. This plan is the
authoritative program list. Each milestone ships a release and records
evidence here.

## Operating rules

- One milestone at a time; no new surface area until the current milestone
  has tests and an honest doc update.
- Every claim gets a measured number, a failing regression test, or both.
- `docs/SECURITY_STATUS.md` is updated whenever a guarantee changes.
- Freeze: compose, Dockerfile builder, control-plane, Windows/WSL2. CRI stays
  scoped to on-prem Kubernetes.

## M1 — Fleet-grade snapshots (the wedge)

Current: MEMORY/ROOTFS, restore 63–119 ms, actors 99–110 ms. Gaps: net-less
restore, no fork/branch, plaintext, no page dedup, no GC, 3.8 s capture,
512 MiB per suspended actor.

| Task | Acceptance |
|---|---|
| Network re-attach on restore | Restore a snapshot with a tap/NIC, start nginx before capture, curl it from the host after restore; `RestoreSnapshotRequest` carries network mode/rules and state records them |
| Fork/branch | Restore one snapshot N times; each guest gets a unique MAC, IP, entropy, and clock; test proves uniqueness and independent writes |
| Page dedup | Content-addressed guest-memory pages in the DAG store; measure RSS/disk for 10 actors from one template; report actual sharing |
| Encryption + integrity | AES-256-GCM snapshot files with authenticated metadata; tamper test fails closed |
| Snapshot GC | Retention policy + `pullrun snapshot gc`; test removes only unreferenced files |
| Incremental capture | Capture delta vs parent template; test measuring reduced 3.8 s suspend |

Release: v0.11.0. Do not call snapshots "fleet-grade" until all rows pass.

## M2 — Python + TypeScript SDKs

- `sdk/python` (async + sync, `with Sandbox()` ergonomics, streaming exec).
- `sdk/typescript` (Node 20+, streaming, AbortSignal).
- Both speak the existing gRPC API; no server rewrite. Publishable packages,
  examples, and a CI job that runs a real workload against a dev runtime.

Acceptance: example from a clean environment boots an image, execs, streams
output, snapshots, restores. Release: v0.12.0.

## M3 — Egress for the untrusted tier

- Fail-closed nftables for Firecracker TAPs (host-side), rootless containers.
- Per-flow audit events and CA rotation.
- Keep credential injection out of the sandbox.

Acceptance: allowed TCP exit 0 / denied exit 1 from a VM; audit event per flow;
CA rotation without restarting running workloads. Release: v0.13.0.

## M4 — Linux sandbox groups + scale demo

- Port the guest supervisor to Firecracker (one VM, N sandboxes, shared
  page cache).
- Publish: 500–1,000 actors on one host with dedup'd snapshots and image
  P2P across three nodes. Measured, reproducible script.

Release: v0.14.0.

## M5 — Trust, distribution, business focus

- Third-party audit of store confinement, vsock, guest init, snapshot loader.
- DCO evaluation; good-first issues; contributor docs.
- Pick two commercial lanes: OEM/embed and enterprise on-prem/air-gapped.
  Retire the generic hosted-API positioning.
- Launch after M1/M2 land (SDK + networked restore).

No release gate; tracked as program completion.

## Evidence log

| 2026-11-23 | M2 TypeScript SDK (verified) | `sdk/typescript`: gRPC-js/proto-loader client, Sandbox API, compiles with `tsc`, mock test passes, and `ListWorkloads` succeeds against the real runtime UDS from Node 22 |
| 2026-11-23 | M2 Python SDK (blocked) | `sdk/python`: `RuntimeClient`, `Sandbox` context manager, run/exec/logs/stop/snapshot/restore, generated stubs, example. Mock gRPC test passes. Integration blocked on **both macOS and Linux**: Python grpc 1.85 gets `RST_STREAM (PROTOCOL_ERROR)` from the tonic UDS server while Go and Node gRPC clients work. Needs a tonic/Python transport fix or a documented Python-on-TCP path |

| Date | Milestone | Evidence |
|---|---|---|
| 2026-11-23 | M1 start | Restore path (`service.rs` ~5460) hardcoded `network_mode: isolated`; docs/SNAPSHOTS.md confirms net-less-only |
| 2026-11-23 | M1 page GC (done) | `snapshot gc` now removes page blobs no remaining paged snapshot references. Local test: h3 orphan removed; h1/h2 kept because `snap-b` referenced them |
| 2026-11-23 | M1 dedup (code) | Guest memory split into 4 KiB pages, stored once under `<store>/snapshot-pages` (convergent AES-GCM), snapshot `mem` is a `PRPAGE1` index. Restore reconstructs raw memory and verifies SHA-256. Unit tests: dedup (2 identical pages → 1 blob), round-trip, tamper. GC of page blobs and measured multi-actor sharing still pending; Firecracker integration blocked on host |
| 2026-11-23 | M1 encryption (code) | AES-256-GCM snapshot files (`PRSNAP1` format) with `<store>/snapshots.key` 0600; capture hashes plaintext then encrypts; restore decrypts to a temp dir, verifies SHA-256, loads, removes temp. Unit tests: round-trip, tamper, key mode/stability. Firecracker integration still blocked on host |
| 2026-11-23 | M1 integrity (done) | Snapshot `state`/`mem`/`rootfs` SHA-256 stored in `meta.json`; restore fails closed on mismatch; tamper test passes. Encryption is still pending |
| 2026-11-23 | M1 GC (done) | `snapshot gc --keep N [--dry-run]`: scans `meta.json`, keeps actor/template-referenced and live-restored snapshots, removes older unreferenced dirs. Local test: removed `snap-old`, kept `snap-new` (newest) and `snap-ref` (actor reference) |
| 2026-11-23 | M1 restore (verified) | Host rebooted. Encrypted + paged snapshot restore: **153 ms**, guest `10.42.0.2`, host ping 0% loss. Capture 283 ms. |
| 2026-11-23 | M1 fork (verified, partial) | Firecracker **1.18.0** accepts `network_overrides` (1.10.1 rejects it). Two concurrent forks from one snapshot: both `running`, restore **152 ms / 200 ms** server-side, unique `pf-*` bridges and `fc-*` taps. Caveats: guest IP/MAC stay the snapshot's (isolated L2, inbound/egress not wired), entropy/clock are not re-seeded; fork requires newer Firecracker |
| 2026-11-23 | M1 fork (in progress) | Fork restores load and run (first fork 320 ms), but the restore RPC can hang after `load_snapshot`; logs show a `pullrun-br0` tap instead of the intended `pf-*` fork bridge, so the fork flag/tap override path is not reaching the executor as expected. Two-fork acceptance is **not** met |
| 2026-11-23 | M1 fork (blocked) | First fork attempt used slirp in a rootful daemon; `slirp4netns --configure` reconfigured the host netns and the KVM host became unreachable (SSH kex reset, ICMP loss). Code changed to an unbridged isolated TAP; **fork verification is blocked until the host is rebooted**, and no fork result is claimed |
| 2026-11-23 | M1 network (done) | Meta stores TAP/MAC/gateway/netmask; restore recreates the persistent TAP before `load_snapshot`. Verified on KVM: snapshot of `alpine:3.18` with `--net bridge` (10.42.0.4), restore in **130 ms**, host `ping 10.42.0.4` succeeds, tap attached to `pullrun-br0`. Note: inbound proxy rules are not re-registered yet |
