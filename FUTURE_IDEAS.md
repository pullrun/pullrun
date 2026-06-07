# Future Ideas

This document catalogs feasible, high-value directions for Nimbus beyond the current implementation. Each entry is self-contained — some are near-term, others are speculative but architecturally sound.

---

## 1. Hybrid Multi-Instance VM (Shared-VM Process Sandbox)

**Idea:** Boot one Firecracker VM with a lightweight guest-runtime (PID 1 over vsock), then spawn subsequent workloads as namespace-isolated processes *inside* that same VM instead of booting N separate VMs.

**Storage:** All workloads share the same DAG rootfs via VirtioFS. Files are mmap'd once in the host page cache — every workload inside the VM reads from the same physical pages.

**Boot time progression:**

| Instance | Time | Memory overhead |
|---|---|---|
| 1st (`base` VM + kernel) | ~5s | Kernel ~25MB + rootfs pages |
| 2nd (`nginx`) | ~50ms (clone+exec) | ~2MB (process only) |
| 3rd (`opencode`) | ~50ms | ~2MB |
| Nth | ~50ms | ~2MB each |

**Why DAG makes this novel:** Kata Containers has an experimental "fcfs" (Firecracker Containers on Same VM) mode but uses in-guest overlayfs — each workload has a separate copy of shared libraries in the guest page cache. Nimbus DAG collapses storage at the host level: every workload `mmap`s the same host-resident DAG pages.

**Workflow:**

```
nimbus vm start --name base      # boots one VM with guest-runtime
nimbus run nginx --vm base        # vsock → clone(CLONE_NEWNS|CLONE_NEWNET) + exec
nimbus run opencode --vm base     # same VM, separate namespaces
```

---

## 2. macOS "Borrow" Environment (Darling + DAG)

**Idea:** Run macOS CLI binaries and toolchains in a Linux Firecracker VM whose rootfs is populated from a DAG of extracted macOS system files. A compatibility shim (dyld + libSystem) translates macOS ABI to Linux syscalls. No macOS kernel, no APFS, no firmware.

**Why this has failed before:**
- Darling required a full 15GB macOS rootfs per user
- Tiny file changes required hours-long image rebuilds
- CI fleets duplicated the full rootfs per runner — cost-prohibitive

**Why DAG changes everything:**
- macOS system files (dyld cache, Frameworks, .dylibs) are stored **once** in the DAG
- Multiple macOS versions coexist, deduped at file granularity (~15GB → ~5GB unique content)
- All running "borrowed" environments mmap the same host-resident pages
- Changing one file = one DAG entry update (instant, O(1))

**Boot time delta:** ~200ms vs Tart's 15s+ cold start from APFS snapshot materialization.

**Upside:** Xcode's `clang`, `swift`, `ruby`, Python — all macOS CLI toolchains — run in a CI pipeline without a single macOS VM. No Tart, no macOS runners, no Apple Silicon licensing cost.

---

## 3. Docker Compose Drop-In Replacement

**Idea:** `nimbus compose up` reads an unmodified `docker-compose.yml` and boots each `service` as a micro-VM instead of a container. Zero migration cost for developers.

**What maps directly:**

| Compose | Nimbus |
|---|---|
| `image:` | OCI pull → DAG → VM rootfs |
| `ports:` | Proxy network port forwarding |
| `volumes:` | VirtioFS/Virtio-9p passthrough |
| `environment:` | vsock env frame |
| `healthcheck:` | gRPC health probe over vsock |
| `command:`/`entrypoint:` | WorkloadSpec.command |

**What needs building:**
- Compose file parser (existing `compose-go` library)
- `nimbus compose [up|down|logs|ps]` CLI
- Per-project bridge network with DNS
- Healthcheck polling over vsock
- Build integration (`nimbus compose build` → DAG)

**Why it matters:** Lowest-friction path to micro-VM adoption. Developers don't rewrite YAML; they change one command.

---

## 4. DAG-Powered Applications Beyond Containers

### 4a. Massive-scale serverless cold starts

Docker/Podman pull layers → decompress → extract → overlayfs before starting. Nimbus boots a VM from a DAG root and **lazy-loads files on page fault** — the binary starts executing while the rest of the rootfs is still being pulled.

### 4b. N-way A/B testing with zero storage overhead

100 versions of a binary differing by 1%. With overlayfs, each gets a new layer blob (MBs each). With DAG, the 99% identical files are stored once — the root digest is the only 32 bytes distinguishing each version. All 100 VMs mmap the same physical pages.

### 4c. File-granular supply chain attestation

Prove `/bin/sh` has digest X without attesting the entire image. Sign individual files, not layers. Enables SLSA Level 3+ at file granularity — impossible with layer-based storage.

### 4d. Delta OTA updates for edge devices

Compute diff between two DAG roots → transfer only files whose digests changed. Delta is O(changed_files), not O(layer_size). A 5MB security patch to one binary doesn't require re-downloading a multi-GB layer.

### 4e. Build caches with near-zero invalidation

