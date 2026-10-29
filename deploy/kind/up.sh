#!/usr/bin/env bash
# Starts the Kubernetes environment for the sandbox pool: the Lima VM
# "sandbox" (deploy/kind/lima.yaml, with /dev/kvm through nested
# virtualization, on Ubuntu's HWE kernel), Docker and kind inside it, and the
# kind cluster "sandbox" (deploy/kind/kind.yaml). Idempotent: reuses a VM or
# cluster that exists, and reboots the VM only when a newer kernel is
# installed than the one it runs.
#
# Docker and kind run only inside the VM. This script never changes the
# host's Docker context or ~/.kube/config: the cluster's kubeconfig goes to
# <repo>/.kube/kind-sandbox.yaml (gitignored). Use it as
#
#   KUBECONFIG=<repo>/.kube/kind-sandbox.yaml kubectl --context kind-sandbox ...
#
# deploy/kind/down.sh deletes the cluster and the VM.
set -euo pipefail

KIND_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_DIR=$(cd "$KIND_DIR/../.." && pwd -P)
VM=sandbox
CLUSTER=sandbox
CONTEXT=kind-$CLUSTER
KUBECONFIG_FILE=$REPO_DIR/.kube/kind-sandbox.yaml
API_PORT=$(awk '/apiServerPort:/ {print $2}' "$KIND_DIR/kind.yaml")

log() { printf '[kind] %s\n' "$*"; }
die() {
  printf '[kind] error: %s\n' "$*" >&2
  exit 1
}
# Runs a command in the VM, from / (the host's cwd may not exist there).
vm() { limactl shell --workdir / "$VM" "$@"; }
host_kubectl() { KUBECONFIG=$KUBECONFIG_FILE kubectl --context "$CONTEXT" "$@"; }

[ "$(uname -s)/$(uname -m)" = Darwin/arm64 ] || die "needs macOS on Apple silicon (M3 or later)"
case $REPO_DIR in
*'"'* | *\\* | *,* | *:*) die "the repo path must not contain quotes, backslashes, commas or colons: $REPO_DIR" ;;
esac
[ -n "$API_PORT" ] || die "no apiServerPort in $KIND_DIR/kind.yaml"
command -v kubectl >/dev/null || die "kubectl is not installed"
for tool in limactl helm; do
  if ! command -v "$tool" >/dev/null; then
    formula=$tool
    [ "$tool" = limactl ] && formula=lima
    log "installing $formula (brew)"
    brew install "$formula"
  fi
done

# --- the VM ---------------------------------------------------------------
if ! limactl list --quiet 2>/dev/null | grep -qx "$VM"; then
  log "creating and starting Lima VM $VM (first start downloads Ubuntu 24.04)"
  limactl start --tty=false --name "$VM" --mount "$REPO_DIR:w" "$KIND_DIR/lima.yaml"
elif [ "$(limactl list --format '{{.Status}}' "$VM")" != Running ]; then
  log "starting Lima VM $VM"
  limactl start --tty=false "$VM"
else
  log "Lima VM $VM is running"
fi

# lima.yaml installs Ubuntu's HWE kernel, which takes effect only after a
# reboot: reboot once when the running kernel is older than the newest
# installed one (the one GRUB boots by default).
running_kernel=$(vm uname -r)
newest_kernel=$(vm sh -c 'ls /boot/vmlinuz-*' | sed 's|^/boot/vmlinuz-||' | sort -V | tail -n 1)
[ -n "$newest_kernel" ] || die "VM $VM: no kernel in /boot"
if [ "$running_kernel" != "$newest_kernel" ] &&
  [ "$(printf '%s\n%s\n' "$running_kernel" "$newest_kernel" | sort -V | tail -n 1)" = "$newest_kernel" ]; then
  log "VM $VM runs kernel $running_kernel; rebooting it into $newest_kernel"
  limactl stop "$VM"
  limactl start --tty=false "$VM"
  running_kernel=$(vm uname -r)
  [ "$running_kernel" = "$newest_kernel" ] ||
    die "VM $VM still runs kernel $running_kernel after a reboot (want $newest_kernel)"
fi
log "VM $VM: kernel $running_kernel"

vm test -w "$REPO_DIR" ||
  die "VM $VM does not mount $REPO_DIR writable; it was created for another checkout (deploy/kind/down.sh deletes it)"
if ! vm test -c /dev/kvm; then
  vm sh -c 'sudo dmesg | grep -i kvm' >&2 || true
  die "VM $VM has no /dev/kvm (nested virtualization needs vz on an M3 or later with macOS 15 or later)"
fi
log "VM $VM: $(vm ls -l /dev/kvm)"

# --- the cluster ----------------------------------------------------------
if vm kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  log "kind cluster $CLUSTER exists; starting its nodes"
  # kind nodes do not restart on their own after the VM restarts.
  vm sh -c "docker start \$(kind get nodes --name $CLUSTER)" >/dev/null
else
  if lsof -nP -iTCP:"$API_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    die "host port $API_PORT (the cluster's API server, forwarded by Lima) is in use"
  fi
  log "creating kind cluster $CLUSTER"
  vm kind create cluster --config "$KIND_DIR/kind.yaml" --wait 5m
fi

# --- the kubeconfig -------------------------------------------------------
mkdir -p "$(dirname "$KUBECONFIG_FILE")"
(
  umask 077
  vm kind get kubeconfig --name "$CLUSTER" >"$KUBECONFIG_FILE.tmp"
)
mv "$KUBECONFIG_FILE.tmp" "$KUBECONFIG_FILE"
log "kubeconfig: $KUBECONFIG_FILE (context $CONTEXT)"

# Lima forwards the API server port once it sees it listen in the VM.
for _ in $(seq 60); do
  host_kubectl get --raw /readyz >/dev/null 2>&1 && break
  sleep 2
done
host_kubectl wait --for=condition=Ready node --all --timeout=300s
host_kubectl get nodes -o wide
log "up in ${SECONDS}s"
