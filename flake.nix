{
  description = "Bridge for synchronizing email and tags between JMAP and notmuch";
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    cargo2nix.url = "github:cargo2nix/cargo2nix/release-0.11.0";
    nixpkgs.follows = "cargo2nix/nixpkgs";
  };

  outputs = { self, nixpkgs, flake-utils, cargo2nix }:
    flake-utils.lib.eachSystem [ "x86_64-linux" "x86_64-darwin" ] (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [cargo2nix.overlays.default];
        };
        rustPkgs = pkgs.rustBuilder.makePackageSet {
          rustVersion = "1.61.0";
          packageFun = import ./Cargo.nix;
        };
      in
      {
        packages = rec {
          mujmap = ((rustPkgs.workspace.mujmap {}).overrideAttrs(oa: {
            propagatedBuildInputs = oa.propagatedBuildInputs ++ [ pkgs.notmuch ];
          })).bin;

          default = mujmap;
        };
      });
}
