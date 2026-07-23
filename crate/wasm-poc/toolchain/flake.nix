{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system:
        f (import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        }));
    in {
      devShells = forAllSystems (pkgs:
        let
          rust = pkgs.rust-bin.stable."1.97.0".default.override {
            targets = [ "wasm32-unknown-unknown" ];
          };
          wasi = pkgs.pkgsCross.wasi32;
          wasiCC = wasi.stdenv.cc;
        in {
          default = pkgs.mkShell {
            nativeBuildInputs = [
              rust pkgs.git pkgs.binaryen pkgs.python3 pkgs.cmake
              pkgs.nodejs pkgs.draco pkgs.wasm-bindgen-cli
            ];
            env = {
              CC_wasm32_unknown_unknown = "${wasiCC}/bin/wasm32-unknown-wasi-cc";
              CXX_wasm32_unknown_unknown = "${wasiCC}/bin/wasm32-unknown-wasi-c++";
              AR_wasm32_unknown_unknown = "${wasiCC}/bin/wasm32-unknown-wasi-ar";
              CFLAGS_wasm32_unknown_unknown = "--target=wasm32-unknown-wasi";
              CXXFLAGS_wasm32_unknown_unknown = "--target=wasm32-unknown-wasi";
              WASI_LIBC_LIB = "${wasiCC.libc}/lib";
              WASI_LIBCXX_LIB = "${wasi.llvmPackages.libcxx}/lib";
            };
          };
        });
    };
}
