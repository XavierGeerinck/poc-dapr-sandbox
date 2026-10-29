#!/usr/bin/env bash
# Starts the Kubernetes environment for the sandbox pool. Two modes:
#
#   macOS (Apple silicon): the Lima VM "sandbox" (deploy/kind/lima.yaml, with
#     /dev/kvm through nested virtualization, on Ubuntu's HWE kernel), Docker
#     and kind inside it, and the kind cluster "sandbox"
#     (deploy/kind/kind.yaml). Reboots the VM at most once in its life, to
#     switch to the HWE kernel.
#   Linux with /dev/kvm: docker and kind run directly on the host, no VM.
#     Tools are not installed for you: docker, kind, kubectl and helm must
#     exist, and the user must be able to run docker.
#
# Idempotent: reuses a VM or cluster that exists.
#
# This script never changes the host's Docker context or ~/.kube/config: the
# cluster's kubeconfig goes to <repo>/.kube/kind-sandbox.yaml (gitignored),
# on Linux also at `kind create cluster` time. Use it as
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
if [ "$(uname -s)" = Linux ]; then
  MODE=linux
else
  MODE=lima
fi
# Runs a command in the VM, from / (the host's cwd may not exist there); on
# Linux directly on the host.
if [ $MODE = linux ]; then
  vm() { (cd / && "$@"); }
else
  vm() { limactl shell --workdir / "$VM" "$@"; }
fi
host_kubectl() { KUBECONFIG=$KUBECONFIG_FILE kubectl --context "$CONTEXT" "$@"; }

if [ $MODE = linux ]; then
  [ -c /dev/kvm ] || die "no /dev/kvm on this host (needs KVM)"
else
  [ "$(uname -s)/$(uname -m)" = Darwin/arm64 ] || die "needs macOS on Apple silicon (M3 or later), or Linux with /dev/kvm"
fi
case $REPO_DIR in
*'"'* | *\\* | *,* | *:*) die "the repo path must not contain quotes, backslashes, commas or colons: $REPO_DIR" ;;
esac
[ -n "$API_PORT" ] || die "no apiServerPort in $KIND_DIR/kind.yaml"
command -v kubectl >/dev/null || die "kubectl is not installed"
if [ $MODE = linux ]; then
  for tool in docker kind helm; do
    command -v "$tool" >/dev/null || die "$tool is not installed (Linux mode installs nothing)"
  done
  docker info >/dev/null 2>&1 || die "cannot talk to docker (is the daemon running, is $USER in the docker group?)"
else
  for tool in limactl helm; do
    if ! command -v "$tool" >/dev/null; then
      formula=$tool
      [ "$tool" = limactl ] && formula=lima
      log "installing $formula (brew)"
      brew install "$formula"
    fi
  done
fi

# --- the VM ---------------------------------------------------------------
if [ $MODE = lima ]; then
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
  # reboot. up.sh reboots the VM for it at most once: a marker in the VM
  # records the reboot, or that the VM already ran its newest kernel. So a
  # kernel that an unattended upgrade installs later waits for the next VM
  # restart, and a GRUB default other than the newest kernel fails here once
  # instead of rebooting the VM on every run.
  reboot_marker=/var/lib/sandbox-kind/kernel-reboot
  running_kernel=$(vm uname -r)
  newest_kernel=$(vm sh -c 'ls /boot/vmlinuz-*' | sed 's|^/boot/vmlinuz-||' | sort -V | tail -n 1)
  [ -n "$newest_kernel" ] || die "VM $VM: no kernel in /boot"
  # shellcheck disable=SC2016 # expanded in the VM
  mark_reboot() { vm sudo sh -c 'mkdir -p "${1%/*}" && echo "$2" >"$1"' sh "$reboot_marker" "$1"; }
  if [ "$running_kernel" = "$newest_kernel" ]; then
    vm test -e "$reboot_marker" || mark_reboot "$running_kernel"
  elif [ "$(printf '%s\n%s\n' "$running_kernel" "$newest_kernel" | sort -V | tail -n 1)" = "$newest_kernel" ]; then
    if vm test -e "$reboot_marker"; then
      log "VM $VM runs kernel $running_kernel; $newest_kernel is installed too, but up.sh does not reboot the VM again ($reboot_marker in the VM): restart it yourself to switch (limactl stop $VM && limactl start $VM)"
    else
      log "VM $VM runs kernel $running_kernel; rebooting it once into $newest_kernel"
      mark_reboot "$newest_kernel"
      limactl stop "$VM"
      limactl start --tty=false "$VM"
      running_kernel=$(vm uname -r)
      [ "$running_kernel" = "$newest_kernel" ] ||
        die "VM $VM booted kernel $running_kernel after its reboot, not the newest installed one, $newest_kernel: GRUB's default entry is another kernel. up.sh will not reboot it again; make $newest_kernel GRUB's default and restart the VM, or delete $reboot_marker in the VM to let up.sh retry"
    fi
  fi
  log "VM $VM: kernel $running_kernel"

  vm test -w "$REPO_DIR" ||
    die "VM $VM does not mount $REPO_DIR writable; it was created for another checkout (deploy/kind/down.sh deletes it)"
  if ! vm test -c /dev/kvm; then
    vm sh -c 'sudo dmesg | grep -i kvm' >&2 || true
    die "VM $VM has no /dev/kvm (nested virtualization needs vz on an M3 or later with macOS 15 or later)"
  fi
  log "VM $VM: $(vm ls -l /dev/kvm)"
