#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
glb="${1:-$root/crate/abgen-wasm/test/fixtures/normal-quad.glb}"

[ -f "$glb" ] || { echo "no such glb: $glb" >&2; exit 2; }

case "$(uname -s)" in
Darwin) libname=libabgen.dylib ;;
MINGW*|MSYS*|CYGWIN*) libname=abgen.dll ;;
*) libname=libabgen.so ;;
esac

echo "building $libname"
cargo build --release -p abgen-native --manifest-path "$root/Cargo.toml"

lib="$root/target/release/$libname"
[ -f "$lib" ] || { echo "missing $lib" >&2; exit 1; }

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT

echo "compiling sanity.c"
cc -std=c11 -Wall -Wextra -Werror \
    -I "$here/../include" \
    "$here/sanity.c" \
    -o "$out/sanity" \
    "$lib" \
    -Wl,-rpath,"$root/target/release"

echo "running"
"$out/sanity" "$glb"
