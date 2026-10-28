#!/usr/bin/env bash
# Runs the sandbox demo on this machine: its own placement and scheduler,
# the microVM driver and one daprd, all built from this repo (see README.md).
#
#   demo/run-selfhosted.sh up     build, start, wait until py-fast serves
#   demo/run-selfhosted.sh down   stop everything, check that no VM is left
set -euo pipefail

DEMO_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "$DEMO_DIR/.." && pwd)
DAPR_SRC=$REPO_DIR/upstream/dapr
DRIVER_SRC=$REPO_DIR/driver
DRIVER_BIN=$DRIVER_SRC/target/release/dapr-sandbox-microvm

RUN_DIR=$DEMO_DIR/.run
BIN_DIR=$RUN_DIR/bin        # daprd, placement, scheduler
LOG_DIR=$RUN_DIR/logs       # one log per process, truncated by each `up`
STATE_DIR=$RUN_DIR/state    # driver: persisted component properties
SCHED_DIR=$RUN_DIR/scheduler
PIDS_FILE=$DEMO_DIR/.pids   # "<name> <pid>" per process this demo started

# daprd connects to every socket in this folder, and macOS caps a socket path
# at 104 bytes: a short folder of the demo's own, not the shared default.
SOCK_DIR=/tmp/dsb-demo/sockets
SOCKET=$SOCK_DIR/microvm.sock # sandbox.microvm <-> microvm.sock

DAPR_HTTP_PORT=3500
DAPR_GRPC_PORT=50101
DAPR_INTERNAL_GRPC_PORT=50102
DAPR_METRICS_PORT=50103
DAPR_PROFILE_PORT=50104
PLACEMENT_PORT=50105
SCHEDULER_PORT=50106
PLACEMENT_HEALTHZ_PORT=50107
PLACEMENT_METRICS_PORT=50108
PLACEMENT_RAFT_PORT=50109
SCHEDULER_HEALTHZ_PORT=50110
SCHEDULER_METRICS_PORT=50111
SCHEDULER_ETCD_CLIENT_PORT=50112
SCHEDULER_ETCD_PEER_PORT=50113
ALL_PORTS="$DAPR_HTTP_PORT $DAPR_GRPC_PORT $DAPR_INTERNAL_GRPC_PORT
  $DAPR_METRICS_PORT $DAPR_PROFILE_PORT $PLACEMENT_PORT $SCHEDULER_PORT
  $PLACEMENT_HEALTHZ_PORT $PLACEMENT_METRICS_PORT $PLACEMENT_RAFT_PORT
  $SCHEDULER_HEALTHZ_PORT $SCHEDULER_METRICS_PORT $SCHEDULER_ETCD_CLIENT_PORT
  $SCHEDULER_ETCD_PEER_PORT"

DAPR_URL=http://127.0.0.1:$DAPR_HTTP_PORT
# Seconds from starting placement until py-fast serves. A first run pulls
# python:3.12-slim and builds the template snapshots.
READY_TIMEOUT=${DEMO_READY_TIMEOUT:-300}
# Seconds each process gets to exit after SIGTERM before SIGKILL.
STOP_TIMEOUT=30
# Stop order: daprd releases its sandboxes first, then the driver destroys
# its pools.
PROCESSES="daprd driver scheduler placement"

log() { printf '[demo] %s\n' "$*"; }
die() {
  printf '[demo] error: %s\n' "$*" >&2
  exit 1
}

# --- processes ---------------------------------------------------------------

binary_of() {
  case $1 in
    driver) echo "$DRIVER_BIN" ;;
    *) echo "$BIN_DIR/$1" ;;
  esac
}

pid_of() {
  [ -f "$PIDS_FILE" ] || return 0
  awk -v n="$1" '$1 == n { print $2 }' "$PIDS_FILE"
}

alive() {
  local stat
  stat=$(ps -p "$1" -o stat= 2>/dev/null) || return 1
  case $stat in Z*) return 1 ;; esac
}

# ours NAME PID: PID is alive and still runs the binary this demo started.
ours() {
  local cmd
  [ -n "$2" ] && alive "$2" || return 1
  cmd=$(ps -p "$2" -o command= 2>/dev/null) || return 1
  case $cmd in "$(binary_of "$1")"*) return 0 ;; esac
  return 1
}