else
  log "native Linux: kernel $(uname -r), $(ls -l /dev/kvm)"
fi

# --- the cluster ----------------------------------------------------------
if vm kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  log "kind cluster $CLUSTER exists; starting its nodes"
  # kind nodes do not restart on their own after the VM restarts.
  vm sh -c "docker start \$(kind get nodes --name $CLUSTER)" >/dev/null
else
  if [ $MODE = linux ]; then
    # Never write ~/.kube/config: kind puts the context only in this file.
    mkdir -p "$(dirname "$KUBECONFIG_FILE")"
    log "creating kind cluster $CLUSTER"
    vm kind create cluster --config "$KIND_DIR/kind.yaml" --kubeconfig "$KUBECONFIG_FILE" --wait 5m
  else
    if lsof -nP -iTCP:"$API_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
      die "host port $API_PORT (the cluster's API server, forwarded by Lima) is in use"
    fi
    log "creating kind cluster $CLUSTER"
    vm kind create cluster --config "$KIND_DIR/kind.yaml" --wait 5m
  fi
fi

# After a VM restart Docker may give the nodes other IPs (it restarts them in
# any order). kind then rewrites a node's old IP in its manifests, but not in
# the control plane's own kubelet.conf, where kubeadm wrote the node's IP:
# that kubelet would lose the API server and its node go NotReady. Point it
# at the control plane's hostname instead, as the workers' kubelets are
# (the API server's certificate covers it).
for node in $(vm kind get nodes --name "$CLUSTER"); do
  case $node in *-control-plane) ;; *) continue ;; esac
  # Exits 0 when it changed the file, 3 when it already used the hostname.
  rc=0
  # shellcheck disable=SC2016 # expanded in the node
  vm docker exec -e NODE="$node" "$node" sh -c '
    conf=/etc/kubernetes/kubelet.conf
    grep -q "server: https://$NODE:6443\$" $conf && exit 3
    sed -i -E "s#server: https://[0-9.]+:6443\$#server: https://$NODE:6443#" $conf
    grep -q "server: https://$NODE:6443\$" $conf' || rc=$?
  case $rc in
  0)
    log "$node: its kubelet now reaches the API server as $node:6443; restarting it"
    vm docker exec "$node" systemctl restart kubelet
    ;;
  3) ;;
  *) die "$node: cannot point its kubelet.conf at $node:6443" ;;
  esac
done

# singleProcessOOMKill (see kind.yaml) on the nodes of a cluster created
# without it. It applies to the containers a kubelet creates afterwards.
for node in $(vm kind get nodes --name "$CLUSTER"); do
  # Exits 0 when it changed the file, 3 when it already had the setting.
  rc=0
  # shellcheck disable=SC2016 # expanded in the node
  vm docker exec "$node" sh -c '
    conf=/var/lib/kubelet/config.yaml
    grep -qx "singleProcessOOMKill: true" $conf && exit 3
    sed -i "/^singleProcessOOMKill:/d" $conf
    [ -z "$(tail -c 1 $conf)" ] || echo >>$conf
    echo "singleProcessOOMKill: true" >>$conf' || rc=$?
  case $rc in
  0)
    log "$node: kubelet singleProcessOOMKill set; restarting it"
    vm docker exec "$node" systemctl restart kubelet
    ;;
  3) ;;
  *) die "$node: cannot set singleProcessOOMKill in its kubelet config" ;;
  esac
done

# --- the kubeconfig -------------------------------------------------------
mkdir -p "$(dirname "$KUBECONFIG_FILE")"
(
  umask 077
  vm kind get kubeconfig --name "$CLUSTER" >"$KUBECONFIG_FILE.tmp"
)
mv "$KUBECONFIG_FILE.tmp" "$KUBECONFIG_FILE"
log "kubeconfig: $KUBECONFIG_FILE (context $CONTEXT)"

if [ $MODE = lima ]; then
  # Lima forwards the API server port once it sees it listen in the VM.
  for _ in $(seq 60); do
    host_kubectl get --raw /readyz >/dev/null 2>&1 && break
    sleep 2
  done
fi
host_kubectl wait --for=condition=Ready node --all --timeout=300s
host_kubectl get nodes -o wide
log "up in ${SECONDS}s"
