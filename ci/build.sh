#!/usr/bin/env bash
set -euo pipefail

TARGET="${1:?usage: build.sh <target-triple>}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

export SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-315532800}"
export ABGEN_GIT_REV="${ABGEN_GIT_REV:-$(git rev-parse HEAD)}"
DIST_DIR="${ABGEN_DIST:-$ROOT/dist}"
RUST_PIN="$(perl -ne 'print $1 if /^channel = "([^"]+)"/' rust-toolchain.toml)"
[ -n "$RUST_PIN" ] || { echo "cannot read channel from rust-toolchain.toml" >&2; exit 1; }

case "$TARGET" in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu) BUILDER=nix ;;
  aarch64-apple-darwin | x86_64-apple-darwin | \
  x86_64-pc-windows-gnu | aarch64-pc-windows-gnullvm) BUILDER=cargo ;;
  *) echo "build.sh: unknown target $TARGET" >&2; exit 1 ;;
esac

nixf() { nix --extra-experimental-features 'nix-command flakes' "$@"; }

if [ -z "${ABGEN_BUILD_ID:-}" ]; then
  case "$BUILDER" in
    nix) ABGEN_BUILD_ID="$(nixf eval --raw .#nixId)" ;;
    *)   ABGEN_BUILD_ID="$(nixf eval --raw .#srcId)" ;;
  esac
fi
case "$ABGEN_BUILD_ID" in
  *[!0-9a-f]*) echo "build id is not lowercase hex: $ABGEN_BUILD_ID" >&2; exit 1 ;;
esac
[ "${#ABGEN_BUILD_ID}" -eq 12 ] \
  || { echo "build id must be 12 chars: $ABGEN_BUILD_ID" >&2; exit 1; }
export ABGEN_BUILD_ID

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p; /^version = "/q' Cargo.toml)"
[ -n "$VERSION" ] || { echo "no workspace version in Cargo.toml" >&2; exit 1; }

echo "target: $TARGET  builder: $BUILDER  version: $VERSION"
echo "build id: $ABGEN_BUILD_ID  rev: $ABGEN_GIT_REV  epoch: $SOURCE_DATE_EPOCH"

pack() { # dir out.tar.gz — deterministic bytes: sorted, epoch-stamped, gzip -n
  local tar=tar
  command -v gtar >/dev/null && tar=gtar
  "$tar" --sort=name --version >/dev/null 2>&1 || {
    echo "GNU tar with --sort is required for reproducible archives" >&2
    exit 1
  }
  local tmp="$2.partial.$$"
  "$tar" --sort=name --owner=0 --group=0 --numeric-owner \
    --mtime="@${SOURCE_DATE_EPOCH}" -cf - "$1" | gzip -n -9 > "$tmp"
  mv -f "$tmp" "$2"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum -- "$1" | cut -d' ' -f1
  else shasum -a 256 -- "$1" | cut -d' ' -f1; fi
}

build_info() { # name — provenance rides BESIDE the archive, never inside:
  {
    printf 'ABGEN_VERSION=%s\n' "$VERSION"
    printf 'ABGEN_TARGET=%s\n' "$TARGET"
    printf 'ABGEN_BUILD_ID=%s\n' "$ABGEN_BUILD_ID"
    printf 'ABGEN_GIT_REV=%s\n' "$ABGEN_GIT_REV"
    printf 'SOURCE_DATE_EPOCH=%s\n' "$SOURCE_DATE_EPOCH"
  } > "$1.BUILD-INFO.txt"
}

finish_archive() { # dir — build-info sidecar, pack, assert repro
  build_info "$1"
  pack "$1" "$1.tar.gz"
  local repack="$1.repack.tar.gz"
  pack "$1" "$repack"
  local a b
  a="$(sha256_of "$1.tar.gz")"; b="$(sha256_of "$repack")"; rm -f "$repack"
  [ "$a" = "$b" ] || {
    echo "NOT REPRODUCIBLE: $1.tar.gz repacked to different bytes" >&2
    exit 1
  }
  shasum -a 256 "$1.tar.gz" > "$1.tar.gz.sha256"
  echo "packed $1.tar.gz ($a)"
}

