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
  inherit commonArgs cargoArtifacts abgenAll abgenPkg abgenConsumersPkg
    abgenCorpusPkg;
}
