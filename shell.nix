{ pkgs ? import <nixpkgs> { } }:

pkgs.mkShell {
  nativeBuildInputs = with pkgs; [
    cargo
    rust-analyzer
    rustc
    rustfmt
  ];

  buildInputs = with pkgs; [
    notmuch
  ];

  shellHook = ''
    export RUST_LOG=mujmap=debug
  '';
}
