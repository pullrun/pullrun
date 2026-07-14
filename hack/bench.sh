#!/usr/bin/env bash
# Pullrun benchmark script — run with `hyperfine` for reproducible results.
#
# Prerequisites:
#   hyperfine (https://github.com/sharkdp/hyperfine)
#   pullrun CLI in PATH
#
# Usage:
#   bash hack/bench.sh              # default (container backend)
#   bash hack/bench.sh --backend vm # VM backend
#
# Results are printed to stdout and saved to pullrun-bench-*.md.

set -eu

BACKEND="${1:-container}"
IMAGE="${IMAGE:-alpine:3.18}"
WARMUPS="${WARMUPS:-3}"
RUNS="${RUNS:-10}"

if ! command -v hyperfine &>/dev/null; then
  echo "Install hyperfine first: brew install hyperfine"
  exit 1
fi

echo "=== Pullrun Benchmarks ==="
echo "Image:   $IMAGE"
echo "Backend: $BACKEND"
echo "Runs:    $RUNS (${WARMUPS} warm-ups)"
echo

# 1. Pull time (cold cache)
hyperfine --warmup 0 --runs 1 \
  --prepare "rm -rf ~/.pullrun/store 2>/dev/null; pullrun gc --apply --force 2>/dev/null || true" \
  "pullrun pull $IMAGE" \
  --export-markdown /tmp/pullrun-bench-pull.md

# 2. Run latency
hyperfine --warmup "$WARMUPS" --runs "$RUNS" \
  "pullrun run $IMAGE --backend $BACKEND --cmd echo --cmd done >/dev/null 2>&1" \
  --export-markdown /tmp/pullrun-bench-run.md

# 3. VM boot (only if --backend vm)
if [ "$BACKEND" = "vm" ]; then
  hyperfine --warmup "$WARMUPS" --runs "$RUNS" \
    "pullrun run $IMAGE --backend $BACKEND --tty --attach --cmd /bin/sh -c 'exit' 2>/dev/null" \
    --export-markdown /tmp/pullrun-bench-vmboot.md
fi

echo
echo "Results written to /tmp/pullrun-bench-*.md"
echo
cat /tmp/pullrun-bench-pull.md
cat /tmp/pullrun-bench-run.md
[ -f /tmp/pullrun-bench-vmboot.md ] && cat /tmp/pullrun-bench-vmboot.md
