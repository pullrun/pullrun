# Pullrun Enterprise & Commercial Strategy

## Philosophy: Don't Monetize the Runtime — Monetize the Organization Around It

### What Docker Did Wrong (and How Pullrun Avoids It)

| Docker's Mistake | Why It Failed | Pullrun's Approach |
|---|---|---|
| **Seat-based pricing for CLI** ($5-$21/dev/mo) | Developers revolted, migrated to Podman, created widespread resentment | **CLI is always free and open source**. No per-seat pricing. No "Docker Desktop requires subscription." |
| **Open core with artificially crippled free version** | Community felt baited, forked projects, ecosystem fragmented | **Free tier gets 100% of core runtime**. No artificial limitations. |
| **Proprietary plugins for extensibility** | Lock-in, developer distrust, ecosystem stagnation | **MCP server is open, P2P is open, CRI is open**. Plugin architecture is fully OSS. |
| **Closed-source Desktop features** | Enterprises couldn't audit, security teams rejected | **Desktop core is open source**. Enterprise features are add-on services, not gatekeepers. |
| **Vendor lock-in (Docker Hub, BuildKit)** | Can't migrate, can't self-host without paying | **Everything works offline**. Registry is any OCI registry. Build engine is in-tree. |

### What Red Hat Did Wrong (and How Pullrun Avoids It)

| Red Hat's Mistake | Why It Failed | Pullrun's Approach |
|---|---|---|
| **Per-seat subscriptions for RHEL** ($799/server/yr) | Cloud-native world moved to Ubuntu/Debian. RHEL became irrelevant for new workloads. | **No per-seat pricing for the engine**. Pay for what you use (workloads, not humans). |
| **Upstream vs downstream confusion** | Fedora → RHEL lag, community felt second-class | **Single source of truth**. No "community edition" vs "enterprise edition" divergence. |
| **Acquired and sunset beloved projects** | CentOS Stream fiasco destroyed trust | **All code is MIT/Apache-2.0**. Forks are welcome. The company provides value through services, not code lock-in. |
| **Kubernetes distro tax** | OpenShift is just K8s + markup, customers know it | **Pullrun is a genuine differentiator** (VM + container from same image, DAG store, P2P). Can't be replicated by a K8s wrapper. |

---

## The Rule of Thumb

> **If a developer needs it to write code, it's free.**
> 
> **If an organization needs it to operate at scale, it's a paid feature.**

This means:
- **Individual developer on their laptop**: everything works, everything is free
- **Small team (≤5)**: everything works, everything is free
- **CI/CD pipeline**: everything works, everything is free
- **Enterprise (>100 devs, multi-team, compliance, audit, etc.)**: paid features unlock

---

## Free Tier (OSS): The Entire Runtime

### Command Line (CLI) — 100% Free

```
pullrun pull              ✅ FREE
pullrun run               ✅ FREE
pullrun run --backend vm   ✅ FREE (this is the killer feature — always free)
pullrun stop              ✅ FREE
pullrun exec              ✅ FREE
pullrun attach            ✅ FREE
pullrun list              ✅ FREE
pullrun inspect           ✅ FREE
pullrun logs              ✅ FREE
pullrun stats             ✅ FREE
pullrun build             ✅ FREE
pullrun push              ✅ FREE
pullrun save/load         ✅ FREE
pullrun commit            ✅ FREE
pullrun diff              ✅ FREE
pullrun update            ✅ FREE
pullrun cp                ✅ FREE
pullrun network create    ✅ FREE
pullrun secret create     ✅ FREE
pullrun config create     ✅ FREE
pullrun login/logout      ✅ FREE
pullrun compose up/down   ✅ FREE
pullrun prune             ✅ FREE
pullrun info/version      ✅ FREE
pullrun mcp               ✅ FREE (MCP server)

# P2P Sync (everything)
pullrun sync daemon       ✅ FREE
pullrun sync join         ✅ FREE
```

### Desktop Application — Core Free

