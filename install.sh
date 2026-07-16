#!/usr/bin/env bash
# Copyright 2026 Mohammed Boukaba.
# SPDX-License-Identifier: Apache-2.0

set -eu

REPO="pullrun/pullrun"
CURRENT_VERSION="v0.6.6"
VERSION="${VERSION:-latest}"

info()  { printf "\033[32m%s\033[0m\n" "$*"; }
warn()  { printf "\033[33m%s\033[0m\n" "$*"; }
error() { printf "\033[31m%s\033[0m\n" "$*" >&2; exit 1; }

# ── Platform detection ──────────────────────────────────────────────
RAW_OS="$(uname -s)"
OS="$(echo "$RAW_OS" | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

# Detect Windows (Git Bash / MSYS2 / Cygwin / WSL2)
case "$RAW_OS" in
  MINGW*|MSYS*|CYGWIN*) IS_WINDOWS=1; OS="windows" ;;
esac
# Also detect WSL2 (runs inside a Windows VM)
if [ -f /proc/sys/kernel/osrelease ] && grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null; then
  IS_WINDOWS=1
  OS="windows"
fi

case "$ARCH" in
  x86_64|amd64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) error "unsupported arch: $ARCH" ;;
esac

# ── macOS: try Homebrew (pre-built binary, no Xcode needed) ─────────
if [ "$OS" = "darwin" ] && command -v brew &>/dev/null; then
  info "Installing via Homebrew..."
  brew tap pullrun/tap 2>/dev/null
  brew trust pullrun/tap 2>/dev/null || true
  if brew install pullrun; then
    info "Done! Run 'pullrun --help' to get started."
    exit 0
  fi
  warn "Homebrew install failed (Xcode version mismatch?). Falling back to direct download..."
fi

# ── Linux: try APT (Debian/Ubuntu) ─────────────────────────────────
if [ "$OS" = "linux" ] && command -v apt-get &>/dev/null; then
  info "Installing via APT..."
  KEY_URL="https://pullrun.github.io/apt/key.gpg"
  KEYRING="/usr/share/keyrings/pullrun.gpg"
  SOURCES="/etc/apt/sources.list.d/pullrun.list"
  curl -fsSL "$KEY_URL" | sudo gpg --dearmor -o "$KEYRING" 2>/dev/null || true
  echo "deb [signed-by=$KEYRING] https://pullrun.github.io/apt stable main" \
    | sudo tee "$SOURCES" >/dev/null 2>/dev/null || true
  if sudo apt-get update -qq && sudo apt-get install -y -qq pullrun; then
    info "Done! Run 'pullrun --help' to get started."
    exit 0
  fi
  warn "APT install failed — falling back to binary download..."
fi

