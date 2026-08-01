{
  description = "abgen — standalone Decentraland asset-bundle converter + ab-cdn JIT server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane/v0.23.4";
  };

  outputs = { self, nixpkgs, crane }:
    let
      lib = nixpkgs.lib;
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];

      sourceDateEpoch = "315532800";

      buildFileset = lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./rust-toolchain.toml
        ./crate
        ./template
      ];

      buildSource = lib.fileset.toSource {
        root = ./.;
        fileset = lib.fileset.traceVal buildFileset;
      };

      buildId = builtins.substring 0 12 (builtins.hashString "sha256"
        (builtins.concatStringsSep "\n" [
          (baseNameOf (builtins.unsafeDiscardStringContext buildSource.outPath))
          (builtins.hashFile "sha256" ./rust-toolchain.toml)
          (builtins.hashFile "sha256" ./flake.lock)
          (builtins.hashFile "sha256" ./flake.nix)
        ]));

      buildEnv = {
        ABGEN_BUILD_ID = buildId;
        SOURCE_DATE_EPOCH = sourceDateEpoch;
      };

      rustChannel = (builtins.fromTOML
        (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;

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

          crateVersion = (builtins.fromTOML (builtins.readFile ./crate/Cargo.toml)).package.version;

          commonArgs = {
            pname = "abgen";
            version = crateVersion;
            src = buildSource;
            nativeBuildInputs = with pkgs; [ cmake pkg-config git ];
            doCheck = false;
            env.SOURCE_DATE_EPOCH = sourceDateEpoch;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          abgenPkg = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            env = buildEnv;
            cargoExtraArgs = "--locked --bin abgen";
          });
        in
        assert _toolchainMatches;
        {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = nativeDeps;

            buildInputs = [ pkgs.libjpeg_turbo ];
            ABGEN_BUILD_ID = buildId;
            SOURCE_DATE_EPOCH = sourceDateEpoch;
            shellHook = ''
              export TURBOJPEG_LIB=${pkgs.libjpeg_turbo.out}/lib/libturbojpeg${sharedLibExt}
            '';
          };

          packages.default = abgenPkg;

          packages.buildId = buildId;

          packages.abgen-native = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "abgen-native";
            env = buildEnv;
            buildPhaseCargoCommand = ''
              cargoBuildLog=$(mktemp cargoBuildLogXXXX.json)
              cargoWithProfile build --message-format json-render-diagnostics --locked --bin abgen >>"$cargoBuildLog"
              cargoWithProfile build --message-format json-render-diagnostics --locked --package abgen-native >>"$cargoBuildLog"
            '';
          });

          packages.abgen-corpus = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "abgen-corpus";
            env = buildEnv;
            cargoExtraArgs = "--locked --bin abgen-corpus";
          });

          packages.dockerImage =
            let
              runtimeData = pkgs.runCommand "abgen-runtime" { } ''
                mkdir -p $out/opt/abgen
                cp -r ${buildSource}/template $out/opt/abgen/template
                cp -r ${buildSource}/crate/shader $out/opt/abgen/shader
              '';
            in
            pkgs.dockerTools.buildLayeredImage {
            name = "abgen";
            tag = "0.1.0";
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
                "HTTP_SERVER_HOST=0.0.0.0"
                "ABGEN_LOG_FORMAT=json"
                "TURBOJPEG_LIB=${pkgs.libjpeg_turbo.out}/lib/libturbojpeg${sharedLibExt}"
                "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
              ];
              ExposedPorts = { "5147/tcp" = { }; };
              User = "10001:10001";
              WorkingDir = "/data";
            };
          };

          packages.abgen-compare =
            let
              pyEnv = pkgs.python3.withPackages (ps: with ps; [ numpy pillow ]);
            in
            pkgs.rustPlatform.buildRustPackage {
              pname = "abgen-compare";
              version = crateVersion;
              env = buildEnv;
              src = self;
              cargoLock = {
                lockFile = ./Cargo.lock;
              };
              nativeBuildInputs = with pkgs; [ cmake pkg-config git makeWrapper ];
              cargoBuildFlags = [ "--bins" "--examples" ];

              doCheck = false;
              postInstall = ''
                lib=$out/lib/abgen
                mkdir -p $lib/result/bin $lib/crate
                exdir=$(find target -type d -path '*/release/examples' | head -1)
                for t in objdump texdump matdump texcmp texpng; do
                  if [ -f "$exdir/$t" ]; then
                    install -m755 "$exdir/$t" "$lib/result/bin/$t"
                  else
                    echo "missing example tool: $t" >&2; exit 1
                  fi
                done
                ln -s $out/bin/abgen $lib/result/bin/abgen
                cp -r pipeline site template $lib/
                cp -r crate/shader $lib/crate/
                find $lib -type d -name __pycache__ -prune -exec rm -rf {} +
                makeWrapper ${pyEnv}/bin/python3 $out/bin/abgen-compare \
                  --add-flags "$lib/pipeline/abgen-compare" \
                  --set-default TURBOJPEG_LIB ${pkgs.libjpeg_turbo.out}/lib/libturbojpeg${sharedLibExt}
              '';
            };
        });
    in
    {
      packages = nixpkgs.lib.mapAttrs (_: v: v.packages) perSystem;
      devShells = nixpkgs.lib.mapAttrs (_: v: v.devShells) perSystem;
    };
}
