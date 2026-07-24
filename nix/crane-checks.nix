# Authoritative CI check definitions (run via `nix flake check` / `just ci-nix`).
# The `ci` recipe in the top-level justfile mirrors these for fast local runs —
# keep the cargo flags in sync.
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
    pname = "tokio-otp";
    src = cargoSrc;
    strictDeps = true;
    version = "0.1.0";
  };
  cargoArtifacts = craneLibStable.buildDepsOnly commonArgs;
in
{
  cargo-fmt = craneLibNightly.cargoFmt (
    commonArgs
    // {
      cargoExtraArgs = "--all";
    }
  );

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

  cargo-doctest = craneLibStable.cargoTest (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoExtraArgs = "--locked --workspace --doc --all-features";
    }
  );

  examples-smoke = craneLibStable.mkCargoDerivation (
    commonArgs
    // {
      inherit cargoArtifacts;
      buildPhaseCargoCommand = ''
        cargo run --locked -p tokio-otp --example trading_engine --features metrics
        cargo run --locked -p tokio-otp --example agent_control --features metrics
        cargo run --locked -p tokio-otp --example supervised_actors
        cargo run --locked -p tokio-otp --example ref_rebind
        cargo run --locked -p tokio-otp --example drain_policy
      '';
      installPhaseCommand = "true";
    }
  );

  cargo-doc = craneLibStable.cargoDoc (
    commonArgs
    // {
      inherit cargoArtifacts;
      cargoExtraArgs = "--locked --workspace --all-features";
      RUSTDOCFLAGS = "-D warnings";
    }
  );
}
