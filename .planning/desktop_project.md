# Pullrun Desktop Architecture Design

## Executive Summary

**Thesis**: Most developer-facing container tools today optimize for the wrong thing. Docker Desktop treats the **developer workstation** as a black-box VM host. Podman Desktop mimics Docker Desktop without solving the root problems. Pullrun Desktop treats the **developer workstation as a first-class compute node** in a distributed system, with a DAG store that is:

1. **Content-addressed** (deduplication at file level, not layer)
2. **Backend-agnostic** (same image: container OR VM on any OS)
3. **P2P-syncable** (team-wide image sharing without registry bottleneck)
4. **AI-native** (MCP server for agent-driven operations)

---

## 😤 Pain Point by Pain Point: Docker Desktop vs Pullrun Desktop

| Pain Point | Docker Desktop | Pullrun Desktop | How |
|---|---|---|---|
| **"6GB RAM for Docker Desktop?!"** | 6-12 GB with K8s enabled | **< 300 MB total** | No hidden VM on macOS/Linux. Native daemon is 25MB. |
| **"VPN breaks my containers"** | `vpnkit` proxy conflicts with corporate VPN | **No network override** | Uses host networking natively. No magic proxy. |
| **"WSL2 conflicts with my dev WSL"** | Own WSL distro, hijacks port 53, breaks `systemd` | **Shares YOUR existing WSL2** | Uses user's Ubuntu WSL2. No separate distro. |
| **"It takes 5 min to start"** | Boots VM, initializes K8s, starts many services | **< 1s on macOS/Linux** | Native binary. No VM on macOS. |
| **"M1/M2 support is buggy"** | Rosetta emulation, slow x86_64 images | **Native Apple Virtualization** | Apple Silicon VMs boot in ~160ms. |
| **"I need a VM AND container from same image"** | Only Linux containers | **container -> VM with `--backend vm`** | Firecracker on Linux, Apple Virt on macOS. |
| **"Everyone pulls same image from registry"** | Nx independent pulls | **P2P block sync via mDNS** | One dev pulls, rest sync from LAN peers. |
| **"I can't inspect what Docker is doing"** | Hidden VM, opaque networking, magic | **Fully transparent** | Every process visible. Store is just files. |
| **"Docker licensing per seat"** | Pro/Business tiers, compliance overhead | **MIT/Apache-2.0, zero cost** | No per-seat licensing. |
| **"Updates break my setup"** | Monolithic update changes VM, networking, engine | **Independent updates** | Daemon, UI, and store update separately. |

---

## 🏗 Architecture: Three-Layer Design

### Layer 1: The Native Runtime (Rust)

```
+---------------------------------------------------------------+
|  pullrun-runtime (daemon)                                     |
|  +-------------+  +------------+  +------------------------+  |
|  |  gRPC API   |  |  DAG Store  |  |   Executor Router      |  |
|  |  (UDS/TCP)  |  | (rkyv+ mmap)|  |  +------+  +--------+  |  |
|  |             |  |             |  |  | runc | |Firecracker|  |  |
|  |             |  |             |  |  |      | |Apple Virt |  |  |
|  +-------------+  +-------------+  +------+  +--------+  |  |
|  +-------------+  +-------------+  +------------------------+  |
|  |  IPAM + DNS  |  |  Policy      |  |  P2P Sync (mDNS)      |  |
|  |  10.42.0.0/16|  |  Cosign/SBOM |  |  Bloom + Gossip       |  |
|  +-------------+  +-------------+  +------------------------+  |
+---------------------------------------------------------------+
```

**Key insight**: On macOS, this daemon runs as a native process. No VM. On Linux, also native. On Windows, it runs inside the user's existing WSL2 (not a separate Docker distro).

### Layer 2: The Desktop Bridge (Go)

```go
// Simplified: Desktop <-> Daemon communication
type DesktopService struct {
    rpcClient *grpc.ClientConn    // to pullrun-runtime
    events    chan Event          // desktop -> UI
    p2p       *SyncClient         // P2P discovery for team sharing
}
```

