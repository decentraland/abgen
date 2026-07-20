#!/usr/bin/env bash
# LOD_1 conversion parity harness: run the SAME baked LOD GLB through the
# Unity converter (decentraland/asset-bundle-converter, LODClient lane) and
# through abgen-lod, then diff the two bundles structurally
# (`abgen-lod compare`) and at material granularity (matdump diff).
#
# Usage:
#   scripts/lod-parity.sh [options] [GLB ...]
#
#   GLB   LOD source .glb — URL or local path, named {sceneId}_{level}.glb
#         (the sceneId is the entity hash both converters resolve parcels
#         from). Default: the reference GLB the Unity repo's convert-lods.sh
#         itself uses.
#
# Options (each also settable via the env var in parens):
#   --abc DIR        asset-bundle-converter checkout      (ABC_REPO,
#                    default: ../asset-bundle-converter next to this repo)
#   --unity PATH     Unity binary                          (UNITY_PATH,
#                    default: the 6000.2.6f2 Hub path convert-lods.sh uses)
#   --content URL    content server both converters query  (CONTENT_URL,
#                    default: https://peer.decentraland.zone/content)
#   --platform P     windows|mac|linux                     (PLATFORM,
#                    default: mac — pass -buildTarget to Unity to match)
#   --out DIR        harness output root
#                    (default: <repo>/lod-parity/<timestamp>)
#   --unity-out DIR  reuse an existing Unity output dir and skip the Unity
#                    run (its layout: <dir>/{level}/{sid}_{level}_{platform})
#   --site           also build a lodsite run dir (pipeline/lodsite.py with
#                    ABGEN_LOD_PROD_BASE=file://<unity output>) so the
#                    verdicts render on the compare site's /lod.html page.
#                    LOD level 1 only — lodsite walks the LOD/1 tree.
#
# Exit code: 0 = every pair PASSed every structural check, 1 otherwise.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_GLB="https://lod-unity-bucket-dev-0871c25.s3.us-east-1.amazonaws.com/lods-unity/lods/bafkreiecbcziuwjcqrs2zbe7ncy2pssefgd4cg7vj5o4ywrn5umt6nobi4_1.glb"

ABC_REPO="${ABC_REPO:-$REPO/../asset-bundle-converter}"
DEFAULT_UNITY="/Applications/Unity/Hub/Editor/6000.2.6f2/Unity.app/Contents/MacOS/Unity"
UNITY_PATH="${UNITY_PATH:-$DEFAULT_UNITY}"
# A stale/typo'd UNITY_PATH in the shell profile is common enough that
# convert-lods.sh warns about it — fall back to the Hub default if the env
# value doesn't point at an executable but the default does.
if [ ! -x "$UNITY_PATH" ] && [ -x "$DEFAULT_UNITY" ]; then
  echo "WARN: UNITY_PATH=$UNITY_PATH is not executable; using $DEFAULT_UNITY" >&2
  UNITY_PATH="$DEFAULT_UNITY"
fi
CONTENT_URL="${CONTENT_URL:-https://peer.decentraland.zone/content}"
PLATFORM="${PLATFORM:-mac}"
OUT=""
UNITY_OUT=""
BUILD_SITE=0
GLBS=()

while [ $# -gt 0 ]; do
  case "$1" in
    --abc)       ABC_REPO="$2"; shift 2 ;;
    --unity)     UNITY_PATH="$2"; shift 2 ;;
    --content)   CONTENT_URL="$2"; shift 2 ;;
    --platform)  PLATFORM="$2"; shift 2 ;;
    --out)       OUT="$2"; shift 2 ;;
    --unity-out) UNITY_OUT="$2"; shift 2 ;;
    --site)      BUILD_SITE=1; shift ;;
    -h|--help)   sed -n '2,36p' "$0"; exit 0 ;;
    -*)          echo "unknown flag: $1" >&2; exit 2 ;;
    *)           GLBS+=("$1"); shift ;;
  esac
