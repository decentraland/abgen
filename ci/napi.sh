#!/usr/bin/env bash
# One @dcl/abgen-node napi leg: build the platform .node, smoke it when
# the runner can execute it, gate the glibc floor on linux, stage into
# dist/. Assumes node and a rust toolchain with <target> are installed
# (the workflow's setup-node + rust-setup, or a local dev environment).
set -euo pipefail

TARGET="${1:?usage: napi.sh <target-triple>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
DIST_DIR="${ABGEN_DIST:-$ROOT/dist}"

cd "$ROOT/crate/abgen-node"

npm ci --no-audit --no-fund
# --cargo-flags, never '-- --locked': napi's build command has an optional
# positional (the copy destination), so a post--- token silently redirects
# the .node into a directory named './--locked' and cargo never sees the
# flag.
npx napi build --platform --release --target "$TARGET" --cargo-flags=--locked

node_bin="$(ls ./*.node)"
[ -n "$node_bin" ] || { echo "napi build produced no .node file" >&2; exit 1; }

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

# The addon is dlopen'd into node and bound by the host's glibc exactly
# like libabgen.so is by Unity's.
case "$TARGET" in
  *-unknown-linux-gnu) bash "$HERE/check-glibc-floor.sh" 2.34 $node_bin ;;
esac

mkdir -p "$DIST_DIR"
cp $node_bin "$DIST_DIR/"
echo "done: $DIST_DIR"
ls -l "$DIST_DIR"
