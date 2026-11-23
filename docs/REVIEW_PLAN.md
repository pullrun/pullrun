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

| Date | Milestone | Evidence |
|---|---|---|
| 2026-11-23 | M1 start | Restore path (`service.rs` ~5460) hardcodes `network_mode: isolated`, no NIC setup; docs/SNAPSHOTS.md confirms net-less-only |
