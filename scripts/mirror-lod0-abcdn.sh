#!/usr/bin/env bash
# Mirror the production LOD_0 asset bundles from ab-cdn.decentraland.org into the abgen LOD
# key layout, so an abgen LOD prefix can answer the client's LOD/0 requests with the same
# objects production serves:
#
#     LOD/0/{sceneId}_0_{platform}        sceneId lowercased, exactly as the client requests it
#
# The production CDN cannot be listed, so the inventory is driven by a scene-id list (one id
# per line, any case). Objects the CDN does not have (404) are recorded once in missing.txt
# and never re-probed. `.br` sidecars are not mirrored: abgen serves none.
#
# Two sinks:
#   --dst DIR              download to DIR/LOD/0/...  (resumable: present files are skipped)
#   --s3 s3://bucket/pfx   stream each object into S3 at pfx/LOD/0/... with no local copy
#                          (skips keys already in the bucket; needs the AWS CLI + a profile
#                          that can write there). Bookkeeping files go to --state DIR.
#
# Usage:
#   scripts/mirror-lod0-abcdn.sh --sids sids.txt --dst ~/Decentraland/lod0-abcdn [--limit 5]
#   scripts/mirror-lod0-abcdn.sh --sids sids.txt --s3 s3://abgen-lod1-public-175651002275/b191e06 --state ~/Decentraland/lod0-abcdn
#
# Options:
#   --platforms windows,mac   default windows,mac
#   --jobs N                  parallel transfers, default 32 (the CDN is request-latency bound)
#   --limit N                 only the first N scene ids, for a smoke run
#   --src URL                 default https://ab-cdn.decentraland.org
#
# Bookkeeping (in --dst or --state):
#   status.log    one line per attempted object: <sid> <platform> <http-code> <bytes>
#   missing.txt   "<sid> <platform>" pairs that 404 on the source
#   failed.txt    transport / 5xx / size-mismatch failures after retries; rerun to retry them
#
# Rerunning the same command resumes: done objects, known-missing pairs and (in --s3 mode)
# keys already in the bucket are skipped.
set -Eeuo pipefail

SRC=https://ab-cdn.decentraland.org
PLATFORMS=windows,mac
JOBS=32
LIMIT=0
SIDS= DST= S3= STATE=
CONTENT_TYPE=application/octet-stream           # what 09-publish-all.sh used for LOD/1
CACHE_CONTROL='public,max-age=31536000,immutable'

while [ $# -gt 0 ]; do
  case "$1" in
    --sids) SIDS=$2; shift 2 ;;
    --dst) DST=$2; shift 2 ;;
    --s3) S3=${2%/}; shift 2 ;;
    --state) STATE=$2; shift 2 ;;
    --platforms) PLATFORMS=$2; shift 2 ;;
    --jobs) JOBS=$2; shift 2 ;;
    --limit) LIMIT=$2; shift 2 ;;
    --src) SRC=${2%/}; shift 2 ;;
    -h|--help) sed -n '2,40p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

[ -n "$SIDS" ] && [ -r "$SIDS" ] || { echo "--sids FILE is required and must be readable" >&2; exit 2; }
if [ -n "$DST" ] && [ -n "$S3" ]; then echo "pick one sink: --dst or --s3" >&2; exit 2; fi
if [ -z "$DST" ] && [ -z "$S3" ]; then echo "a sink is required: --dst DIR or --s3 s3://bucket/prefix" >&2; exit 2; fi
if [ -n "$S3" ]; then
  case "$S3" in s3://*) ;; *) echo "--s3 must look like s3://bucket/prefix" >&2; exit 2 ;; esac
  [ -n "$STATE" ] || { echo "--s3 needs --state DIR for the bookkeeping files" >&2; exit 2; }
  command -v aws >/dev/null || { echo "aws CLI not found" >&2; exit 2; }
else
  STATE=$DST
