{
  pkgs,
  crane,
  rustToolchain,
  nightlyToolchain,
  src,
}:
let
  craneLib = crane.mkLib pkgs;
  craneLibStable = craneLib.overrideToolchain rustToolchain;
  craneLibNightly = craneLib.overrideToolchain nightlyToolchain;
  cargoSrc = pkgs.lib.cleanSourceWith {
    src = src;
    filter =
      path: _type:
      let
        relativePath = pkgs.lib.removePrefix "${toString src}/" (toString path);
      in
      !(
        relativePath == ".git"
        || pkgs.lib.hasPrefix ".git/" relativePath
        || relativePath == "result"
        || pkgs.lib.hasPrefix "result/" relativePath
        || relativePath == "target"
        || pkgs.lib.hasPrefix "target/" relativePath
      );
  };
  commonArgs = {
    src = cargoSrc;
    strictDeps = true;
  };
  cargoArtifacts = craneLibStable.buildDepsOnly commonArgs;
in
{
  cargo-fmt = craneLibNightly.cargoFmt {
    src = cargoSrc;
    cargoExtraArgs = "--all";
  };

  cargo-clippy = craneLibNightly.cargoClippy (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoExtraArgs = "--locked";
      cargoClippyExtraArgs = "--workspace --all-targets --all-features -- -D warnings";
    }
  );

  cargo-build = craneLibStable.cargoBuild (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoExtraArgs = "--locked --workspace --all-targets --all-features";
    }
  );

  cargo-test = craneLibStable.cargoTest (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoExtraArgs = "--locked --workspace --all-targets --all-features";
    }
  );
}
