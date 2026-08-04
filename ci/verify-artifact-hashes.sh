#!/usr/bin/env bash
set -euo pipefail

TARGET="${1:?usage: verify-artifact-hashes.sh <target> <file>...}"
shift
[ "$#" -gt 0 ] || { echo "no artifacts given for $TARGET" >&2; exit 1; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST="$HERE/artifact-hashes/$TARGET.sha256"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum -- "$1" | cut -d' ' -f1
  else
    shasum -a 256 -- "$1" | cut -d' ' -f1
  fi
}

how_to_record() {
  echo "  Re-record with the release workflow's record_hashes dispatch input:" >&2
  echo "    gh workflow run release.yml --ref <ref> -f record_hashes=true" >&2
  echo "  then download the hashes-<target> artifacts it uploads and commit" >&2
  echo "  them under ci/artifact-hashes/. Do not hand-edit: a recorded hash is" >&2
  echo "  what a real build produced, and every later build checks against it." >&2
}

# One-shot record, not a same-run double-build: every later independent build
# re-verifying against this manifest is what actually proves reproducibility.
if [ "${ABGEN_RECORD_HASHES:-0}" = "1" ]; then
  for f in "$@"; do
    [ -f "$f" ] || { echo "recording: $f does not exist" >&2; exit 1; }
  done

  mkdir -p "$HERE/artifact-hashes"
  tmp="$MANIFEST.partial.$$"
  trap 'rm -f "$tmp"' EXIT
  : > "$tmp"
  for f in "$@"; do
    printf '%s  %s\n' "$(sha256_of "$f")" "$(basename "$f")" >> "$tmp"
  done
  LC_ALL=C sort -k2 -o "$tmp" "$tmp"
  mv -f "$tmp" "$MANIFEST"
  echo "recorded $# hashes for $TARGET"
  cat "$MANIFEST"
  exit 0
fi

if [ ! -f "$MANIFEST" ]; then
  echo "NO RECORDED HASHES for $TARGET." >&2
  echo "  expected: $MANIFEST" >&2
  echo "Nothing was verified. This is a hard failure and not a skip: a target" >&2
  echo "with no manifest ships whatever it happens to have built." >&2
  how_to_record
  exit 1
fi

failed=0
for f in "$@"; do
  base="$(basename "$f")"
  want="$(awk -v n="$base" '$2 == n {print $1}' "$MANIFEST")"
  if [ -z "$want" ]; then
    echo "MISSING FROM MANIFEST: $base is built but not recorded in $TARGET.sha256" >&2
    failed=1
    continue
  fi
  got="$(sha256_of "$f")"
  if [ "$got" != "$want" ]; then
    echo "HASH MISMATCH: $base" >&2
    echo "  expected: $want" >&2
    echo "  built:    $got" >&2
    failed=1
  else
    echo "ok  $base  $got"
  fi
done

while read -r _ recorded; do
  [ -n "$recorded" ] || continue
  for f in "$@"; do
    [ "$(basename "$f")" = "$recorded" ] && continue 2
  done
  echo "RECORDED BUT NOT BUILT: $recorded is in $TARGET.sha256 but was not produced" >&2
  failed=1
done < "$MANIFEST"

if [ "$failed" -ne 0 ]; then
  echo "" >&2
  echo "Shipped bytes changed. Either the build stopped being deterministic, or" >&2
  echo "a real change landed and the manifest is stale." >&2
  how_to_record
  exit 1
fi
echo "all $# artifacts match $TARGET.sha256"