**Responsibilities**:
- Starts/stops `pullrun-runtime` daemon as a user service (not system-wide)
- Forwards gRPC calls from Electron/Tauri to the daemon
- Manages tray icon, notifications, auto-updates
- Discovers other Pullrun Desktop instances on LAN for P2P sync

### Layer 3: The UI (Electron or Tauri)

```
+----------------------------------------------------------------+
|  Pullrun Desktop v1.0                                            |
|  +-----------------------------------------------------------+  |
|  |  Navigation: [Dashboard] [Images] [Workloads] [Compose]     |  |
|  |             [Networks] [Build] [Policy] [AI Agent]         |  |
|  +-----------------------------------------------------------+  |
|                                                                  |
|  +---------------------+  +---------------------+              |
|  |   System Status       |  |   Quick Actions     |              |
|  |  +---------------+  |  |  +---------------+  |              |
|  |  | Daemon: 🟢     |  |  |  | 🚀 Run Cont.   |  |              |
|  |  | Store: 2.3 GB  |  |  |  | 🖥️  Run VM     |  |              |
|  |  | Images: 45     |  |  |  | 📦 Pull Img    |  |              |
|  |  | Running: 12    |  |  |  | 🏗️  Build Df    |  |              |
|  |  | Memory: 128 MB |  |  |  | 🤖 AI Agent    |  |              |
|  |  +---------------+  |  |  +---------------+  |              |
|  +---------------------+  +---------------------+              |
|                                                                  |
|  +-----------------------------------------------------------+  |
|  |  Active Workloads                                          |  |
|  |  +--------+-------------+--------+--------+--------+       |  |
|  |  | Name   | Image       |Backend | Status |Actions |       |  |
|  |  +--------+-------------+--------+--------+--------+       |  |
|  |  | web-svc| nginx:1.21  | VM     | 🟢     | ⏹ 📊   |       |  |
|  |  | api    | myapp:latest| Ctr    | 🟢     | ⏹ 📊   |       |  |
|  |  | db     | postgres:15 | VM     | 🟢     | ⏹ 📊   |       |  |
|  |  +--------+-------------+--------+--------+--------+       |  |
|  +-----------------------------------------------------------+  |
+----------------------------------------------------------------+
```

---

## 🍎 macOS: The Best Platform Experience

### Why Pullrun Desktop is Native on macOS

**Docker Desktop on macOS:**

```
+-----------------------------------------+
|  HyperKit VM (Linux)                    |
|  +- Linux kernel                       |
|  +- Docker daemon                       |
|  +- containerd                          |
|  +- VPNKit (networking)                 |
+-----------------------------------------
   2-4 GB RAM used
```

**Pullrun Desktop on macOS:**

```
+-----------------------------------------+
|  Native macOS process                   |
|  +- pullrun-runtime (Rust)              |
|  +- Apple Virtualization (for VMs)     |
|  +- No VM for containers               |
+-----------------------------------------+
   25 MB resident
```

**Containers on macOS without a VM:**
- Use `user_namespaces` + `pasta` (from Passt project)
- Rootless, no `sudo`, no VM
- `pullrun run alpine:latest` just works, native speed

**VMs on macOS with Apple Virtualization:**
- `pullrun run alpine:latest --backend vm`
- Uses `Virtualization.framework` (macOS 13+)
- Boot time: ~160ms
- Signed with `com.apple.security.virtualization` entitlement

### The macOS Experience Flow

```bash
# 1. Install Pullrun Desktop
brew install --cask pullrun-desktop

# 2. Start (daemon auto-starts)
open /Applications/Pullrun\ Desktop.app

# 3. Use it
pullrun run -d -p 8080:80 nginx:1.21      # Container, native, no VM
pullrun run -d --backend vm myapp:latest  # Apple VM, full isolation

# 4. Team sync
pullrun team join "my-team" --auto-sync   # Auto-discover LAN peers, share images
```

---

