#!/usr/bin/env bash
set -euo pipefail

TARGET="${1:?usage: napi.sh <target-triple>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
DIST_DIR="${ABGEN_DIST:-$ROOT/dist}"

cd "$ROOT/crate/abgen-node"

npm ci --no-audit --no-fund
# --cargo-flags, never '-- --locked': napi's positional is a copy destination
npx napi build --platform --release --target "$TARGET" --cargo-flags=--locked

node_bin="$(ls ./*.node)"
[ -n "$node_bin" ] || { echo "napi build produced no .node file" >&2; exit 1; }

git diff --exit-code -- index.js index.d.ts \
  || { echo "index.js/index.d.ts are stale; run 'napi build --platform --release' and commit them" >&2; exit 1; }

case "$TARGET" in
  x86_64-*)  want_arch=x86_64 ;;
  aarch64-*) want_arch=arm64 ;;
esac
run_arch="$(uname -m)"
[ "$run_arch" = aarch64 ] && run_arch=arm64
if [ "$run_arch" = "$want_arch" ]; then
  node test/smoke.mjs
  echo "napi smoke ok"
fi

case "$TARGET" in
  *-unknown-linux-gnu) bash "$HERE/check-glibc-floor.sh" 2.34 $node_bin ;;
esac

mkdir -p "$DIST_DIR"
cp $node_bin "$DIST_DIR/"
echo "done: $DIST_DIR"
ls -l "$DIST_DIR"
