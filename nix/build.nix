# Cargo build derivations. This file is part of the buildId hash set;
# nix/checks.nix is not, so check edits never move buildId.
{ pkgs, craneLib, buildSource, buildEnv, sourceDateEpoch, repoVersion }:

let
  commonArgs = {
    pname = "abgen";
    version = repoVersion;
    src = buildSource;
    nativeBuildInputs = with pkgs; [ cmake pkg-config git ];
    doCheck = false;
    env.SOURCE_DATE_EPOCH = sourceDateEpoch;
  };

  # doCheck = true so dev-deps get real codegen; the checks reuse this
  # single closure instead of rebuilding deps per consumer set.
  cargoArtifacts = craneLib.buildDepsOnly (commonArgs // { doCheck = true; });

  abgenAll = craneLib.buildPackage (commonArgs // {
    inherit cargoArtifacts;
    pname = "abgen-all";
    env = buildEnv;
    cargoExtraArgs = "--locked -p abgen -p abgen-native -p abgen-lambda";
  });

  # Zero-cost selectors preserving the historical per-package layouts
  # that release.yml and the images stage from.
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
  inherit commonArgs cargoArtifacts abgenAll abgenPkg abgenConsumersPkg
    abgenCorpusPkg;
}