```
Dashboard                 ✅ FREE
Workload Management       ✅ FREE
Image Management          ✅ FREE
Network Management        ✅ FREE
Compose Support           ✅ FREE
Build Engine              ✅ FREE
Policy Engine (local)     ✅ FREE
Secret/Config Management  ✅ FREE
Local Metrics             ✅ FREE
Logs & Events            ✅ FREE
AI Agent (MCP)           ✅ FREE (stdio mode)
```

### Why This Works

**A developer never hits a paywall.** They never see "upgrade to Pro" when trying to spin up a container. This is what Docker got wrong — they made the CLI the paywall, which is the exact interface developers use every day.

**The result**: Developers love Pullrun. They adopt it naturally. They tell their teams. Their teams tell their companies. The companies are the ones who need the enterprise features.

---

## Paid Enterprise Tier: What Organizations Pay For

The Enterprise tier monetizes **orchestration**, **scale**, **compliance**, and **centralization**. These are things individual developers don't need, but organizations with 100+ developers absolutely do.

### 1. Pullrun Enterprise Hub (SaaS) — $49/seat/mo (minimum 50 seats)

The Hub is a cloud service that sits in front of multiple Pullrun nodes (developer laptops, CI runners, prod servers) and provides:

#### Team & Workspace Management
- **Multi-team workspaces**: Organize developers into teams with role-based access
- **Workspace isolation**: Team A can't see Team B's workloads
- **SSO/SAML integration**: Okta, Azure AD, Google Workspace
- **Audit logging**: Every pull, run, push logged to centralized SIEM
- **API keys with scopes**: "This CI pipeline can only push, not pull from registry X"

#### Centralized Policy as Code
- **Remote policy engine**: Policies defined in the Hub, enforced on every node
  ```yaml
  # .pullrun/policy.yaml in Hub
  required_signature: true
  trusted_keys:
    - ghcr.io/my-org/*
  max_cvss: 5.0
  deny_licenses: [GPL-3.0, AGPL-3.0]
  required_labels: [approved-by-security]
  ```
- **Policy versioning**: Roll back to previous policy version if a new policy breaks builds
- **Policy impact analysis**: "This new policy would have blocked 23 of last week's 45 image pulls"
- **Override workflows**: "Developer requests exception for image X" -> Security team approves in Hub -> Policy updated temporarily

#### Compliance & Reporting
- **Compliance dashboard**: SLSA Level 3+ attestation, policy adherence rate, incident timeline
- **License audit**: "Your org has 47 images containing GPL-3.0 code. Here's the breakdown."
- **Vulnerability posture**: CVSS distribution across all workloads, trend analysis
- **Export to GRC tools**: Splunk, Datadog, Elastic, custom webhook

#### Team P2P Registry (not the public Docker Hub)
- **Private DAG store sync**: Teams sync images between offices without routing through public registry
- **Bandwidth throttling**: "Sync max 1 Gbps during business hours"
- **Deduplication reports**: "Your 200-node cluster shares 87% of image content. Total unique storage: 12 GB."

**Pricing**: $49/seat/month, minimum 50 seats. Annual billing (10% discount).

---

### 2. Pullrun Cloud (Managed Pullrun) — Usage-Based

For organizations that don't want to manage the daemon themselves:

#### Managed Pullrun Runtime
- **Cloud-hosted daemon**: We run `pullrun-runtime` for you. You just use the CLI.
- **Auto-scaling VM pool**: VMs spin up and down based on workload demand (like Lambda, but for VMs)
- **Global edge presence**: VMs start from the closest PoP (like Cloudflare Workers, but with full VM isolation)
- **Zero-downtime updates**: We manage daemon updates, policy rollouts, kernel patches

#### Pricing Model
- **Per-hour per-VM**: $0.05/hr for a 2 vCPU / 1GB VM
- **Per-GB storage**: $0.10/GB/month for the DAG store
- **Per-GB transfer**: $0.09/GB for P2P sync across regions