done
[ ${#GLBS[@]} -gt 0 ] || GLBS=("$DEFAULT_GLB")

case "$PLATFORM" in
  windows) BUILD_TARGET=Win64 ;;
  mac)     BUILD_TARGET=OSXUniversal ;;
  linux)   BUILD_TARGET=Linux64 ;;
  *) echo "unsupported --platform $PLATFORM (want windows|mac|linux)" >&2; exit 2 ;;
esac

ABGEN_LOD="$REPO/target/release/abgen-lod"
MATDUMP="$REPO/target/release/examples/matdump"
for tool in "$ABGEN_LOD" "$MATDUMP"; do
  [ -x "$tool" ] || { echo "missing $tool — build with: cargo build --release -p abgen --bin abgen-lod --examples" >&2; exit 2; }
done

OUT="${OUT:-$REPO/lod-parity/$(date +%Y%m%d-%H%M%S)}"
SRC_DIR="$OUT/src"
OURS_DIR="$OUT/ours"
DIFF_DIR="$OUT/diff"
mkdir -p "$SRC_DIR" "$OURS_DIR" "$DIFF_DIR"

# --- Stage sources locally; derive {sceneId, level} from each filename ------
SIDS=()
LEVELS=()
LOCALS=()
UNITY_URLS=()
for glb in "${GLBS[@]}"; do
  fname="$(basename "${glb%%\?*}")"
  stem="${fname%.*}"
  level="${stem##*_}"
  sid="$(printf '%s' "${stem%_*}" | tr '[:upper:]' '[:lower:]')"
  case "$level" in (*[!0-9]*|'') echo "cannot parse level from $fname (want {sceneId}_{level}.glb)" >&2; exit 2 ;; esac
  local_path="$SRC_DIR/${sid}_${level}.glb"
  if [ -f "$glb" ]; then
    cp "$glb" "$local_path"
    UNITY_URLS+=("file://$local_path")
  else
    echo "fetching $glb"
    curl -fsSL -A abgen-lod-parity -o "$local_path" "$glb"
    UNITY_URLS+=("$glb")
  fi
  SIDS+=("$sid"); LEVELS+=("$level"); LOCALS+=("$local_path")
done

# --- Unity side --------------------------------------------------------------
if [ -z "$UNITY_OUT" ]; then
  UNITY_OUT="$OUT/unity"
  UNITY_LOG="$OUT/unity.log"
  PROJECT_PATH="$ABC_REPO/asset-bundle-converter"
  [ -d "$PROJECT_PATH/Assets" ] || { echo "no Unity project at $PROJECT_PATH (--abc?)" >&2; exit 2; }
  [ -x "$UNITY_PATH" ] || { echo "no Unity binary at $UNITY_PATH (--unity?)" >&2; exit 2; }
  mkdir -p "$UNITY_OUT"

  # Same clean-state step as convert-lods.sh: stale .meta files from a prior
  # run on a different importer chain break the first import.
  rm -rf "$PROJECT_PATH/Assets/_DownloadedGLBs" "$PROJECT_PATH/Assets/_DownloadedGLBs.meta"

  LODS_ARG="$(IFS=';'; echo "${UNITY_URLS[*]}")"
  echo "== Unity converter ($BUILD_TARGET) -> $UNITY_OUT"
  echo "   lods: $LODS_ARG"
  if ! "$UNITY_PATH" \
      -batchmode \
      -buildTarget "$BUILD_TARGET" \
      -projectPath "$PROJECT_PATH" \
      -executeMethod DCL.ABConverter.LODClient.ExportURLLODsToAssetBundles \
      -lods "$LODS_ARG" \
      -contentServerUrl "$CONTENT_URL" \
      -output "$UNITY_OUT" \
      -logFile "$UNITY_LOG"; then
    echo "Unity converter FAILED — last log lines:" >&2
    tail -25 "$UNITY_LOG" >&2
    exit 1
  fi
else
  echo "== Reusing Unity output: $UNITY_OUT"
fi

