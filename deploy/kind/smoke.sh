#!/usr/bin/env bash
# Smoke test of the sandbox pool on kind (deploy/kind/manifests applied).
# From inside the caller pod (app ID sandbox-caller), over its own daprd on
# localhost:3500 with the header dapr-app-id: sandbox-pool, it runs
# `echo hi` on 10 fresh sandbox IDs of py-fast and deletes each one. It
# passes when:
#   - every exec answers 200 with stdout "hi", and every delete 200;
#   - both sandbox-pool pods served at least one of those IDs, going by the
#     drivers' `acquired ... sandbox=<id>` log lines.
# It first waits (up to SMOKE_READY_TIMEOUT seconds, 300 by default) until
# both pool pods are Ready and their daprd has registered py-fast's actor
# type, which happens once the driver's warm pool is full.
#
#   deploy/kind/smoke.sh
set -euo pipefail

KIND_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO_DIR=$(cd "$KIND_DIR/../.." && pwd -P)
KUBECONFIG_FILE=$REPO_DIR/.kube/kind-sandbox.yaml
CONTEXT=kind-sandbox
HOST_APP=sandbox-pool
CALLER_APP=sandbox-caller
COMPONENT=py-fast
COUNT=10
POOL_PODS=2
ACTOR_TYPE=dapr.internal.default.$HOST_APP.sandbox.$COMPONENT
READY_TIMEOUT=${SMOKE_READY_TIMEOUT:-300}

k() { KUBECONFIG=$KUBECONFIG_FILE kubectl --context "$CONTEXT" "$@"; }
log() { printf '[smoke] %s\n' "$*"; }
fail() {
  printf '[smoke] FAIL %s\n' "$*" >&2
  exit 1
}

[ -f "$KUBECONFIG_FILE" ] || fail "no $KUBECONFIG_FILE (deploy/kind/up.sh)"

# The pods of an app that are not being deleted.
pods() {
  k get pods -l "app=$1" \
    -o go-template='{{range .items}}{{if not .metadata.deletionTimestamp}}{{.metadata.name}}{{"\n"}}{{end}}{{end}}'
}
ready() {
  [ "$(k get pod "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = True ]
}
# Reads the whole log first: under pipefail, `k logs | grep -q` fails when
# grep exits at the first match and kubectl dies of SIGPIPE.
registered() {
  local logs
  logs=$(k logs "$1" -c daprd 2>/dev/null) || return 1
  grep -qF "Registered sandbox actor type '$ACTOR_TYPE'" <<<"$logs"
}

# --- wait until both pool pods serve py-fast and the caller is up ----------
deadline=$((SECONDS + READY_TIMEOUT))
while :; do
  pool=$(pods "$HOST_APP")
  caller=$(pods "$CALLER_APP" | sed -n 1p)
  n=0
  for p in $pool; do
    if ready "$p" && registered "$p"; then n=$((n + 1)); fi
  done
  if [ "$n" -eq "$POOL_PODS" ] && [ "$(printf '%s\n' "$pool" | grep -c .)" -eq "$POOL_PODS" ] &&
    [ -n "$caller" ] && ready "$caller"; then
    break
  fi
  [ $SECONDS -lt $deadline ] ||
    fail "after ${READY_TIMEOUT}s, $n of $POOL_PODS $HOST_APP pods serve $COMPONENT (caller: ${caller:-none})"
  sleep 2
done
for p in $pool; do
  log "$HOST_APP pod $p on $(k get pod "$p" -o jsonpath='{.spec.nodeName}'): Ready, $ACTOR_TYPE registered"
done
log "caller pod $caller on $(k get pod "$caller" -o jsonpath='{.spec.nodeName}')"

# --- exec on fresh IDs, from the caller pod ---------------------------------
prefix=smoke-$(date +%s)
log "exec 'echo hi' on $COUNT fresh IDs ($prefix-1..$COUNT) via $CALLER_APP's daprd"
# One line per ID: "<id> <exec status> <stdout is hi> <acquirePath> <totalUs>
# <delete status>". Keeps going after a failure, so every ID is deleted.
# shellcheck disable=SC2016 # runs in the pod
out=$(k exec -i "$caller" -c bench -- sh -s "$prefix" "$COUNT" "$COMPONENT" "$HOST_APP" <<'EOF'
prefix=$1 count=$2 component=$3 host=$4
api=http://localhost:3500/v1.0-alpha1/sandboxes/$component
body=/tmp/smoke.json
i=1
while [ "$i" -le "$count" ]; do
  id=$prefix-$i
  rm -f "$body"
  code=$(curl -s -o "$body" -w '%{http_code}' --max-time 60 -X POST \
    -H "dapr-app-id: $host" -H 'content-type: application/json' \
    -d '{"command":"echo","args":["hi"]}' "$api/$id/exec") || code=000
  fields=$(jq -r '"\((.stdout // "" | @base64d) == "hi\n") \(.timing.acquirePath // "-") \(.timing.totalUs // "0")"' "$body" 2>/dev/null) ||
    fields="false - 0"
  dcode=$(curl -s -o /dev/null -w '%{http_code}' --max-time 60 -X DELETE \
    -H "dapr-app-id: $host" "$api/$id") || dcode=000
  echo "$id $code $fields $dcode"
  i=$((i + 1))
done
EOF
)
printf '%s\n' "$out" | awk '{ printf "[smoke]   %-22s exec %s  stdout-hi %-5s  %-7s totalUs %8s  delete %s\n", $1, $2, $3, $4, $5, $6 }'

[ "$(printf '%s\n' "$out" | grep -c .)" -eq "$COUNT" ] || fail "want $COUNT results, got: $out"
bad=$(printf '%s\n' "$out" | awk '$2 != 200 || $3 != "true" || $6 != 200')
[ -z "$bad" ] || fail "not every exec and delete succeeded: $bad"
log "ok: $COUNT/$COUNT execs 200 with stdout 'hi', $COUNT/$COUNT deletes 200"

# --- both pool pods served some -------------------------------------------
total=0
for p in $pool; do
  c=$(k logs "$p" -c microvm | grep -cE "acquired component=$COMPONENT sandbox=$prefix-[0-9]+ " || true)
  log "$p: its driver acquired a VM for $c of them"
  [ "$c" -ge 1 ] || fail "$p served none of the $COUNT IDs"
  total=$((total + c))
done
[ "$total" -eq "$COUNT" ] || fail "the drivers logged $total acquisitions for $COUNT IDs"
log "PASS"
