# External audit scope

This is the proposed scope for a third-party security audit. It is written to
be quotable by an auditor, not marketing.

## In scope (security boundary)

1. **OCI extraction and confinement** — `runtime/pullrun-oci/src/confinement.rs`,
   `materializer.rs`, `dockerfile.rs`. Tar entries, symlinks, whiteouts,
   path traversal, hardlinks, device nodes.
2. **Snapshot loader and crypto** — `runtime/pullrun-runtime/src/snapshot_crypto.rs`,
   `snapshot_pages.rs`, `runtime/pullrun-vm/src/snapshot.rs`. Key handling,
   convergent page nonces, tamper handling, restore path, fork TAP overrides.
3. **Vsock and guest init** — `runtime/pullrun-vsock`, `runtime/pullrun-init`,
   frame parsing, command/env injection, group supervisor.
4. **Vsock/CRI streaming** — `cri/pullrun-cri/streaming.go`, SPDY/WebSocket
   handling, TTY/resize, port-forward, exit-status protocol.
5. **Egress enforcement** — `runtime/pullrun-vm/src/egress.rs`,
   `runtime/pullrun-exec/src/container.rs` nftables paths,
   `runtime/pullrun-net/src/egress_proxy.rs`. Allowlist parsing, DNS,
   credential injection, CA rotation, audit log prefixes.
6. **Image policy** — signatures, SBOM/CVSS, seccomp, capabilities,
   privileged/read-only/no-new-privileges handling.

## Out of scope

- The Linux kernel, Firecracker, runc, Apple Virtualization, and their CVEs.
- Registry trust and image contents.
- The stub control plane and multi-node scheduling (not implemented).
- Availability/DoS beyond crash-to-fail-closed behavior.

## Questions for the auditor

- Can a malicious layer escape the extraction root via any path/symlink/race?
- Can a crafted snapshot or page index cause code execution or host escape?
- Can a guest escape via vsock framing, env, mount tags, or group supervisor?
- Can a streaming client escape the CRI sandbox via TTY/resize/port-forward?
- Does the egress policy fail closed under DNS, proxy, and nft rule edge cases?
- Does CA rotation leak secrets or accept old clients unintentionally?

## Evidence to provide

- Test suites: `cargo test --workspace`, CRI conformance 50/50, fuzz target
  (`runtime/pullrun-oci/fuzz`), snapshot tamper tests, egress allow/deny tests.
- `docs/SECURITY_STATUS.md` for the honest current matrix.
- This scope and the exact commit under review.
