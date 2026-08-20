{ lib, system, pkgs, craneLib, commonArgs, cargoArtifacts, abgenConsumersPkg
, wasmCheck }:

let
  src = commonArgs.src;
  withArtifacts = commonArgs // { inherit cargoArtifacts; };
  abgenRoot = ''export ABGEN_ROOT="$PWD"'';

  archIndependent = {
    fmt = craneLib.cargoFmt {
      inherit (commonArgs) pname version src;
    };

    no-node-spawn = pkgs.runCommand "abgen-no-node-spawn" { } ''
      ! grep -rn 'Command::new("npm\|Command::new("node' ${src}/crate/src
      touch $out
    '';

    clippy = craneLib.cargoClippy (withArtifacts // {
      cargoExtraArgs = "--locked";
      cargoClippyExtraArgs = "--workspace --all-targets -- -D warnings";
    });

    wasm-check = wasmCheck;
  };

  archDependent = {
    nextest = craneLib.cargoNextest (withArtifacts // {
      cargoExtraArgs = "--locked";
      cargoNextestExtraArgs = "--workspace";
      preCheck = abgenRoot;
    });

    lambda-tests = craneLib.cargoTest (withArtifacts // {
      pname = "abgen-lambda-tests";
      cargoExtraArgs = "--locked";
      cargoTestExtraArgs = "-p abgen-lambda -p abgen-native --tests";
      preCheck = abgenRoot;
    });

    native-smoke =
      let
        libName = "libabgen${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}";
      in
      pkgs.runCommandCC "abgen-native-smoke" { } ''
        $CC -std=c11 -Wall -Wextra -Werror \
          -I ${src}/crate/abgen-native/include \
          ${src}/crate/abgen-native/tests/smoke.c \
          -o smoke \
          ${abgenConsumersPkg}/lib/${libName} \
          -Wl,-rpath,${abgenConsumersPkg}/lib
        export ABGEN_ROOT=${src}
        ./smoke ${src}/crate/abgen-wasm/test/fixtures/normal-quad.glb
        touch $out
      '';
  };
in
archDependent // lib.optionalAttrs (system == "x86_64-linux") archIndependent