## 🪟 Windows: The WSL2-Integrated Experience

### The Docker Desktop WSL2 Problem

```
+----------------- Windows Host -----------------+
|  +-- Docker Desktop GUI                        |
|  +-- vpnkit (networking proxy)                |
|  |                                            |
|  v                                            |
|  +-- Docker Desktop Data WSL2 VM (separate)  |
|      +- Docker daemon                         |
|      +- containerd                            |
|      +- Linux containers ONLY                 |
+-----------------------------------------------+
```

**Problems**:
- Separate WSL distro, conflicts with your Ubuntu WSL
- VPN breaks because of vpnkit
- Cannot access Linux tools in your main WSL

### Pullrun Desktop on Windows

```
+----------------- Windows Host -----------------+
|  +-- Pullrun Desktop (Electron/Tauri)         |
|  +-- pullrun.exe (Go CLI)                     |
|                                              |
|  +-- Your existing Ubuntu WSL2               |
|      +- pullrun-runtime (backend, systemd)   |
|      +- runc (containers)                    |
|      +- Firecracker (VMs, --backend vm)       |
|      +- P2P sync agent                       |
+-----------------------------------------------+
```

**Advantages**:
- Uses YOUR WSL2. Can `apt install` tools, edit files, etc.
- No separate distro, no port conflicts
- VPN works (native networking)
- Firecracker VMs via nested KVM (x86_64)

### Windows Installation Flow

```powershell
# 1. Install from website (MSI) or winget
winget install Pullrun.PullrunDesktop

# 2. First run: WSL2 setup wizard
#    - Detects if WSL2 is installed
#    - Offers to install Ubuntu 24.04 if not present
#    - Configures .wslconfig (mirrored networking, nested virt, 4GB RAM)
#    - Installs pullrun-runtime as systemd service
#    - Starts keepwsl.service (mitigates WSL 2.7.8 shutdown regression)

# 3. Use it
pullrun.exe pull alpine:latest
pullrun.exe run -d -p 8080:80 nginx:1.21
pullrun.exe run -d --backend vm myapp:latest  # Firecracker VM
```

---

## 🐧 Linux: Native Experience (No VM Required)

```
+------------------------------------------------+
| Linux Host                                     |
|  +-- Pullrun Desktop (Electron/Tauri)        |
|  +-- Starts pullrun-runtime as user systemd   |
|                                        service  |
|  Native Linux:                                 |
|  +-- pullrun-runtime (daemon)                |
|  +-- runc (containers)                         |
|  +-- Firecracker (VMs, if /dev/kvm available) |
|  +-- P2P sync agent                           |
+------------------------------------------------+
```

No VM required. Daemon runs natively.

---

## 🤖 AI Agent Integration (MCP)

Every Pullrun Desktop instance runs an MCP server, making it controllable by AI agents:

```json
// ~/.opencode.json
{
  "mcpServers": {
    "pullrun": {
      "command": "pullrun",
      "args": ["mcp", "--sse", "localhost:8080"]
    }
  }
}
```

### AI-Native Features

- **Natural language ops**: "Show me all running workloads" -> tool call to `list_workloads`
- **Auto-debug**: "Why is my container unhealthy?" -> queries `get_workload`, `stream_logs`, `get_stats`
- **Auto-remediate**: "My web container is restarting, investigate" -> checks policies, inspects health, suggests fixes
- **Intelligent builds**: "Optimize my Dockerfile for size" -> analyzes layers, suggests multi-stage

### UI Integration

```
+------------------------------------------------+
| AI Agent Panel                                 |
| +-------------------------------------------+ |
| | User: "My web container keeps restarting,   | |
| |        what's wrong?"                       | |
| |                                           | |
| | Agent:                                    | |
| |   Investigating workload 'web-01'...      | |
| |   - Health check: FAILING (503 on port 80)| |
| |   - Logs: "Connection refused to db:5432"  | |
| |   - Network: 'web-01' has no rules for db │ |
| |                                           | |
| |   Suggested fix: Add network rule         | |
| |   Apply fix? [Yes] [No] [Explain more]    | |
| +-------------------------------------------+ |
+------------------------------------------------+
```

