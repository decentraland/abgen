#!/usr/bin/env bash
set -euo pipefail

DIR="${1:?usage: verify-reproducible.sh <dir> <archive.tar.gz> <source-date-epoch>}"
ARCHIVE="${2:?usage: verify-reproducible.sh <dir> <archive.tar.gz> <source-date-epoch>}"
EPOCH="${3:?usage: verify-reproducible.sh <dir> <archive.tar.gz> <source-date-epoch>}"

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

repack="$(mktemp -d)/repack.tar.gz"
"$HERE/pack-archive.sh" "$DIR" "$repack" "$EPOCH"

a=$(shasum -a 256 "$ARCHIVE" | cut -d' ' -f1)
b=$(shasum -a 256 "$repack"  | cut -d' ' -f1)

if [ "$a" != "$b" ]; then
  echo "NOT REPRODUCIBLE: $ARCHIVE" >&2
  echo "  first pack:  $a" >&2
  echo "  repack:      $b" >&2
  echo "The published SHA256SUMS would not be verifiable by a rebuild." >&2
  exit 1
fi
echo "reproducible packaging: $ARCHIVE ($a)"

leaked=0
for probe in "$PWD" "$HOME"; do
  case "$probe" in ""|"/") continue ;; esac
  while IFS= read -r f; do
    case "$(file -b --mime-type "$f" 2>/dev/null)" in
      application/x-executable|application/x-sharedlib|application/x-pie-executable|application/x-mach-binary|application/x-dosexec|application/vnd.microsoft.portable-executable)
        if LC_ALL=C grep -qF -- "$probe" "$f" 2>/dev/null; then
          echo "path-leak: $(basename "$f") embeds $probe" >&2
          leaked=1
        fi
        ;;
    esac
  done < <(find "$DIR" -type f)
done

if [ "$leaked" -eq 0 ]; then
  echo "path independent: $DIR"
elif [ "${ABGEN_STRICT_PATHS:-0}" = "1" ]; then
  echo "ABGEN_STRICT_PATHS=1 and build paths leaked into a shipped binary." >&2
  exit 1
else
  echo "note: build paths are embedded (known: env!(\"CARGO_MANIFEST_DIR\")" >&2
  echo "      in crate/src, plus a devshell RUNPATH). Packaging is still" >&2
  echo "      deterministic; the binaries are not directory-independent." >&2
fi
