#!/usr/bin/env bash
# Authoritative CRI gate: Kubernetes 1.37 conformance subset (critest v1.37).
#
# Usage:
#   hack/cri-conformance.sh unix:///run/pullrun/pullrun-cri.sock
#
# Requires `critest` v1.37 in PATH and a running pullrun-cri shim.
set -euo pipefail

ENDPOINT="${1:-unix:///run/pullrun/pullrun-cri.sock}"
CRITEST="${CRITEST:-critest}"

"$CRITEST" \
  -runtime-endpoint "$ENDPOINT" \
  -image-endpoint "$ENDPOINT" \
  -test.timeout 30m \
  --ginkgo.focus=Conformance \
  --ginkgo.timeout=30m \
  --ginkgo.v