---

## 👥 Team / Enterprise Features

### 1. P2P Image Sharing (No Registry Bottleneck)

```yaml
# ~/.config/pullrun/team.yaml
team:
  name: "platform-team"
  discovery: mdns             # Auto-discover LAN peers
  sync:
    auto_sync: true           # Pull images from peers when available
    seed_mode: true           # This machine seeds images to others
    max_bandwidth: 100MB/s    # Throttle to not kill the office WiFi
```

**What happens**:
1. Alice builds `myapp:latest` on her Mac
2. Pullrun Desktop announces the image via mDNS
3. Bob's machine (on same LAN) sees it: "Alice has this, I'll sync"
4. Bob gets the image via P2P block sync (only the blocks he doesn't already have)
5. No registry round-trip. Zero egress cost.

### 2. Centralized Policy Enforcement

```yaml
# /etc/pullrun/policy.yaml (enterprise)
policy:
  required_signature: true
  trusted_keys:
    - /etc/pullrun/keys/cosign-prod.pub
  max_cvss_score: 7.0
  deny_licenses:
    - GPL-3.0
  seccomp: default
  readonly_rootfs: true
```

**When enforced**:
- Every `pullrun run` validates policy
- Violations are logged and blocked
- Compliance dashboard shows policy adherence across team

### 3. Observability Dashboard

```
+------------------------------------------------+
| Team Observability                               |
| +--------------------------------------------+ |
| |                                            | |
| | Chart: Pull Latency over Time              | |
| | p50, p95, p99 lines                        | |
| |                                            | |
| | [Table: Policy Violations]  [Map: P2P      | |
| | Timestamp | User | Image   | Violation     | |
| | 10:42 AM  | alice| badimg  | Missing SBOM  | |
| | 10:38 AM  | bob  | oldvers | CVSS 9.1      | |
| |                                            | |
| +--------------------------------------------+ |
+------------------------------------------------+
```

---

## 📦 Implementation Roadmap

### Phase 1: Foundation (v1.0)
- [ ] Tauri app shell with tray icon
- [ ] Daemon auto-start/stop on macOS/Linux
- [ ] Basic views: Dashboard, Images, Workloads
- [ ] WSL2 integration on Windows
- [ ] Build engine (Dockerfile to DAG)

### Phase 2: VM & Compose (v1.2)
- [ ] Apple Virtualization UI
- [ ] Firecracker VM UI
- [ ] Docker Compose compatibility
- [ ] Volume mount management

### Phase 3: Team & P2P (v1.4)
- [ ] LAN peer discovery
- [ ] P2P sync UI
- [ ] Team policy management
- [ ] Centralized metrics dashboard

### Phase 4: AI & Advanced (v1.6)
- [ ] MCP server UI
- [ ] AI agent chat panel
- [ ] Intelligent build suggestions
- [ ] Auto-remediation workflows

---

## 🎯 The Bottom Line

| Dimension | Docker Desktop | Podman Desktop | Pullrun Desktop |
|-----------|----------------|----------------|-----------------|
| **RAM** | 6-12 GB | 4-6 GB | **< 300 MB** |
| **VM required on macOS** | Yes (Linux VM) | Yes (Linux VM) | **No** |
| **Same image -> ctr + VM** | ❌ | ❌ | **✅** |
| **P2P team sync** | ❌ | ❌ | **✅** |
| **AI agent control** | ❌ | ❌ | **✅** |
| **Open source license** | Proprietary | Apache-2.0 | **MIT/Apache** |
| **Apple Silicon native** | Rosetta | Rosetta | **Apple Virt** |
| **Cost** | $5-$21/seat/mo | Free | **Free** |

**Pullrun Desktop is not a better Docker Desktop. It is a fundamentally different approach to developer tooling: content-addressed, VM-native, P2P-synced, and AI-ready from day one.**
