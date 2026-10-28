#!/usr/bin/env bash
# Copies the sandbox driver contract from upstream/dapr into the driver crate.
# The layout under driver/proto mirrors the upstream import paths, so
# `import "dapr/proto/components/v1/common.proto"` resolves with driver/proto
# as the include root.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/upstream/dapr/dapr/proto/components/v1"
DST="$ROOT/driver/proto/dapr/proto/components/v1"
FILES=(common.proto sandbox.proto)

mkdir -p "$DST"
for f in "${FILES[@]}"; do
  cp "$SRC/$f" "$DST/$f"
done
echo "synced ${FILES[*]} -> ${DST#"$ROOT"/}"
