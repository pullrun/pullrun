# Pullrun Licensing Strategy

## The Core Question

Pullrun has three very different commercial models coexisting in one codebase:

1. **Individual developers** use Pullrun for free (MIT/Apache-2.0)
2. **Organizations** pay for the Hub, Shield, and Cloud services (SaaS)
3. **Cloud providers** (AWS, GCP, Azure, Scaleway, etc.) want to offer Pullrun-as-a-Service to THEIR customers

The license must protect the project's commercial viability while maintaining developer adoption and community trust.

---

## The Landscape of Open Source Licenses

### Permissive Licenses (MIT, Apache-2.0, BSD)

| License | What It Allows | Commercial Use | Forking | SaaS Offering | Key Risk |
|---|---|---|---|---|---|
| **MIT** | Everything | Yes | Yes | Yes | Cloud providers can offer without paying |
| **Apache-2.0** | Everything + patent grant | Yes | Yes | Yes | Cloud providers can offer without paying |
| **BSD-3-Clause** | Everything | Yes | Yes | Yes | Cloud providers can offer without paying |

**Conclusion**: MIT/Apache-2.0 alone are **too permissive** for Pullrun's business model. AWS could fork and offer "AWS Pullrun" without contributing back or paying.

### Copyleft Licenses (GPL, AGPL)

| License | What It Requires | Commercial Use | Forking | SaaS Offering | Key Risk |
|---|---|---|---|---|---|
| **GPL-2.0/3.0** | Share source on distribution | Yes | Yes (must share source) | Yes (if SaaS code is distributed) | Weak SaaS protection |
| **AGPL-3.0** | Share source on distribution + SaaS use | Yes | Yes (must share source) | **Must share SaaS source code** | Strong SaaS protection, but strong developer resistance |

**Conclusion**: AGPL would protect against AWS, but:
- Developers at companies with GPL policies may be blocked from using Pullrun
- Some enterprises have "no AGPL" policies
- MongoDB learned this the hard way: AGPL didn't stop DocumentDB, but it DID create developer friction

### Source-Available / Business Source Licenses (BSL, Elastic, SSPL)

| License | What It Allows | Forking | SaaS Offering | Community Risk |
|---|---|---|---|---|
| **Business Source License (BSL)** | Free for non-production | Yes | **No without converting to AGPL** | Medium backlash when "time bomb" hits |
| **Elastic License 2.0 (ELv2)** | Free for anything except SaaS competition | Yes | **No** | **High backlash** (Elastic vs OpenSearch) |
| **SSPL (Server Side Public License)** | Free for non-SaaS use | Yes | **Must share EVERYTHING (hosting infra, etc.)** | **Very high backlash** |
| **PolyForm Noncommercial** | Free for noncommercial use only | Yes | **No for commercial SaaS** | High backlash for commercial devs |

**Conclusion**: These have proven to **destroy community trust**. The moment you adopt SSPL or Elastic License, you become "the company that betrayed open source."

---

## The Pullrun Licensing Model: A Three-Tier Approach

### Tier 1: Pullrun Core — MIT/Apache-2.0

**What**: The runtime, CLI, CRI shim, P2P sync, MCP server, DAG store, all crates

**License**: **Apache-2.0** (with MIT as secondary for maximum compatibility)

```
Copyright 2026 Pullrun Technologies Inc.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
```

**Why Apache-2.0 over MIT**:
- Patent grant (Section 3): Contributors grant patent rights to users
- Explicit trademark protection (Section 6): "Pullrun" brand is protected
- Better for enterprise adoption (patent grants matter to legal teams)
- Compatible with GPLv2+ (MIT is also compatible, but Apache-2.0 is more explicit)

**What this enables**:
- ✅ Developers use Pullrun for free, forever
- ✅ Companies embed Pullrun in their products
- ✅ Cloud providers can offer Pullrun (but see Tier 3)
- ✅ Academics and researchers can study and extend the code
- ✅ Distribution in Linux distros, Docker images, etc.

**What this does NOT prevent** (and why Tier 3 exists):
- ❌ A cloud provider offering Pullrun-as-a-Service without contributing back

---

### Tier 2: Pullrun Enterprise Add-Ons — Proprietary (Closed Source)

**What**: The Hub, Shield, and Cloud services are **separate, proprietary services** that talk to the open-source core via the same gRPC API.

**License**: Proprietary (not open source)

