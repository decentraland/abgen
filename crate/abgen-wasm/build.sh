#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
DIST=dist
nix develop "path:$PWD/toolchain" --command bash -euo pipefail -c "
  RUSTFLAGS='-C target-feature=+simd128' \
    cargo build --release --target wasm32-unknown-unknown
  ( cd ../wasm-gpu && RUSTFLAGS='-C target-feature=+simd128' \
      cargo build --release --target wasm32-unknown-unknown )
  mkdir -p $DIST/gpu
  wasm-bindgen --target web --out-dir $DIST/gpu \
    ../wasm-gpu/target/wasm32-unknown-unknown/release/abgen_wasm_gpu.wasm
"
cp target/wasm32-unknown-unknown/release/abgen_wasm.wasm "$DIST/abgen_wasm.wasm"
ls -la "$DIST/abgen_wasm.wasm" "$DIST/gpu/abgen_wasm_gpu_bg.wasm"
