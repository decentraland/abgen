#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

declare -A SHADER_SHAS=(
  [scene_ignore_windows]=5a5ce6694c85b77be165e367fc510f2c8f06a05fa1422330fcff4c3793d6c4b5
  [scene_ignore_mac]=4c8519343778b9806d985129dc0c2c7b7ae97c17d0cfb17a30e66189ad591ce9
)

declare -A TEMPLATE_SHAS=(
  [all-types.windows.bundle]=7a2f876ce9436a4ee7fb66c2c4b206dc2f844140f081efee231cfaab2ab6db67
  [animated-types.windows.bundle]=91236453b18b4badd5f5d66412b83d8164f46c03ab577b94b1ff857de9d2e62f
  [emote-types.windows.bundle]=f0f0246cb218cbb31185f66f71d75ed3370aca85dc3af6582de7aba78e02c1f4
  [skinned-types.windows.bundle]=b2ce6065b03ddb9e62d1f8c2e5a1ec7e20d0d92faf4beb736156901d82b5e6d3
)

fail=0

check_sha() { # path expected_sha
  local got
  got=$(sha256sum "$1" | cut -d' ' -f1)
  [ "$got" = "$2" ]
}

for name in "${!SHADER_SHAS[@]}"; do
  p="$ROOT/crate/shader/$name"
  if [ -f "$p" ] && check_sha "$p" "${SHADER_SHAS[$name]}"; then
    echo "ok  shader   crate/shader/$name"
  else
    echo "ERR shader   crate/shader/$name missing or sha mismatch" >&2
    echo "    Shader bundles are vendored-only (the canonical" >&2
    echo "    ab-cdn.decentraland.org dcl/ shader URLs 404);" >&2
    echo "    restore them from git history: git checkout -- crate/shader/" >&2
    echo "    (the converter hard-verifies ${SHADER_SHAS[$name]} at load)" >&2
    fail=1
  fi
done

for name in "${!TEMPLATE_SHAS[@]}"; do
  p="$ROOT/crate/template/$name"
  if [ -f "$p" ] && check_sha "$p" "${TEMPLATE_SHAS[$name]}"; then
    echo "ok  template $name"
  else
    echo "ERR template $name missing or sha mismatch" >&2
    echo "    Templates are typetree-donor bundles that cannot be fetched;" >&2
    echo "    restore them from git history (git checkout -- crate/template/) or" >&2
    echo "    (maintainer regeneration path: see scripts/ + shader.rs pins)" >&2
    fail=1
  fi
done

if [ "$fail" = 0 ]; then
  echo "runtime data complete — zero env vars needed when running from the repo root"
fi
exit "$fail"