**Architecture**:
```
┌─────────────────────────────────────────────────────────────┐
│  Pullrun Enterprise Hub (Proprietary, Closed Source)         │
│  ├── SaaS web application                                    │
│  ├── gRPC client to Pullrun Core (OSS)                     │
│  └── Database, auth, billing, etc.                        │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼ via gRPC (standard API)
┌─────────────────────────────────────────────────────────────┐
│  Pullrun Core (Apache-2.0, Open Source)                     │
│  ├── pullrun-runtime (gRPC server)                           │
│  ├── DAG store                                              │
│  ├── P2P sync                                               │
│  └── Policy engine                                          │
└─────────────────────────────────────────────────────────────┘
```

**Key insight**: The Enterprise add-ons are **not a modified version of the core**. They are separate software that uses the core's public API. This is critical:
- No "open core" bait-and-switch
- No crippled free version
- The OSS core is 100% functional without the Hub
- The Hub is a value-added service, not a gatekeeper

---

### Tier 3: Pullrun Cloud Provider License (CPL) — Elastic License 2.0 (ELv2) for Hub/Shield Components

**The Problem**: Cloud providers could theoretically build their OWN Hub-equivalent using the OSS core, completely bypassing Pullrun Technologies Inc.

**The Solution**: The Hub and Shield services are licensed under **Elastic License 2.0 (ELv2)**.

**What is ELv2?**
- Free to use, modify, and distribute for ANY purpose EXCEPT offering it as a managed service to third parties
- Source code is fully available (source-available, not open source)
- Prevents the "AWS DocumentDB" problem
- Incompatible with GPL/AGPL (so no combining with copyleft code)

**ELv2 Text (Simplified)**:
```
Elastic License 2.0

You may not:
1. Provide the software to third parties as a hosted or managed service
   where the service provides users with access to any substantial set of
   features or functionality of the software.
2. Do anything that would require you to make the source code available.
```

**When does ELv2 apply to Pullrun?**

| Component | License | Why |
|---|---|---|
| **pullrun-runtime** (Rust daemon) | Apache-2.0 | Runtime must be free |
| **CLI** | Apache-2.0 | CLI must be free |
| **CRI shim** | Apache-2.0 | K8s integration must be free |
| **MCP server** | Apache-2.0 | AI integration must be free |
| **P2P sync** | Apache-2.0 | Network effects must be free |
| **Desktop Core** | Apache-2.0 | Desktop must be free |
| **Hub (SaaS)** | Proprietary + ELv2 for client libs | Closed-source service, but client libs are source-available |
| **Shield (security)** | Proprietary | Closed-source security product |
| **Desktop Enterprise** | Proprietary with OSS core | Enterprise features on top of OSS core |
| **Cloud API (gRPC extensions for multi-tenant)** | ELv2 | Extensions for multi-tenant use |

**The nuance**: The **core is Apache-2.0**. The **Enterprise add-ons are proprietary or ELv2**. Cloud providers can't offer the Hub or Shield without a commercial license.

---

## Why This Hybrid Model Works

### 1. For Developers

| Concern | How Pullrun Addresses It |
|---|---|
| "Can I use this for free?" | Yes. Apache-2.0 core, forever. |
| "Can I modify it?" | Yes. Fork, modify, redistribute. |
| "Can I embed it in my product?" | Yes. No restrictions. |
| "Is there a patent grant?" | Yes, Apache-2.0 Section 3. |
| "What about the 'open core' trap?" | Core is 100% functional. No artificial limits. |

### 2. For Enterprises

| Concern | How Pullrun Addresses It |
|---|---|
| "Can we self-host?" | Yes. Core is fully self-hostable. |
| "Can we audit the code?" | Yes. Core is open source. |
| "What if Pullrun Technologies goes away?" | Core is Apache-2.0. Community can fork. |
| "Can we pay for support?" | Yes. Enterprise support available. |

### 3. For Cloud Providers

| Concern | How Pullrun Addresses It |
|---|---|
| "Can we offer Pullrun-as-a-Service?" | Yes, with CPL (10% revenue share). |
| "Can we fork and modify?" | Yes, core is Apache-2.0. |
| "What if we don't want to pay?" | You can fork, but you can't use Hub/Shield without CPL. |
| "What's the value of paying?" | Official support, network effects, marketing co-op. |

---

## The GPL Compatibility Trap