**Example**: A CI pipeline that runs 100 VM workloads/day for 10 minutes each = 1000 min/day = ~16.7 hrs = **$0.84/day**.

---

### 3. Pullrun Shield (Security) — $999/node/yr

Standalone security product for organizations that already have a container runtime but want Pullrun's policy engine.

#### Features
- **Universal policy engine**: Works with Docker, Podman, containerd, AND Pullrun
- **SBOM generation & scanning**: Automatic CycloneDX/SLSA generation on every build
- **Supply chain attestation**: Sign every file, not just images. SLSA Level 3+ compliance
- **Runtime threat detection**: Anomalous workload behavior (from our open `pullrun-runtime` metrics)
- **Incident response**: Auto-quarantine workloads that violate policy post-start

**No per-seat pricing**. Per-node. If you have 50 K8s nodes: 50 x $999 = $49,950/yr.

---

### 4. Pullrun Desktop: Free vs Enterprise

| Feature | Desktop Free | Desktop Enterprise ($199/dev/yr) |
|---------|-------------|--------------------------------|
| **Workload Management** | ✅ Full | ✅ Full |
| **Image Management** | ✅ Full | ✅ Full |
| **Team P2P Sync** | ✅ mDNS discovery on LAN | ✅ Global P2P with Hub |
| **Policy Engine** | ✅ Local policies only | ✅ Synced from Hub |
| **Metrics** | ✅ Local Prometheus | ✅ Hub-aggregated dashboards |
| **Log Streaming** | ✅ Local only | ✅ Centralized with Hub |
| **AI Agent** | ✅ Stdio, local only | ✅ Hub-connected, team-shared context |
| **Build Caching** | ✅ Local cache only | ✅ Distributed cache across team |
| **Multi-team** | ❌ | ✅ Workspace switching |
| **SSO** | ❌ | ✅ Okta, Azure AD, etc. |
| **Audit Logging** | ❌ | ✅ Full audit trail |
| **Support** | ❌ Community | ✅ Enterprise (24/7) |
| **Training** | ❌ Self-service docs | ✅ On-site training |

**Why per-developer for Desktop Enterprise?** Because it's an individual productivity tool. The value is per-person. But:
- The free version is **fully functional** for individual use
- The $199/yr is less than Docker Desktop's $5/mo ($60/yr) but provides enterprise-grade security
- No seat management for small teams (< 5 devs get it free)

---

## What Is NOT Gated (The Line in the Sand)

These features will **never** be paid. They are core to Pullrun's mission and enabling developers:

| Feature | Why It Must Stay Free |
|---------|----------------------|
| **Container/VM execution** | The entire point of Pullrun. Gating this kills adoption. |
| **DAG store** | The core innovation. If it's not free, nobody uses Pullrun. |
| **Content-addressed storage** | Same reason. This is the moat; it must be open. |
| **P2P sync** | Community scaling. Without it, Pullrun is just another Docker. |
| **MCP server** | AI is the future. Locking this behind a paywall prevents adoption. |
| **OCI build/push/pull** | Basic container operations. Table stakes. |
| **Secrets/Configs** | Basic ops. Everyone needs these. |
| **Networking** | Basic networking. Free. |
| **Compose** | Developer productivity. Must be free. |
| **Policy engine (local)** | Security is not a luxury. Local policy enforcement is free. |
| **Metrics (local)** | Observability is not a luxury. Local metrics are free. |
| **CRI shim** | Kubernetes integration. Must be free for adoption. |

## Cloud Provider Model: How Pullrun Works with AWS, Google, Azure, Scaleway, etc.

### The Cloud Provider Dilemma

Cloud providers (AWS, GCP, Azure, Scaleway, DigitalOcean, etc.) are **massive users** of container runtimes. They offer managed Kubernetes, container services, and CI/CD to millions of customers. If Pullrun gains traction, they will want to adopt it — either as a customer-facing service or as an internal runtime.

