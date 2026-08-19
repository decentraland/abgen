# wasm32 flake check. Like nix/checks.nix, outside the buildId hash set:
# check edits must never move buildId. Toolchain pins mirror
# crate/abgen-wasm/toolchain/flake.nix, which stays the local-dev entry
# point.
{ pkgs, crane, rust-overlay, buildSource, repoVersion }:

let
  rustBin = (pkgs.extend rust-overlay.overlays.default).rust-bin;
  wasmToolchain = rustBin.stable."1.97.0".default.override {
    targets = [ "wasm32-unknown-unknown" ];
  };
  craneWasm = (crane.mkLib pkgs).overrideToolchain wasmToolchain;

  wasi = pkgs.pkgsCross.wasi32;
  wasiCC = wasi.stdenv.cc;

  commonArgs = {
    pname = "abgen-wasm-check";
    version = repoVersion;
    # abgen-wasm is workspace-excluded with its own lockfile; the path dep
    # abgen = { path = ".." } needs the surrounding repo tree, so keep the
    # full build source and re-root cargo at the crate.
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
      # The vendored C deps (libjpeg9c/crunch/draco) compile against wasi
      # headers even for the unknown-unknown target; abgen-wasm's build.rs
      # links the wasi static libs via WASI_*_LIB.
      CC_wasm32_unknown_unknown = "${wasiCC}/bin/wasm32-unknown-wasi-cc";
      CXX_wasm32_unknown_unknown = "${wasiCC}/bin/wasm32-unknown-wasi-c++";
      AR_wasm32_unknown_unknown = "${wasiCC}/bin/wasm32-unknown-wasi-ar";
      CFLAGS_wasm32_unknown_unknown = "--target=wasm32-unknown-wasi";
      CXXFLAGS_wasm32_unknown_unknown = "--target=wasm32-unknown-wasi";
      WASI_LIBC_LIB = "${wasiCC.libc}/lib";
      WASI_LIBCXX_LIB = "${wasi.llvmPackages.libcxx}/lib";
    };
  };

  cargoArtifacts = craneWasm.buildDepsOnly (commonArgs // {
    buildPhaseCargoCommand = "cargo check --locked --release";
  });
in
craneWasm.cargoBuild (commonArgs // {
  inherit cargoArtifacts;
  cargoBuildCommand = "cargo check --locked --release";
})
