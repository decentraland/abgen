#!/usr/bin/env bash
# Serve the demo: http://127.0.0.1:5189/wasm/
# Plain static serving — no COOP/COEP, so the WebGPU bridge stays off and
# conversion runs CPU-SIMD. For the crossOriginIsolated variant (WebGPU
# bridge armed), run site/server.py instead.
cd "$(dirname "$0")/../../site"
exec python3 -m http.server "${1:-5189}" --bind 127.0.0.1
