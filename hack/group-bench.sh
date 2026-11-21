#!/usr/bin/env bash
# Sandbox-group benchmark: concurrency, latency, and shared page cache.
#
# Requirements (macOS Apple Virtualization):
#   - pullrun CLI and a signed pullrun-runtime with the virtualization
#     entitlement
#   - PULLRUN_KERNEL_PATH, PULLRUN_INITRAMFS_PATH (group-capable initramfs)
#
# Usage:
#   PULLRUN_BIN=... PULLRUN_RUNTIME=... \
#   PULLRUN_KERNEL_PATH=... PULLRUN_INITRAMFS_PATH=... \
#   bash hack/group-bench.sh [IMAGE] [N]
#
# Prints a Markdown table with:
#   - group creation time
#   - N sandboxes sequentially vs concurrently (wall clock)
#   - guest page-cache growth after reading the whole image once, and
#     after N concurrent/sequential readers (shared cache check)

set -euo pipefail

IMAGE="${1:-alpine:3.18}"
N="${2:-5}"

PULLRUN_BIN="${PULLRUN_BIN:-pullrun}"
PULLRUN_RUNTIME="${PULLRUN_RUNTIME:-pullrun-runtime}"
: "${PULLRUN_KERNEL_PATH:?set PULLRUN_KERNEL_PATH}"
: "${PULLRUN_INITRAMFS_PATH:?set PULLRUN_INITRAMFS_PATH}"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/pullrun-group-bench.XXXXXX")"
SOCK="$WORKDIR/runtime.sock"
STORE="$WORKDIR/store"
DAEMON_PID=""
GROUP=""

cleanup() {
  if [[ -n "$GROUP" && -n "$DAEMON_PID" ]]; then
    "$PULLRUN_BIN" --direct=false --socket "$SOCK" group rm "$GROUP" >/dev/null 2>&1 || true
  fi
  if [[ -n "$DAEMON_PID" ]]; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

cli() { "$PULLRUN_BIN" --direct=false --socket "$SOCK" "$@"; }

echo "## Sandbox group benchmark"
echo
echo "- image: \`$IMAGE\`"
echo "- sandboxes: $N"
echo "- host: $(uname -sm)"
echo

ceil_kb() { awk -v v="$1" 'BEGIN { printf "%.1f", v/1024 }'; }

# --- Boot -------------------------------------------------------------------
START=$(now_ms)
"$PULLRUN_RUNTIME" daemon --socket "$SOCK" --store-root "$STORE" \
  >"$WORKDIR/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 100); do
  [[ -S "$SOCK" ]] && break
  sleep 0.1
done
[[ -S "$SOCK" ]] || { echo "runtime did not start" >&2; cat "$WORKDIR/daemon.log" >&2; exit 1; }

cli pull "$IMAGE" >/dev/null 2>&1
PULL_MS=$(( $(now_ms) - START ))

START=$(now_ms)
GROUP=$(cli group create --image "$IMAGE" 2>/dev/null | tail -1)
[[ -n "$GROUP" ]] || { echo "group create failed" >&2; exit 1; }
BOOT_MS=$(( $(now_ms) - START ))

run_one() { cli group run "$GROUP" -- /bin/sh -c "$1" >/dev/null 2>&1; }

# --- Concurrency: N x true --------------------------------------------------
START=$(now_ms)
for _ in $(seq 1 "$N"); do run_one 'true'; done
SEQ_MS=$(( $(now_ms) - START ))

START=$(now_ms)
pids=()
for _ in $(seq 1 "$N"); do run_one 'true' & pids+=($!); done
for pid in "${pids[@]}"; do wait "$pid" || true; done
CONC_MS=$(( $(now_ms) - START ))

# --- Overlap: N x sleep 0.5 -------------------------------------------------
START=$(now_ms)
pids=()
for _ in $(seq 1 "$N"); do run_one 'sleep 0.5' & pids+=($!); done
for pid in "${pids[@]}"; do wait "$pid" || true; done
SLEEP_CONC_MS=$(( $(now_ms) - START ))

# --- Guest page cache -------------------------------------------------------
cached_kb() {
  cli group run "$GROUP" -- /bin/sh -c 'grep ^Cached /proc/meminfo' 2>/dev/null |
    awk '/^Cached:/{print $2}'
}
READ_ALL='find / -xdev -type f -exec cat {} + >/dev/null 2>&1; grep ^Cached /proc/meminfo'

C0=$(cached_kb)
C1=$(cli group run "$GROUP" -- /bin/sh -c "$READ_ALL" 2>/dev/null | awk '/^Cached:/{print $2}')

START=$(now_ms)
pids=()
for _ in $(seq 1 "$N"); do
  cli group run "$GROUP" -- /bin/sh -c "$READ_ALL" >"$WORKDIR/c$_.out" 2>/dev/null & pids+=($!);
done
for pid in "${pids[@]}"; do wait "$pid" || true; done
C_CONC_MS=$(( $(now_ms) - START ))
C_CONC=$(awk '/^Cached:/{print $2}' "$WORKDIR"/c*.out | sort -n | tail -1)

START=$(now_ms)
for i in $(seq 1 "$N"); do
  cli group run "$GROUP" -- /bin/sh -c "$READ_ALL" >"$WORKDIR/s$i.out" 2>/dev/null
done
C_SEQ_MS=$(( $(now_ms) - START ))
C_SEQ=$(awk '/^Cached:/{print $2}' "$WORKDIR"/s*.out | sort -n | tail -1)

echo "| metric | value |"
echo "|---|---|"
echo "| pull + daemon start | ${PULL_MS} ms |"
echo "| group boot (one VM) | ${BOOT_MS} ms |"
echo "| ${N} x \`true\` sequential | ${SEQ_MS} ms |"
echo "| ${N} x \`true\` concurrent | ${CONC_MS} ms |"
echo "| ${N} x \`sleep 0.5\` concurrent | ${SLEEP_CONC_MS} ms |"
echo "| guest Cached, idle | $(ceil_kb "${C0:-0}") MiB |"
echo "| guest Cached after 1 reader | $(ceil_kb "${C1:-0}") MiB |"
echo "| guest Cached after ${N} concurrent readers | $(ceil_kb "${C_CONC:-0}") MiB |"
echo "| guest Cached after ${N} sequential readers | $(ceil_kb "${C_SEQ:-0}") MiB |"
echo "| ${N} concurrent readers wall time | ${C_CONC_MS} ms |"
echo "| ${N} sequential readers wall time | ${C_SEQ_MS} ms |"
