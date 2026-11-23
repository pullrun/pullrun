# Good first issues

These are scoped, public, and useful. Each has a clear acceptance test.

1. **Python gRPC over UDS** — tonic returns `RST_STREAM` to grpc-core, while
   Go/Node work over the same socket. Make Python work over UDS (or document a
   sanctioned TCP mode and remove the footgun).
2. **Guest HTTPS through credential proxy** — a VM gets proxy env + CA but
   HTTPS requests from a curl image still time out. Find the connection-level
   stall and add an integration test with a local TLS upstream.
3. **Fork guest re-seed** — after restore, IP/MAC/entropy/clock are inherited.
   Add a guest-side re-seed channel and a test proving unique identity across
   two forks.
4. **Linux sandbox groups** — the guest supervisor supports Linux, but the host
   path is Apple-only. Add a Firecracker group backend and a 10-sandbox demo.
5. **Page-store accounting** — expose page-store bytes/dedup ratio via metrics
   and `store info`.
6. **CRI full-suite triage** — run the 142-spec suite with a 50-minute test
   timeout, classify failures into optional/feature/real, and publish the list.

## How to contribute

- Read `docs/AUDIT_SCOPE.md` and `docs/SECURITY_STATUS.md` first.
- Keep claims measured; add a test or benchmark with each change.
- `CONTRIBUTING.md` has build/test instructions.
