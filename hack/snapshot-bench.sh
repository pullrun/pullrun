#!/usr/bin/env bash
# Firecracker snapshot/restore benchmark.
#
# Starts a daemon, boots one VM, captures a memory snapshot, then restores
# it N times and reports capture/restore wall times.
#
# Requirements: Linux + /dev/kvm + Firecracker + kernel.
#
# Usage:
#   PULLRUN_BIN=... PULLRUN_RUNTIME=... \
#   PULLRUN_DAEMON_ARGS="--vm-firecracker ... --vm-kernel ... --vm-root ..." \
#   bash hack/snapshot-bench.sh [IMAGE] [N]

set -euo pipefail

IMAGE="${1:-alpine:3.18}"
N="${2:-5}"
PULLRUN_BIN="${PULLRUN_BIN:-pullrun}"
PULLRUN_RUNTIME="${PULLRUN_RUNTIME:-pullrun-runtime}"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/pullrun-snapshot-bench.XXXXXX")"
SOCK="$WORKDIR/runtime.sock"
STORE="$WORKDIR/store"
DAEMON_PID=""

cleanup() {
  [[ -n "$DAEMON_PID" ]] && kill "$DAEMON_PID" 2>/dev/null || true
  [[ -n "$DAEMON_PID" ]] && wait "$DAEMON_PID" 2>/dev/null || true
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

cli() { "$PULLRUN_BIN" --direct=false --socket "$SOCK" "$@"; }

daemon_args=()
if [[ -n "${PULLRUN_DAEMON_ARGS:-}" ]]; then
  read -r -a daemon_args <<<"$PULLRUN_DAEMON_ARGS"
fi

"$PULLRUN_RUNTIME" daemon --socket "$SOCK" --store-root "$STORE" \
  "${daemon_args[@]}" >"$WORKDIR/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 100); do [[ -S "$SOCK" ]] && break; sleep 0.1; done
[[ -S "$SOCK" ]] || { echo "runtime did not start" >&2; cat "$WORKDIR/daemon.log" >&2; exit 1; }

cli pull "$IMAGE" >/dev/null 2>&1
RUN_OUT=$(cli run "$IMAGE" --backend vm --net none \
  --cmd /bin/sh --cmd -c --cmd 'i=0; while true; do i=$((i+1)); echo tick $i; sleep 1; done' 2>&1)
ID=$(echo "$RUN_OUT" | awk '/Started/{print $2; exit}')
[[ -n "$ID" ]] || { echo "failed to start VM" >&2; echo "$RUN_OUT" >&2; exit 1; }
sleep 2

CREATE_OUT=$(cli snapshot create "$ID" 2>&1)
SNAP=$(echo "$CREATE_OUT" | awk '/Snapshot created/{print $3; exit}')
CAPTURE_MS=$(echo "$CREATE_OUT" | awk '/capture:/{print $(NF-1); exit}')
[[ -n "$SNAP" ]] || { echo "snapshot failed" >&2; echo "$CREATE_OUT" >&2; exit 1; }

restore_ms=()
for _ in $(seq 1 "$N"); do
  OUT=$(cli snapshot restore "$SNAP" 2>&1)
  RESTORED=$(echo "$OUT" | awk '/Restored/{print $5; exit}')
  MS=$(echo "$OUT" | sed -n 's/.*, \([0-9]\+\) ms).*/\1/p' | tail -1)
  [[ -n "$RESTORED" && -n "$MS" ]] || { echo "restore failed" >&2; echo "$OUT" >&2; exit 1; }
  restore_ms+=("$MS")
  cli stop "$RESTORED" >/dev/null 2>&1 || true
done

stats=$(printf '%s\n' "${restore_ms[@]}" | python3 -c '
import sys
v = sorted(int(x) for x in sys.stdin if x.strip())
print(f"min={v[0]} avg={sum(v)/len(v):.0f} max={v[-1]}")')

echo "## Snapshot benchmark"
echo
echo "- image: \`$IMAGE\`"
echo "- restores: $N"
echo "- host: $(uname -sm)"
echo
echo "| metric | value |"
echo "|---|---|"
echo "| snapshot capture | ${CAPTURE_MS} ms |"
echo "| restore (${stats}) | ${restore_ms[*]} ms |"
