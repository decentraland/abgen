#!/usr/bin/env bash
set -euo pipefail

real="${ABGEN_REAL_DLLTOOL:-x86_64-w64-mingw32-dlltool}"
stable_root="${ABGEN_DLLTOOL_DIR:-/tmp/abgen-stable-dlltool}"

out=""
out_idx=-1
def=""
def_idx=-1
args=("$@")
for i in "${!args[@]}"; do
  case "${args[i]}" in
    -l|--output-lib) out_idx=$((i + 1)); out="${args[out_idx]-}" ;;
    -d|--input-def)  def_idx=$((i + 1)); def="${args[def_idx]-}" ;;
  esac
done

if [ -z "$out" ] || [ -z "$def" ] || [ ! -f "$def" ]; then
  exec "$real" "$@"
fi

key="$(
  {
    cat "$def"
    printf '\0'
    for i in "${!args[@]}"; do
      if [ "$i" -eq "$out_idx" ] || [ "$i" -eq "$def_idx" ]; then continue; fi
      printf '%s\0' "${args[i]}"
    done
  } | sha256sum | cut -c1-16
)"

dir="$stable_root/$key"
mkdir -p "$dir"
args[out_idx]="$dir/$(basename "$out")"

exec 9>"$dir/.lock"
flock 9
"$real" "${args[@]}"
cp -f "${args[out_idx]}" "$out"
