#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:?usage: publish.sh <version> <artifacts-dir> [--dry-run]}"
ARTIFACTS="$(cd "${2:?usage: publish.sh <version> <artifacts-dir> [--dry-run]}" && pwd)"
DRY_RUN=0
[ "${3:-}" = "--dry-run" ] && DRY_RUN=1
SCOPE="${ABGEN_NPM_SCOPE:-@dcl}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="${GITHUB_REPOSITORY:-decentraland/abgen}"

TARGETS=(
  "x86_64-unknown-linux-gnu linux x64 abgen-linux-x64"
  "aarch64-unknown-linux-gnu linux arm64 abgen-linux-arm64"
  "x86_64-pc-windows-gnu win32 x64 abgen-win32-x64"
  "aarch64-pc-windows-gnullvm win32 arm64 abgen-win32-arm64"
  "aarch64-apple-darwin darwin arm64 abgen-darwin-arm64"
  "x86_64-apple-darwin darwin x64 abgen-darwin-x64"
)

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
OUT="$ARTIFACTS/npm-dist"
mkdir -p "$OUT"

publish_pkg() {
  local dir="$1"
  local name
  name="$(node -p "require('$dir/package.json').name")"
  if [ "$DRY_RUN" = 1 ]; then
    (cd "$dir" && npm pack --pack-destination "$OUT" >/dev/null)
    echo "packed: ${name}@${VERSION}"
    return
  fi
  if npm view "${name}@${VERSION}" version >/dev/null 2>&1; then
    echo "already published: ${name}@${VERSION} - skipping"
    return
  fi
  local flags=(--access public)
  [ "${GITHUB_ACTIONS:-}" = "true" ] && flags+=(--provenance)
  (cd "$dir" && npm publish "${flags[@]}")
}

for spec in "${TARGETS[@]}"; do
  read -r target os cpu pkg <<<"$spec"
  tar="$ARTIFACTS/abgen-v${VERSION}-${target}.tar.gz"
  [ -f "$tar" ] || { echo "missing release archive: $tar" >&2; exit 1; }
  dir="$WORK/$pkg"
  mkdir -p "$dir"
  tar -xzf "$tar" -C "$dir" --strip-components=1
  cat >"$dir/package.json" <<EOF
{
  "name": "${SCOPE}/${pkg}",
  "version": "${VERSION}",
  "description": "abgen prebuilt binary for ${os} ${cpu} (installed by ${SCOPE}/abgen)",
  "license": "AGPL-3.0-or-later",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/${REPO}.git"
  },
  "os": ["${os}"],
  "cpu": ["${cpu}"]
}
EOF
  publish_pkg "$dir"
done

main="$WORK/abgen"
cp -R "$HERE/abgen" "$main"
node - "$main/package.json" "$VERSION" "$SCOPE" "$REPO" <<'EOF'
const fs = require('fs')
const [file, version, scope, repo] = process.argv.slice(2)
const pkg = JSON.parse(fs.readFileSync(file, 'utf8'))
pkg.name = pkg.name.replace(/^@dcl\//, `${scope}/`)
pkg.version = version
pkg.repository.url = `git+https://github.com/${repo}.git`
pkg.optionalDependencies = Object.fromEntries(
  Object.keys(pkg.optionalDependencies).map((k) => [k.replace(/^@dcl\//, `${scope}/`), version])
)
fs.writeFileSync(file, JSON.stringify(pkg, null, 2) + '\n')
EOF
if [ "$SCOPE" != "@dcl" ]; then
  sed -i.bak "s;@dcl/abgen;${SCOPE}/abgen;g" "$main/index.js" "$main/index.d.ts" "$main/README.md" "$main/bin/abgen.js"
  rm -f "$main"/*.bak "$main"/bin/*.bak
fi
publish_pkg "$main"
