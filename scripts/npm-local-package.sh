#!/usr/bin/env bash
# Assembles a local platform npm package (@dcl/abgen-<platform>) from the release build,
# for consuming @dcl/abgen via file: links during development (e.g. from creator-hub).
#
#   scripts/npm-local-package.sh
#
# Output: npm/local/abgen-<os>-<arch>/ with the binary next to template/ + shader/,
# the layout the exe-dir asset fallback expects (same as npm/publish.sh).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HERE/target/release/abgen"
[ -f "$BIN" ] || { echo "missing $BIN — run: cargo build --release --bin abgen" >&2; exit 1; }

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) OS=darwin; CPU=arm64; BIN_NAME=abgen ;;
  Darwin-x86_64) OS=darwin; CPU=x64; BIN_NAME=abgen ;;
  Linux-x86_64) OS=linux; CPU=x64; BIN_NAME=abgen ;;
  Linux-aarch64) OS=linux; CPU=arm64; BIN_NAME=abgen ;;
  *) echo "unsupported host: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

PKG="abgen-$OS-$CPU"
OUT="$HERE/npm/local/$PKG"
rm -rf "$OUT"
mkdir -p "$OUT"

cp "$BIN" "$OUT/$BIN_NAME"
cp -R "$HERE/template" "$OUT/template"
mkdir -p "$OUT/shader"
cp -R "$HERE/crate/shader/." "$OUT/shader/"

cat >"$OUT/package.json" <<EOF
{
  "name": "@dcl/$PKG",
  "version": "0.0.0-dev",
  "description": "abgen prebuilt binary for $OS $CPU (local dev package)",
  "license": "AGPL-3.0-or-later",
  "os": ["$OS"],
  "cpu": ["$CPU"]
}
EOF

echo "assembled: $OUT"
