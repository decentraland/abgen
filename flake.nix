{
  description = "abgen — standalone Decentraland asset-bundle converter + ab-cdn JIT server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # cargo dep-split: dependency crates compile in their own derivation keyed
    # on the manifests/lockfile, so releases only recompile the abgen crate.
    # Release-tag pin + narHash in flake.lock; audited 2026-07-22 (pure nix
    # lib, ~3k lines, only Cargo.lock-checksum-pinned fixed-output fetches).
    crane.url = "github:ipetkov/crane/v0.23.4";
  };

  outputs = { self, nixpkgs, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];

      perSystem = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          craneLib = crane.mkLib pkgs;

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
          gitCommit = if self ? rev then builtins.substring 0 12 self.rev else "unknown";

          commonArgs = {
            pname = "abgen";
            version = crateVersion;
            src = self;
            nativeBuildInputs = with pkgs; [ cmake pkg-config git ];
            doCheck = false;
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          abgenPkg = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            # final derivation only: on cargoArtifacts this would defeat
            # commit-to-commit dep caching
            env.ABGEN_GIT_COMMIT = gitCommit;
            cargoExtraArgs = "--bin abgen";
          });
        in
        {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = nativeDeps;

            buildInputs = [ pkgs.libjpeg_turbo ];
            shellHook = ''
              export TURBOJPEG_LIB=${pkgs.libjpeg_turbo.out}/lib/libturbojpeg${sharedLibExt}
            '';
          };

          packages.default = abgenPkg;

          packages.abgen-corpus = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            pname = "abgen-corpus";
            env.ABGEN_GIT_COMMIT = gitCommit;
            cargoExtraArgs = "--bin abgen-corpus";
          });

          packages.dockerImage =
            let
              runtimeData = pkgs.runCommand "abgen-runtime" { } ''
                mkdir -p $out/opt/abgen
                cp -r ${self}/template $out/opt/abgen/template
                cp -r ${self}/crate/shader $out/opt/abgen/shader
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
              env.ABGEN_GIT_COMMIT = gitCommit;
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
