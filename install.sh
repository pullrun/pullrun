#!/usr/bin/env bash
set -euo pipefail

REPO="pullrun/pullrun"
VERSION="${VERSION:-latest}"

info()  { printf "\033[32m%s\033[0m\n" "$*"; }
error() { printf "\033[31m%s\033[0m\n" "$*" >&2; exit 1; }

# ── Platform detection ──────────────────────────────────────────────
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) error "unsupported arch: $ARCH" ;;
esac

# ── macOS: Homebrew ─────────────────────────────────────────────────
if [ "$OS" = "darwin" ]; then
  if command -v brew &>/dev/null; then
    info "Installing via Homebrew..."
    brew tap pullrun/tap
    if brew trust pullrun/tap 2>/dev/null; then : ; fi
    brew install pullrun
    info "Done! Run 'pullrun --help' to get started."
    exit 0
  fi
  info "Homebrew not found. Falling back to binary download..."
fi

# ── Linux: APT ──────────────────────────────────────────────────────
if [ "$OS" = "linux" ]; then
  if command -v apt-get &>/dev/null; then
    info "Installing via APT..."
    curl -fsSL "https://pullrun.github.io/apt/key.gpg" \
      | sudo gpg --dearmor -o /usr/share/keyrings/pullrun.gpg
    echo "deb [signed-by=/usr/share/keyrings/pullrun.gpg] https://pullrun.github.io/apt stable main" \
      | sudo tee /etc/apt/sources.list.d/pullrun.list
    sudo apt-get update
    sudo apt-get install -y pullrun
    info "Done! Run 'pullrun --help' to get started."
    exit 0
  fi
  info "APT not found. Falling back to binary download..."
fi

# ── Binary download fallback ────────────────────────────────────────
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

for BIN in pullrun pullrun-runtime; do
  URL="$BASE/${BIN}-${TAG#v}-${OS}-${ARCH}.tar.gz"
  info "  downloading $BIN..."
  curl -fsSL "$URL" | tar -xzf - -C "$TMPDIR"
  sudo mv "$TMPDIR/$BIN" "/usr/local/bin/$BIN"
  sudo chmod +x "/usr/local/bin/$BIN"
done

info "Done! Run 'pullrun --help' to get started."
