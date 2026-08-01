#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
plugins="$here/Packages/org.decentraland.abgen/Runtime/Plugins"

host_target() {
    case "$(uname -s)/$(uname -m)" in
    Darwin/arm64)  echo aarch64-apple-darwin ;;
    Darwin/x86_64) echo x86_64-apple-darwin ;;
    Linux/aarch64) echo aarch64-unknown-linux-gnu ;;
    Linux/x86_64)  echo x86_64-unknown-linux-gnu ;;
    MINGW*|MSYS*|CYGWIN*) echo x86_64-pc-windows-gnu ;;
    *) echo "unsupported host: $(uname -s)/$(uname -m)" >&2; exit 1 ;;
    esac
}

target="${1:-$(host_target)}"

case "$target" in
*-apple-darwin)   built=libabgen.dylib; dest="$plugins/macOS" ;;
*-windows-*)      built=abgen.dll;      dest="$plugins/Windows/x86_64" ;;
*-linux-*)        built=libabgen.so;    dest="$plugins/Linux/x86_64" ;;
*) echo "don't know where to deploy $target" >&2; exit 1 ;;
esac

echo "building abgen-native for $target"
cargo build --release --locked --target "$target" \
    --manifest-path "$root/Cargo.toml" -p abgen-native

src="$root/target/$target/release/$built"
[ -f "$src" ] || { echo "expected $src, not found" >&2; exit 1; }

mkdir -p "$dest"
cp "$src" "$dest/"

if [[ "$target" == *-apple-darwin ]]; then
    codesign -f -s - "$dest/$built"
fi

echo "deployed: $dest/$built ($(du -h "$dest/$built" | cut -f1))"

case "$(uname -s)" in
Darwin) otool -L "$dest/$built" | sed -n '2,$p' | grep -v '/usr/lib/\|/System/' && \
            echo "WARNING: unexpected non-system dylib dependency above" || \
            echo "link check: system libraries only" ;;
Linux)  if command -v ldd >/dev/null; then
            if ldd "$dest/$built" | grep -qv 'linux-vdso\|libc\.\|libm\.\|libgcc_s\|libdl\|libpthread\|librt\|ld-linux\|=>.*ld\.so'; then
                echo "link check: see 'ldd $dest/$built' for the full list"
            fi
            echo "link check: $(ldd "$dest/$built" | wc -l) shared dependencies"
        fi ;;
esac
