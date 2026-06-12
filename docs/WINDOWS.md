# Windows Support

pullrun runs on Windows via **WSL2 (Windows Subsystem for Linux 2)**. The native
Windows CLI (`pullrun.exe`) connects to the Linux daemon running inside WSL2
through a TCP-to-UDS proxy.

## Architecture

```
Windows (pullrun.exe)
  │  localhost:9501 (auto — no --server flag needed)
  ▼
localhost:9501 ──► socat (TCP→UDS proxy) ──► /tmp/pullrun.sock
                                                    │
                                               pullrun-runtime (systemd)
                                                    │
                                               ├── runc (container executor)
                                               └── Firecracker (VM executor, /dev/kvm)
```

## Prerequisites

- **Windows 11** (22H2 or later) — recommended for mirrored networking
- **Windows 10** version 1903 Build 18362+ — WSL2 supported but uses NAT
  networking (requires `netsh portproxy` for host access)
- **WSL2** installed with an Ubuntu 24.04 LTS distribution
- **winget** or Microsoft Store access for WSL installation

## Installation

### 1. Enable WSL2 on Windows

```powershell
# Enable required Windows features (admin)
dism.exe /online /enable-feature /featurename:Microsoft-Windows-Subsystem-Linux /all /norestart
dism.exe /online /enable-feature /featurename:VirtualMachinePlatform /all /norestart

# Reboot, then install WSL from Microsoft Store
winget install Microsoft.WSL

# Reboot again, then install Ubuntu
wsl --install -d Ubuntu
```

Or use the automated method from our repository.

### 2. Configure WSL2

Create `%USERPROFILE%\.wslconfig`:

```ini
[wsl2]
memory=4GB
processors=4
networkingMode=mirrored
nestedVirtualization=true
```

- **mirrored** networking (Win11 22H2+): WSL2 shares the Windows host IP;
  `localhost:9501` is transparent
- **NAT** mode (Win10): use `netsh interface portproxy` to forward
  `localhost:9501` to the WSL2 VM IP

### 3. Install pullrun — Automated

Run the installer from Git Bash, MSYS2, or WSL2 bash:

```bash
curl -fsSL https://github.com/pullrun/pullrun/raw/main/install.sh | bash
```

This installs `pullrun.exe`, `pullrun-runtime`, sets up systemd services, and
configures Firecracker if KVM is available.

### 3.b Install pullrun — Manual

Download the latest release from GitHub:

```powershell
# Windows CLI
curl.exe -LO https://github.com/pullrun/pullrun/releases/latest/download/pullrun.exe

# Linux daemon (place inside WSL2)
curl.exe -LO https://github.com/pullrun/pullrun/releases/latest/download/pullrun-runtime-linux-amd64
```

Or build from source:

```bash
# Build Windows CLI (from macOS/Linux or within WSL2)
cd cli/pullrun
GOOS=windows GOARCH=amd64 CGO_ENABLED=0 go build -o pullrun.exe .

# Build Linux daemon (requires Rust cross-compilation target)
cargo build --release --target x86_64-unknown-linux-musl -p pullrun-runtime
# Binary at target/x86_64-unknown-linux-musl/release/pullrun-runtime
```

### 4. Deploy daemon to WSL2

```bash
# Inside WSL2 (Ubuntu)
sudo cp pullrun-runtime /usr/local/bin/
sudo chmod 755 /usr/local/bin/pullrun-runtime

# Install dependencies
sudo apt-get update
sudo apt-get install -y runc socat iproute2 iptables e2fsprogs

# Ensure kernel modules load at boot
printf 'bridge\nkvm\nkvm_intel\n' | sudo tee /etc/modules-load.d/pullrun.conf

# Create systemd service
sudo tee /etc/systemd/system/pullrun-runtime.service > /dev/null << 'EOF'
[Unit]
Description=pullrun container runtime daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/pullrun-runtime daemon --store-root /var/lib/pullrun
Restart=always
RestartSec=5
User=root

[Install]
WantedBy=multi-user.target
EOF

# Create TCP proxy service for native Windows CLI
sudo tee /etc/systemd/system/pullrun-tcp-proxy.service > /dev/null << 'EOF'
[Unit]
Description=pullrun TCP-to-UDS proxy
After=pullrun-runtime.service
Requires=pullrun-runtime.service

[Service]
Type=simple
ExecStart=/usr/bin/socat TCP-LISTEN:9501,reuseaddr,fork UNIX-CONNECT:/tmp/pullrun.sock
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl enable pullrun-runtime pullrun-tcp-proxy
sudo systemctl start pullrun-runtime pullrun-tcp-proxy
```

