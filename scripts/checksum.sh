#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
NAME="$(sed -n 's/^ *"id": "\(.*\)".*/\1/p' pack.json | head -1)"
ZIP="${ROOT}/dist/${NAME}-${VERSION}.zip"

if [[ ! -f "$ZIP" ]]; then
  echo "missing pack zip: $ZIP" >&2
  exit 1
fi

if command -v shasum >/dev/null 2>&1; then
  SUM="$(shasum -a 256 "$ZIP" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  SUM="$(sha256sum "$ZIP" | awk '{print $1}')"
else
  echo "need shasum or sha256sum" >&2
  exit 1
fi

OUT="${ZIP}.sha256"
printf '%s  %s\n' "$SUM" "$(basename "$ZIP")" >"$OUT"
echo "checksum: $OUT"
cat "$OUT"
