#!/usr/bin/env bash
# Exits non-zero if driver/proto drifted from upstream/dapr (changed, missing
# or extra files). Fix drift with scripts/sync-proto.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/upstream/dapr/dapr/proto/components/v1"
DST="$ROOT/driver/proto/dapr/proto/components/v1"
FILES=(common.proto sandbox.proto)

# Stage the upstream files we track in a directory of their own, so diff -r
# compares exactly that set against the driver's copy.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
for f in "${FILES[@]}"; do
  cp "$SRC/$f" "$TMP/$f"
done

if ! diff -r "$TMP" "$DST"; then
  echo "proto drift: driver/proto differs from upstream/dapr; run scripts/sync-proto.sh" >&2
  exit 1
fi
echo "proto in sync"
