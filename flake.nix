{
  description = "abgen — standalone Decentraland asset-bundle converter + ab-cdn JIT server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane/v0.23.4";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay }:
    let
      lib = nixpkgs.lib;
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];

      sourceDateEpoch = "315532800";

      buildFileset = lib.fileset.difference
        (lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./rust-toolchain.toml
          ./.config/nextest.toml
          ./crate
          ./template
          ./lambda/Cargo.toml
          ./lambda/src
        ])
        (lib.fileset.unions [
          ./crate/abgen-node/npm
          (lib.fileset.fileFilter (file: file.hasExt "md") ./crate)
        ]);

      buildSource = lib.fileset.toSource {
        root = ./.;
        fileset = buildFileset;
      };

      # srcId must cover everything that determines shipped bytes: release.yml skips whole legs on it
      srcId = builtins.substring 0 12 (builtins.hashString "sha256"
        (builtins.concatStringsSep "\n" [
          (baseNameOf (builtins.unsafeDiscardStringContext buildSource.outPath))
          (builtins.hashFile "sha256" ./ci/build.sh)
          (builtins.hashFile "sha256" ./ci/napi.sh)
          (builtins.hashFile "sha256" ./ci/stable-dlltool.sh)
          (builtins.hashFile "sha256" ./ci/check-glibc-floor.sh)
          (builtins.hashFile "sha256" ./LICENSE)
          (builtins.hashFile "sha256" ./README.md)
          (builtins.hashFile "sha256" ./unity/README.md)
        ]));

      nixId = builtins.substring 0 12 (builtins.hashString "sha256"
        (builtins.concatStringsSep "\n" [
          srcId
          (builtins.hashFile "sha256" ./flake.lock)
          (builtins.hashFile "sha256" ./flake.nix)
          (builtins.hashFile "sha256" ./nix/build.nix)
        ]));

      buildEnv = {
        ABGEN_BUILD_ID = nixId;
        SOURCE_DATE_EPOCH = sourceDateEpoch;
      };

      rustChannel = (builtins.fromTOML
        (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;

      repoVersion = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

      perSystem = lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          craneLib = crane.mkLib pkgs;

          _toolchainMatches = lib.assertMsg (pkgs.rustc.version == rustChannel) ''
            toolchain mismatch: rust-toolchain.toml pins ${rustChannel} but this
            nixpkgs ships rustc ${pkgs.rustc.version}. The rustup legs would build
            with one compiler and the nix legs with the other. Move flake.lock and
            rust-toolchain.toml together, or pick the nixpkgs rev that carries the
            version you want.
          '';

          nativeDeps = with pkgs; [
            cargo
            rustc
            gnumake
            cmake
            pkg-config
            git

            (python3.withPackages (ps: with ps; [ numpy pillow ]))
          ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ gcc ];
          sharedLibExt = pkgs.stdenv.hostPlatform.extensions.sharedLibrary;

          build = import ./nix/build.nix {
            inherit pkgs craneLib buildSource buildEnv sourceDateEpoch repoVersion;
          };
          inherit (build) abgenPkg abgenConsumersPkg;

          wasmCheck = import ./nix/wasm.nix {
            inherit pkgs crane rust-overlay buildSource repoVersion;
          };

          abgenNativePkg = pkgs.runCommand "abgen-native-${repoVersion}" { } ''
            mkdir -p $out/bin $out/lib
            cp ${abgenPkg}/bin/abgen $out/bin/
            cp ${abgenConsumersPkg}/bin/abgen-host $out/bin/
            cp ${abgenConsumersPkg}/lib/* $out/lib/
          '';

          runtimeData = pkgs.runCommand "abgen-runtime" { } ''
            mkdir -p $out/opt/abgen
            cp -r ${buildSource}/template $out/opt/abgen/template
            cp -r ${buildSource}/crate/shader $out/opt/abgen/shader
          '';
        in
        assert _toolchainMatches;
        {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = nativeDeps;

            buildInputs = [ pkgs.libjpeg_turbo ];
            ABGEN_BUILD_ID = srcId;
            SOURCE_DATE_EPOCH = sourceDateEpoch;
            shellHook = ''
              export TURBOJPEG_LIB=${pkgs.libjpeg_turbo.out}/lib/libturbojpeg${sharedLibExt}
            '';
          };

          packages.default = abgenPkg;

          packages.srcId = srcId;

          packages.nixId = nixId;

          packages.abgen-native = abgenNativePkg;

          packages.abgen-corpus = build.abgenCorpusPkg;

          packages.dockerImage = pkgs.dockerTools.buildLayeredImage {
            name = "abgen";
            tag = repoVersion;
            contents = [ abgenPkg pkgs.tini pkgs.cacert pkgs.libjpeg_turbo runtimeData ];
            fakeRootCommands = ''
              mkdir -p data/out data/cache
              chown -R 10001:10001 data
            '';
            config = {
              Entrypoint = [ "${pkgs.tini}/bin/tini" "-g" "--" "${abgenPkg}/bin/abgen" ];
              Env = [
                "ABGEN_ROOT=/opt/abgen"
                "ABGEN_SHADER_BUNDLE=/opt/abgen/shader/scene_ignore_windows"
                "ABGEN_OUT_ROOT=/data/out"
                "ABGEN_CACHE_DIR=/data/cache"
                "ABGEN_HTTP_HOST=0.0.0.0"
                "ABGEN_LOG_FORMAT=json"
                "TURBOJPEG_LIB=${pkgs.libjpeg_turbo.out}/lib/libturbojpeg${sharedLibExt}"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              ExposedPorts = { "5147/tcp" = { }; };
              User = "10001:10001";
              WorkingDir = "/data";
            };
          };

          packages.lambdaImage = pkgs.dockerTools.buildLayeredImage {
            name = "abgen-lambda";
            tag = repoVersion;
            contents = [ abgenConsumersPkg pkgs.cacert pkgs.libjpeg_turbo runtimeData ];
            config = {
              Entrypoint = [ "${abgenConsumersPkg}/bin/abgen-lambda" ];
              Env = [
                "ABGEN_ROOT=/opt/abgen"
                "ABGEN_CACHE_DIR=/tmp/abgen-cache"
                "OUT_ROOT=/tmp/abgen-out"
                "ALLOWED_CONTENT_SERVER_HOSTS=peer.decentraland.org"
                "TURBOJPEG_LIB=${pkgs.libjpeg_turbo.out}/lib/libturbojpeg${sharedLibExt}"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              User = "10001:10001";
            };
          };

          checks = import ./nix/checks.nix {
            inherit lib system pkgs craneLib wasmCheck;
            inherit (build) commonArgs cargoArtifacts cargoArtifactsCheckfast
              lambdaCargoArtifacts
              abgenConsumersPkg;
          };

        });
    in
    {
      packages = nixpkgs.lib.mapAttrs (_: v: v.packages) perSystem;
      devShells = nixpkgs.lib.mapAttrs (_: v: v.devShells) perSystem;
      checks = nixpkgs.lib.mapAttrs (_: v: v.checks) perSystem;
    };
}
