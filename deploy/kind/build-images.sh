#!/usr/bin/env bash
# Builds the images the kind deployment runs and loads them into the kind
# cluster "sandbox" (start it with deploy/kind/up.sh first):
#
#   dapr    Dapr's control plane and daprd from upstream/dapr: `make
#           build-linux` on this Mac cross-compiles the linux/arm64 binaries
#           into upstream/dapr/dist/linux_arm64/release, then `make
#           docker-build` in the Lima VM wraps each one in a distroless image,
#           localhost/dapr/<binary>:dev-linux-arm64 (deploy/kind/helm-values.yaml).
#   driver  dapr-sandbox-microvm:dev (deploy/images/driver.Dockerfile).
#   bench   sandbox-bench:dev, the caller pod's image (deploy/images/bench.Dockerfile).
#
#   deploy/kind/build-images.sh                 # all three
#   deploy/kind/build-images.sh driver bench    # only these
#
# Needs Go (the version in upstream/dapr/go.mod) and make on the Mac. Docker
# and kind run only inside the VM; the repo is mounted there at its own path.
# Pods use these images with imagePullPolicy: Never, so a rebuilt image takes
# effect for new pods only.
set -euo pipefail

KIND_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_DIR=$(cd "$KIND_DIR/../.." && pwd -P)
DAPR_SRC=$REPO_DIR/upstream/dapr
VM=sandbox
CLUSTER=sandbox
ARCH=arm64
DAPR_REGISTRY=localhost/dapr
DAPR_TAG=dev
# The images the Helm chart and the injector use; `make docker-build` also
# builds an all-in-one localhost/dapr/dapr image, which nothing here runs.
DAPR_BINARIES="daprd placement operator injector sentry scheduler"
DRIVER_IMAGE=dapr-sandbox-microvm:dev
BENCH_IMAGE=sandbox-bench:dev

log() { printf '[images] %s\n' "$*"; }
die() {
  printf '[images] error: %s\n' "$*" >&2
  exit 1
}
# vm_in DIR CMD...: runs CMD in the VM, in DIR.
vm_in() {
  local dir=$1
  shift
  limactl shell --workdir "$dir" "$VM" "$@"
}
# timed NAME CMD...: runs CMD and logs how long it took.
timed() {
  local name=$1 t0=$SECONDS
  shift
  "$@"
  log "$name: $((SECONDS - t0))s"
}

targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=(dapr driver bench)
for t in "${targets[@]}"; do
  case $t in
  dapr | driver | bench) ;;
  *) die "unknown target $t (want dapr, driver or bench)" ;;
  esac
done
want() {
  local t
  for t in "${targets[@]}"; do [ "$t" = "$1" ] && return 0; done
  return 1
}

[ "$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null)" = Running ] ||
  die "Lima VM $VM is not running (deploy/kind/up.sh)"
vm_in / test -w "$REPO_DIR" || die "VM $VM does not mount $REPO_DIR writable"
vm_in / kind get clusters 2>/dev/null | grep -qx "$CLUSTER" ||
  die "no kind cluster $CLUSTER in VM $VM (deploy/kind/up.sh)"

images=()
if want dapr; then
  command -v go >/dev/null || die "go is not installed"
  [ -d "$DAPR_SRC/cmd/daprd" ] || die "$DAPR_SRC is not a dapr checkout"
  log "upstream/dapr at $(git -C "$DAPR_SRC" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  timed "make build-linux (Mac)" make -C "$DAPR_SRC" build-linux GOARCH="$ARCH"
  timed "make docker-build (VM)" vm_in "$DAPR_SRC" make docker-build \
    TARGET_OS=linux TARGET_ARCH="$ARCH" DAPR_REGISTRY="$DAPR_REGISTRY" DAPR_TAG="$DAPR_TAG"
  for b in $DAPR_BINARIES; do
    images+=("$DAPR_REGISTRY/$b:$DAPR_TAG-linux-$ARCH")
  done
fi
if want driver; then
  timed "driver image (VM)" vm_in "$REPO_DIR" \
    docker build -f deploy/images/driver.Dockerfile -t "$DRIVER_IMAGE" .
  images+=("$DRIVER_IMAGE")
fi
if want bench; then
  timed "bench image (VM)" vm_in "$REPO_DIR" \
    docker build -f deploy/images/bench.Dockerfile -t "$BENCH_IMAGE" .
  images+=("$BENCH_IMAGE")
fi

timed "kind load (${#images[@]} images)" vm_in / kind load docker-image --name "$CLUSTER" "${images[@]}"
log "loaded: ${images[*]}"
log "done in ${SECONDS}s"