### 4.a (Optional) Install Firecracker VM backend

Only needed for `--backend vm` on x86_64 Windows 11 with nested virtualization:

```bash
# Inside WSL2 (Ubuntu) — only if /dev/kvm exists
if [ -c /dev/kvm ]; then
  # Install Firecracker
  FC_VER=v1.16.0
  curl -fsSL https://github.com/firecracker-microvm/firecracker/releases/download/$FC_VER/firecracker-${FC_VER}-x86_64.tgz \
    | sudo tar xz --strip-components=1 -C /usr/local/bin/
  sudo chmod +x /usr/local/bin/firecracker

  # Download vmlinux kernel from Firecracker CI
  sudo mkdir -p /var/lib/pullrun
  S3="https://s3.amazonaws.com/spec.ccfc.min"
  PREFIX=$(curl -fsSL "$S3?list-type=2&prefix=firecracker-ci/&delimiter=/" \
    | grep -oP "(?<=<Prefix>)firecracker-ci/[0-9]{8}-[^/]+/(?=</Prefix>)" | sort | tail -1)
  KERNEL_KEY=$(curl -fsSL "$S3?list-type=2&prefix=${PREFIX}x86_64/vmlinux-" \
    | grep -oP "(?<=<Key>)${PREFIX}x86_64/vmlinux-[0-9]+\.[0-9]+\.[0-9]{1,3}(?=</Key>)" | sort -V | tail -1)
  sudo curl -fsSL "$S3/$KERNEL_KEY" -o /var/lib/pullrun/vmlinux

  # Add Firecracker flags to daemon service
  sudo sed -i 's|ExecStart=/usr/local/bin/pullrun-runtime daemon|ExecStart=/usr/local/bin/pullrun-runtime daemon --vm-firecracker /usr/local/bin/firecracker --vm-kernel /var/lib/pullrun/vmlinux|' \
    /etc/systemd/system/pullrun-runtime.service
  sudo systemctl daemon-reload
  sudo systemctl restart pullrun-runtime
fi
```

### 4.a Install keepalive service (WSL2 workaround)

WSL2 v2.6.1+ terminates the VM ~15–20s after the last `wsl.exe` disconnects,
even with `systemd=true`. This kills all running containers (exit 137 / SIGKILL).
The workaround is a systemd service that keeps a persistent self-connection open,
preventing `/init` (PID 2) from being orphaned:

```bash
# Inside WSL2 (Ubuntu)
sudo tee /etc/systemd/system/keepwsl.service > /dev/null << 'EOF'
[Unit]
Description=Keep WSL VM alive (self-connection prevents session-disconnect shutdown)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/mnt/c/Windows/System32/wsl.exe -d Ubuntu -u root -- sleep infinity
Restart=always
RestartSec=3
KillMode=process

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl enable keepwsl.service
sudo systemctl start keepwsl.service
```

### 5. Install the Windows CLI

