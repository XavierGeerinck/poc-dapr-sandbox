#!/usr/bin/env bash
# Smoke test of a running demo (demo/run-selfhosted.sh up), over daprd's HTTP
# API on sandbox smoke-1 of py-fast: an exec served from the warm pool, a
# Python session, a stale activation and a delete. Exits non-zero on the
# first failed check.
set -euo pipefail

DAPR_URL=${DAPR_URL:-http://127.0.0.1:3500}
COMPONENT=py-fast
ID=smoke-1
BASE=$DAPR_URL/v1.0-alpha1/sandboxes/$COMPONENT/$ID

TMP=$(mktemp -d)
STATUS=""
BODY=""
PASSED=0

cleanup() {
  local rc=$?
  # A failed run must not keep its sandbox (and its VM).
  if [ $rc -ne 0 ]; then
    curl -s -o /dev/null --max-time 30 -X DELETE "$BASE" || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

ok() {
  PASSED=$((PASSED + 1))
  printf 'ok   %s\n' "$*"
}
fail() {
  printf 'FAIL %s\n' "$*" >&2
  exit 1
}

# call METHOD PATH [JSON]: sets STATUS and BODY.
call() {
  local args=(-s -o "$TMP/body" -w '%{http_code}' --max-time 60 -X "$1")
  if [ $# -ge 3 ]; then
    args+=(-H 'content-type: application/json' -d "$3")
  fi
  STATUS=$(curl "${args[@]}" "$BASE$2") || STATUS=000
  BODY=$(cat "$TMP/body" 2>/dev/null || true)
}

# field JQ: evaluates JQ on BODY.
field() { jq -r "$1" <<<"$BODY"; }

expect_status() {
  [ "$STATUS" = "$1" ] || fail "$2: want HTTP $1, got $STATUS: $BODY"
}

# timing: protojson renders uint64 as strings; zero fields are omitted.
timing() {
  field '.timing | "acquirePath=\(.acquirePath // "-") totalUs=\(.totalUs // "0" | tonumber) hostUs=\(.hostUs // "0" | tonumber) activateUs=\(.activateUs // "0" | tonumber) execUs=\(.execUs // "0" | tonumber)"'
}

# send INPUT UNTIL: one session turn; bytes fields are base64.
send() {
  call POST "/sessions/$SESSION" "$(jq -nc --arg in "$1" --arg until "$2" --arg act "$ACTIVATION" \
    '{input: ($in | @base64), until: ($until | @base64), timeoutMs: 10000, expectedActivation: $act}')"
}

# 1. An exec on a new ID activates it from the warm pool.
call POST /exec '{"command": "echo", "args": ["hi"]}'
expect_status 200 "exec echo hi"
[ "$(field '.timing.acquirePath')" = POOL ] || fail "exec: want acquirePath POOL: $BODY"
[ "$(field '.stdout // "" | @base64d')" = hi ] || fail "exec: want stdout 'hi': $BODY"
[ "$(field '.exitCode // 0')" = 0 ] || fail "exec: want exit code 0: $BODY"
ACTIVATION=$(field '.activation // ""')
[ -n "$ACTIVATION" ] || fail "exec: no activation: $BODY"
ok "exec echo hi -> POOL, stdout 'hi' ($(timing))"

# 2. A Python session keeps state across turns. Python writes its prompt to
# stderr and results to stdout, so turn 2 waits for the result itself.
call POST /sessions "$(jq -nc --arg act "$ACTIVATION" \
  '{command: "python3", args: ["-u", "-i", "-q"], expectedActivation: $act}')"
expect_status 200 "session start"
SESSION=$(field '.sessionId // ""')
[ -n "$SESSION" ] || fail "session start: no sessionId: $BODY"
send $'x = 42\n' '>>> '
expect_status 200 "session turn 1"
[ "$(field '.timedOut // false')" = false ] || fail "session turn 1 timed out: $BODY"
send $'print(x * 2)\n' $'84\n'
expect_status 200 "session turn 2"
[ "$(field '.stdout // "" | @base64d')" = 84 ] || fail "session turn 2: want stdout '84': $BODY"
ok "session python3 -u -i -q: x = 42; print(x * 2) -> 84 ($(timing))"
call DELETE "/sessions/$SESSION"
expect_status 200 "session close"
ok "session close -> 200"

# 3. A call that expects another activation fails with 410 and leaves the
# sandbox as it is.
call POST /exec "$(jq -nc --arg act "stale-$ACTIVATION" \
  '{command: "echo", args: ["stale"], expectedActivation: $act}')"
expect_status 410 "exec with a stale activation"
[ "$(field '.errorCode')" = ERR_SANDBOX_LOST ] || fail "stale activation: want ERR_SANDBOX_LOST: $BODY"
ok "exec with a stale expectedActivation -> 410 ERR_SANDBOX_LOST"
call POST /exec "$(jq -nc --arg act "$ACTIVATION" '{command: "true", expectedActivation: $act}')"
expect_status 200 "exec with the current activation"
ok "exec with the current activation still -> 200"

# 4. Delete releases the sandbox.
call DELETE ""
expect_status 200 "delete"
[ "$(field 'tojson')" = '{}' ] || fail "delete: want {}: $BODY"
ok "delete -> 200 {}"

printf 'smoke: %d checks passed\n' "$PASSED"
