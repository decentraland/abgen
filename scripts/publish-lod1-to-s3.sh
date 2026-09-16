#!/usr/bin/env bash
# Publish a downloaded abgen LOD run to a dedicated S3 prefix, in the key layout
# `crate/src/lods.rs::published_objects` defines and the client resolves from one base URL:
#
#   {prefix}/LOD/{level}/{sid}_{level}_{platform}                  asset bundles
#   {prefix}/LOD/lods-unity/manifests/{sid}_InitialSceneState.json ISS descriptors
#   {prefix}/LOD/lods-unity/lods/{sid}_{level}.glb                 gltfpack-layout GLBs
#   {prefix}/LOD/lod-reuse/by-inputs|by-state/{digest}.json        reuse records (run-level)
#
# Everything sits under one top-level LOD/ folder; the bundles already carried that prefix, the
# other three families are nested into it (`publish_key` in crate/src/bin/abgen-lod/qualify.rs).
#
# Per-family metadata carries the values `crate/src/space.rs::object_headers` derives for
# these families, so objects match what abgen's own uploader writes.
#
# Usage:
#   scripts/publish-lod1-to-s3.sh --src ~/Decentraland/lod1-b191e06 \
#       --dst s3://my-bucket/b191e06 [--profile NAME] [--level 1] \
#       [--families bundles,manifests,glbs,reuse] [--dry-run]
#
# --src is either:
#   * a publish tree, i.e. a directory that already holds LOD/ at its root. `abgen-lod
#     qualify-corpus` writes one at {out}/publish (see --publish-dir); it is uploaded as it
#     stands, so its paths are the keys.
#   * a raw download root holding the run's per-scene output directories (the pre-publish-tree
#     layout, e.g. the b191e06 download). Files are hard-linked into <src>/flat/ in the nested
#     layout first (no extra disk) and that becomes the publish tree.
#
# Rerunning resumes: s3 sync skips objects already present at the same size.
set -Eeuo pipefail

SRC= DST= PROFILE= LEVEL=1 FAMILIES=bundles,manifests,glbs,reuse DRY=
while [ $# -gt 0 ]; do
  case "$1" in
    --src) SRC=${2%/}; shift 2 ;;
    --dst) DST=${2%/}; shift 2 ;;
    --profile) PROFILE=$2; shift 2 ;;
    --level) LEVEL=$2; shift 2 ;;
    --families) FAMILIES=$2; shift 2 ;;
    --dry-run) DRY=--dryrun; shift ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ -d "$SRC" ] || { echo "--src must be the download root or a publish tree" >&2; exit 2; }
case "$DST" in s3://*/*) ;; *) echo "--dst must look like s3://bucket/prefix" >&2; exit 2 ;; esac
[ -n "$PROFILE" ] && export AWS_PROFILE=$PROFILE
command -v aws >/dev/null || { echo "aws CLI not found" >&2; exit 2; }
has() { case ",$FAMILIES," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

# A publish tree needs no staging: its paths are already the keys.
if [ -d "$SRC/LOD/lods-unity" ] || [ -d "$SRC/LOD/$LEVEL" ]; then
  FLAT=$SRC
  echo "[$(date '+%F %T')] $SRC is already a publish tree; uploading it as it stands"
else
  FLAT=$SRC/flat
  echo "[$(date '+%F %T')] staging hard links under $FLAT"
  mkdir -p "$FLAT/LOD/$LEVEL" "$FLAT/LOD/lods-unity/manifests" "$FLAT/LOD/lods-unity/lods"
  if has bundles;   then find "$SRC" -path "*/LOD/$LEVEL/*" -type f ! -path "$FLAT/*" ! -name "*.tmp.*" ! -name "*.br" -exec ln -f {} "$FLAT/LOD/$LEVEL/" ';'; fi
  if has manifests; then find "$SRC" -name "*_InitialSceneState.json" -type f ! -path "$FLAT/*" ! -path "*/lod-work/*" ! -name "*.tmp.*" -exec ln -f {} "$FLAT/LOD/lods-unity/manifests/" ';'; fi
  if has glbs;      then find "$SRC" -name "*_$LEVEL.glb" -type f ! -path "$FLAT/*" ! -path "*/.work/*" ! -name "*.tmp.*" -exec ln -f {} "$FLAT/LOD/lods-unity/lods/" ';'; fi
  printf 'staged: %s bundles, %s manifests, %s glbs\n' \
    "$(ls "$FLAT/LOD/$LEVEL" | wc -l | tr -d ' ')" \
    "$(ls "$FLAT/LOD/lods-unity/manifests" | wc -l | tr -d ' ')" \
    "$(ls "$FLAT/LOD/lods-unity/lods" | wc -l | tr -d ' ')"
fi
# A publish tree holds its reuse records inside it; a raw download root has them beside it.
REUSE=$FLAT/LOD/lod-reuse
[ -d "$REUSE" ] || REUSE=$SRC/lod-reuse

# family -> content-type + cache-control. The values are the ones
# crate/src/space.rs::object_headers derives for these families; it classifies on the
# un-nested prefixes, so under LOD/ they have to be passed explicitly, as here.
push() {  # push <path, identical on disk and as a key> <content-type> <cache-control>
  local key=$1 ct=$2 cc=$3
  local dir=$FLAT/$key
  [ -n "$(ls -A "$dir" 2>/dev/null)" ] || { echo "skip $key (nothing staged)"; return 0; }
  echo "[$(date '+%F %T')] sync $key  ($(ls "$dir" | wc -l | tr -d ' ') objects, $(du -sh "$dir" | cut -f1))"
  aws s3 sync "$dir/" "$DST/$key/" $DRY --only-show-errors --exclude '*.tmp.*' \
    --content-type "$ct" --cache-control "$cc"
}
if has bundles;   then push "LOD/$LEVEL"               application/wasm  'public,max-age=31536000,immutable'; fi
if has manifests; then push "LOD/lods-unity/manifests" application/json  'public, max-age=31536000'; fi
if has glbs;      then push "LOD/lods-unity/lods"      model/gltf-binary 'public, max-age=31536000'; fi

# Records go last and on purpose: each one authorises a later deployment to reuse the keys it
# names, so a record must never become visible before the objects it points at. They are also
# the one family that must not be cached (object_headers gives lod-reuse/ NO_CACHE), because a
# reusing job rewrites them in place.
if has reuse && [ -d "$REUSE" ]; then
  for idx in by-inputs by-state; do
    [ -d "$REUSE/$idx" ] || continue
    echo "[$(date '+%F %T')] sync lod-reuse/$idx  ($(ls "$REUSE/$idx" | wc -l | tr -d ' ') records)"
    aws s3 sync "$REUSE/$idx/" "$DST/LOD/lod-reuse/$idx/" $DRY --only-show-errors \
      --exclude '*.tmp.*' --content-type application/json --cache-control 'private, max-age=0, no-cache'
  done
fi

echo "[$(date '+%F %T')] done"
if [ -n "$DRY" ]; then echo "(dry run: nothing uploaded)"; exit 0; fi
for k in "LOD/$LEVEL" LOD/lods-unity/manifests LOD/lods-unity/lods; do
  printf '%-24s %s objects in destination\n' "$k" "$(aws s3 ls "$DST/$k/" | wc -l | tr -d ' ')"
done
