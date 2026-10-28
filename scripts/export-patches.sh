#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/proposal/dapr-poc-patches"
mkdir -p "$OUT"
rm -f "$OUT"/*.patch
git -C "$ROOT/upstream/dapr" format-patch v1.19.0..feat/sandbox-building-block -o "$OUT/"