**The problem**: Cloud providers are not going to pay $49/seat/month for Pullrun. They have thousands of nodes, not thousands of developers. They need a different model.

### The MongoDB/Elastic Lesson

| Company | What Happened | Result |
|---|---|---|
| **MongoDB** | AGPL licensed. AWS launched Amazon DocumentDB (fork). | MongoDB had to relicensing to SSPL to prevent AWS from offering MongoDB-as-a-Service without contributing back. |
| **Elastic** | Apache 2.0 licensed. AWS launched Amazon OpenSearch (fork). | Elastic relicensed to SSPL + Elastic License. Community backlash. 
| **Redis** | BSD licensed. AWS launched Amazon ElastiCache (fork). | Redis Labs added Commons Clause, then RSAL. Community backlash. |
| **Docker** | Docker Hub had proprietary parts. AWS launched Amazon ECR, Google GCR. | Docker lost registry market share. |

**The pattern**: Cloud providers adopt open-source projects, offer them as managed services, and the original company gets nothing.

### Pullrun's Answer: The Cloud Provider License (CPL)

Pullrun uses a **dual licensing model** that allows cloud providers to adopt the runtime while generating revenue:

#### 1. Pullrun Core (Open Source)
- **License**: MIT/Apache-2.0
- **What**: The full runtime, CLI, CRI shim, P2P sync, MCP server
- **Who can use**: Anyone, including cloud providers
- **Restriction**: NONE. Cloud providers can ship it, fork it, modify it.

#### 2. Pullrun Cloud Provider License (CPL) — Special Terms for Cloud Providers

This is a **separate license** for cloud providers who want to offer **Pullrun-as-a-Service** (managed Pullrun) to their customers:

```
Pullrun Cloud Provider License (CPL)

1. GRANT: Cloud Provider may offer Pullrun-as-a-Service to its customers.
2. CONSIDERATION: Cloud Provider pays Pullrun Technologies Inc. a 
   percentage of revenue from Pullrun-based services.
3. PERCENTAGE: 10% of gross revenue from Pullrun-based services.
4. MINIMUM: $25,000/quarter minimum commitment.
5. REPORTING: Quarterly revenue reports within 15 days of quarter end.
6. BRANDING: Must include "Powered by Pullrun" in marketing materials.
7. SUPPORT: Priority technical support, access to roadmap, early access 
   to new features.
8. NON-EXCLUSIVE: Multiple cloud providers may hold CPL simultaneously.
```

**What this gives cloud providers**:
- Legal certainty to offer Pullrun-as-a-Service
- Priority support (critical for production services)
- Early access to new features (competitive advantage)
- Marketing co-operation ("Powered by Pullrun")
- No hostility from the Pullrun community

**What this gives Pullrun**:
- Revenue from every cloud provider offering Pullrun
- No need to become a cloud provider ourselves
- Cloud providers become advocates, not adversaries
- Community stays healthy (no SSPL backlash)

#### 3. What Cloud Providers Get for Free (No CPL Required)

Cloud providers can do the following **without any license**:

| Use Case | Requires CPL? | Why |
|---|---|---|
| Run Pullrun internally for their own infrastructure | ❌ No | Internal use is always free |
| Offer Pullrun as a managed service to customers | ⚠️ Yes | Revenue share model |
| Fork Pullrun and modify for their own needs | ❌ No | MIT allows forks |
| Ship Pullrun in their Linux distribution | ❌ No | Packaging is free |
| Use Pullrun in their CI/CD | ❌ No | Free for CI/CD |
| Offer Pullrun image mirroring | ❌ No | Registry operations are free |
| Build a container platform on top of Pullrun | ⚠️ Yes | If charging for it |
| Include Pullrun in managed Kubernetes | ⚠️ Yes | If it's a paid feature |

### 4. The "AWS DocumentDB" Prevention Strategy

**The fear**: AWS forks Pullrun, offers "AWS Pullrun", and Pullrun gets no revenue.

**The answer**:

a) **Pullrun is not just a runtime — it's a network effect**.
   - The DAG store is content-addressed. The more nodes share the DAG, the more valuable the network.
   - AWS offering a fork would be a **worse experience** because it wouldn't share the global P2P network.
   - Customers would prefer "official Pullrun" because it has the network.

b) **The Hub creates a moat**.
   - The Enterprise Hub is a SaaS product. AWS can't offer the Hub without a CPL.
   - Organizations using AWS Pullrun would ask: "Where's the team sync? Where's the policy engine?"
   - AWS would have to build their own. They don't want to.

c) **The community effect**.
   - Developers trust the "real" Pullrun. An AWS fork would be seen as "yet another AWS lock-in."
   - Pullrun's brand is "the open, portable runtime." AWS can't replicate that.

### 5. Realistic Cloud Provider Adoption Scenarios

#### Scenario A: AWS Offers "AWS Pullrun"

```
AWS announces "AWS Pullrun" — a managed VM + container service.
- Built on Pullrun open source
- Integrated with AWS networking, IAM, CloudWatch
- Customer pays AWS usage fees
- AWS pays Pullrun 10% of Pullrun-related revenue

Customer perspective:
- "I get Pullrun VMs with AWS networking and IAM."
- "I can still use the same CLI on my laptop."
- "I can migrate to GCP Pullrun or self-hosted Pullrun anytime."
```

**Pullrun revenue**: AWS generates $10M/yr from Pullrun-based services → Pullrun gets $1M/yr.

#### Scenario B: GCP Offers "Google Pullrun Cloud"

```
GCP announces Google Pullrun Cloud:
- Managed Pullrun with Google Kubernetes Engine integration
- Native support for Google Artifact Registry
- Deep integration with Google Cloud Monitoring

Customer perspective:
- "I get Pullrun VMs with Google networking."
- "Same CLI, same DAG store, but Google-managed."
```

**Pullrun revenue**: GCP generates $5M/yr → Pullrun gets $500K/yr.

#### Scenario C: DigitalOcean / Scaleway Offers "Managed Pullrun"

```
DigitalOcean offers Managed Pullrun:
- $5/mo for a small VM workload
- Integrated with DO's networking and DNS
- Simple pricing, no surprises
```

**Pullrun revenue**: $50K/yr (small, but contributes to ecosystem).

### 6. What Cloud Providers Would Pay For (CPL Add-Ons)

Beyond the base 10% revenue share, cloud providers want:

| Add-On | Description | Price |
|---|---|---|
| **Marketplace Listing** | Featured placement in cloud provider marketplace | $25,000/year |
| **Joint Marketing** | Co-branded webinars, case studies, white papers | $50,000/year |
| **Technical Integration** | Custom APIs, IAM integration, billing hooks | $100,000/year |
| **Dedicated Support** | 24/7 support with SLA, dedicated engineer | $50,000/year |
| **Training & Certification** | Cloud provider engineers get Pullrun-certified | $10,000/session |
| **Roadmap Influence** | Vote on features, priority access to betas | Included in CPL |

### 7. The Self-Hosted Organization Model

Not every organization wants to use a cloud provider. Some want to self-host Pullrun on bare metal or their own cloud:

| Organization Type | How They Use Pullrun | What They Pay For |
|---|---|---|
| **SMB (10-50 devs)** | Self-hosted on VPS | Free (Hub optional at $49/seat/mo) |
| **Mid-size (50-500 devs)** | Self-hosted on private cloud | Hub + Shield |
| **Enterprise (500+ devs)** | Self-hosted on bare metal + multi-region | Hub + Shield + Support |
| **Government** | Air-gapped, self-hosted | Shield + Compliance + Audit |
| **Cloud Provider** | Offered as SaaS to their customers | CPL (10% revenue share) |

### 8. Why Cloud Providers Would Actually Sign CPL

**Argument for AWS/GCP/Azure**: Why pay Pullrun when they can fork it for free?

**Answer**:

1. **Brand and Trust**:
   - Customers trust "official Pullrun" more than "AWS Pullrun Fork"
   - "Powered by Pullrun" is a selling point
   - An unlicensed fork looks bad (see: OpenSearch backlash)

2. **Network Effects**:
   - The P2P sync network is global. A fork is isolated.
   - A fork can't leverage the shared DAG store.
   - Customers on different cloud providers can't sync images.

3. **Support and Updates**:
   - CPL includes priority support and access to new features
   - Maintaining a fork is expensive (engineer time)
   - AWS knows this: maintaining OpenSearch is costing them millions

4. **Community Goodwill**:
   - Contributing back to Pullrun builds goodwill
   - Being seen as a good open-source citizen helps hiring
   - No one likes the "embrace, extend, extinguish" narrative

5. **Legal Risk**:
   - Even though MIT allows forks, there's reputational risk
   - "AWS copied Pullrun" → bad PR
   - "AWS partners with Pullrun" → good PR

---

## How to Enforce Licensing Without Being Evil

### Model 1: Open Core with Clear Boundaries (Recommended)

```
Core (OSS):
├── runtime/          (Rust workspace)
├── cli/              (Go CLI)
├── proto/            (gRPC definitions)
├── cri/              (Kubernetes CRI shim)
├── mcp/              (AI agent integration)
└── desktop/          (Basic desktop app)

Enterprise (Proprietary, but with clear API):
├── hub/               (SaaS — auth, policy, audit)
├── shield/            (Security scanning — standalone binary)
└── desktop-enterprise/  (SSO, team sync, Hub integration)
```

**Rule**: The Enterprise features are **built on top of** the OSS core, not **replacing** it. If the Enterprise binary vanished, the OSS core would still run your workloads.

### Model 2: Dual Licensing

```
Community: MIT/Apache-2.0 (do anything)
Enterprise: Elastic License 2.0 (source available, no SaaS competition)
```

This allows competitors to fork the OSS but prevents them from offering Pullrun-as-a-Service without contributing back.

### Enforcement: Graceful Degradation

When an Enterprise feature is not licensed, **don't crash, don't block**. Inform:

```bash
$ pullrun team sync --auto
ℹ️  Auto-sync requires Pullrun Enterprise Hub.
   Your workloads are running fine. To enable team-wide P2P sync,
   connect to a Hub: pullrun login --hub https://hub.pullrun.io
   
   Free tier: Manual sync via `pullrun sync daemon` + `pullrun sync join`
   Enterprise: Auto-sync, policy enforcement, audit logging
```

---

## Why This Strategy Wins

| Stakeholder | Docker's Approach | Pullrun's Approach | Result |
|---|---|---|---|
| **Individual developer** | "Pay $5/mo or no Desktop" | "Everything's free. Use it." | Love. Viral adoption. |
| **Small team (< 10)** | "Pay $5/mo x 10 = $50/mo" | "Everything's free. 
  <10 devs." | Zero friction. Easy choice. |
| **Startup (10-50)** | Expensive, legal review | "$49/seat/mo for Hub. Optional." | Can grow without worry. |
| **Enterprise (500+)** | "Pay $21/seat/mo or else" | "Pay for Hub, Shield, and Cloud. 
  Runtime is free." | Technical decision, not procurement fight. |
| **Security team** | "Buy Docker Scout separately" | "Shield is included with Hub. 
  Free tier still has policies." | No separate budget line. |
| **Compliance officer** | "No audit trail" | "Full audit trail in Hub. 
  Or export to SIEM." | Passes audits out of the box. |

---

## The Anti-Pattern Checklist

### ❌ What NOT to Do (Learned from Docker/Red Hat)