# --- abgen side --------------------------------------------------------------
echo "== abgen-lod ($PLATFORM) -> $OURS_DIR"
for i in "${!SIDS[@]}"; do
  "$ABGEN_LOD" bundle "${LOCALS[$i]}" \
    --entity "${SIDS[$i]}" --level "${LEVELS[$i]}" \
    --platform "$PLATFORM" --catalyst "$CONTENT_URL" --out "$OURS_DIR"
done

# --- Pair + verdicts ----------------------------------------------------------
FAILS=0
echo
echo "== Verdicts"
for i in "${!SIDS[@]}"; do
  sid="${SIDS[$i]}"; level="${LEVELS[$i]}"
  ours="$OURS_DIR/$sid/LOD/$level/${sid}_${level}_${PLATFORM}"
  theirs="$UNITY_OUT/$level/${sid}_${level}_${PLATFORM}"
  if [ ! -f "$theirs" ]; then
    echo "FAIL $sid: Unity bundle missing at $theirs" ; FAILS=$((FAILS+1)); continue
  fi
  if [ ! -f "$ours" ]; then
    echo "FAIL $sid: abgen bundle missing at $ours" ; FAILS=$((FAILS+1)); continue
  fi

  if "$ABGEN_LOD" compare "$ours" "$theirs" > "$DIFF_DIR/$sid.compare.txt" 2>&1; then
    verdict=PASS
  else
    verdict=FAIL; FAILS=$((FAILS+1))
  fi

  "$MATDUMP" "$ours"   > "$DIFF_DIR/$sid.mat.ours.txt"   2>&1 || true
  "$MATDUMP" "$theirs" > "$DIFF_DIR/$sid.mat.unity.txt"  2>&1 || true
  matdiff="$DIFF_DIR/$sid.materials.diff"
  if diff -u "$DIFF_DIR/$sid.mat.ours.txt" "$DIFF_DIR/$sid.mat.unity.txt" > "$matdiff"; then
    matnote="materials identical"
  else
    matnote="material diff: $(grep -c '^[+-][^+-]' "$matdiff") lines -> $matdiff"
  fi
  echo "$verdict $sid  ($(grep -c '^PASS' "$DIFF_DIR/$sid.compare.txt") PASS / $(grep -c '^FAIL' "$DIFF_DIR/$sid.compare.txt") FAIL rows; $matnote)"
  grep '^FAIL' "$DIFF_DIR/$sid.compare.txt" | sed 's/^/       /' || true
done

# --- Optional: lodsite run dir for the compare site's /lod.html --------------
if [ "$BUILD_SITE" = 1 ]; then
  PROD_FLAT="$OUT/prod-flat"
  mkdir -p "$PROD_FLAT"
  for i in "${!SIDS[@]}"; do
    f="$UNITY_OUT/${LEVELS[$i]}/${SIDS[$i]}_${LEVELS[$i]}_${PLATFORM}"
    [ -f "$f" ] && cp "$f" "$PROD_FLAT/"
  done
  RUN_ID="lod-parity-$(date +%Y%m%d-%H%M%S)"
  echo
  echo "== lodsite run dir ($RUN_ID)"
  ABGEN_LOD_PROD_BASE="file://$PROD_FLAT/" \
  ABGEN_LOD_CONTENT_URL="$CONTENT_URL/contents/" \
  ABGEN_LOD_BIN="$ABGEN_LOD" \
    python3 "$REPO/pipeline/lodsite.py" --out-root "$OURS_DIR" \
      --platform "$PLATFORM" --run-id "$RUN_ID"
  echo "serve it: ./pipeline/abgen-compare serve   ->  /lod.html (run $RUN_ID)"
fi

echo
if [ "$FAILS" -gt 0 ]; then
  echo "RESULT: $FAILS of ${#SIDS[@]} pair(s) failed — details in $DIFF_DIR"
  exit 1
fi
echo "RESULT: all ${#SIDS[@]} pair(s) structurally PASS — artifacts in $OUT"