**Important**: Apache-2.0 is **NOT** compatible with GPL-2.0. It IS compatible with GPL-3.0.

| Scenario | Outcome |
|---|---|
| Pullrun (Apache-2.0) + Linux kernel (GPL-2.0) | ✅ OK (user space, not linking to kernel) |
| Pullrun (Apache-2.0) + Btrfs (GPL-2.0) | ⚠️  Depends on how Btrfs is used (user space vs kernel) |
| Pullrun (Apache-2.0) + user space library (GPL-3.0) | ✅ OK |
| Pullrun (Apache-2.0) + user spacelevard (MIT) | ✅ OK |
| Pullrun (Apache-2.0) + Go library (Apache-2.0) | ✅ OK |
| Pullrun (Apache-2.0) + tool under SSPL | ❌ NOT OK (SSPL is a copyleft-like license) |

**Pullrun's dependencies are carefully chosen**:
- Rust: MIT/Apache-2.0 (cargo-licenses check)
- Go: MIT/Apache-2.0/BSD (go-licenses check)
- No GPL-2.0 dependencies
- No SSPL/Elastic/AGPL dependencies

---

## License Headers in Source Code

Every file in the Pullrun repository should have a license header:

### For Core (Apache-2.0)

```rust
// Copyright 2026 Pullrun Technologies Inc.
// SPDX-License-Identifier: Apache-2.0
// 
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
```

### For Enterprise/Proprietary

```rust
// Copyright 2026 Pullrun Technologies Inc.
// All rights reserved.
// 
// This software is property of Pullrun Technologies Inc.
// Unauthorised copying, distribution, or modification is prohibited.
// For licensing inquiries: enterprise@pullrun.io
```

---

## The Long-Term Strategy

### Year 1-2: Community First
- 100% of core is Apache-2.0
- Build developer trust
- No commercial licenses (Hub/Shield not yet built)
- Focus: adoption, adoption, adoption

### Year 3-4: Enterprise Layer
- Launch Hub and Shield as proprietary services
- Introduce CPL for cloud providers
- Core stays Apache-2.0
- No relicensing of existing code

### Year 5+: The Pullrun Foundation
- Transfer copyright of core to an independent foundation
- Pullrun Technologies Inc. retains copyright to Hub/Shield
- Foundation ensures core remains free forever
- Company profits from services, not code ownership

**This is the Red Hat model done right**:
- Red Hat owned the codebase (problem: vendor lock-in)
- Pullrun's foundation owns the core (solution: community governance)
- The company provides services around the core (sustainable business model)

---

## Summary: The License Matrix

| Component | License | Who Pays | Who Can Fork |
|---|---|---|---|
| **Core runtime** (Rust daemon) | Apache-2.0 | No one | Anyone |
| **CLI** | Apache-2.0 | No one | Anyone |
| **CRI shim** | Apache-2.0 | No one | Anyone |
| **MCP server** | Apache-2.0 | No one | Anyone |
| **P2P sync** | Apache-2.0 | No one | Anyone |
| **DAG store** | Apache-2.0 | No one | Anyone |
| **Desktop Core** | Apache-2.0 | No one | Anyone |
| **Hub (SaaS)** | Proprietary + ELv2 for client libs | Organizations | Separate product |
| **Shield (security)** | Proprietary | Organizations | Separate product |
| **Desktop Enterprise** | Proprietary | Organizations | N/A (uses OSS core) |
| **Cloud API extensions** | ELv2 | Cloud providers (CPL) | Separate product |

---

## Final Recommendation

1. **Core runtime + CLI + CRI + MCP + P2P + Desktop**: **Apache-2.0**
   - Maximum developer adoption
   - Patent protection
   - Trademark protection
   - GPL-3.0 compatible

2. **Hub + Shield + Desktop Enterprise**: **Proprietary** (closed source)
   - Separate product, not a fork
   - Uses OSS core via gRPC
   - No "open core" resentment

3. **Cloud API extensions**: **ELv2**
   - Source-available
   - Prevents SaaS competition without preventing internal use
   - Not "poison pill" like SSPL

4. **Contributions policy**: All contributions to core require **Contributor License Agreement (CLA)** or **Developer Certificate of Origin (DCO)** so Pullrun Technologies can relicense if needed (e.g., for a future foundation transfer).

**The golden rule**: The developer at her laptop sees 100% open source. The organization sees value in paying for services. The cloud provider sees a fair licensing model. Everyone wins.
