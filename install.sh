#!/usr/bin/env bash
set -euo pipefail

REPO="pullrun/pullrun"
VERSION="${VERSION:-latest}"

info()  { printf "\033[32m%s\033[0m\n" "$*"; }
warn()  { printf "\033[33m%s\033[0m\n" "$*"; }
error() { printf "\033[31m%s\033[0m\n" "$*" >&2; exit 1; }

# ── Platform detection ──────────────────────────────────────────────
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
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
  if brew install pullrun 2>/dev/null; then
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
  curl -fsSL "$KEY_URL" | sudo gpg --dearmor -o "$KEYRING" 2>/dev/null
  echo "deb [signed-by=$KEYRING] https://pullrun.github.io/apt stable main" \
    | sudo tee "$SOURCES" >/dev/null
  sudo apt-get update -qq 2>/dev/null
  sudo apt-get install -y -qq pullrun 2>/dev/null
  info "Done! Run 'pullrun --help' to get started."
  exit 0
fi

# ── Binary download (works on any OS) ──────────────────────────────
info "Downloading pre-built binary for $OS/$ARCH..."
if [ "$VERSION" = "latest" ]; then
  API_URL="https://api.github.com/repos/$REPO/releases/latest"
  TAG=$(curl -fsSL "$API_URL" | grep '"tag_name"' | cut -d'"' -f4)
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

# Install binaries
for BIN in pullrun pullrun-runtime apple-virt-exec; do
  if [ -f "$TMPDIR/bin/$BIN" ]; then
    sudo mv "$TMPDIR/bin/$BIN" "/usr/local/bin/$BIN"
    sudo chmod +x "/usr/local/bin/$BIN"
  fi
done

info "Done! Run 'pullrun --help' to get started."