fi
command -v curl >/dev/null || { echo "curl not found" >&2; exit 2; }

mkdir -p "$STATE"
[ -n "$DST" ] && mkdir -p "$DST/LOD/0"
touch "$STATE/status.log" "$STATE/missing.txt" "$STATE/failed.txt"

# ---- build the job list: "<sid> <platform>" per line, minus what is already done -------------
TMP=$(mktemp -d "${TMPDIR:-/tmp}/mirror-lod0.XXXXXX"); trap 'rm -rf "$TMP"' EXIT
tr 'A-Z' 'a-z' < "$SIDS" | awk 'NF{print $1}' | awk '!seen[$0]++' > "$TMP/sids"
[ "$LIMIT" -gt 0 ] && { head -n "$LIMIT" "$TMP/sids" > "$TMP/sids.lim"; mv "$TMP/sids.lim" "$TMP/sids"; }
IFS=, read -r -a PLATS <<< "$PLATFORMS"
for p in "${PLATS[@]}"; do awk -v p="$p" '{print $1, p}' "$TMP/sids"; done > "$TMP/jobs.all"

# already-present objects
if [ -n "$DST" ]; then
  ( cd "$DST/LOD/0" && ls 2>/dev/null ) | awk -F'_0_' 'NF==2 && $2 !~ /\.tmp\./ {print $1, $2}' > "$TMP/done"
else
  aws s3 ls "$S3/LOD/0/" | awk '{print $4}' | awk -F'_0_' 'NF==2 {print $1, $2}' > "$TMP/done"
fi
sort -u "$TMP/done" "$STATE/missing.txt" > "$TMP/skip"
sort -u "$TMP/jobs.all" | comm -23 - "$TMP/skip" > "$TMP/jobs"

echo "scene ids: $(wc -l < "$TMP/sids" | tr -d ' ')   objects wanted: $(wc -l < "$TMP/jobs.all" | tr -d ' ')   already present: $(wc -l < "$TMP/done" | tr -d ' ')   known missing: $(wc -l < "$STATE/missing.txt" | tr -d ' ')   to transfer: $(wc -l < "$TMP/jobs" | tr -d ' ')"
[ -s "$TMP/jobs" ] || { echo "nothing to do"; exit 0; }
: > "$STATE/failed.txt"      # failures are re-attempted on every run; the file reflects this run
START_LINES=$(wc -l < "$STATE/status.log" | tr -d ' ')