| Anti-Pattern | Why It's Bad | Pullrun's Rule |
|---|---|---|
| **Gate the CLI** | Kills adoption. Developers are the users — they must never see a paywall. | CLI is always 100% free. |
| **Gate the runtime** | Prevents usage. The runtime is the product. | Runtime is always 100% free. |
| **Artificial limits** | "Free: 1 VM, Pro: 10 VMs, Enterprise: Unlimited" breeds resentment. | Free: unlimited VMs, unlimited workloads, unlimited storage. |
| **Freemium crippling** | "Free: no secrets, no networks" — forces upgrade. | Everything needed for local dev is free. |
| **Seat-based runtime pricing** | "You can run containers, but only if you pay per developer" | Runtime is free. Enterprise Hub is per-seat for the service, not the runtime. |
| **Proprietary formats** | `.dmg`, `.msi`, `.deb` — all bundled with telemetry, no audit | All formats are open source. Build from source if you want. |
| **Surprise billing** | "Oops, you used $500 of resources" | All pricing is transparent. No surprise bills. |

### ✅ What TO Do

| Pattern | Why It Works |
|---|---|
| **Bake value into the free tier** | "Everything works locally" — developers love it, bring it to work. |
| **Monetize scale, not creation** | A developer creating a container is free. A company managing 500 containers needs Hub. |
| **Monetize security and compliance** | Security teams have budget. They're happy to pay for Shield. Developers don't pay. |
| **Monetize convenience** | "You CAN self-manage, OR you can pay us to manage it for you" (Cloud). |
| **Transparent pricing** | List prices on the website. No "contact sales" for basic plans. |
| **Credit card, not contract** | SMBs can pay by credit card. Enterprise can do PO. Both are available. |

---

## The Long Game: How Pullrun Becomes a Billion-Dollar Company

### Year 1-2: Developer Love (Free)
- 100% free for developers
- Build the best container/VM runtime in the world
- Get pullrun into every Linux distro, every cloud image, every CI pipeline
- Revenue: $0 (or minimal from early Enterprise adopters)

### Year 2-3: Team Adoption (Hub)
- Teams of 10-50 start using Hub ($49/seat/mo)
- Revenue: $10K-$100K MRR
- Focus: Team P2P, policy enforcement, basic audit

### Year 3-4: Enterprise Land (Shield + Cloud)
- Large enterprises adopt Shield for compliance ($999/node/yr)
- Government, healthcare, finance (regulated industries)
- Cloud offering for CI/CD pipelines (usage-based)
- Revenue: $1M-$10M ARR

### Year 4-5: Platform Play
- Pullrun becomes the default runtime for edge, IoT, and serverless
- P2P block sync replaces Docker registry as the primary distribution method
- Pullrun Cloud competes with AWS Lambda (VM-based, not container-based)
- Revenue: $50M+ ARR

### Year 5+: Exit or IPO
- Acquisition by a major cloud provider (AWS, Google, Azure)
- Or: IPO as the "GitHub for infrastructure" — the default runtime for the cloud-native era

---

## Summary

| Tier | Who Pays | What They Get | Price |
|---|---|---|---|
| **Free (OSS)** | No one. Developer tools are free. | Full runtime, CLI, Desktop, P2P, MCP, CRI | $0 |
| **Hub (SaaS)** | Teams/Organizations | Team sync, policy as code, audit, SSO, centralized metrics | $49/seat/mo |
| **Shield (Security)** | Security-conscious orgs | Universal policy engine, SBOM, attestation, threat detection | $999/node/yr |
| **Cloud (Managed)** | Orgs that don't want to manage infrastructure | Managed runtime, auto-scaling, global edge | Usage-based |
| **Desktop Enterprise** | Individual devs in orgs | Desktop with Hub integration, SSO, support | $199/dev/yr |
| **Cloud Provider** | Cloud providers (AWS, GCP, Azure, etc.) | License to offer Pullrun-as-a-Service | 10% revenue share |

**The Golden Rule**: 

> *The developer who writes the code should never pay for the tools to run it. The organization that deploys the code at scale should happily pay for the tools to manage it.*

This is how Pullrun avoids Docker's fate (developer revolt) and Red Hat's fate (irrelevance). By making the core free and charging for the organizational layer, Pullrun becomes the default — and the default is where the money is.