# start NAME [VAR=value...] -- ARGS...: runs NAME's binary in the background,
# logging to $LOG_DIR/NAME.log, and records its PID.
start() {
  local name=$1 bin
  shift
  bin=$(binary_of "$name")
  local envs=()
  while [ "$1" != "--" ]; do
    envs+=("$1")
    shift
  done
  shift
  # `env` and `nohup` exec, so $! is the binary's own PID.
  nohup env ${envs[@]+"${envs[@]}"} "$bin" "$@" </dev/null >"$LOG_DIR/$name.log" 2>&1 &
  echo "$name $!" >>"$PIDS_FILE"
  log "started $name (pid $!), log: ${LOG_DIR#"$REPO_DIR"/}/$name.log"
}

stop() {
  local name=$1 pid i=0
  pid=$(pid_of "$name")
  ours "$name" "$pid" || return 0
  log "stopping $name (pid $pid)"
  kill -TERM "$pid" 2>/dev/null || true
  while alive "$pid"; do
    if [ $i -ge $((STOP_TIMEOUT * 5)) ]; then
      log "$name did not exit within ${STOP_TIMEOUT}s: SIGKILL"
      kill -KILL "$pid" 2>/dev/null || true
      break
    fi
    sleep 0.2
    i=$((i + 1))
  done
}

# --- leftover VMs ------------------------------------------------------------

