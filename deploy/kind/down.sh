#!/usr/bin/env bash
# Deletes what deploy/kind/up.sh created: the kind cluster "sandbox", the
# Lima VM "sandbox" (with its Docker images and build cache) and
# <repo>/.kube/kind-sandbox.yaml. Leaves Lima and helm installed, and never
# touches the host's Docker or ~/.kube/config.
set -euo pipefail

KIND_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_DIR=$(cd "$KIND_DIR/../.." && pwd -P)
VM=sandbox
CLUSTER=sandbox
KUBECONFIG_FILE=$REPO_DIR/.kube/kind-sandbox.yaml

log() { printf '[kind] %s\n' "$*"; }

if command -v limactl >/dev/null && limactl list --quiet 2>/dev/null | grep -qx "$VM"; then
  if [ "$(limactl list --format '{{.Status}}' "$VM")" = Running ]; then
    log "deleting kind cluster $CLUSTER"
    limactl shell --workdir / "$VM" kind delete cluster --name "$CLUSTER" || true
  fi
  log "deleting Lima VM $VM"
  limactl delete --force "$VM"
else
  log "no Lima VM $VM"
fi
rm -f "$KUBECONFIG_FILE" "$KUBECONFIG_FILE.tmp"
log "down"
