{ lib, system, pkgs, craneLib, commonArgs, cargoArtifacts
, cargoArtifactsCheckfast, lambdaCargoArtifacts, abgenConsumersPkg
, wasmCheck }:

let
  src = commonArgs.src;
  withArtifacts = commonArgs // { inherit cargoArtifacts; };
  withCheckfast = commonArgs // {
    cargoArtifacts = cargoArtifactsCheckfast;
    CARGO_PROFILE = "checkfast";
  };
  abgenRoot = ''export ABGEN_ROOT="$PWD"'';
  # in-drv: a green zero-test output must not exist, so no cache can memoize it
  assertRanTests = ''
    junit=target/nextest/default/junit.xml
    [ -f "$junit" ] || { echo "nextest junit report missing - no tests ran" >&2; exit 1; }
    count=$(grep -o 'tests="[0-9]*"' "$junit" | head -n1 | grep -o '[0-9]*' || echo 0)
    [ "''${count:-0}" -gt 0 ] || { echo "zero tests executed" >&2; exit 1; }
    echo "$count tests executed"
  '';

  archIndependent = {
    wasm-deps = wasmCheck.cargoArtifacts;

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
    deps = cargoArtifacts;
    deps-checkfast = cargoArtifactsCheckfast;

    nextest = craneLib.cargoNextest (withCheckfast // {
      doCheck = true;
      __darwinAllowLocalNetworking = true;
      cargoExtraArgs = "--locked";
      cargoNextestExtraArgs = "--workspace";
      preCheck = abgenRoot;
      postCheck = assertRanTests;
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

  # aarch64-only like prod; folding into nextest rejected: feature unification differs
  lambdaTests = {
    lambda-deps = lambdaCargoArtifacts;

    lambda-tests = craneLib.cargoNextest (commonArgs // {
      cargoArtifacts = lambdaCargoArtifacts;
      CARGO_PROFILE = "checkfast";
      doCheck = true;
      __darwinAllowLocalNetworking = true;
      pname = "abgen-lambda-tests";
      cargoExtraArgs = "--locked";
      cargoNextestExtraArgs = "-p abgen-lambda -p abgen-native";
      preCheck = abgenRoot;
      postCheck = assertRanTests;
    });
  };
in
archDependent
// lib.optionalAttrs (system != "x86_64-linux") lambdaTests
// lib.optionalAttrs (system == "aarch64-linux") archIndependent