# The VM name prefix of each sandbox.microvm component in demo/components:
# `dsb-<first 8 hex of sha256(component)>-`, as driver/src/pool.rs names them.
vm_prefixes() {
  local f name
  for f in "$DEMO_DIR"/components/*.yaml; do
    grep -q '^  type: sandbox\.microvm$' "$f" || continue
    name=$(awk '/^metadata:/ { m = 1; next } m && /^  name:/ { print $2; exit }' "$f")
    printf 'dsb-%s-\n' "$(printf '%s' "$name" | shasum -a 256 | cut -c1-8)"
  done
}

# Prints "<pid> <command>" for each VM process of this demo's components.
# microsandbox 0.7.11 runs every VM as `msb machine --name <vm> ...`.
leftover_vms() {
  local procs re
  re=$(vm_prefixes | paste -sd'|' -)
  [ -n "$re" ] || die "no sandbox.microvm component in demo/components"
  procs=$(ps -A -o pid=,command=)
  printf '%s\n' "$procs" | grep -E "[ /]msb machine .*--name ($re)" || true
}

check_no_leftover_vms() {
  local vms i=0
  vms=$(leftover_vms)
  while [ -n "$vms" ] && [ $i -lt 50 ]; do
    sleep 0.2
    i=$((i + 1))
    vms=$(leftover_vms)
  done
  if [ -z "$vms" ]; then
    log "no leftover VMs"
    return 0
  fi
  printf '[demo] error: leftover VMs after the driver exited:\n%s\n' "$vms" >&2
  log "killing them"
  printf '%s\n' "$vms" | awk '{ print $1 }' | xargs kill -KILL 2>/dev/null || true
  return 1
}

# --- up ----------------------------------------------------------------------

require_tools() {
  local t
  for t in go cargo curl jq lsof shasum; do
    command -v "$t" >/dev/null || die "$t not found on PATH"
  done
  if [ -n "${PROTOC:-}" ]; then
    [ -x "$PROTOC" ] || die "PROTOC=$PROTOC is not executable"
  else
    command -v protoc >/dev/null || die "protoc not found on PATH (or set PROTOC)"
  fi
  [ -d "$DAPR_SRC/cmd/daprd" ] || die "$DAPR_SRC is not a dapr checkout"
}

refuse_if_running() {
  local name
  [ -f "$PIDS_FILE" ] || return 0
  for name in $PROCESSES; do
    if ours "$name" "$(pid_of "$name")"; then
      die "the demo is already running ($name, pid $(pid_of "$name")): run '$0 down' first"
    fi
  done
  log "removing stale ${PIDS_FILE#"$REPO_DIR"/}"
  rm -f "$PIDS_FILE"
}

check_ports() {
  local p busy=""
  for p in $ALL_PORTS; do
    if lsof -nP -iTCP:"$p" -sTCP:LISTEN -t >/dev/null 2>&1; then
      busy="$busy $p"
    fi
  done
  [ -z "$busy" ] || die "port(s) already in use:$busy (README.md lists the demo's ports)"
}

build() {
  local c rev
  rev=$(git -C "$DAPR_SRC" rev-parse --short HEAD 2>/dev/null || echo unknown)
  log "building daprd, placement and scheduler from upstream/dapr ($rev)"
  mkdir -p "$BIN_DIR"
  for c in daprd placement scheduler; do
    (cd "$DAPR_SRC" && CGO_ENABLED=0 go build -tags=allcomponents -o "$BIN_DIR/$c" "./cmd/$c")
  done
  # cargo must run inside the repo: .cargo/config.toml keeps microsandbox's
  # runtime download in driver/target/msb-runtime.
  log "building the driver"
  (cd "$DRIVER_SRC" && cargo build --release --quiet)
}

# wait_until NAME DESC CHECK...: polls CHECK until it succeeds; fails when
# NAME's process exits or READY_TIMEOUT passes.
wait_until() {
  local name=$1 desc=$2
  shift 2
  until "$@"; do
    alive "$(pid_of "$name")" || die "$name exited while waiting for $desc"
    [ $SECONDS -lt $DEADLINE ] || die "timed out after ${READY_TIMEOUT}s waiting for $desc"
    sleep 0.2
  done
}

http_ok() { curl -fs -o /dev/null --max-time 2 "$1"; }

# Creates a probe sandbox on py-fast: true on 200, false while daprd is not
# serving yet or answers 429 (the driver's pool is not ready yet).
py_fast_serves() {
  local code
  code=$(curl -s -o "$RUN_DIR/probe.json" -w '%{http_code}' --max-time 10 \
    -X POST -H 'content-type: application/json' -d '{}' \
    "$DAPR_URL/v1.0-alpha1/sandboxes/py-fast") || code=000
  case $code in
    200) return 0 ;;
    000 | 429) return 1 ;;
  esac
  die "creating a probe sandbox on py-fast: HTTP $code: $(cat "$RUN_DIR/probe.json")"
}

delete_probe() {
  local id code
  id=$(jq -r .sandboxId "$RUN_DIR/probe.json")
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 -X DELETE \
    "$DAPR_URL/v1.0-alpha1/sandboxes/py-fast/$id") || code=000
  [ "$code" = 200 ] || die "deleting probe sandbox $id: HTTP $code"
  log "probe sandbox: $(jq -r '"acquirePath \(.timing.acquirePath), totalUs \(.timing.totalUs)"' "$RUN_DIR/probe.json")"
  rm -f "$RUN_DIR/probe.json"
}

start_placement() {
  start placement -- \
    --id dsb-demo-placement \
    --port "$PLACEMENT_PORT" --listen-address 127.0.0.1 \
    --healthz-port "$PLACEMENT_HEALTHZ_PORT" --healthz-listen-address 127.0.0.1 \
    --metrics-port "$PLACEMENT_METRICS_PORT" --metrics-listen-address 127.0.0.1 \
    --initial-cluster "dsb-demo-placement=127.0.0.1:$PLACEMENT_RAFT_PORT" \
    --tls-enabled=false
  wait_until placement "placement" http_ok "http://127.0.0.1:$PLACEMENT_HEALTHZ_PORT/healthz"
}

start_scheduler() {
  mkdir -p "$SCHED_DIR/etcd"
  chmod 700 "$SCHED_DIR/etcd"
  start scheduler -- \
    --id dsb-demo-scheduler-0 \
    --port "$SCHEDULER_PORT" --listen-address 127.0.0.1 \
    --healthz-port "$SCHEDULER_HEALTHZ_PORT" --healthz-listen-address 127.0.0.1 \
    --metrics-port "$SCHEDULER_METRICS_PORT" --metrics-listen-address 127.0.0.1 \
    --etcd-data-dir "$SCHED_DIR/etcd" \
    --etcd-client-port "$SCHEDULER_ETCD_CLIENT_PORT" --etcd-client-listen-address 127.0.0.1 \
    --etcd-initial-cluster "dsb-demo-scheduler-0=http://127.0.0.1:$SCHEDULER_ETCD_PEER_PORT" \
    --identity-directory-write "$SCHED_DIR/tls" \
    --tls-enabled=false
  wait_until scheduler "scheduler" http_ok "http://127.0.0.1:$SCHEDULER_HEALTHZ_PORT/healthz"
}

# The driver binds its socket at once; daprd must find it when it starts.
start_driver() {
  mkdir -p "$SOCK_DIR"
  # NO_COLOR: plain text in the log file.
  start driver NO_COLOR=1 -- --socket "$SOCKET" --state-dir "$STATE_DIR"
  wait_until driver "the driver's socket" test -S "$SOCKET"
}

start_daprd() {
  start daprd DAPR_COMPONENTS_SOCKETS_FOLDER="$SOCK_DIR" DAPR_HOST_IP=127.0.0.1 -- \
    --app-id sandbox-host \
    --resources-path "$DEMO_DIR/components" \
    --dapr-http-port "$DAPR_HTTP_PORT" --dapr-listen-addresses 127.0.0.1 \
    --dapr-grpc-port "$DAPR_GRPC_PORT" \
    --dapr-internal-grpc-port "$DAPR_INTERNAL_GRPC_PORT" --dapr-internal-grpc-listen-address 127.0.0.1 \
    --metrics-port "$DAPR_METRICS_PORT" --metrics-listen-address 127.0.0.1 \
    --profile-port "$DAPR_PROFILE_PORT" \
    --placement-host-address "127.0.0.1:$PLACEMENT_PORT" \
    --scheduler-host-address "127.0.0.1:$SCHEDULER_PORT" \
    --max-body-size 16Mi
  wait_until daprd "daprd healthz" http_ok "$DAPR_URL/v1.0/healthz"
  wait_until daprd "py-fast to serve" py_fast_serves
  delete_probe
}

# On a failed `up`, show the logs' tails and stop what was started.
up_failed() {
  local name
  for name in $PROCESSES; do
    [ -f "$LOG_DIR/$name.log" ] || continue
    printf '\n--- %s (last 15 lines) ---\n' "$name" >&2
    tail -n 15 "$LOG_DIR/$name.log" >&2
  done
  printf '\n' >&2
  log "up failed: stopping what it started"
  cmd_down || true
}

cmd_up() {
  local t0=$SECONDS t1 t2
  require_tools
  refuse_if_running
  check_ports
  [ -z "$(leftover_vms)" ] || die "VMs of an earlier run are still running: run '$0 down' first"

  build
  t1=$SECONDS

  mkdir -p "$LOG_DIR"
  : >"$PIDS_FILE"
  trap 'up_failed' EXIT
  DEADLINE=$((SECONDS + READY_TIMEOUT))
  start_placement
  start_scheduler
  start_driver
  start_daprd
  trap - EXIT
  t2=$SECONDS

  log "up in $((t2 - t0))s (build $((t1 - t0))s, start to ready $((t2 - t1))s)"
  log "Dapr HTTP API: $DAPR_URL, components: py-fast py-restore py-boot"
}

# --- down --------------------------------------------------------------------

cmd_down() {
  local name
  if [ -f "$PIDS_FILE" ]; then
    for name in $PROCESSES; do
      stop "$name"
    done
    rm -f "$PIDS_FILE"
  else
    log "no ${PIDS_FILE#"$REPO_DIR"/}: no demo process to stop"
  fi
  # The driver removes its socket on exit, but not after a SIGKILL.
  if [ -S "$SOCKET" ]; then rm -f "$SOCKET"; fi
  rmdir "$SOCK_DIR" "$(dirname "$SOCK_DIR")" 2>/dev/null || true
  # Persisted pools would come back on the next driver start.
  rm -rf "$STATE_DIR" "$SCHED_DIR" "$RUN_DIR/probe.json"
  check_no_leftover_vms
}

case ${1:-} in
  up) cmd_up ;;
  down) cmd_down ;;
  *)
    echo "usage: $0 up|down" >&2
    exit 2
    ;;
esac
