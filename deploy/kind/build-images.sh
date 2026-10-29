#!/usr/bin/env bash
# Builds the images the kind deployment runs and loads them into the kind
# cluster "sandbox" (start it with deploy/kind/up.sh first):
#
#   dapr    Dapr's control plane and daprd from upstream/dapr: `make
#           build-linux` compiles the linux/<arch> binaries into
#           upstream/dapr/dist/linux_<arch>/release (on a Mac with the host's
#           Go; on Linux in a golang:<go.mod version> container, as the
#           invoking user, with a Go cache in ~/.cache/dsb-go), then `make
#           docker-build` (in the Lima VM on a Mac, on the host on Linux)
#           wraps each one in a distroless image,
#           localhost/dapr/<binary>:dev-linux-<arch>, which is also tagged
#           localhost/dapr/<binary>:dev (deploy/kind/helm-values.yaml uses it).
#   driver  dapr-sandbox-microvm:dev (deploy/images/driver.Dockerfile).
#   bench   sandbox-bench:dev, the caller pod's image (deploy/images/bench.Dockerfile).
#
#   deploy/kind/build-images.sh                 # all three
#   deploy/kind/build-images.sh driver bench    # only these
#
# macOS: needs Go (the version in upstream/dapr/go.mod) and make on the Mac.
# Docker and kind run only inside the Lima VM; the repo is mounted there at
# its own path. Linux (with /dev/kvm): needs make, docker and kind on the host;
# no Go. Nothing is installed for you.
# Pods use these images with imagePullPolicy: Never, so a rebuilt image takes
# effect for new pods only.
set -euo pipefail

KIND_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_DIR=$(cd "$KIND_DIR/../.." && pwd -P)
DAPR_SRC=$REPO_DIR/upstream/dapr
VM=sandbox
CLUSTER=sandbox
if [ "$(uname -s)" = Linux ]; then
  MODE=linux
else
  MODE=lima
fi
case $(uname -m) in
x86_64 | amd64) ARCH=amd64 ;;
aarch64 | arm64) ARCH=arm64 ;;
*) printf '[images] error: unsupported architecture %s\n' "$(uname -m)" >&2; exit 1 ;;
esac
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
# vm_in DIR CMD...: runs CMD in the VM (on Linux: on the host), in DIR.
vm_in() {
  local dir=$1
  shift
  if [ $MODE = linux ]; then
    (cd "$dir" && "$@")
  else
    limactl shell --workdir "$dir" "$VM" "$@"
  fi
}
# build_dapr_linux: `make build-linux` in a golang container, as the invoking
# uid/gid, with a persistent Go cache, so the host needs no Go. The image tag
# is the go line of upstream/dapr/go.mod, or the newest 1.26.x if that tag
# does not exist.
build_dapr_linux() {
  local gover cache=${DSB_GO_CACHE:-$HOME/.cache/dsb-go}
  gover=$(awk '/^go / {print $2; exit}' "$DAPR_SRC/go.mod")
  [ -n "$gover" ] || die "no go line in $DAPR_SRC/go.mod"
  if ! docker image inspect "golang:$gover" >/dev/null 2>&1 && ! docker pull -q "golang:$gover" >/dev/null 2>&1; then
    log "golang:$gover does not exist; looking for the newest ${gover%.*}.x"
    local newest
    newest=$(curl -fsS "https://registry.hub.docker.com/v2/repositories/library/golang/tags?name=${gover%.*}.&page_size=100" |
      grep -o "\"name\":\"${gover%.*}\.[0-9]*\"" | sed 's/.*:"//;s/"//' | sort -V | tail -n 1) || true
    [ -n "$newest" ] || die "no golang:${gover%.*}.x image found"
    gover=$newest
    docker pull -q "golang:$gover" >/dev/null || die "cannot pull golang:$gover"
  fi
  mkdir -p "$cache"
  docker run --rm -u "$(id -u):$(id -g)" \
    -v "$DAPR_SRC:/src" -w /src -v "$cache:/cache" \
    -e HOME=/tmp -e GOCACHE=/cache/build -e GOMODCACHE=/cache/mod -e GOFLAGS=-buildvcs=false \
    -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e 'GIT_CONFIG_VALUE_0=*' \
    "golang:$gover" make build-linux GOARCH="$ARCH"
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

if [ $MODE = linux ]; then
  for tool in docker kind make; do
    command -v "$tool" >/dev/null || die "$tool is not installed (Linux mode installs nothing)"
  done
else
  [ "$(limactl list --format '{{.Status}}' "$VM" 2>/dev/null)" = Running ] ||
    die "Lima VM $VM is not running (deploy/kind/up.sh)"
  vm_in / test -w "$REPO_DIR" || die "VM $VM does not mount $REPO_DIR writable"
fi
vm_in / kind get clusters 2>/dev/null | grep -qx "$CLUSTER" ||
  die "no kind cluster $CLUSTER (deploy/kind/up.sh)"

images=()
if want dapr; then
  [ $MODE = linux ] || command -v go >/dev/null || die "go is not installed"
  [ -d "$DAPR_SRC/cmd/daprd" ] || die "$DAPR_SRC is not a dapr checkout"
  log "upstream/dapr at $(git -C "$DAPR_SRC" rev-parse --short HEAD 2>/dev/null || echo unknown)"
  if [ $MODE = linux ]; then
    timed "make build-linux (golang container)" build_dapr_linux
  else
    timed "make build-linux (Mac)" make -C "$DAPR_SRC" build-linux GOARCH="$ARCH"
  fi
  timed "make docker-build" vm_in "$DAPR_SRC" make docker-build \
    TARGET_OS=linux TARGET_ARCH="$ARCH" DAPR_REGISTRY="$DAPR_REGISTRY" DAPR_TAG="$DAPR_TAG"
  for b in $DAPR_BINARIES; do
    # The arch-neutral tag is the one the Helm values use and kind loads.
    vm_in / docker tag "$DAPR_REGISTRY/$b:$DAPR_TAG-linux-$ARCH" "$DAPR_REGISTRY/$b:$DAPR_TAG"
    images+=("$DAPR_REGISTRY/$b:$DAPR_TAG")
  done
fi
if want driver; then
  timed "driver image" vm_in "$REPO_DIR" \
    docker build -f deploy/images/driver.Dockerfile -t "$DRIVER_IMAGE" .
  images+=("$DRIVER_IMAGE")
fi
if want bench; then
  timed "bench image" vm_in "$REPO_DIR" \
    docker build -f deploy/images/bench.Dockerfile -t "$BENCH_IMAGE" .
  images+=("$BENCH_IMAGE")
fi

timed "kind load (${#images[@]} images)" vm_in / kind load docker-image --name "$CLUSTER" "${images[@]}"
log "loaded: ${images[*]}"
log "done in ${SECONDS}s"