Place `pullrun.exe` in your PATH (e.g., `C:\Windows\System32\`).

## Usage

```powershell
# Check daemon status (auto-connects to WSL2 via localhost:9501)
pullrun.exe info

# Pull an image
pullrun.exe pull alpine:latest

# Run a container (default backend)
pullrun.exe run alpine:latest --cmd echo --cmd hello

# Run as a Firecracker VM
pullrun.exe run alpine:latest --backend vm --cmd echo --cmd hello

# List workloads
pullrun.exe list

# Exec into a running container
pullrun.exe exec <id> /bin/echo hello

# Stop a workload
pullrun.exe stop <id>
```

No `--server` flag needed — the CLI auto-detects the daemon at `localhost:9501`.

## Windows Version Tier Matrix

| Tier | Version | WSL2 | Mirrored Networking | NAT Portproxy | Nested Virt (KVM) |
|------|---------|------|-------------------|---------------|-------------------|
| A | Win11 24H2+ | ✅ | ✅ | N/A | ✅ |
| B | Win11 22H2-23H2 | ✅ | ✅ | N/A | ✅ |
| C | Win10 22H2 | ✅ | ❌ | ✅ | ❌ |
| D | Win10 1903-22H2 | ✅ | ❌ | ✅ | ❌ |
| E | Win10 < 1903 | ❌ (WSL1) | ❌ | ❌ | ❌ |
| F | Win10 ARM64 | WSL2 ARM64 | ✅ | N/A | ❌ (EL1 limit) |

- **Tier C/D** requires `netsh interface portproxy add v4tov4 listenport=9501 listenaddress=0.0.0.0 connectport=9501 connectaddress=<WSL2_VM_IP>`
- **Nested Virtualization** for Firecracker VMs is gated by Microsoft to
  Windows 11+ via `IsWindows11OrAbove()` policy check in the HCS API

## Known Limitations

- **TTY mode** (`--tty`) is not supported on native Windows `cmd.exe`.
  Use terminal emulators (Windows Terminal, PowerShell 7+) or run the CLI
  from within WSL2
- **Bridge networking** requires loading the `bridge.ko` kernel module.
  The systemd service above handles this automatically
- **Multi-word `--cmd` values** may need careful quoting when passing
  through `cmd.exe`. Prefer single-word commands or use a script
- **ARM64 Windows** cannot run Firecracker VMs (KVM needs EL2; WSL2 ARM64
  kernels boot at EL1) — same limitation as ARM64 macOS
- **DAG store** is platform-agnostic: the same content-addressed block store
  works across Windows/WSL2, macOS, and Linux with byte-identical output
- **WSL 2.7.8 regression (microsoft/WSL#13416)**: WSL2 shuts down the VM
  ~15–20 seconds after the last `wsl.exe` process exits, even with
  `systemd=true` and active systemd services. This causes all runc containers
  to receive SIGKILL (exit code 137). The fix (see "Install keepalive
  service" below) keeps a persistent WSL connection open to prevent the
  session-disconnect shutdown.

## Troubleshooting

### Daemon won't start ("Not supported (os error 95)")
Load the bridge kernel module:
```bash
sudo modprobe bridge
```
Ensure `/etc/modules-load.d/pullrun.conf` contains `bridge`.

### DNS resolution fails inside WSL2
```bash
sudo rm -f /etc/resolv.conf
echo "nameserver 8.8.8.8" | sudo tee /etc/resolv.conf
echo "nameserver 1.1.1.1" | sudo tee -a /etc/resolv.conf
sudo chattr +i /etc/resolv.conf 2>/dev/null || true
```
Also set `generateResolvConf=false` under `[network]` in `/etc/wsl.conf`.

### Containers exit with code 137 (SIGKILL)

This is caused by the WSL2 VM shutting down when the last `wsl.exe` session
disconnects. Verify the keepalive service is running:

```powershell
wsl -d Ubuntu -u root systemctl status keepwsl.service
```

If inactive, re-install and start it:

```bash
sudo systemctl enable --now keepwsl.service
```

The service runs `/mnt/c/Windows/System32/wsl.exe -d Ubuntu -u root -- sleep infinity`,
which creates a persistent child of `/init` (PID 2) and prevents the VM from
shutting down between CLI invocations.

### CLI cannot connect to daemon
Verify the proxy is running:
```powershell
wsl -d Ubuntu -u root systemctl status pullrun-tcp-proxy
```
Test the TCP port:
```powershell
curl.exe --ssl-no-revoke http://localhost:9501/
```

## MCP Server on Windows

The MCP server works over the same TCP bridge. When configuring your AI
agent, point the `--server` flag to `localhost:9501`:

```json
{
  "mcpServers": {
    "pullrun": {
      "command": "pullrun.exe",
      "args": ["--server", "localhost:9501", "mcp"]
    }
  }
}
```

> **Note:** The MCP server starts a new daemon by default (`--direct` mode).
> To connect to an existing system daemon, **always pass `--server`**.
