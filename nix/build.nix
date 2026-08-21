{ pkgs, craneLib, buildSource, buildEnv, sourceDateEpoch, repoVersion }:

let
  commonArgs = {
    pname = "abgen";
    version = repoVersion;
    src = buildSource;
    # Explicit so crane never derives it from `src`: its fallback scans src
    # for Cargo.lock and .cargo/config.toml at EVAL time, and when src is
    # the dummy tree (a derivation, in the deps closures) that scan builds
    # the derivation during evaluation — IFD, which breaks cross-system
    # eval and `nix eval` purity. Vendoring from buildSource is pure.
    cargoVendorDir = craneLib.vendorCargoDeps { src = buildSource; };
    nativeBuildInputs = with pkgs; [ cmake pkg-config git ];
    doCheck = false;
    env.SOURCE_DATE_EPOCH = sourceDateEpoch;
  };

  thirdPartySrc = builtins.path {
    name = "abgen-third-party";
    path = ../crate/third_party;
  };

  workspaceMemberNames = [ "abgen" "abgen-native" "abgen-lambda" "dcl-contents" ];
  normalizedCargoLock = builtins.toFile "Cargo.lock" (
    builtins.replaceStrings
      (map (n: "name = \"${n}\"\nversion = \"${repoVersion}\"") workspaceMemberNames)
      (map (n: "name = \"${n}\"\nversion = \"0.0.0\"") workspaceMemberNames)
      (builtins.readFile ../Cargo.lock)
  );

  dummyInputSrc = builtins.path {
    name = "source";
    path = buildSource;
    filter = path: _type: builtins.baseNameOf path != "Cargo.lock";
  };

  dummySrc = craneLib.mkDummySrc {
    src = dummyInputSrc;
    cargoLock = normalizedCargoLock;
    cleanCargoTomlFilter =
      p:
      craneLib.filters.cargoTomlDefault p
      && p != [ "package" "version" ]
      && p != [ "workspace" "package" "version" ];
    extraDummyScript = ''
      rm -rf $out/crate/third_party
      mkdir -p $out/crate/third_party
      cp --recursive --no-preserve=ownership ${thirdPartySrc}/. -t $out/crate/third_party
      chmod +w -R $out/crate/third_party
    '';
  };

  cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
    inherit dummySrc;
    version = "0";
    doCheck = true;
  });

  # Deps for the checkfast profile (lto off, codegen-units 16): the test
  # checks compile 4.9x faster against these — measured 157s -> 32s per
  # workspace test compile — with byte-identical test verdicts. Shipped
  # artifacts and clippy keep the release-profile deps above.
  cargoArtifactsCheckfast = craneLib.buildDepsOnly (commonArgs // {
    inherit dummySrc;
    pname = "abgen-checkfast";
    version = "0";
    doCheck = true;
    CARGO_PROFILE = "checkfast";
  });

  # lambda-tests selects `-p abgen-lambda -p abgen-native`, and resolver v2
  # unifies features over the selected packages only — ~20 shared deps
  # (serde, syn, tracing, chrono, http, ...) resolve narrower there than in
  # the workspace-wide closure above, which cascades into a near-full dep
  # recompile inside the check on every run. Prime that exact configuration
  # too — under the checkfast profile, which is what lambda-tests compiles
  # with; like the main closure it rotates only with lockfile/toolchain.
  lambdaCargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
    inherit dummySrc;
    pname = "abgen-lambda";
    version = "0";
    doCheck = true;
    CARGO_PROFILE = "checkfast";
    cargoExtraArgs = "--locked -p abgen-lambda -p abgen-native";
  });

  abgenAll = craneLib.buildPackage (commonArgs // {
    inherit cargoArtifacts;
    pname = "abgen-all";
    env = buildEnv;
    cargoExtraArgs = "--locked -p abgen -p abgen-native -p abgen-lambda";
  });

  abgenPkg = pkgs.runCommand "abgen-${repoVersion}" { } ''
    mkdir -p $out/bin
    cp ${abgenAll}/bin/abgen $out/bin/
  '';

  abgenConsumersPkg = pkgs.runCommand "abgen-consumers-${repoVersion}" { } ''
    mkdir -p $out/bin $out/lib
    cp ${abgenAll}/bin/abgen-host ${abgenAll}/bin/abgen-lambda $out/bin/
    cp ${abgenAll}/lib/* $out/lib/
  '';

  abgenCorpusPkg = craneLib.buildPackage (commonArgs // {
    inherit cargoArtifacts;
    pname = "abgen-corpus";
    env = buildEnv;
    cargoExtraArgs = "--locked --bin abgen-corpus";
  });
in
{
  inherit commonArgs cargoArtifacts cargoArtifactsCheckfast
    lambdaCargoArtifacts abgenAll abgenPkg
    abgenConsumersPkg abgenCorpusPkg;
}
