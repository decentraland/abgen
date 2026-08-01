#!/usr/bin/env bash
set -euo pipefail

MAX="${1:?usage: check-glibc-floor.sh <max-glibc> <file>...}"
shift
[ "$#" -gt 0 ] || { echo "check-glibc-floor: no files given" >&2; exit 1; }

newest() { printf '%s\n' "$@" | sort -uV | tail -1; }

fail=0
for f in "$@"; do
  [ -f "$f" ] || { echo "check-glibc-floor: no such file: $f" >&2; exit 1; }
  case "$f" in
    *.a)
      tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
      nm -u "$f" 2>/dev/null | awk '$NF ~ /^__isoc23_/ {print $NF}' | sort -u > "$tmp/need"
      nm --defined-only "$f" 2>/dev/null | awk '$NF ~ /^__isoc23_/ {print $NF}' | sort -u > "$tmp/have"
      comm -23 "$tmp/need" "$tmp/have" > "$tmp/missing"
      if [ -s "$tmp/missing" ]; then
        echo "FAIL $f: __isoc23_* referenced but not defined in-archive:" >&2
        sed 's/^/  /' "$tmp/missing" >&2
        echo "  a consumer on glibc < 2.38 cannot link this archive." >&2
        fail=1
      else
        echo "ok   $f: all $(wc -l < "$tmp/need") __isoc23_* reference(s) resolve in-archive"
      fi
      rm -rf "$tmp"; trap - EXIT
      ;;
    *)
      ver=$(readelf -V "$f")
      if printf '%s' "$ver" | grep -q 'GLIBC_ABI_DT_RELR'; then
        echo "FAIL $f: GLIBC_ABI_DT_RELR verneed (DT_RELR packed relocs) implies glibc >= 2.36" >&2
        fail=1
      fi
      worst=$(printf '%s' "$ver" | sed -n 's/.*Name: GLIBC_\([0-9.]*\).*/\1/p' | sort -uV | tail -1)
      if [ -z "$worst" ]; then
        echo "FAIL $f: no GLIBC_* verneed entries — not a glibc-linked ELF?" >&2
        fail=1
      elif [ "$(newest "$worst" "$MAX")" != "$MAX" ]; then
        echo "FAIL $f: requires GLIBC_$worst, floor is GLIBC_$MAX. Offending symbols:" >&2
        readelf --dyn-syms -W "$f" \
          | awk -v v="@GLIBC_$worst" '$NF ~ /^\(/ && index($(NF-1), v) {print $(NF-1)}' \
          | sort -u | sed 's/^/  /' >&2 || true
        fail=1
      else
        echo "ok   $f: max GLIBC_$worst <= GLIBC_$MAX"
      fi
      ;;
  esac
done
exit "$fail"
