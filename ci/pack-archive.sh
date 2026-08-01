#!/usr/bin/env bash
set -euo pipefail

DIR="${1:?usage: pack-archive.sh <dir> <out.tar.gz> <source-date-epoch>}"
OUT="${2:?usage: pack-archive.sh <dir> <out.tar.gz> <source-date-epoch>}"
EPOCH="${3:?usage: pack-archive.sh <dir> <out.tar.gz> <source-date-epoch>}"

TAR=tar
command -v gtar >/dev/null && TAR=gtar

if ! "$TAR" --sort=name --version >/dev/null 2>&1; then
  echo "$TAR does not support --sort; entry order would be unspecified and" >&2
  echo "the archive unreproducible. Install GNU tar (as gtar) on this runner." >&2
  exit 1
fi

tmp="$OUT.partial.$$"
trap 'rm -f "$tmp"' EXIT
"$TAR" --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@${EPOCH}" -cf - "$DIR" | gzip -n -9 > "$tmp"
mv -f "$tmp" "$OUT"
