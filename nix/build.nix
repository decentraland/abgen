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

  # The vendored C codecs are path deps, which mkDummySrc stubs — so they
  # used to recompile inside every workspace rebuild (a one-line .rs change
  # re-ran the whole cmake draco build). Keeping their real sources in the
  # dummy tree moves them into cargoArtifacts: compiled once per
  # lockfile/toolchain/third_party change, cached across every source push.
  #
  # Copied via its own content-addressed store path: interpolating
  # ${buildSource} here would make the dummy source — and with it the whole
  # deps closure — depend on every source file. crane's dummy source
  # deliberately depends on manifests alone.
  thirdPartySrc = builtins.path {
    name = "abgen-third-party";
    path = ../crate/third_party;
  };

  # Version-invariance: a release bump edits only version strings, which no
  # dependency's bytes depend on — but crane stubs the manifests at EVAL
  # time, so the version must be normalized before crane sees it (a sed
  # inside the dummy derivation cannot change the derivation's own hash).
  # The stub filter drops both version attrs (cargo >= 1.75 defaults a
  # missing package.version to 0.0.0) and the lock is normalized to match.
  # The real build keeps the real version.
  workspaceMemberNames = [ "abgen" "abgen-native" "abgen-lambda" "dcl-contents" ];
  normalizedCargoLock = builtins.toFile "Cargo.lock" (
    builtins.replaceStrings
      (map (n: "name = \"${n}\"\nversion = \"${repoVersion}\"") workspaceMemberNames)
      (map (n: "name = \"${n}\"\nversion = \"0.0.0\"") workspaceMemberNames)
      (builtins.readFile ../Cargo.lock)
  );

  # mkDummySrc's internal filter keeps the real Cargo.lock as a drv input
  # (the normalized cargoLock only overwrites its content), so the bumped
  # lock would re-key the deps closure anyway. Strip every lock from what
  # it sees; the name must stay "source" — crane keys the dummy unpack dir
  # on it, and a different name breaks cross-phase cargo fingerprints.
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

  # doCheck = true so dev-deps get real codegen; the checks reuse this
  # single closure instead of rebuilding deps per consumer set.
  # version pinned: the deps drv name must not re-key on release bumps.
  cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
    inherit dummySrc;
    version = "0";
    doCheck = true;
  });

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