Monorepo where one tiny header changes: with Docker, the layer containing it is invalidated. With DAG, only that file's entry changes — the rest stay valid. CI rebuilds go from 10 minutes to 30 seconds.

### 4f. Multi-tenant ML inference serving

1000 tenants, each with a VM holding their model + PyTorch + CUDA. With Docker, 1000× PyTorch stored as distinct layers. With DAG, `libcuda.so`, `libtorch.so`, `libpython3.so` are stored once and mmap'd by all 1000 VMs. Memory pressure is the bottleneck — DAG collapses it.

### 4g. Instant forensic diff between any two images

`diff --dag root_a root_b` → enumerates exactly which files differ in O(total_files). No extraction to temp, no `diff -r` over gigabytes.

### 4h. Composable base images

New "image" = union of files from 5 different DAG roots (e.g., `python:3.12` + `pytorch:latest` + `ubuntu:24.04`) without storing new data. Metadata is just a DAG node referencing existing content. Layer systems can't do this because files are baked into opaque tarballs.

### 4i. Kernel-as-a-package

Already partially implemented: kernel is an OCI image in the DAG. Security CVE hits Linux → push a new kernel OCI ref → update one string in workload config → next VM boots the patched kernel. No host reboot, no apt upgrade.

---

## 5. Block-Level DAG for macOS VM Storage

**Idea:** Replace Tar (Tart)'s opaque disk-delta layers with a content-addressed **block-level** DAG for macOS VM images. Each 64KB block is stored by SHA256, deduped across all macOS VMs on the host.

**What it improves:**
- 50 developer macOS VMs on a CI host dedupe common blocks (same dyld cache, same kernelcollection, same System.xxhash.arm64e)
- Still requires full APFS volume materialization before boot (macOS reads its disk via Virtio Block, not VirtioFS)
- Reduces *storage* overhead dramatically but not boot time

**Why it's not file-granular:** macOS requires a sealed APFS volume to boot. Files aren't first-class; the block device is.

---

## 6. Block-Level DAG File Transfer

**Idea:** Transfer files between hosts (scp replacement) using content-addressed DAG blocks.

**How it works:**
- Split file into fixed-size blocks
- Compute SHA256 of each block
- Send only blocks the receiver doesn't have
- Receiver reassembles + verifies

**Why DAG helps:** Two files on different hosts that share blocks (e.g., same `.dylib` in different apps) only transfer the unique blocks once.

---

## 7. DAG as Git LFS Backend

**Idea:** Store large files in Git LFS using the DAG store instead of S3/GCS.

**Why it fits:** Git LFS already uses SHA256 addressing. DAG already has content-addressed storage with dedup. Files shared across repos (SDKs, toolchains) would be stored once per developer machine.

---

## 8. Immutable System Images (OSTree Alternative)

**Idea:** System rootfs as DAG root digest. Atomic updates by switching digest. Rollback is O(1). Boot from mmap'd DAG files.

**Similar to:** OSTree, Fedora Silverblue, but with file-granular dedup and lazy loading.

---

## 9. DAG-as-Registry: Peer-to-Peer Image Distribution

**Idea:** Nodes share DAG blocks over BitTorrent-like protocol instead of pulling from a central registry. Each file block is content-addressed — any peer can serve it.

**Why DAG fits:** Block-level content addressing is the transport format. No layer tarballs. No central server bottleneck. Ideal for air-gapped or edge deployments.

---

## 10. Cross-Platform Remote Execution

**Idea:** `nimbus exec --arch aarch64 --os linux` compiles on an arm64 Linux CI runner while the developer works on x86 macOS. The DAG stores both platform's files for the same image — deduped at the file level where identical.

**Already partially supported:** OCI image index with platform selection exists in the puller.

---

## Evaluation Matrix

| Idea | Effort | Impact | Dependency |
|---|---|---|---|
| Docker Compose compat | Medium | High (adoption) | compose-go lib, CLI subcommands |
| Hybrid multi-instance VM | Medium | High (multi-tenancy density) | Guest-runtime binary, vsock RPC |
| Kernel-as-package | Low (partial impl) | Medium | OCI kernel image publishing |
| File-granular attestation | Low | Medium | DAG walk tooling |
| Delta OTA | Medium | High (edge/IoT) | Diff tool, transport protocol |
| Build cache invalidation | Low | High (CI) | Build integration |
| macOS "borrow" env | Very High | Transformative | Darling-level ABI shim, legal |
| Block-level DAG for Tart | Medium | Medium | Tart integration |
| P2P DAG registry | High | High (edge) | Transport protocol |
| Remote exec | Medium | High (developer UX) | Cross-compile infra |
| Immutable system images | Medium | Medium | Boot loader integration |
| DAG as Git LFS | Low | Medium | Git LFS custom transfer |

---

## Principles

Ideas in this document share a common thread: **make the DAG the universal storage substrate.** Every proposal extracts more value from the fact that Nimbus stores files by content hash, not by layer tarball. The more workloads share the same DAG store, the more they benefit from each other's cached data — a network effect that layer-based systems cannot replicate.