# ── Windows / WSL2 install ─────────────────────────────────────────
if [ "${IS_WINDOWS:-0}" = "1" ]; then
  INSTALL_DIR="${PULLRUN_HOME:-$HOME/pullrun}"
  mkdir -p "$INSTALL_DIR"

  info "Installing pullrun for Windows..."

  # Resolve latest release tag
  if [ "$VERSION" = "latest" ]; then
    API_URL="https://api.github.com/repos/$REPO/releases/latest"
    TAG=$(curl -fsSL "$API_URL" | grep '"tag_name"' | cut -d'"' -f4)
    if [ -z "$TAG" ]; then
      error "Could not determine latest release tag (GitHub API rate-limited?). Set VERSION=$CURRENT_VERSION $0"
      exit 1
    fi
  else
    TAG="$VERSION"
  fi

  BASE="https://github.com/$REPO/releases/download/$TAG"
  TMPDIR=$(mktemp -d)
  cleanup() { rm -rf "$TMPDIR"; }
  trap cleanup EXIT

  # Download Windows CLI
  TARBALL="pullrun-${TAG#v}-windows-${ARCH}.tar.gz"
  info "  downloading $TARBALL..."
  if ! curl -fsSL "$BASE/$TARBALL" | tar -xzf - -C "$TMPDIR"; then
    warn "  Failed to download $TARBALL"
    warn "  Check that version $TAG exists at:"
    warn "    $BASE/$TARBALL"
    error "Windows download failed."
  fi

  SRC=$(find "$TMPDIR" -maxdepth 3 -type f -name "pullrun-windows-${ARCH}" 2>/dev/null | head -1)
  if [ -z "$SRC" ]; then
    SRC=$(find "$TMPDIR" -maxdepth 3 -type f -name "pullrun.exe" 2>/dev/null | head -1)
  fi
  if [ -z "$SRC" ]; then
    SRC=$(find "$TMPDIR" -maxdepth 3 -type f -name "pullrun" 2>/dev/null | head -1)
  fi
  if [ -n "$SRC" ]; then
    cp "$SRC" "$INSTALL_DIR/pullrun.exe"
    chmod +x "$INSTALL_DIR/pullrun.exe"
    info "  CLI installed to $INSTALL_DIR/pullrun.exe"
  else
    warn "  pullrun.exe not found in release; skipping CLI install"
  fi

  # Download Linux runtime
  RUNTIME_TARBALL="pullrun-${TAG#v}-linux-${ARCH}.tar.gz"
  info "  downloading $RUNTIME_TARBALL..."
  RUNTIME_DIR=$(mktemp -d)
  if ! curl -fsSL "$BASE/$RUNTIME_TARBALL" | tar -xzf - -C "$RUNTIME_DIR"; then
    warn "  Failed to download $RUNTIME_TARBALL"
    warn "  WSL2 runtime setup skipped. Run the installer again after the release is published."
  fi

  # Add install dir to PATH for this session
  export PATH="$INSTALL_DIR:$PATH"

  # ── WSL2 setup ──
  WSL_EXE=""
  if command -v wsl.exe &>/dev/null; then
    WSL_EXE="wsl.exe"
  elif [ -x /mnt/c/Windows/System32/wsl.exe ]; then
    WSL_EXE="/mnt/c/Windows/System32/wsl.exe"
  fi

  if [ -n "$WSL_EXE" ]; then
    info "  WSL2 detected. Setting up pullrun-runtime inside WSL2..."

    # Check WSL2 status
    if ! $WSL_EXE --status 2>/dev/null | grep -qi "default version.*2"; then
      warn "  WSL2 is not the default version. Run: wsl --set-default-version 2"
    fi

    # Check for Ubuntu distro
    UBUNTU_DISTRO=$($WSL_EXE -l -q 2>/dev/null | grep -i ubuntu | head -1 | tr -d '\r')
    if [ -z "$UBUNTU_DISTRO" ]; then
      warn "  No Ubuntu WSL distro found. Install one with:"
      warn "    wsl --install -d Ubuntu-24.04"
      warn "  Then re-run this script."
    else
      info "  Using WSL distro: $UBUNTU_DISTRO"

      # Copy runtime binary into WSL2
      RUNTIME_SRC=$(find "$RUNTIME_DIR" -maxdepth 3 -type f -name "pullrun-runtime-linux-${ARCH}" 2>/dev/null | head -1)
      if [ -z "$RUNTIME_SRC" ]; then
        RUNTIME_SRC=$(find "$RUNTIME_DIR" -maxdepth 3 -type f -name "pullrun-runtime" 2>/dev/null | head -1)
      fi
      if [ -n "$RUNTIME_SRC" ]; then
        info "  Installing pullrun-runtime into WSL2..."
        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- mkdir -p /usr/local/bin
        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- sh -c "cat > /usr/local/bin/pullrun-runtime" < "$RUNTIME_SRC"
        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- chmod +x /usr/local/bin/pullrun-runtime

        # Install Firecracker and kernel if KVM is available
        HAS_KVM=$($WSL_EXE -d "$UBUNTU_DISTRO" -u root -- sh -c "ls /dev/kvm 2>/dev/null && echo 1 || echo 0" | tr -d '\r')
        if [ "$HAS_KVM" = "1" ]; then
          info "  KVM available — installing Firecracker..."
          FC_VER=$(curl -fsSL https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest | grep '"tag_name"' | cut -d'"' -f4)
          $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- sh -c "curl -fsSL https://github.com/firecracker-microvm/firecracker/releases/download/\$FC_VER/firecracker-\$FC_VER-x86_64.tgz | tar xz --strip-components=1 -C /usr/local/bin/"
          $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- chmod +x /usr/local/bin/firecracker
          info "  Kernel setup skipped — run 'pullrun kernel-pull' after install"
        else
          info "  KVM not available (expected on ARM64 or Windows 10); VM backend disabled"
        fi

        # Create systemd service
        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- sh -c "cat > /etc/systemd/system/pullrun-runtime.service << 'UNIT'
[Unit]
Description=Pullrun Runtime Daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/pullrun-runtime daemon --store-root /var/lib/pullrun
Restart=always
RestartSec=5
Environment=RUST_BACKTRACE=full
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT"

        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- sh -c "cat > /etc/systemd/system/pullrun-tcp-proxy.service << 'UNIT'
[Unit]
Description=Pullrun TCP Proxy
After=pullrun-runtime.service
Requires=pullrun-runtime.service

