#!/usr/bin/env bash
# Deletes what deploy/kind/up.sh created: the kind cluster "sandbox", on a Mac
# the Lima VM "sandbox" (with its Docker images and build cache), and
# <repo>/.kube/kind-sandbox.yaml. On Linux only the cluster and the kubeconfig
# file go (kind delete uses --kubeconfig, so ~/.kube/config is not touched);
# the built images stay in the host's Docker. Leaves Lima and helm installed.
set -euo pipefail

KIND_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_DIR=$(cd "$KIND_DIR/../.." && pwd -P)
VM=sandbox
CLUSTER=sandbox
KUBECONFIG_FILE=$REPO_DIR/.kube/kind-sandbox.yaml

log() { printf '[kind] %s\n' "$*"; }

if [ "$(uname -s)" = Linux ]; then
  if command -v kind >/dev/null && kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    log "deleting kind cluster $CLUSTER"
    kind delete cluster --name "$CLUSTER" --kubeconfig "$KUBECONFIG_FILE"
  else
    log "no kind cluster $CLUSTER"
  fi
elif command -v limactl >/dev/null && limactl list --quiet 2>/dev/null | grep -qx "$VM"; then
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
