{ pkgs, crane, rust-overlay, buildSource, repoVersion }:

let
  rustBin = (pkgs.extend rust-overlay.overlays.default).rust-bin;
  wasmToolchain = rustBin.stable."1.97.0".default.override {
    targets = [ "wasm32-unknown-unknown" ];
  };
  craneWasm = (crane.mkLib pkgs).overrideToolchain wasmToolchain;

  wasi = pkgs.pkgsCross.wasi32;
  wasiCC = wasi.stdenv.cc;
  wasiPrefix = wasiCC.targetPrefix;
  wasiTriple = pkgs.lib.removeSuffix "-" wasiPrefix;

  commonArgs = {
    pname = "abgen-wasm-check";
    version = repoVersion;
    src = buildSource;
    cargoLock = ../crate/abgen-wasm/Cargo.lock;
    cargoToml = ../crate/abgen-wasm/Cargo.toml;
    postUnpack = ''
      cd $sourceRoot/crate/abgen-wasm
      sourceRoot="."
    '';
    cargoExtraArgs = "";
    nativeBuildInputs = with pkgs; [ cmake pkg-config git ];
    doCheck = false;
    env = {
      CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
      RUSTFLAGS = "-C target-feature=+simd128";
      CC_wasm32_unknown_unknown = "${wasiCC}/bin/${wasiPrefix}cc";
      CXX_wasm32_unknown_unknown = "${wasiCC}/bin/${wasiPrefix}c++";
      AR_wasm32_unknown_unknown = "${wasiCC.bintools}/bin/${wasiPrefix}ar";
      CFLAGS_wasm32_unknown_unknown = "--target=${wasiTriple}";
      CXXFLAGS_wasm32_unknown_unknown = "--target=${wasiTriple}";
    };
    preBuild = ''
      WASI_LIBC_LIB="$(dirname "$(find ${wasiCC.libc} -name libc.a -print -quit)")"
      WASI_LIBCXX_LIB="$(dirname "$(find ${wasi.llvmPackages.libcxx} -name 'libc++.a' -print -quit)")"
      [ -n "$WASI_LIBC_LIB" ] && [ -n "$WASI_LIBCXX_LIB" ] || {
        echo "wasi libc/libc++ archives not found" >&2; exit 1
      }
      export WASI_LIBC_LIB WASI_LIBCXX_LIB
    '';
  };

  cargoArtifacts = craneWasm.buildDepsOnly (commonArgs // {
    buildPhaseCargoCommand = "cargo check --locked --release";
  });
in
craneWasm.cargoBuild (commonArgs // {
  inherit cargoArtifacts;
  cargoBuildCommand = "cargo check --locked --release";
})
