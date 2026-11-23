# Business focus (2026 review)

The review’s conclusion: the substrate is stronger than the positioning, and
the positioning is stronger than adoption. The business plan is therefore
narrow on purpose.

## Two lanes

### 1. OEM / embed

**Buyer:** an agent platform (coding agent, workflow builder, vertical agent
product) that already has customers but needs stronger isolation than shared
containers.

**What they buy:** a license to embed `pullrun-runtime` + MCP control, bringing
their own compute. Differentiators: one store across containers/VMs, ~150 ms
memory restore, fork for speculative runs, air-gap capability.

**Motion:** design partner → OEM agreement; no hosted control plane required.

### 2. Enterprise on-prem / air-gapped

**Buyer:** regulated or sovereign fleets that cannot use US-hosted sandbox
APIs (defense, health, finance, public sector).

**What they buy:** per-node/per-cluster license, offline install, P2P image
distribution, CRI runtime, policy gates, support.

**Motion:** pilot on their hardware → production fleet; channel/SI partners.

## Explicitly deferred

- Generic hosted sandbox API (competes with E2B, Modal, Daytona, Cloudflare).
- Compose/build/control-plane expansion; Windows/WSL2 depth.
- A per-seat developer SaaS.

## Proof required before scaling either lane

1. Fleet-grade snapshots (networked restore done; guest reseed + incremental
   capture open).
2. Python/TS SDKs (done; Python uses opt-in TCP until UDS is fixed).
3. VM egress (done for bridged VMs; audit/CA rotation open).
4. One 500–1,000 actor demo on a single box with dedup'd snapshots.
5. Third-party audit of the escape surface.

## Metrics that matter

- Time-to-first-sandbox in a customer’s environment.
- Restore/fork p50 and p95.
- Disk and host-RAM per 100 actors.
- Number of external teams running real workloads (not stars).
