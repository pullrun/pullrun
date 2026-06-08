#!/usr/bin/env bash
set -euo pipefail
#
# test-push-auth.sh — End-to-end push/pull auth round-trip test.
#
# Prerequisites:
#   1. docker (to run registry:2)
#   2. nimbus-runtime + nimbusctl installed
#
# Usage:
#   bash hack/test-push-auth.sh
#

REGISTRY_PORT="${REGISTRY_PORT:-5000}"
REGISTRY="localhost:${REGISTRY_PORT}"
SOCKET="${SOCKET:-/tmp/nimbus.sock}"
NIMBUSCTL="nimbusctl --socket ${SOCKET}"

cleanup() {
  echo "=== Cleaning up ==="
  docker stop nimbus-test-registry 2>/dev/null || true
  docker rm nimbus-test-registry 2>/dev/null || true
}

trap cleanup EXIT

echo "=== Step 1: Start registry:2 (plain HTTP) ==="
docker run -d --rm \
  --name nimbus-test-registry \
  -p "${REGISTRY_PORT}:5000" \
  registry:2

for i in $(seq 1 10); do
  if curl -s "http://${REGISTRY}/v2/" >/dev/null 2>&1; then
    echo "  Registry ready on ${REGISTRY}"
    break
  fi
  sleep 1
done

# Make sure daemon is configured
echo "=== Step 2: Ensure daemon has --insecure-registry ${REGISTRY} ==="
if ! ps aux | grep -q "nimbus-runtime.*daemon.*insecure-registry.*${REGISTRY}"; then
  echo "  Restarting daemon with --insecure-registry ${REGISTRY}"
  pkill -f "nimbus-runtime" 2>/dev/null || true
  sleep 1
  rm -f "${SOCKET}" 2>/dev/null
  env PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    nohup /usr/local/bin/nimbus-runtime daemon \
      --insecure-registry "${REGISTRY}" > /var/log/nimbus.log 2>&1 &
  sleep 2
  echo "  Daemon restarted"
fi

echo "=== Step 3: Pull alpine:3.18 from Docker Hub ==="
PULL_OUTPUT=$(${NIMBUSCTL} pull alpine:3.18 2>&1)
echo "${PULL_OUTPUT}"

# Extract root digest from pull output (line after "root digest:")
ROOT_DIGEST=$(echo "${PULL_OUTPUT}" | grep "root digest:" | awk '{print $3}')
if [ -z "${ROOT_DIGEST}" ]; then
  echo "  ✗ Could not extract root digest from pull output"
  exit 1
fi
echo "  Extracted root digest: ${ROOT_DIGEST}"

echo "=== Step 4: Push to local registry ==="
${NIMBUSCTL} push "${ROOT_DIGEST}" "${REGISTRY}/alpine:3.18"

echo "=== Step 5: Pull back from local registry ==="
PULL_BACK_OUTPUT=$(${NIMBUSCTL} pull "${REGISTRY}/alpine:3.18" 2>&1)
echo "${PULL_BACK_OUTPUT}"

PULL_BACK_DIGEST=$(echo "${PULL_BACK_OUTPUT}" | grep "root digest:" | awk '{print $3}')
if [ -z "${PULL_BACK_DIGEST}" ]; then
  echo "  ✗ Could not extract root digest from pull-back output"
  exit 1
fi

echo "=== Step 6: Compare digests ==="
echo "  Original: ${ROOT_DIGEST}"
echo "  Pulled:   ${PULL_BACK_DIGEST}"

if [ "${ROOT_DIGEST}" = "${PULL_BACK_DIGEST}" ]; then
  echo "  ✓ Digests match — push/pull round-trip OK"
else
  echo "  ✗ Digest MISMATCH"
  exit 1
fi

echo ""
echo "=== Push/pull auth round-trip: PASS ==="