# s3api head-object wants the bare bucket name and the full key
BUCKET= PREFIX=
if [ -n "$S3" ]; then
  BUCKET=${S3#s3://}; BUCKET=${BUCKET%%/*}
  PREFIX=${S3#s3://$BUCKET}; PREFIX=${PREFIX#/}
fi

# ---- one object --------------------------------------------------------------------------------
work() {
  # one assignment per line: bash expands every value of a single `local a=.. b=$a` before assigning any
  local sid=$1 p=$2 code bytes
  local key="LOD/0/${sid}_0_${p}"
  local url="$SRC/LOD/0/${sid}_0_${p}"
  if [ -n "$DST" ]; then
    local out="$DST/$key" tmp="$DST/$key.tmp.$$"
    code=$(curl -sS --retry 3 --retry-all-errors --retry-delay 2 --connect-timeout 15 --max-time 900 \
                -o "$tmp" -w '%{http_code}' "$url" 2>/dev/null) || code=000
    if [ "$code" = 200 ]; then
      mv -f "$tmp" "$out"
      bytes=$(stat -f%z "$out" 2>/dev/null || stat -c%s "$out")
      echo "$sid $p 200 $bytes" >> "$STATE/status.log"
    elif [ "$code" = 404 ]; then
      rm -f "$tmp"; echo "$sid $p" >> "$STATE/missing.txt"; echo "$sid $p 404 0" >> "$STATE/status.log"
    else
      rm -f "$tmp"; echo "$sid $p $code" >> "$STATE/failed.txt"; echo "$sid $p $code 0" >> "$STATE/status.log"
    fi
  else
    # HEAD first: a 404 must never become an empty object in the bucket.
    local hdr; hdr=$(curl -sI --retry 3 --retry-all-errors --connect-timeout 15 "$url" 2>/dev/null) || hdr=
    code=$(printf "%s" "$hdr" | awk "toupper(\$1) ~ /^HTTP\\// {c=\$2} END{print c+0}"); [ "$code" = 0 ] && code=000
    local want; want=$(printf '%s' "$hdr" | awk 'tolower($1)=="content-length:" {gsub(/\r/,"",$2); print $2}' | tail -1)
    if [ "$code" = 404 ]; then
      echo "$sid $p" >> "$STATE/missing.txt"; echo "$sid $p 404 0" >> "$STATE/status.log"; return 0
    elif [ "$code" != 200 ] || [ -z "$want" ]; then
      echo "$sid $p ${code:-000}" >> "$STATE/failed.txt"; echo "$sid $p ${code:-000} 0" >> "$STATE/status.log"; return 0
    fi
    if curl -sfS --retry 3 --retry-all-errors --connect-timeout 15 --max-time 900 "$url" 2>/dev/null \
        | aws s3 cp - "$S3/$key" --only-show-errors --expected-size "$want" \
            --content-type "$CONTENT_TYPE" --cache-control "$CACHE_CONTROL"; then
      # a mid-stream curl failure still lets aws finish a truncated upload: verify the size
      bytes=$(aws s3api head-object --bucket "$BUCKET" --key "${PREFIX:+$PREFIX/}$key" --query ContentLength --output text 2>/dev/null || echo -1)
      bytes=${bytes:-0}
      if [ "$bytes" = "$want" ]; then
        echo "$sid $p 200 $bytes" >> "$STATE/status.log"
      else
        aws s3 rm "$S3/$key" --only-show-errors >/dev/null 2>&1 || true
        echo "$sid $p size-mismatch($bytes!=$want)" >> "$STATE/failed.txt"; echo "$sid $p 000 0" >> "$STATE/status.log"
      fi
    else
      aws s3 rm "$S3/$key" --only-show-errors >/dev/null 2>&1 || true
      echo "$sid $p stream-failed" >> "$STATE/failed.txt"; echo "$sid $p 000 0" >> "$STATE/status.log"
    fi
  fi
}
export -f work
export SRC DST S3 STATE CONTENT_TYPE CACHE_CONTROL BUCKET PREFIX

echo "[$(date '+%F %T')] transferring with $JOBS workers -> ${DST:-$S3}"
xargs -P "$JOBS" -n 2 bash -c 'work "$@"' _ < "$TMP/jobs" || true
echo "[$(date '+%F %T')] done"

# ---- summary of this run -----------------------------------------------------------------------
tail -n +"$((START_LINES + 1))" "$STATE/status.log" | awk '
  $3==200 {ok++; b+=$4} $3==404 {miss++} $3!=200 && $3!=404 {fail++}
  END {printf "this run: ok=%d (%.2f GB)  missing-on-source=%d  failed=%d\n", ok, b/1e9, miss, fail}'
if [ -n "$DST" ]; then PRESENT=$(ls "$DST/LOD/0" | grep -v "\.tmp\." | wc -l | tr -d " "); else PRESENT=$(aws s3 ls "$S3/LOD/0/" | wc -l | tr -d " "); fi
echo "totals:   present=$PRESENT   known-missing=$(wc -l < "$STATE/missing.txt" | tr -d ' ')   failed=$(wc -l < "$STATE/failed.txt" | tr -d ' ')"
[ -n "$DST" ] && du -sh "$DST/LOD/0" | awk '{print "on disk:  " $1}'
[ -s "$STATE/failed.txt" ] && { echo "failures listed in $STATE/failed.txt; rerun the same command to retry them"; exit 1; }
exit 0
