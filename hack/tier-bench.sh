#!/usr/bin/env bash
# Compare Pullrun execution tiers on this host.
#
#   macOS: sandbox group vs per-task Apple VM
#   Linux: rootless container vs per-task Firecracker (if KVM is usable)
#
# Requirements: a pullrun CLI and runtime that agree on the socket.
# On macOS the runtime must be signed with the virtualization entitlement
# and PULLRUN_KERNEL_PATH / PULLRUN_INITRAMFS_PATH must point at a
# group-capable kernel/initramfs.
#
# Usage:
#   PULLRUN_BIN=... PULLRUN_RUNTIME=... bash hack/tier-bench.sh [IMAGE] [N]

set -euo pipefail

IMAGE="${1:-alpine:3.18}"
N="${2:-5}"

PULLRUN_BIN="${PULLRUN_BIN:-pullrun}"
PULLRUN_RUNTIME="${PULLRUN_RUNTIME:-pullrun-runtime}"
OS="$(uname -s)"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/pullrun-tier-bench.XXXXXX")"
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

cli() { "$PULLRUN_BIN" --direct=false --socket "$SOCK" "$@"; }
now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }

vm_args=(--backend vm --attach --cmd /bin/sh --cmd -c --cmd true)
container_args=(--backend container --net none --attach --cmd /bin/sh --cmd -c --cmd true)

time_seq() {
  local start; start=$(now_ms)
  for _ in $(seq 1 "$N"); do "$@" >/dev/null 2>&1; done
  echo $(( $(now_ms) - start ))
}

time_par() {
  local start; start=$(now_ms); local pids=()
  for _ in $(seq 1 "$N"); do "$@" >/dev/null 2>&1 & pids+=($!); done
  for pid in "${pids[@]}"; do wait "$pid" || true; done
  echo $(( $(now_ms) - start ))
}

echo "## Tier benchmark"
echo
echo "- image: \`$IMAGE\`"
echo "- sandboxes per run: $N"
echo "- host: $(uname -sm)"
echo

# daemon
"$PULLRUN_RUNTIME" daemon --socket "$SOCK" --store-root "$STORE" \
  >"$WORKDIR/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 100); do [[ -S "$SOCK" ]] && break; sleep 0.1; done
[[ -S "$SOCK" ]] || { echo "runtime did not start" >&2; cat "$WORKDIR/daemon.log" >&2; exit 1; }
cli pull "$IMAGE" >/dev/null 2>&1

echo "| tier | sequential | concurrent |"
echo "|---|---:|---:|"

if [[ "$OS" == "Darwin" ]]; then
  # --- sandbox group -------------------------------------------------------
  gstart=$(now_ms)
  GROUP=$(cli group create --image "$IMAGE" 2>/dev/null | tail -1)
  grow=$(( $(now_ms) - gstart ))
  group_run() { cli group run "$GROUP" -- /bin/sh -c true; }
  group_run >/dev/null 2>&1 || true
  gseq=$(time_seq group_run)
  gpar=$(time_par group_run)
  echo "| group (one VM, boot ${grow} ms) | ${gseq} ms | **${gpar} ms** |"
  cli group rm "$GROUP" >/dev/null 2>&1; GROUP=""

  # --- per-task Apple VM ---------------------------------------------------
  vm_run() { cli run "$IMAGE" "${vm_args[@]}"; }
  vm_run >/dev/null 2>&1 || true
  vseq=$(time_seq vm_run)
  vpar=$(time_par vm_run)
  echo "| per-task Apple VM | ${vseq} ms | **${vpar} ms** |"
else
  # --- container -----------------------------------------------------------
  if cli run --help >/dev/null 2>&1; then
    ctr_run() { cli run "$IMAGE" "${container_args[@]}"; }
    if ctr_run >/dev/null 2>&1; then
      cseq=$(time_seq ctr_run)
      cpar=$(time_par ctr_run)
      echo "| container (rootless runc) | ${cseq} ms | **${cpar} ms** |"
    else
      echo "| container (rootless runc) | unavailable | unavailable |"
    fi
  fi
  if [[ -r /dev/kvm ]]; then
    vm_run() { cli run "$IMAGE" "${vm_args[@]}"; }
    if vm_run >/dev/null 2>&1; then
      vseq=$(time_seq vm_run)
      vpar=$(time_par vm_run)
      echo "| per-task Firecracker VM | ${vseq} ms | **${vpar} ms** |"
    else
      echo "| per-task Firecracker VM | unavailable | unavailable |"
    fi
  else
    echo "| per-task Firecracker VM | /dev/kvm not accessible | /dev/kvm not accessible |"
  fi
  echo "| sandbox group | not implemented on Linux | not implemented on Linux |"
fi
