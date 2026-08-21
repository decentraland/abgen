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
  # The zero-test guard lives INSIDE the derivation: a green output with
  # zero tests cannot exist, so neither the binary cache nor a verdict
  # artifact can ever memoize that state (the doCheck=false class went
  # unnoticed for weeks when this was a post-hoc log grep).
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
    # The crane dependency closure as a first-class attr: the pipeline's
    # deps stage builds exactly this, then publishes the binary cache, so a
    # failure in any later stage never costs the next run its warm deps.
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

  # server-off config. The lambda binary ships and runs on aarch64 only, so
  # its test lane follows the prod arch; the config stays covered fleet-wide
  # (aarch64-linux in CI, darwin locally) without a second server-off
  # workspace compile+test on the slower x86 lane. Folding it into nextest
  # is rejected: feature unification differs, a merged compile would not
  # test the no-server config the mac/windows legs ship.
  lambdaTests = {
    # the pipeline's deps stage builds it, then publishes the binary cache.
    lambda-deps = lambdaCargoArtifacts;

    # lambdaCargoArtifacts, not the workspace closure: the `-p` selection
    # resolves shared deps to narrower feature sets, so workspace
    # artifacts never matched and every run recompiled the deps here.
    # nextest (not cargo test): same selection and features, plus the
    # junit report the in-drv guard asserts on.
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
# Arch-independent checks ride the aarch64 lane: those runners are ~1.8x
# faster per stage, which rebalances the two pipelines' critical paths.
archDependent
// lib.optionalAttrs (system != "x86_64-linux") lambdaTests
// lib.optionalAttrs (system == "aarch64-linux") archIndependent
