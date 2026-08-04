#!/usr/bin/env bash
# Assembles a local @dcl/abgen-<platform> npm package from the release build, for
# `file:` links during development (e.g. creator-hub).
# Output layout must match npm/publish.sh's exe-dir asset fallback expectations.
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
