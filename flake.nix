{
  description = "Bridge for synchronizing email and tags between JMAP and notmuch";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    rust-overlay,
    advisory-db,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      craneLib = crane.mkLib nixpkgs.legacyPackages.${system};

      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };

      filterMd = path: _type: null != builtins.match ".*md" path;
      filterSrc = path: type: (filterMd path type) || (craneLib.filterCargoSources path type);

      src = pkgs.lib.cleanSourceWith {
        src = craneLib.path ./.; # The original, unfiltered source
        filter = filterSrc;
      };

      buildInputs = with pkgs;
        lib.optionals stdenv.isDarwin [
          darwin.apple_sdk.frameworks.Security
          pkg-config
        ];

      nativeBuildInputs = with pkgs;
        lib.optionals stdenv.isDarwin [
          # darwin-specific inputs
          clang
          libiconv
        ];

      # Build *just* the cargo dependencies, so we can reuse
      # all of that work (e.g. via cachix) when running in CI
      cargoArtifacts = craneLib.buildDepsOnly {inherit src buildInputs nativeBuildInputs;};

      # Build ripsecrets itself, reusing the dependency artifacts from above.
      mujmap = craneLib.buildPackage {
        inherit cargoArtifacts src buildInputs nativeBuildInputs;
        propagatedBuildInputs = [pkgs.notmuch];
        doCheck = false;
        meta = with pkgs.lib; {
          description = "Bridge for synchronizing email and tags between JMAP and notmuch";
          homepage = "https://github.com/elizagamedev/mujmap";
          maintainers = [maintainers.lafrenierejm];
          license = licenses.gpl3;
        };
      };
    in {
      packages = {
        inherit mujmap;
        default = mujmap;
      };

      apps.default = flake-utils.lib.mkApp {drv = mujmap;};

      # `nix flake check`
      checks = {
        audit = craneLib.cargoAudit {inherit src advisory-db;};

        clippy = craneLib.cargoClippy {
          inherit cargoArtifacts src nativeBuildInputs;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        };

        doc = craneLib.cargoDoc {inherit cargoArtifacts src;};

        fmt = craneLib.cargoFmt {inherit src;};

        nextest = craneLib.cargoNextest {
          inherit cargoArtifacts src nativeBuildInputs;
          partitions = 1;
          partitionType = "count";
        };
      };

      # `nix develop`
      devShells.default = pkgs.mkShell {
        inherit buildInputs nativeBuildInputs;
        inputsFrom = builtins.attrValues self.checks;
        packages = with pkgs; [cargo clippy rustc];
      };
    });
}
