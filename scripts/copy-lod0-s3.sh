#!/usr/bin/env bash
# Server-side copy of production LOD_0 bundles from the ab-cdn source bucket into an abgen LOD
# prefix, driven by the key list that scripts/mirror-lod0-abcdn.sh's HEAD sweep produced
# (~/Decentraland/lod0-abcdn/lod0-keys-present.txt: "<key> <bytes>" per line, key = LOD/0/{sid}_0_{platform}).
#
# No bytes leave S3: `aws s3 cp s3://a/k s3://b/k` is a CopyObject. Metadata is rewritten on the
# way so the objects match the LOD_1 set published by 09-publish-all.sh (octet-stream, immutable).
#
# Usage:
#   scripts/copy-lod0-s3.sh --src s3://<SOURCE_BUCKET> --dst s3://abgen-lod1-public-175651002275/b191e06 \
#       [--keys FILE] [--jobs 32] [--limit N] [--profile NAME]
#
# The identity used needs s3:GetObject on the source keys and s3:PutObject on the destination.
# Resumable: keys already present in the destination (same size) are skipped. Failures are
# listed in copy-failed.txt next to the key file; rerun to retry them.
set -Eeuo pipefail

KEYS=~/Decentraland/lod0-abcdn/lod0-keys-present.txt
SRC= DST= JOBS=32 LIMIT=0 PROFILE=
while [ $# -gt 0 ]; do
  case "$1" in
    --src) SRC=${2%/}; shift 2 ;;
    --dst) DST=${2%/}; shift 2 ;;
    --keys) KEYS=$2; shift 2 ;;
    --jobs) JOBS=$2; shift 2 ;;
    --limit) LIMIT=$2; shift 2 ;;
    --profile) PROFILE=$2; shift 2 ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
case "$SRC" in s3://*) ;; *) echo "--src s3://bucket[/prefix] is required" >&2; exit 2 ;; esac
case "$DST" in s3://*) ;; *) echo "--dst s3://bucket/prefix is required" >&2; exit 2 ;; esac
[ -r "$KEYS" ] || { echo "key list not readable: $KEYS" >&2; exit 2; }
[ -n "$PROFILE" ] && export AWS_PROFILE=$PROFILE
STATE=$(dirname "$KEYS"); FAILED=$STATE/copy-failed.txt; DONE_LOG=$STATE/copy-done.log
touch "$FAILED" "$DONE_LOG"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/copy-lod0.XXXXXX"); trap 'rm -rf "$TMP"' EXIT
awk 'NF>=1{print $1, ($2==""?0:$2)}' "$KEYS" | sort -u > "$TMP/want"
[ "$LIMIT" -gt 0 ] && { head -n "$LIMIT" "$TMP/want" > "$TMP/w"; mv "$TMP/w" "$TMP/want"; }
# one listing of the destination prefix instead of one HEAD per key
aws s3 ls "$DST/LOD/0/" | awk '{print "LOD/0/" $4, $3}' | sort > "$TMP/have"
comm -23 "$TMP/want" "$TMP/have" > "$TMP/todo"        # differs in key or size => copy
echo "keys listed: $(wc -l < "$TMP/want" | tr -d ' ')   already in destination: $(( $(wc -l < "$TMP/want") - $(wc -l < "$TMP/todo") ))   to copy: $(wc -l < "$TMP/todo" | tr -d ' ')   bytes to copy: $(awk '{s+=$2} END{printf "%.2f GB", s/1e9}' "$TMP/todo")"
[ -s "$TMP/todo" ] || { echo "nothing to do"; exit 0; }
: > "$FAILED"

copy_one() {
  local key=$1 size=$2
  if aws s3 cp "$SRC/$key" "$DST/$key" --only-show-errors \
       --content-type application/octet-stream --cache-control 'public,max-age=31536000,immutable' \
       --metadata-directive REPLACE; then
    echo "$key $size" >> "$DONE_LOG"
  else
    echo "$key" >> "$FAILED"
  fi
}
export -f copy_one; export SRC DST FAILED DONE_LOG
echo "[$(date '+%F %T')] copying with $JOBS workers: $SRC -> $DST"
xargs -P "$JOBS" -n 2 bash -c 'copy_one "$@"' _ < "$TMP/todo" || true
echo "[$(date '+%F %T')] done"
echo "destination now holds $(aws s3 ls "$DST/LOD/0/" | wc -l | tr -d ' ') LOD/0 objects; failed this run: $(wc -l < "$FAILED" | tr -d ' ')"
[ -s "$FAILED" ] && { echo "failures in $FAILED; rerun the same command to retry"; exit 1; }
exit 0
