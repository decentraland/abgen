#!/usr/bin/env bash
# Rebuild both wasm modules and refresh the site copies. wasm-poc and
# wasm-gpu are excluded from the parent workspace, so each builds from its
# own dir against its own committed Cargo.lock.
#   abgen_poc.wasm       — bindgen-free C-ABI converter (CPU-SIMD)
#   wasm/gpu/…           — wasm-bindgen WebGPU encode module + glue
set -euo pipefail
cd "$(dirname "$0")"
SITE=../../site/wasm
nix develop "path:$PWD/toolchain" --command bash -euo pipefail -c "
  RUSTFLAGS='-C target-feature=+simd128' \
    cargo build --release --target wasm32-unknown-unknown
  ( cd ../wasm-gpu && RUSTFLAGS='-C target-feature=+simd128' \
      cargo build --release --target wasm32-unknown-unknown )
  mkdir -p $SITE/gpu
  wasm-bindgen --target web --out-dir $SITE/gpu \
    ../wasm-gpu/target/wasm32-unknown-unknown/release/abgen_wasm_gpu.wasm
"
cp target/wasm32-unknown-unknown/release/abgen_wasm_poc.wasm "$SITE/abgen_poc.wasm"
ls -la "$SITE/abgen_poc.wasm" "$SITE/gpu/abgen_wasm_gpu_bg.wasm"
