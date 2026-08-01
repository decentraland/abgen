#!/usr/bin/env bash
set -euo pipefail

DIR="${1:?usage: write-build-info.sh <dir> <target>}"
TARGET="${2:?usage: write-build-info.sh <dir> <target>}"

[ -d "$DIR" ] || { echo "write-build-info: $DIR is not a directory" >&2; exit 1; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

: "${ABGEN_BUILD_ID:?write-build-info: ABGEN_BUILD_ID is unset (setup job output)}"
: "${ABGEN_GIT_REV:?write-build-info: ABGEN_GIT_REV is unset (github.sha)}"
: "${SOURCE_DATE_EPOCH:?write-build-info: SOURCE_DATE_EPOCH is unset (pinned 315532800)}"

case "$ABGEN_BUILD_ID" in
  *[!0-9a-f]*)
    echo "write-build-info: ABGEN_BUILD_ID is not lowercase hex: $ABGEN_BUILD_ID" >&2
    exit 1
    ;;
esac
if [ "${#ABGEN_BUILD_ID}" -ne 12 ]; then
  echo "write-build-info: ABGEN_BUILD_ID must be 12 chars, got ${#ABGEN_BUILD_ID}" >&2
  exit 1
fi

version="$(sed -n 's/^version = "\(.*\)"$/\1/p; /^version = "/q' "$ROOT/crate/Cargo.toml")"
[ -n "$version" ] || {
  echo "write-build-info: no version in $ROOT/crate/Cargo.toml" >&2
  exit 1
}

out="$DIR/BUILD-INFO.txt"
{
  printf 'ABGEN_VERSION=%s\n' "$version"
  printf 'ABGEN_RELEASE=%s\n' "${ABGEN_RELEASE:-dev}"
  printf 'ABGEN_TARGET=%s\n' "$TARGET"
  printf 'ABGEN_BUILD_ID=%s\n' "$ABGEN_BUILD_ID"
  printf 'ABGEN_GIT_REV=%s\n' "$ABGEN_GIT_REV"
  printf 'SOURCE_DATE_EPOCH=%s\n' "$SOURCE_DATE_EPOCH"
} > "$out"

echo "wrote $out"
cat "$out"