[Service]
Type=simple
ExecStart=/usr/bin/socat TCP-LISTEN:9501,reuseaddr,fork UNIX-CONNECT:/tmp/pullrun.sock
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT"

        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- sh -c "echo bridge > /etc/modules-load.d/pullrun.conf"

        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- systemctl daemon-reload
        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- systemctl enable --now pullrun-runtime.service
        $WSL_EXE -d "$UBUNTU_DISTRO" -u root -- systemctl enable --now pullrun-tcp-proxy.service

        info "  WSL2 services installed and started."
      fi
    fi
  else
    warn "  WSL2 not found. Install it from: https://learn.microsoft.com/en-us/windows/wsl/install"
  fi

  # Print PATH instructions
  case "$SHELL" in
    *zsh*) PROFILE="$HOME/.zshrc" ;;
    *bash*) PROFILE="$HOME/.bashrc" ;;
    *) PROFILE="$HOME/.profile" ;;
  esac
  if ! grep -q "$INSTALL_DIR" "$PROFILE" 2>/dev/null; then
    echo "export PATH=\"\$PATH:$INSTALL_DIR\"" >> "$PROFILE"
    info "  Added $INSTALL_DIR to PATH in $PROFILE"
  fi

  info ""
  info "Done! Open a new terminal (or 'source $PROFILE') then:"
  info "  pullrun --help"
  if [ -n "$WSL_EXE" ]; then
    info ""
    info "Pro tip — create a '.wslconfig' in your home directory:"
    info "  [wsl2]"
    info "  networkingMode=mirrored"
    info "  nestedVirtualization=true"
    info "  memory=4GB"
    info "  kernelCommandLine=mitigations=off vsyscall=none"
  fi
  exit 0
fi

# ── Binary download (works on any OS) ──────────────────────────────
info "Downloading pre-built binary for $OS/$ARCH..."
if [ "$VERSION" = "latest" ]; then
  API_URL="https://api.github.com/repos/$REPO/releases/latest"
  TAG=$(curl -fsSL "$API_URL" | grep '"tag_name"' | cut -d'"' -f4)
  if [ -z "$TAG" ]; then
    error "Could not determine latest release tag (GitHub API rate-limited?). Set VERSION=$CURRENT_VERSION $0"
    exit 1
  fi
else
  TAG="$VERSION"
fi

BASE="https://github.com/$REPO/releases/download/$TAG"
TMPDIR=$(mktemp -d)
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

# Download combined tarball
TARBALL="pullrun-${TAG#v}-${OS}-${ARCH}.tar.gz"
URL="$BASE/$TARBALL"
info "  downloading $TARBALL..."
curl -fsSL "$URL" | tar -xzf - -C "$TMPDIR"

# Install binaries (find them wherever they landed — the tarball may use
# ./bin/<name> or <name-arch> layout depending on the release pipeline).
installed=0
for BIN in pullrun pullrun-runtime apple-virt-exec; do
  SRC=""
  # 1. Check directly under the extraction root
  for f in "$TMPDIR/$BIN" "$TMPDIR/$BIN.exe"; do
    [ -f "$f" ] && SRC="$f" && break
  done
  # 2. Check under a bin/ subdirectory
  if [ -z "$SRC" ]; then
    for f in "$TMPDIR/bin/$BIN" "$TMPDIR/bin/$BIN.exe"; do
      [ -f "$f" ] && SRC="$f" && break
    done
  fi
  # 3. Try arch-suffixed name (the release tarball convention: $BIN-$OS-$ARCH)
  if [ -z "$SRC" ]; then
    for f in "$TMPDIR/$BIN-${OS}-${ARCH}" "$TMPDIR/$BIN-${OS}-${ARCH}.exe"; do
      [ -f "$f" ] && SRC="$f" && break
    done
  fi
  # 4. Last resort: exact basename match (not $BIN-* glob, which would
  #    match pullrun-runtime-* when searching for pullrun-*).
  if [ -z "$SRC" ]; then
    SRC=$(find "$TMPDIR" -maxdepth 2 -type f -name "$BIN" -o -name "$BIN.exe" 2>/dev/null | head -1)
  fi
  if [ -n "$SRC" ]; then
    # Try /usr/local/bin with sudo; fall back to ~/.local/bin.
    if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
      sudo mv "$SRC" "/usr/local/bin/$BIN"
      sudo chmod "+x" "/usr/local/bin/$BIN"
    else
      LOCAL_BIN="${XDG_DATA_HOME:-$HOME/.local}/bin"
      mkdir -p "$LOCAL_BIN"
      mv "$SRC" "$LOCAL_BIN/$BIN"
      chmod "+x" "$LOCAL_BIN/$BIN"
      info "  installed to $LOCAL_BIN/$BIN (add $LOCAL_BIN to your PATH)"
    fi
    installed=1
  fi
done

if [ "$installed" = "0" ]; then
  error "No binaries found in tarball. Please report this at https://github.com/$REPO/issues"
fi

info "Done! Run 'pullrun --help' to get started."
