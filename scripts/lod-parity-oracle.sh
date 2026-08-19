#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [ "$#" -lt 2 ]; then
  echo "usage: $0 <manifest-builder-checkout> <X,Y> [<X,Y> ...]" >&2
  exit 2
fi

checkout="$1"
shift
if [ ! -f "$checkout/package.json" ]; then
  echo "error: $checkout does not look like a scene-lod-entities-manifest-builder checkout" >&2
  exit 2
fi

abgen_lod() {
  if [ -n "${ABGEN_LOD:-}" ]; then
    "$ABGEN_LOD" "$@"
  else
    (cd "$repo_root" && nix develop -c cargo run --quiet --release --bin abgen-lod -- "$@")
  fi
}

npm_run() {
  nix shell nixpkgs#nodejs --command "$@"
}

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

tool="$workdir/tool"
mkdir "$tool"
(cd "$checkout" && tar --exclude=./node_modules --exclude=./dist --exclude=./.git -cf - .) |
  tar -xf - -C "$tool"
(cd "$tool" && npm_run npm ci --ignore-scripts && npm_run npm run build)

fail=0
for coords in "$@"; do
  echo "== $coords =="
  tag="$(printf '%s' "$coords" | tr -c 'A-Za-z0-9' '_')"
  npm_log="$workdir/$tag.npm.log"
  if ! (cd "$tool" && npm_run npm run start "--coords=$coords" --overwrite) \
    >"$npm_log" 2>&1; then
    echo "FAIL $coords: npm tool exited nonzero" >&2
    cat "$npm_log" >&2
    fail=1
    continue
  fi
  if ! grep -q 'Finished running frames!' "$npm_log"; then
    echo "FAIL $coords: npm tool never reached the done marker" >&2
    cat "$npm_log" >&2
    fail=1
    continue
  fi
  scene_id="$(sed -n 's/.*scene id:\([^;[:space:]]*\).*/\1/p' "$npm_log" | head -n 1)"
  manifest="$tool/output-manifests/$scene_id-lod-manifest.json"
  npm_json="$workdir/$tag.npm.json"
  emb_json="$workdir/$tag.embedded.json"
  if [ -n "$scene_id" ] && [ -f "$manifest" ]; then
    echo "npm manifest: $manifest"
    abgen_lod parse-manifest "$manifest" --scene "$coords" >"$npm_json"
  else
    echo "npm tool wrote no manifest (empty scene)"
    echo '[]' >"$npm_json"
  fi
  abgen_lod placements --coords "$coords" --iss off >"$emb_json"
  if diff -u "$npm_json" "$emb_json"; then
    echo "PASS $coords"
  else
    echo "FAIL $coords: npm-manifest placements differ from the embedded runtime"
    echo "--- npm ($npm_json)"
    cat "$npm_json"
    echo "--- embedded ($emb_json)"
    cat "$emb_json"
    fail=1
  fi
done

exit "$fail"