if [ "$BUILDER" = cargo ]; then
  # fixed /build,/home mirror the nix sandbox; the prefix-map ORDER is load-bearing ($PWD nests inside $HOME, last match wins)
  RUSTFLAGS="--remap-path-prefix $PWD=/build --remap-path-prefix $HOME=/home"
  export CFLAGS="-ffile-prefix-map=$HOME=/home -ffile-prefix-map=$PWD=/build"
  export CXXFLAGS="-ffile-prefix-map=$HOME=/home -ffile-prefix-map=$PWD=/build"
  export CARGO_INCREMENTAL=0

  if ! command -v rustup >/dev/null 2>&1 && [ "${CI:-}" = "true" ]; then
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain none --profile minimal --no-modify-path
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
  if command -v rustup >/dev/null 2>&1; then
    rustup toolchain install "$RUST_PIN" --profile minimal --no-self-update
    rustup default "$RUST_PIN"
    rustup target add "$TARGET"
  else
    have="$(rustc --version | awk '{print $2}')"
    [ "$have" = "$RUST_PIN" ] || {
      echo "rustc on PATH is $have, the pin is $RUST_PIN (no rustup to fix it)" >&2
      exit 1
    }
  fi

  if [ "$TARGET" = x86_64-pc-windows-gnu ]; then
    if ! command -v x86_64-w64-mingw32-g++-posix >/dev/null 2>&1; then
      sudo apt-get update
      sudo apt-get install -y --no-install-recommends \
        g++-mingw-w64-x86-64-posix cmake
    fi
    # a dir with ONLY libstdc++.a: -lstdc++ must never resolve to an import library; these bytes ship inside abgen.dll
    STDCXX_A="$(x86_64-w64-mingw32-g++-posix -print-file-name=libstdc++.a)"
    case "$STDCXX_A" in
      /*) ;;
      *) echo "libstdc++.a unresolved: $STDCXX_A" >&2; exit 1 ;;
    esac
    sha256sum "$STDCXX_A"
    sudo mkdir -p /opt/mingw-static-cxx
    sudo cp "$STDCXX_A" /opt/mingw-static-cxx/libstdc++.a
    cat > /tmp/mingw-toolchain.cmake <<'EOF'
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER x86_64-w64-mingw32-gcc-posix)
set(CMAKE_CXX_COMPILER x86_64-w64-mingw32-g++-posix)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
EOF
    export CMAKE_TOOLCHAIN_FILE=/tmp/mingw-toolchain.cmake
    export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc-posix
    export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc-posix
    export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++-posix
    export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
    export ABGEN_REAL_DLLTOOL=x86_64-w64-mingw32-dlltool
    RUSTFLAGS="$RUSTFLAGS -C link-arg=-Wl,--no-insert-timestamp"
    RUSTFLAGS="$RUSTFLAGS -C target-feature=+crt-static -C link-arg=-static-libgcc"
    RUSTFLAGS="$RUSTFLAGS -L native=/opt/mingw-static-cxx"
    # raw dlltool bakes its per-invocation tmp path into import-lib symbols — the wrapper makes it content-addressed
    RUSTFLAGS="$RUSTFLAGS -C dlltool=$PWD/ci/stable-dlltool.sh"
  fi

  if [ "$TARGET" = aarch64-pc-windows-gnullvm ]; then
    LLVM_MINGW=llvm-mingw-20260616-ucrt-ubuntu-22.04-x86_64
    LLVM_MINGW_SHA=534b92e067b22a6b4441f48ae9240a3341b17825d04d577eab0cf85c44b4deda
    if [ ! -d "/opt/$LLVM_MINGW" ]; then
      curl -fsSL -o /tmp/llvm-mingw.tar.xz \
        "https://github.com/mstorsjo/llvm-mingw/releases/download/20260616/$LLVM_MINGW.tar.xz"
      echo "$LLVM_MINGW_SHA  /tmp/llvm-mingw.tar.xz" | sha256sum -c
      sudo tar -xJf /tmp/llvm-mingw.tar.xz -C /opt && rm /tmp/llvm-mingw.tar.xz
      sudo rm "/opt/$LLVM_MINGW/aarch64-w64-mingw32/lib/libc++.dll.a" \
              "/opt/$LLVM_MINGW/aarch64-w64-mingw32/lib/libunwind.dll.a"
    fi
    export PATH="/opt/$LLVM_MINGW/bin:$PATH"
    cat > /tmp/aarch64-mingw-toolchain.cmake <<'EOF'
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR aarch64)
set(CMAKE_C_COMPILER aarch64-w64-mingw32-clang)
set(CMAKE_CXX_COMPILER aarch64-w64-mingw32-clang++)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
EOF
    export CMAKE_TOOLCHAIN_FILE=/tmp/aarch64-mingw-toolchain.cmake
    export CARGO_TARGET_AARCH64_PC_WINDOWS_GNULLVM_LINKER=aarch64-w64-mingw32-clang
    export CC_aarch64_pc_windows_gnullvm=aarch64-w64-mingw32-clang
    export CXX_aarch64_pc_windows_gnullvm=aarch64-w64-mingw32-clang++
    export AR_aarch64_pc_windows_gnullvm=aarch64-w64-mingw32-ar
    export BINDGEN_EXTRA_CLANG_ARGS="--target=aarch64-w64-mingw32 --sysroot=/opt/$LLVM_MINGW/aarch64-w64-mingw32"
    RUSTFLAGS="$RUSTFLAGS -C target-feature=+crt-static -C link-arg=-Wl,--no-insert-timestamp"
  fi

  export RUSTFLAGS
fi

REL="target/$TARGET/release"

if [ "$BUILDER" = nix ]; then
  nixf build .#abgen-native \
    || nixf build .#abgen-native --print-build-logs
  mkdir -p "$REL"
  install -m755 result/bin/abgen "$REL/abgen"
  install -m755 result/bin/abgen-host "$REL/abgen-host"
  install -m644 result/lib/libabgen.so "$REL/libabgen.so"
  install -m644 result/lib/libabgen.a "$REL/libabgen.a"
else
  cargo build --release --locked --target "$TARGET" --bin abgen
  cargo build --release --locked --target "$TARGET" -p abgen-native
fi

if [ "$TARGET" = x86_64-pc-windows-gnu ]; then
  rc=0
  for f in abgen.dll abgen.exe abgen-host.exe; do
    imports="$(x86_64-w64-mingw32-objdump -p "$REL/$f" \
      | sed -n 's/^\tDLL Name: //p' | sort -u)"
    echo "$f imports:"; echo "$imports" | sed 's/^/  /'
    if echo "$imports" | grep -qiE '^(libstdc\+\+-6|libgcc_s_seh-1|libwinpthread-1)\.dll$'; then
      echo "$f imports the MinGW runtime; not self-contained" >&2
      rc=1
    fi
  done
  [ "$rc" -eq 0 ] || exit 1
  echo "windows-gnu artifacts are self-contained"
fi

mkdir -p "$DIST_DIR"
BIN=abgen
case "$TARGET" in *windows*) BIN=abgen.exe ;; esac

case "$TARGET" in
  x86_64-*)  WANT_ARCH=x86_64 ;;
  aarch64-*) WANT_ARCH=arm64 ;;
esac
RUN_ARCH="$(uname -m)"
[ "$RUN_ARCH" = aarch64 ] && RUN_ARCH=arm64
CAN_SMOKE=false
case "$TARGET" in
  *windows*) ;;  # cross builds; execution coverage is ci.yml's windows job
  *) [ "$RUN_ARCH" = "$WANT_ARCH" ] && CAN_SMOKE=true ;;
esac

dist="abgen-$VERSION-$TARGET"
rm -rf "$dist"
mkdir -p "$dist"
cp LICENSE README.md "$dist/"
if [ "$BUILDER" = nix ]; then
  mkdir -p "$dist/bin" "$dist/lib"
  install -m755 result/bin/abgen "$dist/bin/abgen.bin"
  for lib in $(ldd result/bin/abgen | awk '$3 ~ /^\// {print $3}'); do
    install -m644 "$lib" "$dist/lib/"
  done
  interp="$(readelf -l result/bin/abgen | sed -n 's/.*interpreter: \(.*\)]/\1/p')"
  install -m755 "$interp" "$dist/lib/ld.so"
  cat > "$dist/abgen" <<'EOF'
#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
exec "$here/lib/ld.so" --library-path "$here/lib" "$here/bin/abgen.bin" "$@"
EOF
  chmod 755 "$dist/abgen"
else
  cp "$REL/$BIN" "$dist/"
fi
finish_archive "$dist"

if [ "$CAN_SMOKE" = true ]; then
  rm -rf /tmp/abgen-smoke && mkdir /tmp/abgen-smoke
  tar -xzf "$dist.tar.gz" -C /tmp/abgen-smoke
  (
    cd "/tmp/abgen-smoke/$dist"
    ./abgen --version
    HTTP_SERVER_PORT=5199 ./abgen &
    server=$!
    ok=0
    for _ in $(seq 1 40); do
      curl -sf http://127.0.0.1:5199/readyz && ok=1 && break
      sleep 0.5
    done
    kill "$server" 2>/dev/null || true
    test "$ok" = 1
  )
  echo "smoke: abgen --version + /readyz ok"
fi

nat="abgen-native-$VERSION-$TARGET"
rm -rf "$nat"
mkdir -p "$nat/lib" "$nat/include"
cp crate/abgen-native/include/abgen.h "$nat/include/"
cp LICENSE "$nat/"
cp unity/README.md "$nat/README.md"
for f in libabgen.so libabgen.dylib abgen.dll; do
  [ -f "$REL/$f" ] && cp "$REL/$f" "$nat/lib/"
done
test -n "$(ls -A "$nat/lib")" || { echo "no native library built" >&2; exit 1; }

if [ -f "$nat/lib/libabgen.so" ]; then
  # highest verneed = oldest distro the dlopen'd lib loads on
  bash ci/check-glibc-floor.sh 2.34 "$nat/lib/libabgen.so"
fi

if [ "$BUILDER" = nix ]; then
  hb="$REL/abgen-host"
  mkdir -p "$nat/bin" "$nat/host-lib"
  install -m755 "$hb" "$nat/bin/abgen-host.bin"
  for lib in $(ldd "$hb" | awk '$3 ~ /^\// {print $3}'); do
    install -m644 "$lib" "$nat/host-lib/"
  done
  interp="$(readelf -lW "$hb" | sed -n 's/.*interpreter: \(.*\)]/\1/p')"
  install -m755 "$interp" "$nat/host-lib/ld.so"
  cat > "$nat/abgen-host" <<'EOF'
#!/bin/sh
here="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)"
ABGEN_HOST_LOADER="$here/host-lib/ld.so"
ABGEN_HOST_LIBPATH="$here/host-lib"
ABGEN_HOST_BIN="$here/bin/abgen-host.bin"
export ABGEN_HOST_LOADER ABGEN_HOST_LIBPATH ABGEN_HOST_BIN
exec "$ABGEN_HOST_LOADER" --library-path "$ABGEN_HOST_LIBPATH" \
     "$ABGEN_HOST_BIN" "$@"
EOF
  chmod 755 "$nat/abgen-host"
else
  for f in abgen-host abgen-host.exe; do
    [ -f "$REL/$f" ] && install -m755 "$REL/$f" "$nat/"
  done
fi
finish_archive "$nat"

if [ "$CAN_SMOKE" = true ]; then
  rm -rf /tmp/abgen-natsmoke && mkdir /tmp/abgen-natsmoke
  tar -xzf "$nat.tar.gz" -C /tmp/abgen-natsmoke
  got="$("/tmp/abgen-natsmoke/$nat/abgen-host" --version)"
  test -n "$got" || { echo "abgen-host printed no version" >&2; exit 1; }
  echo "native smoke: abgen-host $got"
  set +e
  # empty stdin -> EXIT_PROTOCOL 64 (linux re-exec path); darwin has no per-process rlimit -> EXIT_LIMIT 65; ld.so exit 1 = cap never bound
  "/tmp/abgen-natsmoke/$nat/abgen-host" --max-memory-mb 512 </dev/null
  rc=$?
  set -e
  case "$(uname -s)" in
    Darwin) want=65; what=refusal ;;
    *)      want=64; what=re-exec ;;
  esac
  test "$rc" -eq "$want" \
    || { echo "abgen-host $what is broken: expected $want, got $rc" >&2; exit 1; }
  echo "native smoke: --max-memory-mb $what ok"
  if [ "$BUILDER" = nix ]; then
    bad="$(readelf -lW "/tmp/abgen-natsmoke/$nat/bin/abgen-host.bin" \
          | sed -n 's/.*interpreter: \(.*\)]/\1/p')"
    case "$bad" in
      /nix/store/*) ;;
      *) echo "unexpected interpreter $bad" >&2; exit 1 ;;
    esac
    test -x "/tmp/abgen-natsmoke/$nat/host-lib/ld.so" \
      || { echo "bundled loader missing from the archive" >&2; exit 1; }
    echo "native smoke: bundled loader present"
  fi
fi

if [ "$BUILDER" = nix ] && [ -n "${BUILD_IMAGES:-}" ]; then
  for image in $(echo "$BUILD_IMAGES" | tr ',' ' '); do
    nixf build ".#$image" --out-link image-result \
      || nixf build ".#$image" --out-link image-result --print-build-logs
    cp -L image-result "$DIST_DIR/$image-$TARGET.tar.gz"
    rm -f image-result
    echo "image: $DIST_DIR/$image-$TARGET.tar.gz"

    # boot smoke: the binaries are smoked above, but only running the
    # CONTAINER proves entrypoint/env/layout survive image assembly
    command -v docker > /dev/null || { echo "image smoke: no docker, skipped"; continue; }
    tag="$(docker load < "$DIST_DIR/$image-$TARGET.tar.gz" | sed -n 's/^Loaded image: //p')"
    test -n "$tag" || { echo "image smoke: docker load produced no tag" >&2; exit 1; }
    case "$image" in
      dockerImage)
        cid="$(docker run -d -p 127.0.0.1::5147 "$tag")"
        port="$(docker port "$cid" 5147/tcp | head -n1 | cut -d: -f2)"
        ok=false
        for _ in $(seq 1 30); do
          curl -fsS "http://127.0.0.1:$port/readyz" > /dev/null 2>&1 && { ok=true; break; }
          sleep 1
        done
        [ "$ok" = true ] || docker logs "$cid" | tail -n 40 >&2 || true
        docker rm -f "$cid" > /dev/null
        [ "$ok" = true ] || { echo "image smoke: /readyz never answered" >&2; exit 1; }
        echo "image smoke: $tag serves /readyz"
        ;;
      lambdaImage)
        # no shell in the image and the entrypoint needs the AWS runtime
        # API, so assert the config instead of booting
        ep="$(docker inspect --format '{{join .Config.Entrypoint " "}}' "$tag")"
        case "$ep" in
          */abgen-lambda) echo "image smoke: $tag entrypoint $ep" ;;
          *) echo "image smoke: unexpected lambda entrypoint: $ep" >&2; exit 1 ;;
        esac
        ;;
    esac
  done
fi

mv "$dist.tar.gz" "$dist.tar.gz.sha256" "$dist.BUILD-INFO.txt" \
  "$nat.tar.gz" "$nat.tar.gz.sha256" "$nat.BUILD-INFO.txt" "$DIST_DIR/"
rm -rf "$dist" "$nat"
echo "done: $DIST_DIR"
ls -l "$DIST_DIR"
