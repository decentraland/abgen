#!/usr/bin/env bash
set -euo pipefail

TARGET="${1:?usage: promote-target.sh <target> <artifact-zip> <out-dir>}"
ZIP="${2:?usage: promote-target.sh <target> <artifact-zip> <out-dir>}"
OUT="${3:?usage: promote-target.sh <target> <artifact-zip> <out-dir>}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
REF="${GITHUB_REF_NAME:-dev}"; REF="${REF//\//-}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$OUT"

unzip -q "$ZIP" -d "$work/zip"

nat_tgz="$(find "$work/zip" -name 'abgen-native-*.tar.gz' | head -1)"
dist_tgz="$(find "$work/zip" -name 'abgen-*.tar.gz' ! -name 'abgen-native-*' | head -1)"
[ -n "$nat_tgz" ] && [ -n "$dist_tgz" ] || {
  echo "promote: artifact for $TARGET is missing a tarball (dist='$dist_tgz' nat='$nat_tgz')" >&2
  exit 1
}

extract_root() { # tarball extract-dir -> dir containing BUILD-INFO.txt
  mkdir -p "$2"
  tar -xzf "$1" -C "$2"
  local info
  info="$(find "$2" -name BUILD-INFO.txt | head -1)"
  [ -n "$info" ] || { echo "promote: no BUILD-INFO.txt in $1" >&2; exit 1; }
  dirname "$info"
}

dist_src="$(extract_root "$dist_tgz" "$work/dist")"
nat_src="$(extract_root "$nat_tgz" "$work/nat")"

for d in "$dist_src" "$nat_src"; do
  got_target="$(sed -n 's/^ABGEN_TARGET=//p' "$d/BUILD-INFO.txt")"
  got_id="$(sed -n 's/^ABGEN_BUILD_ID=//p' "$d/BUILD-INFO.txt")"
  [ "$got_target" = "$TARGET" ] || {
    echo "promote: $d is for '$got_target', wanted '$TARGET'" >&2; exit 1; }
  [ "$got_id" = "$ABGEN_BUILD_ID" ] || {
    echo "promote: $d has build id '$got_id', wanted '$ABGEN_BUILD_ID'" >&2; exit 1; }
done

vdir="$work/verify"
mkdir -p "$vdir"
stage() { # src manifest-name
  [ -f "$1" ] || return 0
  cp "$1" "$vdir/$2"
}
stage "$dist_src/abgen"              abgen
stage "$dist_src/abgen.exe"          abgen.exe
stage "$dist_src/bin/abgen.bin"      abgen
stage "$nat_src/abgen-host"          abgen-host
stage "$nat_src/abgen-host.exe"      abgen-host.exe
stage "$nat_src/bin/abgen-host.bin"  abgen-host
for lib in libabgen.so libabgen.dylib abgen.dll; do
  stage "$nat_src/lib/$lib" "$lib"
done

files=("$vdir"/*)
[ "${#files[@]}" -ge 3 ] || {
  echo "promote: only ${#files[@]} verifiable binaries for $TARGET; refusing" >&2
  exit 1
}
ABGEN_VERIFY_SUBSET=1 bash "$HERE/verify-artifact-hashes.sh" "$TARGET" "${files[@]}"

dist="abgen-${REF}-${TARGET}"
nat="abgen-native-${REF}-${TARGET}"
mv "$dist_src" "$work/$dist"
mv "$nat_src" "$work/$nat"
(
  cd "$work"
  ABGEN_RELEASE="$REF" bash "$ROOT/ci/write-build-info.sh" "$dist" "$TARGET"
  ABGEN_RELEASE="$REF" bash "$ROOT/ci/write-build-info.sh" "$nat" "$TARGET"
  bash "$ROOT/ci/pack-archive.sh" "$dist" "$dist.tar.gz" "$SOURCE_DATE_EPOCH"
  bash "$ROOT/ci/pack-archive.sh" "$nat" "$nat.tar.gz" "$SOURCE_DATE_EPOCH"
  shasum -a 256 "$dist.tar.gz" > "$dist.tar.gz.sha256"
  shasum -a 256 "$nat.tar.gz" > "$nat.tar.gz.sha256"
)
mv "$work/$dist.tar.gz" "$work/$dist.tar.gz.sha256" \
   "$work/$nat.tar.gz" "$work/$nat.tar.gz.sha256" "$OUT/"

if [ "$TARGET" = "x86_64-unknown-linux-gnu" ] && [ "$(uname -m)" = "x86_64" ]; then
  rm -rf "$work/smoke" && mkdir "$work/smoke"
  tar -xzf "$OUT/$dist.tar.gz" -C "$work/smoke"
  v="$("$work/smoke/$dist/abgen" --version)"
  echo "promote smoke: abgen --version -> $v"
  [ -n "$v" ]
fi

echo "promoted $TARGET -> $OUT/$dist.tar.gz + $OUT/$nat.tar.gz"
